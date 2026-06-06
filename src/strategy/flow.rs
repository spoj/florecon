//! The `flow` strategy leaf: the global min-cost-flow arbiter.
//!
//! This is one [`Strategy`](super::Strategy) among many, but a special one — it
//! is the only stateful leaf, keeping a live network-simplex basis warm across
//! solves. You describe your domain once via the [`Model`] trait; the leaf owns
//! candidate-arc generation (a 1-D proximity window over `block_key` plus
//! exact-join `match_keys`) and maps solved flow back to netted [`Group`]s.
//!
//! Currency lives entirely inside your opaque `Tx`: the engine conserves the
//! single shared numeraire carried on each [`Item::amount`](super::Item) and
//! reads only whatever your `cost`/`match_keys`/`block_key` hooks inspect. An
//! "FX reprice" is therefore just a re-`run` with an updated amount — no special
//! verb, no FX table in the engine. Warm vs cold is decided purely by whether
//! the caller keeps the compiled strategy alive between runs.

use super::{Group, Item, Resolution, Strategy};
use crate::engine::{ArcId, Network, NodeId, SolveStatus};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// External, caller-owned identity for a transaction/lot.
pub type ExtId = u64;

/// A signed matched or unmatched quantity allocated to one external row/lot id.
/// Positive amounts come from source lots; negative amounts from sink lots.
/// Also the wire shape a host sends to request a manual group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Allocation {
    pub id: ExtId,
    pub amount: i64,
}

/// Describes how to turn your transactions into a transportation problem. The
/// conserved amount is *not* here — it rides on [`Item::amount`](super::Item),
/// so a residual that an upstream leaf shrank flows through unchanged.
pub trait Model {
    /// Your opaque per-transaction payload (all currency lanes, dates, refs).
    type Tx;

    /// Cost of leaving this transaction unmatched.
    fn penalty(&self, tx: &Self::Tx) -> f64;

    /// 1-D ordering key used for candidate generation (e.g. GL date in days).
    fn block_key(&self, tx: &Self::Tx) -> i64;

    /// Proximity radius on `block_key`: only pairs within this window become
    /// candidate arcs. Negative disables the proximity window (exact-join only).
    fn window(&self) -> i64;

    /// Cost of matching source `a` with sink `b`, or `None` to forbid the pair.
    fn cost(&self, a: &Self::Tx, b: &Self::Tx) -> Option<f64>;

    /// Cost hook for lot wrappers that conserve a current residual amount
    /// different from the row's original amount. Domain models that price
    /// amount-dependent conditions override this; the default ignores the lot
    /// amounts and prices the whole rows.
    fn cost_lot(&self, a: &Self::Tx, _a_amount: i64, b: &Self::Tx, _b_amount: i64) -> Option<f64> {
        self.cost(a, b)
    }

    /// Optional exact-join keys for candidate generation (e.g. hashed reference
    /// tokens). Opposite-sign transactions that share any key become candidate
    /// pairs, *in addition to* the `block_key` proximity window. This is how
    /// non-ordinal signals (a reference that appears in the other book's
    /// description) drive matching. Default: none.
    fn match_keys(&self, _tx: &Self::Tx) -> Vec<u64> {
        Vec::new()
    }

    /// Exact-join keys for a lot whose current residual amount differs from the
    /// original row amount. Defaults to [`Model::match_keys`].
    fn match_keys_lot(&self, tx: &Self::Tx, _amount: i64) -> Vec<u64> {
        self.match_keys(tx)
    }
}

/// Exact-join key buckets larger than this carry no discriminating signal
/// (a reference shared by thousands of rows, or a ubiquitous round amount), so
/// they are skipped during candidate generation to bound work.
const MATCH_BUCKET_CAP: usize = 256;

/// One transaction loaded into the warm engine.
struct Entry<Tx> {
    node: NodeId,
    tx: Tx,
    key: i64,
    base: i64,
    /// Exact-join keys this transaction is indexed under.
    keys: Vec<u64>,
    /// Real arcs incident to this transaction, by the *other* endpoint's ExtId.
    arcs: Vec<(ExtId, ArcId)>,
}

/// The candidate-generation fingerprint of a loaded row. A run re-`upsert`s an
/// id only when this changes, so a no-op recalc touches the engine not at all.
#[derive(Clone, PartialEq, Eq)]
struct FlowSig {
    amount: i64,
    penalty_bits: u64,
    key: i64,
    keys: Vec<u64>,
}

