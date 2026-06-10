//! The `flow` strategy leaf: the global min-cost-flow arbiter.
//!
//! This is one [`Strategy`](super::Strategy) among many. You describe your
//! domain once via a [`FlowSpec`] (closures for
//! penalty / block_key / window / match_keys / cost); the leaf owns
//! candidate-arc generation (a 1-D proximity window over `block_key` plus
//! exact-join `match_keys`) and maps solved flow back to netted [`Group`]s.
//!
//! Currency lives entirely inside your opaque payload `E`: the engine conserves
//! the single shared numeraire carried on each [`Item::amount`](super::Item) and
//! reads only whatever your `cost`/`match_keys`/`block_key` closures inspect. An
//! "FX reprice" is therefore just a re-`run` with an updated amount — no special
//! verb, no FX table in the engine. The leaf is stateless: each `run` builds the
//! network cold from the bag and solves it.
use super::{Group, Item, Resolution, Strategy};
use crate::engine::{ArcId, Network, NodeId, SolveStatus};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

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

/// Describes how to turn your payloads `E` into a transportation problem: the
/// five hooks the [`flow`] leaf needs, each a closure consistent with the rest
/// of the strategy algebra. Build it with the chained setters from
/// [`FlowSpec::new`]. The conserved amount is *not* here — it rides on
/// [`Item::amount`](super::Item), so a residual an upstream leaf shrank flows
/// through unchanged.
///
/// Closures live behind `Arc`, so `FlowSpec` is cheaply `Clone` (each `run`
/// clones the spec into a fresh cold build).
///
/// ```ignore
/// flow(
///     FlowSpec::new()
///         .window(15)
///         .penalty(1000.0)
///         .block_key(|r: &Row| r.day)
///         .match_keys(|r| r.tokens.clone())
///         .cost(|a, b| (a.amount == -b.amount).then_some(1.0)),
/// )
/// ```
/// Lot-aware exact-join key hook: `(payload, residual amount) -> keys`.
type MatchKeysFn<E> = dyn Fn(&E, i64) -> Vec<u64>;
/// Lot-aware pair-cost hook: `(src, src amount, snk, snk amount) -> cost`.
type CostFn<E> = dyn Fn(&E, i64, &E, i64) -> Option<f64>;

pub struct FlowSpec<E> {
    /// Cost of leaving a lot unmatched.
    penalty: Arc<dyn Fn(&E) -> f64>,
    /// 1-D ordering key used for candidate generation (e.g. GL date in days).
    block_key: Arc<dyn Fn(&E) -> i64>,
    /// Proximity radius on `block_key`: only pairs within this window become
    /// candidate arcs. Negative disables the proximity window (exact-join only).
    window: i64,
    /// Exact-join keys (hashed reference tokens, amount bridges). Opposite-sign
    /// lots sharing any key become candidate pairs, *in addition to* the
    /// `block_key` proximity window. Lot-aware: receives the current residual
    /// amount so amount bridges track partial matches.
    match_keys: Arc<MatchKeysFn<E>>,
    /// Cost of matching source `a` (amount `a_amt`) with sink `b` (amount
    /// `b_amt`), or `None` to forbid the pair. Lot-aware so amount-dependent
    /// conditions can price the current residual rather than the whole row.
    cost: Arc<CostFn<E>>,
}

impl<E> Clone for FlowSpec<E> {
    fn clone(&self) -> Self {
        FlowSpec {
            penalty: self.penalty.clone(),
            block_key: self.block_key.clone(),
            window: self.window,
            match_keys: self.match_keys.clone(),
            cost: self.cost.clone(),
        }
    }
}

impl<E> Default for FlowSpec<E> {
    /// Penalty 0, block_key 0, window -1 (exact-join only), no match keys, and a
    /// `cost` that forbids every pair. A usable spec sets at least `cost`.
    fn default() -> Self {
        FlowSpec {
            penalty: Arc::new(|_| 0.0),
            block_key: Arc::new(|_| 0),
            window: -1,
            match_keys: Arc::new(|_, _| Vec::new()),
            cost: Arc::new(|_, _, _, _| None),
        }
    }
}

