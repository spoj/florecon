//! Layer 2 — the incremental min-cost-flow matcher.
//!
//! You describe your domain once via the [`Model`] trait, then drive the
//! engine with three verbs: [`Matcher::upsert`], [`Matcher::remove`], and
//! [`Matcher::solve`]. The matcher owns candidate-arc generation (using a 1-D
//! proximity window over `block_key` plus exact-join `match_keys`) and maps the
//! solved flow back to netted groups. It is the engine behind the `flow`
//! strategy leaf, and — with the `serde` feature — its
//! [`MatcherSnapshot`] warm-starts next month off this month's basis.
//!
//! Currency lives entirely inside your opaque `Tx` type; the engine reads only
//! `base_amount` (the single shared numeraire it conserves) plus whatever your
//! `cost` closure inspects. An "FX reprice" is therefore just an `upsert` with
//! updated lanes — no special verb, no FX table in the engine.

use crate::engine::{Network, NodeId, SolveStatus};
use std::collections::{BTreeMap, HashMap};

/// External, caller-owned identity for a transaction (hash your reference/UUID
/// to a `u64` upstream).
pub type ExtId = u64;

/// Describes how to turn your transactions into a transportation problem.
pub trait Model {
    /// Your opaque per-transaction payload (all currency lanes, dates, refs).
    type Tx;

    /// Signed amount in the single shared numeraire the network conserves.
    /// Positive = receivable/source, negative = payable/sink.
    fn base_amount(&self, tx: &Self::Tx) -> i64;

    /// Cost of leaving this transaction unmatched.
    fn penalty(&self, tx: &Self::Tx) -> f64;

    /// 1-D ordering key used for candidate generation (e.g. GL date in days).
    fn block_key(&self, tx: &Self::Tx) -> i64;

    /// Proximity radius on `block_key`: only pairs within this window become
    /// candidate arcs.
    fn window(&self) -> i64;

    /// Cost of matching source `a` with sink `b`, or `None` to forbid the pair.
    fn cost(&self, a: &Self::Tx, b: &Self::Tx) -> Option<f64>;

    /// Cost hook for lot wrappers that conserve a current residual amount
    /// different from the row's original base amount. Domain models that price
    /// amount-dependent conditions can override this; the default preserves the
    /// legacy whole-row behavior.
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

struct Entry<Tx> {
    node: NodeId,
    tx: Tx,
    key: i64,
    base: i64,
    /// Exact-join keys this transaction is indexed under.
    keys: Vec<u64>,
    /// Real arcs incident to this transaction, by the *other* endpoint's ExtId.
    arcs: Vec<(ExtId, crate::engine::ArcId)>,
}

#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
struct EntrySer<Tx> {
    node: NodeId,
    tx: Tx,
    key: i64,
    base: i64,
    keys: Vec<u64>,
    arcs: Vec<(ExtId, crate::engine::ArcId)>,
}

/// Serializable, persistent state of a [`Matcher`] (the engine basis plus
/// the transaction index). Produce with [`Matcher::snapshot`] and rebuild
/// with [`Matcher::restore`]. Requires the `serde` feature and
/// `Model::Tx: Serialize + Deserialize`.
#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct MatcherSnapshot<Tx> {
    net: crate::engine::Snapshot,
    entries: Vec<(ExtId, EntrySer<Tx>)>,
    by_key: BTreeMap<i64, Vec<ExtId>>,
    by_match_key: HashMap<u64, Vec<ExtId>>,
}

/// A reconciled group: a connected component of matched transactions, in the
/// legacy whole-row view.
#[derive(Debug, Clone)]
pub struct Group {
    pub members: Vec<ExtId>,
    /// Residual in the numeraire; zero means it nets out perfectly.
    pub net_base: i64,
    /// True when the group nets to zero.
    pub clean: bool,
}

/// A signed matched or unmatched quantity allocated to one external row/lot id.
/// Positive amounts come from source lots; negative amounts from sink lots.
/// Also the wire shape a host sends to request a manual group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Allocation {
    pub id: ExtId,
    pub amount: i64,
}

/// A reconciled group in allocation view. Unlike [`Group`], this represents the
/// actual flow routed through matched arcs, so a partially consumed row appears
/// with only the consumed amount and its remainder is returned separately by
/// [`Matcher::unmatched_allocations`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocationGroup {
    pub members: Vec<Allocation>,
    pub net_base: i64,
    pub clean: bool,
}

/// A persistent, incremental reconciler over your `Model`.
pub struct Matcher<M: Model> {
    model: M,
    net: Network,
    entries: HashMap<ExtId, Entry<M::Tx>>,
    /// block_key -> set of ExtIds at that key (for windowed candidate lookup).
    by_key: BTreeMap<i64, Vec<ExtId>>,
    /// exact-join key -> set of ExtIds carrying it (reference/amount bridges).
    by_match_key: HashMap<u64, Vec<ExtId>>,
}