/// The warm min-cost-flow leaf: a live [`Network`] basis, the transaction index
/// that maps it back to `ExtId`s, and the fingerprint of what is loaded. Each
/// `run` applies only the membership/lane delta — upsert changed ids, remove
/// departed ones — then re-solves off the cached basis. Sharding is the
/// caller's job ([`partition_by`](super::partition_by) gives each shard its own
/// `Flow`), so this leaf only ever sees one shard's rows.
struct Flow<M: Model> {
    model: M,
    net: Network,
    entries: HashMap<ExtId, Entry<M::Tx>>,
    /// block_key -> ExtIds at that key (for windowed candidate lookup).
    by_key: BTreeMap<i64, Vec<ExtId>>,
    /// exact-join key -> ExtIds carrying it (reference/amount bridges).
    by_match_key: HashMap<u64, Vec<ExtId>>,
    /// What is currently loaded, by fingerprint, to diff against next run.
    loaded: HashMap<ExtId, FlowSig>,
}

impl<M: Model> Flow<M> {
    fn new(model: M) -> Self {
        Flow {
            model,
            net: Network::new(),
            entries: HashMap::new(),
            by_key: BTreeMap::new(),
            by_match_key: HashMap::new(),
            loaded: HashMap::new(),
        }
    }

    fn flow_sig(&self, item: &Item<M::Tx>) -> FlowSig {
        let amount = item.amount;
        let mut keys = self.model.match_keys_lot(&item.data, amount);
        keys.sort_unstable();
        FlowSig {
            amount,
            penalty_bits: self.model.penalty(&item.data).to_bits(),
            key: self.model.block_key(&item.data),
            keys,
        }
    }

    /// Add a new transaction or correct/reprice an existing one. `base` is the
    /// conserved lot amount (the [`Item::amount`](super::Item)); a single verb
    /// covers insert, amount correction, and lane edits.
    fn upsert(&mut self, id: ExtId, tx: M::Tx, base: i64) {
        let key = self.model.block_key(&tx);
        let keys = self.model.match_keys_lot(&tx, base);

        if self.entries.contains_key(&id) {
            // Drop old candidate arcs and re-key; we will regenerate.
            self.detach_arcs(id);
            let (old_node, old_key, old_base, old_keys) = {
                let e = &self.entries[&id];
                (e.node, e.key, e.base, e.keys.clone())
            };
            if old_key != key {
                self.unindex_key(old_key, id);
                self.by_key.entry(key).or_default().push(id);
            }
            if old_keys != keys {
                self.unindex_match_keys(id, &old_keys);
                self.index_match_keys(id, &keys);
            }
            if old_base != base {
                self.net.set_supply(old_node, base);
            }
            self.net.set_penalty(old_node, self.model.penalty(&tx));
            {
                let e = self.entries.get_mut(&id).unwrap();
                e.tx = tx;
                e.key = key;
                e.base = base;
                e.keys = keys;
            }
            self.generate_arcs(id);
        } else {
            let node = self.net.add_node(base, self.model.penalty(&tx));
            self.by_key.entry(key).or_default().push(id);
            self.index_match_keys(id, &keys);
            self.entries.insert(
                id,
                Entry { node, tx, key, base, keys, arcs: Vec::new() },
            );
            self.generate_arcs(id);
        }
    }

    /// Remove a transaction and all its candidate arcs.
    fn remove(&mut self, id: ExtId) {
        if let Some(e) = self.entries.remove(&id) {
            self.unindex_key(e.key, id);
            self.unindex_match_keys(id, &e.keys);
            for (other, _) in &e.arcs {
                if let Some(oe) = self.entries.get_mut(other) {
                    oe.arcs.retain(|(x, _)| *x != id);
                }
            }
            self.net.remove_node(e.node);
        }
    }

    /// Re-optimize incrementally (warm when a basis was already loaded).
    fn solve(&mut self) -> SolveStatus {
        self.net.solve()
    }

    /// Total objective (matched arc costs plus unmatched penalties). The unique
    /// invariant a warm re-solve preserves exactly versus a cold rebuild.
    fn objective(&self) -> f64 {
        self.net.total_cost()
    }

