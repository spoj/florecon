//! Layer 2 — the reconciliation facade.
//!
//! You describe your domain once via the [`Model`] trait, then drive the
//! engine with three verbs: [`Reconciler::upsert`], [`Reconciler::remove`],
//! and [`Reconciler::solve`]. The facade owns candidate-arc generation (using
//! a 1-D proximity window over `block_key`) and maps results back to netted
//! groups.
//!
//! Currency lives entirely inside your opaque `Tx` type; the engine reads only
//! `base_amount` (the single shared numeraire it conserves) plus whatever your
//! `cost` closure inspects. An "FX reprice" is therefore just an `upsert` with
//! updated lanes — no special verb, no FX table in the engine.

use crate::net::{NodeId, Network, SolveStatus};
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
}

struct Entry<Tx> {
    node: NodeId,
    tx: Tx,
    key: i64,
    base: i64,
    /// Real arcs incident to this transaction, by the *other* endpoint's ExtId.
    arcs: Vec<(ExtId, crate::net::ArcId)>,
}

#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
struct EntrySer<Tx> {
    node: NodeId,
    tx: Tx,
    key: i64,
    base: i64,
    arcs: Vec<(ExtId, crate::net::ArcId)>,
}

/// Serializable, persistent state of a [`Reconciler`] (the engine basis plus
/// the transaction index). Produce with [`Reconciler::snapshot`] and rebuild
/// with [`Reconciler::restore`]. Requires the `serde` feature and
/// `Model::Tx: Serialize + Deserialize`.
#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ReconSnapshot<Tx> {
    net: crate::net::Snapshot,
    entries: Vec<(ExtId, EntrySer<Tx>)>,
    by_key: BTreeMap<i64, Vec<ExtId>>,
}

/// A reconciled group: a connected component of matched transactions.
#[derive(Debug, Clone)]
pub struct Group {
    pub members: Vec<ExtId>,
    /// Residual in the numeraire; zero means it nets out perfectly.
    pub net_base: i64,
    /// True when the group nets to zero.
    pub clean: bool,
}

/// A persistent, incremental reconciler over your `Model`.
pub struct Reconciler<M: Model> {
    model: M,
    net: Network,
    entries: HashMap<ExtId, Entry<M::Tx>>,
    /// block_key -> set of ExtIds at that key (for windowed candidate lookup).
    by_key: BTreeMap<i64, Vec<ExtId>>,
}