impl<M: Model> Matcher<M> {
    /// Create a fresh reconciler for the given model.
    pub fn new(model: M) -> Self {
        Matcher {
            model,
            net: Network::new(),
            entries: HashMap::new(),
            by_key: BTreeMap::new(),
            by_match_key: HashMap::new(),
        }
    }

    /// Borrow the underlying engine (read-only).
    pub fn network(&self) -> &Network {
        &self.net
    }

    /// Add a new transaction or correct/reprice an existing one. A single verb
    /// covers insert, amount correction, and FX/lane edits; the engine detects
    /// whether the numeraire changed and repairs accordingly.
    pub fn upsert(&mut self, id: ExtId, tx: M::Tx) {
        let base = self.model.base_amount(&tx);
        let key = self.model.block_key(&tx);
        let keys = self.model.match_keys(&tx);

        if self.entries.contains_key(&id) {
            // Drop the old candidate arcs and re-key; we will regenerate.
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
    }

    /// Remove a transaction and all its candidate arcs.
    pub fn remove(&mut self, id: ExtId) {
        if let Some(e) = self.entries.remove(&id) {
            self.unindex_key(e.key, id);
            self.unindex_match_keys(id, &e.keys);
            // Detach mirror references held by neighbors.
            for (other, _) in &e.arcs {
                if let Some(oe) = self.entries.get_mut(other) {
                    oe.arcs.retain(|(x, _)| *x != id);
                }
            }
            self.net.remove_node(e.node);
        }
    }

    /// Re-optimize incrementally and return the netted groups.
    pub fn solve(&mut self) -> SolveStatus {
        self.net.solve()
    }

    /// Total objective of the current solution (matched arc costs plus
    /// unmatched penalties). This is the invariant a warm re-solve preserves
    /// exactly versus a cold rebuild: the optimal *cost* is unique even when
    /// the optimal *matching* is degenerate (equal-cost arcs interchangeable).
    pub fn objective(&self) -> f64 {
        self.net.total_cost()
    }

    /// Total real candidate arcs in the graph (for diagnostics).
    pub fn arc_count(&self) -> usize {
        self.entries.values().map(|e| e.arcs.len()).sum::<usize>() / 2
    }

    /// Compute the reconciled groups from the current solution.
    pub fn groups(&self) -> Vec<Group> {
        // Union-find over matched transactions by ExtId.
        let mut adj: HashMap<ExtId, Vec<ExtId>> = HashMap::new();
        // Map node slot -> ExtId for translating engine matches back.
        let mut slot_to_ext: HashMap<NodeId, ExtId> = HashMap::new();
        for (id, e) in &self.entries {
            slot_to_ext.insert(e.node, *id);
        }
        for (from, to, _flow) in self.net.matches() {
            if let (Some(&a), Some(&b)) = (slot_to_ext.get(&from), slot_to_ext.get(&to)) {
                adj.entry(a).or_default().push(b);
                adj.entry(b).or_default().push(a);
            }
        }

        let mut visited: HashMap<ExtId, bool> = HashMap::new();
        let mut groups = Vec::new();
        for &start in adj.keys() {
            if visited.get(&start).copied().unwrap_or(false) {
                continue;
            }
            let mut stack = vec![start];
            let mut members = Vec::new();
            visited.insert(start, true);
            while let Some(n) = stack.pop() {
                members.push(n);
                if let Some(neighbors) = adj.get(&n) {
                    for &nb in neighbors {
                        if !visited.get(&nb).copied().unwrap_or(false) {
                            visited.insert(nb, true);
                            stack.push(nb);
                        }
                    }
                }
            }
            let net_base: i64 = members.iter().map(|id| self.entries[id].base).sum();
            members.sort_unstable();
            groups.push(Group {
                clean: net_base == 0,
                net_base,
                members,
            });
        }
        groups
    }

    /// ExtIds with no matched arc in the current solution.
    pub fn unmatched(&self) -> Vec<ExtId> {
        self.unmatched_allocations()
            .into_iter()
            .map(|a| a.id)
            .collect()
    }

    /// Matched allocations grouped by connected component of positive-flow real
    /// arcs. This is the lot-level readback: if a row is only partly consumed,
    /// the group contains the consumed amount and the remainder appears in
    /// [`Self::unmatched_allocations`].
    pub fn allocation_groups(&self) -> Vec<AllocationGroup> {
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
            let net_base: i64 = members.iter().map(|a| a.amount).sum();
            groups.push(AllocationGroup {
                clean: net_base == 0,
                net_base,
                members,
            });
        }
        groups
    }