    /// Total real candidate arcs in the graph (diagnostics only).
    fn arc_count(&self) -> usize {
        self.entries.values().map(|e| e.arcs.len()).sum::<usize>() / 2
    }

    /// Matched allocations grouped by connected component of positive-flow real
    /// arcs, each with its residual net. A partially consumed row appears with
    /// only the consumed amount; its remainder is returned by
    /// [`Self::unmatched_allocations`].
    fn allocation_groups(&self) -> Vec<(Vec<Allocation>, i64)> {
        let (matched_by_id, adj) = self.flow_readback();
        let mut visited: HashMap<ExtId, bool> = HashMap::new();
        let mut groups = Vec::new();
        for &start in adj.keys() {
            if visited.get(&start).copied().unwrap_or(false) {
                continue;
            }
            let mut stack = vec![start];
            let mut ids = Vec::new();
            visited.insert(start, true);
            while let Some(n) = stack.pop() {
                ids.push(n);
                if let Some(neighbors) = adj.get(&n) {
                    for &nb in neighbors {
                        if !visited.get(&nb).copied().unwrap_or(false) {
                            visited.insert(nb, true);
                            stack.push(nb);
                        }
                    }
                }
            }
            ids.sort_unstable();
            let mut members: Vec<Allocation> = ids
                .into_iter()
                .filter_map(|id| {
                    let amount = *matched_by_id.get(&id).unwrap_or(&0);
                    (amount != 0).then_some(Allocation { id, amount })
                })
                .collect();
            members.sort_by_key(|a| a.id);
            let net: i64 = members.iter().map(|a| a.amount).sum();
            groups.push((members, net));
        }
        groups
    }

    /// Matched amount plus unmatched remainder per row/lot id. Remainders keep
    /// the sign of the original base amount.
    fn unmatched_allocations(&self) -> Vec<Allocation> {
        let (matched_by_id, _adj) = self.flow_readback();
        let mut out = Vec::new();
        for (&id, e) in &self.entries {
            let matched = *matched_by_id.get(&id).unwrap_or(&0);
            let rem = e.base - matched;
            if rem != 0 {
                out.push(Allocation { id, amount: rem });
            }
        }
        out.sort_by_key(|a| a.id);
        out
    }

    fn flow_readback(&self) -> (HashMap<ExtId, i64>, HashMap<ExtId, Vec<ExtId>>) {
        let mut slot_to_ext: HashMap<NodeId, ExtId> = HashMap::new();
        for (id, e) in &self.entries {
            slot_to_ext.insert(e.node, *id);
        }
        let mut matched_by_id: HashMap<ExtId, i64> = HashMap::new();
        let mut adj: HashMap<ExtId, Vec<ExtId>> = HashMap::new();
        for (from, to, f) in self.net.matches() {
            if let (Some(&a), Some(&b)) = (slot_to_ext.get(&from), slot_to_ext.get(&to)) {
                let ea = &self.entries[&a];
                let eb = &self.entries[&b];
                let (src, snk) = if ea.base > 0 && eb.base < 0 {
                    (a, b)
                } else if eb.base > 0 && ea.base < 0 {
                    (b, a)
                } else {
                    continue;
                };
                *matched_by_id.entry(src).or_insert(0) += f;
                *matched_by_id.entry(snk).or_insert(0) -= f;
                adj.entry(a).or_default().push(b);
                adj.entry(b).or_default().push(a);
            }
        }
        (matched_by_id, adj)
    }

    // --- candidate generation -------------------------------------------