impl<E> FlowSpec<E> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Constant cost of leaving a lot unmatched.
    pub fn penalty(mut self, p: f64) -> Self {
        self.penalty = Arc::new(move |_| p);
        self
    }

    /// Per-lot unmatched penalty (when it varies by payload).
    pub fn penalty_fn(mut self, f: impl Fn(&E) -> f64 + 'static) -> Self {
        self.penalty = Arc::new(f);
        self
    }

    /// Proximity radius on `block_key`; negative = exact-join only.
    pub fn window(mut self, w: i64) -> Self {
        self.window = w;
        self
    }

    /// 1-D ordering key for the proximity window.
    pub fn block_key(mut self, f: impl Fn(&E) -> i64 + 'static) -> Self {
        self.block_key = Arc::new(f);
        self
    }

    /// Amount-independent exact-join keys (the common case).
    pub fn match_keys(mut self, f: impl Fn(&E) -> Vec<u64> + 'static) -> Self {
        self.match_keys = Arc::new(move |e, _amount| f(e));
        self
    }

    /// Lot-aware exact-join keys (when a key depends on the residual amount,
    /// e.g. an `AMT:<n>` bridge).
    pub fn match_keys_lot(mut self, f: impl Fn(&E, i64) -> Vec<u64> + 'static) -> Self {
        self.match_keys = Arc::new(f);
        self
    }

    /// Amount-independent pair cost (the common case); `None` forbids the pair.
    pub fn cost(mut self, f: impl Fn(&E, &E) -> Option<f64> + 'static) -> Self {
        self.cost = Arc::new(move |a, _aa, b, _bb| f(a, b));
        self
    }

    /// Lot-aware pair cost: prices the current residual amounts of `a` and `b`.
    pub fn cost_lot(mut self, f: impl Fn(&E, i64, &E, i64) -> Option<f64> + 'static) -> Self {
        self.cost = Arc::new(f);
        self
    }
}

/// Exact-join key buckets larger than this carry no discriminating signal
/// (a reference shared by thousands of rows, or a ubiquitous round amount), so
/// they are skipped during candidate generation to bound work.
const MATCH_BUCKET_CAP: usize = 256;

/// One transaction loaded into a flow build.
struct Entry<E> {
    node: NodeId,
    tx: E,
    key: i64,
    base: i64,
    /// Exact-join keys this transaction is indexed under.
    keys: Vec<u64>,
    /// Real arcs incident to this transaction, by the *other* endpoint's ExtId.
    arcs: Vec<(ExtId, ArcId)>,
}

/// The transient min-cost-flow build for a single `run`: a fresh [`Network`],
/// the transaction index that maps it back to `ExtId`s, and the candidate
/// lookups. Built cold from the bag every solve -- the leaf holds no state, so
/// there is no warm basis to keep and no diff to maintain. Sharding is the
/// caller's job ([`partition_by`](super::partition_by) hands each shard its own
/// bag), so this build only ever sees one shard's rows.
struct FlowRun<E> {
    spec: FlowSpec<E>,
    net: Network,
    entries: HashMap<ExtId, Entry<E>>,
    /// block_key -> ExtIds at that key (for windowed candidate lookup).
    by_key: BTreeMap<i64, Vec<ExtId>>,
    /// exact-join key -> ExtIds carrying it (reference/amount bridges).
    by_match_key: HashMap<u64, Vec<ExtId>>,
}

impl<E> FlowRun<E> {
    fn new(spec: FlowSpec<E>) -> Self {
        FlowRun {
            spec,
            net: Network::new(),
            entries: HashMap::new(),
            by_key: BTreeMap::new(),
            by_match_key: HashMap::new(),
        }
    }

    /// Add a transaction to the build. `base` is the conserved lot amount (the
    /// [`Item::amount`](super::Item)); each id is inserted exactly once.
    fn insert(&mut self, id: ExtId, tx: E, base: i64) {
        let key = (self.spec.block_key)(&tx);
        let keys = (self.spec.match_keys)(&tx, base);
        let node = self.net.add_node(base, (self.spec.penalty)(&tx));
        self.by_key.entry(key).or_default().push(id);
        self.index_match_keys(id, &keys);
        self.entries.insert(
            id,
            Entry {
                node,
                tx,
                key,
                base,
                keys,
                arcs: Vec::new(),
            },
        );
        self.generate_arcs(id);
    }

    /// Total real candidate arcs in the graph (diagnostics only).
    fn arc_count(&self) -> usize {
        self.entries.values().map(|e| e.arcs.len()).sum::<usize>() / 2
    }