    /// Matched amount plus unmatched remainder per row/lot id. Remainders keep
    /// the sign of the original base amount.
    pub fn unmatched_allocations(&self) -> Vec<Allocation> {
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

        // Collect candidate partners (opposite sign): the proximity window over
        // block_key, plus everyone sharing an exact-join key. Dedup so the two
        // sources can't create duplicate arcs.
        let mut partners: std::collections::HashSet<ExtId> = std::collections::HashSet::new();
        let consider = |this: &Self, other: ExtId, set: &mut std::collections::HashSet<ExtId>| {
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
        // across builds (HashSet iteration order is not stable, and ties in the
        // ambiguous tail would otherwise resolve differently run to run).
        let mut partners: Vec<ExtId> = partners.into_iter().collect();
        partners.sort_unstable();
        for other in partners {
            // Orient source -> sink and cost(source_tx, sink_tx).
            let (src_id, snk_id) = if base > 0 { (id, other) } else { (other, id) };
            let (src_node, snk_node) = if base > 0 {
                (node, self.entries[&other].node)
            } else {
                (self.entries[&other].node, node)
            };
            let cost = {
                let s = &self.entries[&src_id].tx;
                let t = &self.entries[&snk_id].tx;
                self.model.cost(s, t)
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

#[cfg(feature = "serde")]
impl<M: Model> Matcher<M>
where
    M::Tx: Clone + serde::Serialize,
{
    /// Capture the full reconciler state for caching between runs.
    pub fn snapshot(&self) -> MatcherSnapshot<M::Tx> {
        let entries = self
            .entries
            .iter()
            .map(|(id, e)| {
                (
                    *id,
                    EntrySer {
                        node: e.node,
                        tx: e.tx.clone(),
                        key: e.key,
                        base: e.base,
                        keys: e.keys.clone(),
                        arcs: e.arcs.clone(),
                    },
                )
            })
            .collect();
        MatcherSnapshot {
            net: self.net.snapshot(),
            entries,
            by_key: self.by_key.clone(),
            by_match_key: self.by_match_key.clone(),
        }
    }
}

#[cfg(feature = "serde")]
impl<M: Model> Matcher<M> {
    /// Rebuild a reconciler from a snapshot and a (re-supplied) model. Node and
    /// arc handles, the basis, and the ExtId index are all preserved, so the
    /// next `solve` is a warm start.
    pub fn restore(model: M, snap: MatcherSnapshot<M::Tx>) -> Self {
        let entries = snap
            .entries
            .into_iter()
            .map(|(id, e)| {
                (
                    id,
                    Entry {
                        node: e.node,
                        tx: e.tx,
                        key: e.key,
                        base: e.base,
                        keys: e.keys,
                        arcs: e.arcs,
                    },
                )
            })
            .collect();
        Matcher {
            model,
            net: Network::restore(snap.net),
            entries,
            by_key: snap.by_key,
            by_match_key: snap.by_match_key,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct Tx {
        amount: i64,
        date: i64,
    }

    struct Demo;
    impl Model for Demo {
        type Tx = Tx;
        fn base_amount(&self, tx: &Tx) -> i64 {
            tx.amount
        }
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
            let resid = (a.amount + b.amount).abs();
            Some(1.0 + resid as f64 * 0.1 + (a.date - b.date).abs() as f64)
        }
    }

    #[test]
    fn basic_recon() {
        let mut r = Matcher::new(Demo);
        r.upsert(
            1,
            Tx {
                amount: 100,
                date: 0,
            },
        );
        r.upsert(
            2,
            Tx {
                amount: -100,
                date: 1,
            },
        );
        r.solve();
        let groups = r.groups();
        assert_eq!(groups.len(), 1);
        assert!(groups[0].clean);
        assert_eq!(groups[0].members, vec![1, 2]);
    }

    #[test]
    fn streaming_add() {
        let mut r = Matcher::new(Demo);
        r.upsert(
            1,
            Tx {
                amount: 100,
                date: 0,
            },
        );
        r.upsert(
            2,
            Tx {
                amount: -100,
                date: 0,
            },
        );
        r.solve();
        assert_eq!(r.groups().len(), 1);

        // stream more
        r.upsert(
            3,
            Tx {
                amount: 70,
                date: 5,
            },
        );
        r.upsert(
            4,
            Tx {
                amount: -70,
                date: 5,
            },
        );
        r.solve();
        let g = r.groups();
        assert_eq!(g.len(), 2);
        assert!(g.iter().all(|g| g.clean));
    }

    #[test]
    fn allocation_readback_exposes_partial_matches() {
        let mut r = Matcher::new(Demo);
        r.upsert(
            1,
            Tx {
                amount: 100,
                date: 0,
            },
        );
        r.upsert(
            2,
            Tx {
                amount: 200,
                date: 1,
            },
        );
        r.upsert(
            3,
            Tx {
                amount: -250,
                date: 0,
            },
        );
        r.solve();

        let groups = r.allocation_groups();
        assert_eq!(groups.len(), 1);
        assert!(groups[0].clean);
        let mut members = groups[0].members.clone();
        members.sort_by_key(|a| a.id);
        assert_eq!(members.iter().map(|a| a.amount).sum::<i64>(), 0);
        assert_eq!(
            members
                .iter()
                .filter(|a| a.amount > 0)
                .map(|a| a.amount)
                .sum::<i64>(),
            250
        );
        assert_eq!(
            members
                .iter()
                .filter(|a| a.amount < 0)
                .map(|a| a.amount)
                .sum::<i64>(),
            -250
        );
        assert!(
            members
                .iter()
                .all(|a| a.amount.abs() <= r.entries[&a.id].base.abs())
        );
        let rem = r.unmatched_allocations();
        assert_eq!(rem.iter().map(|a| a.amount).sum::<i64>(), 50);
        assert_eq!(rem.len(), 1);
        // The legacy whole-row view still reports connected row ids.
        assert_eq!(r.groups()[0].members, vec![1, 2, 3]);
    }

    #[test]
    fn out_of_window_unmatched() {
        let mut r = Matcher::new(Demo);
        r.upsert(
            1,
            Tx {
                amount: 100,
                date: 0,
            },
        );
        r.upsert(
            2,
            Tx {
                amount: -100,
                date: 100,
            },
        ); // far outside window
        r.solve();
        assert_eq!(r.groups().len(), 0);
        assert_eq!(r.unmatched(), vec![1, 2]);
    }

    #[test]
    fn correction_reprice() {
        let mut r = Matcher::new(Demo);
        r.upsert(
            1,
            Tx {
                amount: 100,
                date: 0,
            },
        );
        r.upsert(
            2,
            Tx {
                amount: -100,
                date: 0,
            },
        );
        r.upsert(
            3,
            Tx {
                amount: -50,
                date: 0,
            },
        );
        r.solve();
        // 1 matches 2 (exact)
        assert!(
            r.groups()
                .iter()
                .any(|g| g.members.contains(&1) && g.members.contains(&2))
        );

        // correct tx 1 down to 50 -> should now prefer matching 3
        r.upsert(
            1,
            Tx {
                amount: 50,
                date: 0,
            },
        );
        r.solve();
        let g = r.groups();
        assert!(
            g.iter()
                .any(|g| g.clean && g.members.contains(&1) && g.members.contains(&3))
        );
    }

    #[test]
    fn remove_tx() {
        let mut r = Matcher::new(Demo);
        r.upsert(
            1,
            Tx {
                amount: 100,
                date: 0,
            },
        );
        r.upsert(
            2,
            Tx {
                amount: -100,
                date: 0,
            },
        );
        r.solve();
        assert_eq!(r.groups().len(), 1);
        r.remove(2);
        r.solve();
        assert_eq!(r.groups().len(), 0);
        assert_eq!(r.unmatched(), vec![1]);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn facade_snapshot_roundtrip() {
        #[derive(Clone, serde::Serialize, serde::Deserialize)]
        struct STx {
            amount: i64,
            date: i64,
        }
        struct SModel;
        impl Model for SModel {
            type Tx = STx;
            fn base_amount(&self, t: &STx) -> i64 {
                t.amount
            }
            fn penalty(&self, _t: &STx) -> f64 {
                1e6
            }
            fn block_key(&self, t: &STx) -> i64 {
                t.date
            }
            fn window(&self) -> i64 {
                3
            }
            fn cost(&self, a: &STx, b: &STx) -> Option<f64> {
                Some(1.0 + (a.amount + b.amount).abs() as f64 * 0.1)
            }
        }

        // "Month 1": match a pair, then cache.
        let mut r = Matcher::new(SModel);
        r.upsert(
            1,
            STx {
                amount: 100,
                date: 0,
            },
        );
        r.upsert(
            2,
            STx {
                amount: -100,
                date: 0,
            },
        );
        r.solve();
        let json = serde_json::to_string(&r.snapshot()).unwrap();

        // "Month 2": restore the cached basis, stream a new pair, warm-solve.
        let snap: MatcherSnapshot<STx> = serde_json::from_str(&json).unwrap();
        let mut r2 = Matcher::restore(SModel, snap);
        assert_eq!(r2.groups().len(), 1); // basis survived the round-trip
        r2.upsert(
            3,
            STx {
                amount: 70,
                date: 1,
            },
        );
        r2.upsert(
            4,
            STx {
                amount: -70,
                date: 1,
            },
        );
        r2.solve();
        let g = r2.groups();
        assert_eq!(g.len(), 2);
        assert!(g.iter().all(|g| g.clean));
    }
}