    fn generate_arcs(&mut self, id: ExtId) {
        let window = self.model.window();
        let (key, base, node, keys) = {
            let e = &self.entries[&id];
            (e.key, e.base, e.node, e.keys.clone())
        };
        if base == 0 {
            return;
        }

        // Candidate partners (opposite sign): the proximity window over
        // block_key, plus everyone sharing an exact-join key. Dedup so the two
        // sources can't create duplicate arcs.
        let mut partners: HashSet<ExtId> = HashSet::new();
        let consider = |this: &Self, other: ExtId, set: &mut HashSet<ExtId>| {
            if other == id {
                return;
            }
            let ob = this.entries[&other].base;
            if (base > 0) == (ob > 0) {
                return; // same sign: not a source/sink pair
            }
            set.insert(other);
        };
        if window >= 0 {
            for (_k, ids) in self.by_key.range(key - window..=key + window) {
                for &other in ids {
                    consider(self, other, &mut partners);
                }
            }
        }
        for k in &keys {
            if let Some(ids) = self.by_match_key.get(k) {
                if ids.len() > MATCH_BUCKET_CAP {
                    continue; // non-discriminating bucket
                }
                for &other in ids {
                    consider(self, other, &mut partners);
                }
            }
        }

        // Add arcs in a deterministic order so the matching is reproducible
        // across builds (HashSet iteration order is not stable).
        let mut partners: Vec<ExtId> = partners.into_iter().collect();
        partners.sort_unstable();
        for other in partners {
            // Orient source -> sink and cost(source, sink) on the lot amounts.
            let (src_id, snk_id) = if base > 0 { (id, other) } else { (other, id) };
            let (src_node, snk_node) = if base > 0 {
                (node, self.entries[&other].node)
            } else {
                (self.entries[&other].node, node)
            };
            let cost = {
                let s = &self.entries[&src_id];
                let t = &self.entries[&snk_id];
                self.model.cost_lot(&s.tx, s.base, &t.tx, t.base)
            };
            if let Some(cost) = cost
                && let Some(arc) = self.net.add_arc(src_node, snk_node, cost)
            {
                self.entries.get_mut(&id).unwrap().arcs.push((other, arc));
                self.entries.get_mut(&other).unwrap().arcs.push((id, arc));
            }
        }
    }

    fn detach_arcs(&mut self, id: ExtId) {
        let arcs = std::mem::take(&mut self.entries.get_mut(&id).unwrap().arcs);
        for (other, arc) in arcs {
            self.net.remove_arc(arc);
            if let Some(oe) = self.entries.get_mut(&other) {
                oe.arcs.retain(|(x, _)| *x != id);
            }
        }
    }

    fn unindex_key(&mut self, key: i64, id: ExtId) {
        if let Some(v) = self.by_key.get_mut(&key) {
            v.retain(|x| *x != id);
            if v.is_empty() {
                self.by_key.remove(&key);
            }
        }
    }

    fn index_match_keys(&mut self, id: ExtId, keys: &[u64]) {
        for &k in keys {
            self.by_match_key.entry(k).or_default().push(id);
        }
    }

    fn unindex_match_keys(&mut self, id: ExtId, keys: &[u64]) {
        for &k in keys {
            if let Some(v) = self.by_match_key.get_mut(&k) {
                v.retain(|x| *x != id);
                if v.is_empty() {
                    self.by_match_key.remove(&k);
                }
            }
        }
    }
}