    /// Positive-flow real arcs as `(source id, sink id, amount)`, source being
    /// the positive-base side. This is flow's primitive output: the **matching
    /// itself**, one edge per arc -- not a settlement view. Grouping arcs into
    /// connected-component settlements is [`super::coalesce`]'s job (and is what
    /// [`super::reclaim`] composes on top).
    ///
    /// Sorted canonically by `(src, snk, amount)` so the readback is stable run
    /// to run, independent of this build's internal arc-vec layout (insertion
    /// order can change which equal-cost arcs carry flow at a degenerate
    /// optimum; the optimum is unique in cost, not in arc selection).
    fn matched_arcs(&self) -> Vec<(ExtId, ExtId, i64)> {
        let mut slot_to_ext: HashMap<NodeId, ExtId> = HashMap::new();
        for (id, e) in &self.entries {
            slot_to_ext.insert(e.node, *id);
        }
        let mut arcs: Vec<(ExtId, ExtId, i64)> = Vec::new();
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
                arcs.push((src, snk, f));
            }
        }
        arcs.sort_unstable();
        arcs
    }

    /// Matched mass per id (positive on sources, negative on sinks), summed over
    /// every arc. Used only to compute each row's unmatched remainder.
    fn matched_by_id(&self) -> HashMap<ExtId, i64> {
        let mut m: HashMap<ExtId, i64> = HashMap::new();
        for (src, snk, f) in self.matched_arcs() {
            *m.entry(src).or_insert(0) += f;
            *m.entry(snk).or_insert(0) -= f;
        }
        m
    }

    /// Matched amount plus unmatched remainder per row/lot id. Remainders keep
    /// the sign of the original base amount.
    fn unmatched_allocations(&self) -> Vec<Allocation> {
        let matched_by_id = self.matched_by_id();
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

    // --- candidate generation -------------------------------------------

    fn generate_arcs(&mut self, id: ExtId) {
        let window = self.spec.window;
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
                (self.spec.cost)(&s.tx, s.base, &t.tx, t.base)
            };
            if let Some(cost) = cost
                && let Some(arc) = self.net.add_arc(src_node, snk_node, cost)
            {
                self.entries.get_mut(&id).unwrap().arcs.push((other, arc));
                self.entries.get_mut(&other).unwrap().arcs.push((id, arc));
            }
        }
    }

    fn index_match_keys(&mut self, id: ExtId, keys: &[u64]) {
        for &k in keys {
            self.by_match_key.entry(k).or_default().push(id);
        }
    }
}

/// The stateless min-cost-flow leaf: it owns only the [`FlowSpec`]. Every `run`
/// builds a fresh [`FlowRun`] from the bag, solves cold, and reads the matching
/// back -- no warm basis, no cross-call state, so shards never interfere and
/// repeated solves are trivially reproducible.
struct Flow<E> {
    spec: FlowSpec<E>,
}

