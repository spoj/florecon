//! The `flow` leaf: a min-cost-flow arbiter that emits **whole-row** groups.
//!
//! Describe the domain via a [`FlowSpec`] of closures (amount/penalty/block_key/
//! window/match_keys/cost); the leaf builds a transportation network over
//! [`netsimplex`], solves it cold, and reads the optimal matching back as
//! **connected-component clusters of whole entries**. A row is never split: if
//! the solver routes part of it to one counterparty and part elsewhere, all the
//! rows it touches land in one cluster, and the break is just the cluster's net
//! (which a downstream [`accept_if`](super::accept_if) can gate).

use super::{Entry, Group, Id, Resolution, Strategy};
use netsimplex::{Network, NodeId};
use std::collections::{HashMap, HashSet};

type MatchKeysFn<E> = Box<dyn Fn(&E) -> Vec<u64>>;
type CostFn<E> = Box<dyn Fn(&E, &E) -> Option<f64>>;

/// How to turn payloads into a transportation problem. Build from
/// [`FlowSpec::new`] with the chained setters. All closures read the payload
/// `E`; identity is the framework's [`Entry::id`].
pub struct FlowSpec<E> {
    amount: Box<dyn Fn(&E) -> i64>,
    penalty: Box<dyn Fn(&E) -> f64>,
    block_key: Box<dyn Fn(&E) -> i64>,
    window: i64,
    match_keys: MatchKeysFn<E>,
    cost: CostFn<E>,
}

impl<E> Default for FlowSpec<E> {
    fn default() -> Self {
        FlowSpec {
            amount: Box::new(|_| 0),
            penalty: Box::new(|_| 0.0),
            block_key: Box::new(|_| 0),
            window: -1,
            match_keys: Box::new(|_| Vec::new()),
            cost: Box::new(|_, _| None),
        }
    }
}

impl<E> FlowSpec<E> {
    /// Penalty 0, window -1 (exact-join only), no keys, all pairs forbidden.
    pub fn new() -> Self {
        Self::default()
    }
    /// The conserved supply per entry (the lane flow balances).
    pub fn amount(mut self, f: impl Fn(&E) -> i64 + 'static) -> Self {
        self.amount = Box::new(f);
        self
    }
    /// Cost of leaving an entry unmatched.
    pub fn penalty(mut self, p: f64) -> Self {
        self.penalty = Box::new(move |_| p);
        self
    }
    /// Per-entry unmatched penalty.
    pub fn penalty_fn(mut self, f: impl Fn(&E) -> f64 + 'static) -> Self {
        self.penalty = Box::new(f);
        self
    }
    /// 1-D proximity ordering (e.g. day).
    pub fn block_key(mut self, f: impl Fn(&E) -> i64 + 'static) -> Self {
        self.block_key = Box::new(f);
        self
    }
    /// Proximity radius on `block_key`; negative disables the window.
    pub fn window(mut self, w: i64) -> Self {
        self.window = w;
        self
    }
    /// Exact-join keys: opposite-sign entries sharing any key are candidates.
    pub fn match_keys(mut self, f: impl Fn(&E) -> Vec<u64> + 'static) -> Self {
        self.match_keys = Box::new(f);
        self
    }
    /// Cost of matching source `a` with sink `b`, or `None` to forbid the pair.
    pub fn cost(mut self, f: impl Fn(&E, &E) -> Option<f64> + 'static) -> Self {
        self.cost = Box::new(f);
        self
    }
}

struct Flow<E> {
    spec: FlowSpec<E>,
}

impl<E> Strategy<E> for Flow<E> {
    fn run(&self, bag: Vec<Entry<E>>) -> Resolution<E> {
        let n = bag.len();
        if n == 0 {
            return Resolution {
                groups: Vec::new(),
                residual: Vec::new(),
            };
        }
        let amt: Vec<i64> = bag.iter().map(|e| (self.spec.amount)(&e.data)).collect();

        // Build the network: one node per entry.
        let mut net = Network::new();
        let mut node: Vec<NodeId> = Vec::with_capacity(n);
        let mut index: HashMap<NodeId, usize> = HashMap::new();
        for (i, e) in bag.iter().enumerate() {
            let id = net.add_node(amt[i], (self.spec.penalty)(&e.data));
            node.push(id);
            index.insert(id, i);
        }

        // Candidate arcs from each source (amount > 0) to each sink (< 0):
        // within the block-key window, plus any exact match-key join.
        let sources: Vec<usize> = (0..n).filter(|&i| amt[i] > 0).collect();
        let sinks: Vec<usize> = (0..n).filter(|&i| amt[i] < 0).collect();
        let blk: Vec<i64> = bag.iter().map(|e| (self.spec.block_key)(&e.data)).collect();

        // Sink index by match-key token.
        let mut sink_by_tok: HashMap<u64, Vec<usize>> = HashMap::new();
        for &j in &sinks {
            for t in (self.spec.match_keys)(&bag[j].data) {
                sink_by_tok.entry(t).or_default().push(j);
            }
        }

        let mut seen: HashSet<(usize, usize)> = HashSet::new();
        for &i in &sources {
            let consider = |j: usize, net: &mut Network, seen: &mut HashSet<(usize, usize)>| {
                if !seen.insert((i, j)) {
                    return;
                }
                if let Some(c) = (self.spec.cost)(&bag[i].data, &bag[j].data) {
                    net.add_arc(node[i], node[j], c);
                }
            };
            if self.spec.window >= 0 {
                for &j in &sinks {
                    if (blk[i] - blk[j]).abs() <= self.spec.window {
                        consider(j, &mut net, &mut seen);
                    }
                }
            }
            let mut tokens = (self.spec.match_keys)(&bag[i].data);
            tokens.sort_unstable();
            tokens.dedup();
            for t in tokens {
                if let Some(js) = sink_by_tok.get(&t) {
                    for &j in js.clone().iter() {
                        consider(j, &mut net, &mut seen);
                    }
                }
            }
        }

        net.solve();

        // Read back: union entries linked by any positive-flow arc into clusters.
        let mut uf = UnionFind::new(n);
        let mut touched = vec![false; n];
        for (from, to, f) in net.matches() {
            if f != 0 {
                let (a, b) = (index[&from], index[&to]);
                uf.union(a, b);
                touched[a] = true;
                touched[b] = true;
            }
        }

        // Cluster id-sets in first-seen order; untouched entries are residual.
        let mut clusters: Vec<Vec<Id>> = Vec::new();
        let mut root_slot: HashMap<usize, usize> = HashMap::new();
        for i in 0..n {
            if !touched[i] {
                continue;
            }
            let r = uf.find(i);
            let slot = *root_slot.entry(r).or_insert_with(|| {
                clusters.push(Vec::new());
                clusters.len() - 1
            });
            clusters[slot].push(bag[i].id);
        }

        let in_group: HashSet<Id> = clusters.iter().flatten().copied().collect();
        let groups = clusters
            .into_iter()
            .map(|ids| Group::new(ids, "flow"))
            .collect();
        let residual = bag.into_iter().filter(|e| !in_group.contains(&e.id)).collect();
        Resolution { groups, residual }
    }
}

/// The min-cost-flow arbiter over the ambiguous residual; emits whole-row
/// settlement clusters.
pub fn flow<E: 'static>(spec: FlowSpec<E>) -> Box<dyn Strategy<E>> {
    Box::new(Flow { spec })
}

struct UnionFind {
    parent: Vec<usize>,
}
impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind {
            parent: (0..n).collect(),
        }
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}