impl<M> Strategy<M::Tx> for Flow<M>
where
    M: Model + Clone,
    M::Tx: Clone,
{
    fn run(&mut self, bag: Vec<Item<M::Tx>>) -> Resolution<M::Tx> {
        #[cfg(not(target_arch = "wasm32"))]
        let timed = std::env::var_os("FLORECON_TIME").is_some();
        #[cfg(target_arch = "wasm32")]
        let timed = false;
        let want: BTreeSet<ExtId> = bag.iter().map(|i| i.id).collect();
        // id -> lot reference (no clones unless we actually upsert). The leaf
        // conserves the lot's *current* amount, so partial residuals compose
        // through `seq`.
        let data: HashMap<ExtId, &Item<M::Tx>> = bag.iter().map(|i| (i.id, i)).collect();
        let sigs: HashMap<ExtId, FlowSig> = bag.iter().map(|i| (i.id, self.flow_sig(i))).collect();

        // Diff want vs loaded. Upsert new ids and same-id rows whose amount or
        // candidate signature changed; this keeps warm lot recalc correct when
        // an upstream step shrinks the residual of an id that remains present.
        let mut upserts: Vec<ExtId> = sigs
            .iter()
            .filter_map(|(&id, sig)| (self.loaded.get(&id) != Some(sig)).then_some(id))
            .collect();
        upserts.sort_by_key(|&id| flow_upsert_rank(id));
        let drops: Vec<ExtId> = self
            .loaded
            .keys()
            .copied()
            .filter(|id| !want.contains(id))
            .collect();

        let tb = timed.then(std::time::Instant::now);
        for id in upserts {
            if let Some(item) = data.get(&id) {
                self.upsert(id, item.data.clone(), item.amount);
            }
        }
        for id in drops {
            self.remove(id);
        }
        let build = tb.map(|t| t.elapsed().as_secs_f64() * 1000.0);
        let ts = timed.then(std::time::Instant::now);
        let status = self.solve(); // warm when a basis was already loaded.
        if let (Some(build), Some(ts)) = (build, ts) {
            eprintln!(
                "    flow: delta {build:>6.1} ms ({} arcs), solve {:>6.1} ms",
                self.arc_count(),
                ts.elapsed().as_secs_f64() * 1000.0,
            );
        }
        debug_assert_eq!(status, SolveStatus::Optimal);
        self.loaded = sigs;

        // Determinism guard: in debug (or under FLORECON_VERIFY_WARM) rebuild a
        // fresh cold leaf on the same id set and assert equal *objective*. A
        // min-cost-flow optimum is unique in cost but can be degenerate in which
        // equal-cost arcs carry flow, so we assert the objective (the real
        // failure mode is a warm re-solve drifting to a worse objective), not
        // the grouping (see the `warm_flow_matches_cold_*` equivalence tests).
        if cfg!(debug_assertions) || std::env::var_os("FLORECON_VERIFY_WARM").is_some() {
            let mut cold = Flow::new(self.model.clone());
            let mut ids: Vec<ExtId> = data.keys().copied().collect();
            ids.sort_unstable();
            for id in ids {
                if let Some(item) = data.get(&id) {
                    cold.upsert(id, item.data.clone(), item.amount);
                }
            }
            cold.solve();
            let (warm_obj, cold_obj) = (self.objective(), cold.objective());
            assert!(
                (warm_obj - cold_obj).abs() < 1e-6,
                "warm flow solve diverged from a fresh cold rebuild: \
                 warm objective {warm_obj} != cold objective {cold_obj}"
            );
        }

        let groups = self
            .allocation_groups()
            .into_iter()
            .map(|(members, net)| Group {
                members,
                origin: "flow".to_string(),
                net,
                reason: Some("min-cost flow".to_string()),
            })
            .collect();
        let unmatched: HashMap<ExtId, i64> = self
            .unmatched_allocations()
            .into_iter()
            .map(|a| (a.id, a.amount))
            .collect();
        let residual = bag
            .into_iter()
            .filter_map(|mut i| {
                unmatched.get(&i.id).map(|&amount| {
                    i.amount = amount;
                    i
                })
            })
            .collect();
        Resolution { groups, residual }
    }
}

/// The global arbiter: hand the residual to the min-cost-flow engine, which
/// resolves competing candidates into one consistent grouping. This is where
/// *proposing* signals (reference + amount + date, via the [`Model`]) become a
/// committed partition. The returned leaf is *stateful* — it keeps its basis
/// warm across solves — but that is invisible to the caller: a one-shot solve
/// just runs it once.
pub fn flow<M>(model: M) -> Box<dyn Strategy<M::Tx>>
where
    M: Model + Clone + 'static,
    M::Tx: Clone + 'static,
{
    Box::new(Flow::new(model))
}