impl<E> Strategy<E> for Flow<E>
where
    E: Clone,
{
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        #[cfg(not(target_arch = "wasm32"))]
        let timed = std::env::var_os("FLORECON_TIME").is_some();
        #[cfg(target_arch = "wasm32")]
        let timed = false;

        // Build the network cold. Insert in a stable, well-mixed id order so the
        // ambiguous tail of equal-cost arcs resolves identically run to run,
        // independent of the host's feed order.
        let mut order: Vec<&Item<E>> = bag.iter().collect();
        order.sort_by_key(|i| flow_upsert_rank(i.id));
        let mut run = FlowRun::new(self.spec.clone());
        let tb = timed.then(std::time::Instant::now);
        for item in order {
            run.insert(item.id, item.data.clone(), item.amount);
        }
        let build = tb.map(|t| t.elapsed().as_secs_f64() * 1000.0);

        let ts = timed.then(std::time::Instant::now);
        let status = run.net.solve();
        if let (Some(build), Some(ts)) = (build, ts) {
            eprintln!(
                "    flow: build {build:>6.1} ms ({} arcs), solve {:>6.1} ms",
                run.arc_count(),
                ts.elapsed().as_secs_f64() * 1000.0,
            );
        }
        debug_assert_eq!(status, SolveStatus::Optimal);

        let groups = run
            .matched_arcs()
            .into_iter()
            .map(|(src, snk, f)| Group {
                members: vec![
                    Allocation { id: src, amount: f },
                    Allocation {
                        id: snk,
                        amount: -f,
                    },
                ],
                origin: "flow".to_string(),
                net: 0,
                reason: Some("min-cost flow".to_string()),
            })
            .collect();
        let unmatched: HashMap<ExtId, i64> = run
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
/// resolves competing candidates into one consistent matching. This is where
/// *proposing* signals (reference + amount + date, via the [`FlowSpec`]) become
/// committed **arcs**.
///
/// `flow` is a strict primitive node: it returns the matching at its most
/// atomic -- **one two-member group per positive-flow arc** (`{source: +f,
/// sink: -f}`, net 0), plus the residual -- and nothing else. It deliberately
/// does *not* fold those arcs into settlement clusters; grouping is a separate
/// concern owned by the composition layer ([`super::coalesce`], and the
/// [`super::reclaim`] sugar built on it). Reaching for bare `flow`
/// means you want the raw edges (per-arc reshaping, who-matched-whom analysis);
/// note they carry the optimizer's optimal-face degeneracy that aggregation
/// collapses, so canonical run-to-run identity comes only after coalescing.
pub fn flow<E>(spec: FlowSpec<E>) -> Box<dyn Strategy<E>>
where
    E: Clone + 'static,
{
    Box::new(Flow { spec })
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

    /// The demo spec: a date-proximity window with a lot-aware cost that prefers
    /// the cleaner net (penalizing leftover residual), like a real model.
    fn demo() -> FlowSpec<Tx> {
        FlowSpec::new()
            .penalty(1_000_000.0)
            .window(3)
            .block_key(|tx: &Tx| tx.date)
            .cost_lot(|a: &Tx, a_amt, b: &Tx, b_amt| {
                Some(1.0 + (a_amt + b_amt).abs() as f64 * 0.1 + (a.date - b.date).abs() as f64)
            })
    }

    fn item(id: ExtId, amount: i64, date: i64) -> Item<Tx> {
        Item::new(id, amount, Tx { date })
    }

    fn ids(g: &Group) -> Vec<ExtId> {
        g.member_ids()
    }

    #[test]
    fn basic_recon() {
        let s = flow(demo());
        let r = s.run(vec![item(1, 100, 0), item(2, -100, 1)]);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].net, 0); // clean
        assert_eq!(ids(&r.groups[0]), vec![1, 2]);
        assert!(r.residual.is_empty());
    }

    #[test]
    fn bare_flow_emits_raw_arcs_not_settlements() {
        // A partial-match shape: id 3 (-250) draws from id 1 (100) and id 2
        // (200). Bare `flow` exposes that as *two arcs* (the primitive
        // matching); `coalesce` folds them into one {1,2,3} settlement.
        let s = flow(demo());
        let r = s.run(vec![item(1, 100, 0), item(2, 200, 1), item(3, -250, 0)]);
        assert_eq!(r.groups.len(), 2, "one group per positive-flow arc");
        // Every arc is a clean two-member, net-0 edge sharing the -250 sink.
        assert!(r.groups.iter().all(|g| g.members.len() == 2 && g.net == 0));
        assert!(r.groups.iter().all(|g| ids(g).contains(&3)));
        let matched: i64 = r
            .groups
            .iter()
            .flat_map(|g| &g.members)
            .filter(|a| a.amount > 0)
            .map(|a| a.amount)
            .sum();
        assert_eq!(matched, 250);
        // Residual is identical to the settled view: 50 of the 300 unmatched.
        assert_eq!(r.residual.iter().map(|i| i.amount).sum::<i64>(), 50);
        assert_eq!(r.residual.len(), 1);
    }

    #[test]
    fn streaming_add_re_solves() {
        let s = flow(demo());
        let r = s.run(vec![item(1, 100, 0), item(2, -100, 0)]);
        assert_eq!(r.groups.len(), 1);
        // Re-run with a second pair added: the stateless leaf rebuilds and solves.
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
    fn out_of_window_unmatched() {
        let s = flow(demo());
        let r = s.run(vec![item(1, 100, 0), item(2, -100, 100)]); // far apart
        assert_eq!(r.groups.len(), 0);
        let mut rem: Vec<ExtId> = r.residual.iter().map(|i| i.id).collect();
        rem.sort_unstable();
        assert_eq!(rem, vec![1, 2]);
    }

    #[test]
    fn correction_reprice_re_solves() {
        let s = flow(demo());
        let r = s.run(vec![item(1, 100, 0), item(2, -100, 0), item(3, -50, 0)]);
        assert!(
            r.groups
                .iter()
                .any(|g| ids(g).contains(&1) && ids(g).contains(&2))
        );
        // Correct id 1 down to 50 -> now prefers matching id 3.
        let r = s.run(vec![item(1, 50, 0), item(2, -100, 0), item(3, -50, 0)]);
        assert!(
            r.groups
                .iter()
                .any(|g| g.net == 0 && ids(g).contains(&1) && ids(g).contains(&3))
        );
    }

    #[test]
    fn remove_re_solves() {
        let s = flow(demo());
        let r = s.run(vec![item(1, 100, 0), item(2, -100, 0)]);
        assert_eq!(r.groups.len(), 1);
        // Drop id 2 from the bag; the next run rebuilds without it.
        let r = s.run(vec![item(1, 100, 0)]);
        assert_eq!(r.groups.len(), 0);
        assert_eq!(r.residual.iter().map(|i| i.id).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn lot_cost_sees_residual_amount() {
        // A spec whose cost depends on the lot amounts: forbid matching unless
        // the residual magnitudes are equal. Exercises cost_lot threading.
        let spec = FlowSpec::new()
            .penalty(1e9)
            .window(5)
            .block_key(|t: &Tx| t.date)
            .cost_lot(|_a: &Tx, a_amt, _b: &Tx, b_amt| (a_amt.abs() == b_amt.abs()).then_some(1.0));
        let s = flow(spec);
        let r = s.run(vec![item(1, 100, 0), item(2, -100, 0)]);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].net, 0);
    }
}