impl<M: Model> Reconciler<M> {
    /// Create a fresh reconciler for the given model.
    pub fn new(model: M) -> Self {
        Reconciler {
            model,
            net: Network::new(),
            entries: HashMap::new(),
            by_key: BTreeMap::new(),
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

        if self.entries.contains_key(&id) {
            // Drop the old candidate arcs and re-key; we will regenerate.
            self.detach_arcs(id);
            let (old_node, old_key, old_base) = {
                let e = &self.entries[&id];
                (e.node, e.key, e.base)
            };
            if old_key != key {
                self.unindex_key(old_key, id);
                self.by_key.entry(key).or_default().push(id);
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
            }
            self.generate_arcs(id);
        } else {
            let node = self.net.add_node(base, self.model.penalty(&tx));
            self.by_key.entry(key).or_default().push(id);
            self.entries.insert(
                id,
                Entry {
                    node,
                    tx,
                    key,
                    base,
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
        let mut matched: HashMap<NodeId, bool> = HashMap::new();
        for (from, to, _) in self.net.matches() {
            matched.insert(from, true);
            matched.insert(to, true);
        }
        let mut out: Vec<ExtId> = self
            .entries
            .iter()
            .filter(|(_, e)| !matched.get(&e.node).copied().unwrap_or(false))
            .map(|(id, _)| *id)
            .collect();
        out.sort_unstable();
        out
    }

    // --- candidate generation -------------------------------------------

    fn generate_arcs(&mut self, id: ExtId) {
        let window = self.model.window();
        let (key, base, node) = {
            let e = &self.entries[&id];
            (e.key, e.base, e.node)
        };
        if base == 0 {
            return;
        }

        // Collect candidate partner ExtIds within the window and opposite sign.
        let mut partners: Vec<ExtId> = Vec::new();
        for (_k, ids) in self.by_key.range(key - window..=key + window) {
            for &other in ids {
                if other == id {
                    continue;
                }
                let ob = self.entries[&other].base;
                if (base > 0) == (ob > 0) {
                    continue; // same sign: not a source/sink pair
                }
                partners.push(other);
            }
        }

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
}

#[cfg(feature = "serde")]
impl<M: Model> Reconciler<M>
where
    M::Tx: Clone + serde::Serialize,
{
    /// Capture the full reconciler state for caching between runs.
    pub fn snapshot(&self) -> ReconSnapshot<M::Tx> {
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
                        arcs: e.arcs.clone(),
                    },
                )
            })
            .collect();
        ReconSnapshot {
            net: self.net.snapshot(),
            entries,
            by_key: self.by_key.clone(),
        }
    }
}

#[cfg(feature = "serde")]
impl<M: Model> Reconciler<M> {
    /// Rebuild a reconciler from a snapshot and a (re-supplied) model. Node and
    /// arc handles, the basis, and the ExtId index are all preserved, so the
    /// next `solve` is a warm start.
    pub fn restore(model: M, snap: ReconSnapshot<M::Tx>) -> Self {
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
                        arcs: e.arcs,
                    },
                )
            })
            .collect();
        Reconciler {
            model,
            net: Network::restore(snap.net),
            entries,
            by_key: snap.by_key,
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
        let mut r = Reconciler::new(Demo);
        r.upsert(1, Tx { amount: 100, date: 0 });
        r.upsert(2, Tx { amount: -100, date: 1 });
        r.solve();
        let groups = r.groups();
        assert_eq!(groups.len(), 1);
        assert!(groups[0].clean);
        assert_eq!(groups[0].members, vec![1, 2]);
    }

    #[test]
    fn streaming_add() {
        let mut r = Reconciler::new(Demo);
        r.upsert(1, Tx { amount: 100, date: 0 });
        r.upsert(2, Tx { amount: -100, date: 0 });
        r.solve();
        assert_eq!(r.groups().len(), 1);

        // stream more
        r.upsert(3, Tx { amount: 70, date: 5 });
        r.upsert(4, Tx { amount: -70, date: 5 });
        r.solve();
        let g = r.groups();
        assert_eq!(g.len(), 2);
        assert!(g.iter().all(|g| g.clean));
    }

    #[test]
    fn out_of_window_unmatched() {
        let mut r = Reconciler::new(Demo);
        r.upsert(1, Tx { amount: 100, date: 0 });
        r.upsert(2, Tx { amount: -100, date: 100 }); // far outside window
        r.solve();
        assert_eq!(r.groups().len(), 0);
        assert_eq!(r.unmatched(), vec![1, 2]);
    }

    #[test]
    fn correction_reprice() {
        let mut r = Reconciler::new(Demo);
        r.upsert(1, Tx { amount: 100, date: 0 });
        r.upsert(2, Tx { amount: -100, date: 0 });
        r.upsert(3, Tx { amount: -50, date: 0 });
        r.solve();
        // 1 matches 2 (exact)
        assert!(r.groups().iter().any(|g| g.members.contains(&1) && g.members.contains(&2)));

        // correct tx 1 down to 50 -> should now prefer matching 3
        r.upsert(1, Tx { amount: 50, date: 0 });
        r.solve();
        let g = r.groups();
        assert!(g.iter().any(|g| g.clean && g.members.contains(&1) && g.members.contains(&3)));
    }

    #[test]
    fn remove_tx() {
        let mut r = Reconciler::new(Demo);
        r.upsert(1, Tx { amount: 100, date: 0 });
        r.upsert(2, Tx { amount: -100, date: 0 });
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
        let mut r = Reconciler::new(SModel);
        r.upsert(1, STx { amount: 100, date: 0 });
        r.upsert(2, STx { amount: -100, date: 0 });
        r.solve();
        let json = serde_json::to_string(&r.snapshot()).unwrap();

        // "Month 2": restore the cached basis, stream a new pair, warm-solve.
        let snap: ReconSnapshot<STx> = serde_json::from_str(&json).unwrap();
        let mut r2 = Reconciler::restore(SModel, snap);
        assert_eq!(r2.groups().len(), 1); // basis survived the round-trip
        r2.upsert(3, STx { amount: 70, date: 1 });
        r2.upsert(4, STx { amount: -70, date: 1 });
        r2.solve();
        let g = r2.groups();
        assert_eq!(g.len(), 2);
        assert!(g.iter().all(|g| g.clean));
    }
}