/// Stable, well-mixed upsert order (SplitMix64 over the id), so the ambiguous
/// tail of equal-cost arcs resolves identically run to run regardless of the
/// host's feed order.
fn flow_upsert_rank(id: ExtId) -> u64 {
    let mut z = id.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

// ---------------------------------------------------------------------------
// Tests — drive the leaf through the `Strategy` interface it exposes.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct Tx {
        date: i64,
    }

    #[derive(Clone)]
    struct Demo;
    impl Model for Demo {
        type Tx = Tx;
        fn penalty(&self, _tx: &Tx) -> f64 {
            1_000_000.0
        }
        fn block_key(&self, tx: &Tx) -> i64 {
            tx.date
        }
        fn window(&self) -> i64 {
            3
        }
        fn cost(&self, a: &Tx, b: &Tx) -> Option<f64> {
            Some(1.0 + (a.date - b.date).abs() as f64)
        }
        fn cost_lot(&self, a: &Tx, a_amt: i64, b: &Tx, b_amt: i64) -> Option<f64> {
            // Prefer the cleaner net: penalize leftover residual, like a real model.
            Some(1.0 + (a_amt + b_amt).abs() as f64 * 0.1 + (a.date - b.date).abs() as f64)
        }
    }

    fn item(id: ExtId, amount: i64, date: i64) -> Item<Tx> {
        Item::new(id, amount, Tx { date })
    }

    fn ids(g: &Group) -> Vec<ExtId> {
        g.member_ids()
    }

    #[test]
    fn basic_recon() {
        let mut s = flow(Demo);
        let r = s.run(vec![item(1, 100, 0), item(2, -100, 1)]);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].net, 0); // clean
        assert_eq!(ids(&r.groups[0]), vec![1, 2]);
        assert!(r.residual.is_empty());
    }

    #[test]
    fn streaming_add_is_warm() {
        let mut s = flow(Demo);
        let r = s.run(vec![item(1, 100, 0), item(2, -100, 0)]);
        assert_eq!(r.groups.len(), 1);
        // Stream a second pair into the same (warm) leaf.
        let r = s.run(vec![
            item(1, 100, 0),
            item(2, -100, 0),
            item(3, 70, 5),
            item(4, -70, 5),
        ]);
        assert_eq!(r.groups.len(), 2);
        assert!(r.groups.iter().all(|g| g.net == 0));
    }

    #[test]
    fn allocation_readback_exposes_partial_matches() {
        let mut s = flow(Demo);
        let r = s.run(vec![item(1, 100, 0), item(2, 200, 1), item(3, -250, 0)]);
        assert_eq!(r.groups.len(), 1);
        let g = &r.groups[0];
        assert_eq!(g.net, 0);
        assert_eq!(g.members.iter().map(|a| a.amount).sum::<i64>(), 0);
        assert_eq!(
            g.members.iter().filter(|a| a.amount > 0).map(|a| a.amount).sum::<i64>(),
            250
        );
        assert_eq!(ids(g), vec![1, 2, 3]);
        // 50 of the 300 source remains unmatched.
        assert_eq!(r.residual.iter().map(|i| i.amount).sum::<i64>(), 50);
        assert_eq!(r.residual.len(), 1);
    }

    #[test]
    fn out_of_window_unmatched() {
        let mut s = flow(Demo);
        let r = s.run(vec![item(1, 100, 0), item(2, -100, 100)]); // far apart
        assert_eq!(r.groups.len(), 0);
        let mut rem: Vec<ExtId> = r.residual.iter().map(|i| i.id).collect();
        rem.sort_unstable();
        assert_eq!(rem, vec![1, 2]);
    }

    #[test]
    fn correction_reprice_is_warm() {
        let mut s = flow(Demo);
        let r = s.run(vec![item(1, 100, 0), item(2, -100, 0), item(3, -50, 0)]);
        assert!(r.groups.iter().any(|g| ids(g).contains(&1) && ids(g).contains(&2)));
        // Correct id 1 down to 50 -> now prefers matching id 3.
        let r = s.run(vec![item(1, 50, 0), item(2, -100, 0), item(3, -50, 0)]);
        assert!(r.groups.iter().any(|g| g.net == 0 && ids(g).contains(&1) && ids(g).contains(&3)));
    }

    #[test]
    fn remove_is_warm() {
        let mut s = flow(Demo);
        let r = s.run(vec![item(1, 100, 0), item(2, -100, 0)]);
        assert_eq!(r.groups.len(), 1);
        // Drop id 2 from the bag; the warm leaf removes it and re-solves.
        let r = s.run(vec![item(1, 100, 0)]);
        assert_eq!(r.groups.len(), 0);
        assert_eq!(r.residual.iter().map(|i| i.id).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn lot_cost_sees_residual_amount() {
        // A model whose cost depends on the lot amounts: forbid matching unless
        // the residual magnitudes are equal. Exercises cost_lot threading.
        #[derive(Clone)]
        struct LotModel;
        impl Model for LotModel {
            type Tx = Tx;
            fn penalty(&self, _t: &Tx) -> f64 {
                1e9
            }
            fn block_key(&self, t: &Tx) -> i64 {
                t.date
            }
            fn window(&self) -> i64 {
                5
            }
            fn cost(&self, _a: &Tx, _b: &Tx) -> Option<f64> {
                Some(1.0)
            }
            fn cost_lot(&self, _a: &Tx, a_amt: i64, _b: &Tx, b_amt: i64) -> Option<f64> {
                (a_amt.abs() == b_amt.abs()).then_some(1.0)
            }
        }
        let mut s = flow(LotModel);
        let r = s.run(vec![item(1, 100, 0), item(2, -100, 0)]);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].net, 0);
    }
}
