//! Layer 1 — a stateful min-cost transportation engine with stable identity.
//!
//! This is a domain-agnostic network-simplex core. It knows nothing about money,
//! currencies, or reconciliation; it speaks only nodes (with signed integer
//! supply) and directed arcs (with floating-point cost).
//!
//! ## Model
//!
//! A single artificial **dummy node** `D` absorbs all imbalance. Every real node
//! is connected to `D` by an engine-managed *penalty arc* whose cost is the
//! node's unmatched penalty. Routing a node's flow through `D` means "leave it
//! unmatched". User-supplied *real arcs* connect sources to sinks.
//!
//! The basis is a spanning tree over all alive nodes plus `D`, rooted at `D`.
//!
//! ## Identity
//!
//! Nodes and arcs are addressed by stable `NodeId`/`ArcId` handles
//! (`{ slot, generation }`). Slots are reused after removal; the generation counter
//! makes stale handles detectable. Holes left by removals are tolerated and
//! skipped (compaction can happen during refactorization later).
//!
//! ## Warm starts
//!
//! `add_node`, `add_arc`, and `set_cost` preserve the current basis and flows,
//! so re-solving after them is a true warm start. `set_supply` and removing a
//! *basic* arc/node currently fall back to a feasible rebuild before
//! re-optimizing (correct, not yet minimal — that is what the dual pivot, a
//! later step, will make incremental).

use log::{debug, warn};

const NONE: u32 = u32::MAX;
const PRICING_TOLERANCE: f64 = -1e-9;

/// Stable handle to a node. Cheap to copy; hold it to address the node later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct NodeId {
    slot: u32,
    generation: u32,
}

/// Stable handle to an arc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ArcId {
    slot: u32,
    generation: u32,
}

/// Outcome of a `solve()` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SolveStatus {
    /// Reached a proven optimal basis.
    Optimal,
    /// Hit the iteration cap; the returned basis may be sub-optimal.
    IterationLimit,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
struct Node {
    alive: bool,
    generation: u32,
    supply: i64,
    /// The engine-managed penalty arc connecting this node to the dummy.
    penalty_arc: u32,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
struct Arc {
    alive: bool,
    generation: u32,
    from: u32,
    to: u32,
    cost: f64,
    /// Whether this arc is part of the current basis (spanning tree).
    basic: bool,
    /// Flow carried; only meaningful while `basic`.
    flow: i64,
    /// True for engine-managed penalty arcs (node <-> dummy).
    is_penalty: bool,
}

/// Serializable, persistent basis state of a [`Network`]. Produce one with
/// [`Network::snapshot`] and rebuild with [`Network::restore`]. Implements
/// `Serialize`/`Deserialize` when the `serde` feature is enabled.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Snapshot {
    nodes: Vec<Node>,
    free_nodes: Vec<u32>,
    arcs: Vec<Arc>,
    free_arcs: Vec<u32>,
    dummy: u32,
}

#[derive(Debug, Clone)]
pub struct Network {
    nodes: Vec<Node>,
    free_nodes: Vec<u32>,
    arcs: Vec<Arc>,
    free_arcs: Vec<u32>,
    dummy: u32,

    // Tree state, indexed by node slot.
    potential: Vec<f64>,
    parent: Vec<u32>,
    parent_arc: Vec<u32>,
    parent_forward: Vec<bool>,
    depth: Vec<u32>,

    // Reusable buffers.
    adj: Vec<Vec<(u32, u32, bool)>>, // (neighbor, arc_slot, forward_from_curr)
    queue: std::collections::VecDeque<u32>,
    path_u: Vec<u32>,
    path_v: Vec<u32>,
    /// O(1)-reset visited marker for subtree traversals.
    stamp: Vec<u32>,
    cur_stamp: u32,

    needs_rebuild: bool,
    dirty: bool,
}

impl Default for Network {
    fn default() -> Self {
        Self::new()
    }
}

impl Network {
    /// Create an empty network containing only the dummy node.
    pub fn new() -> Self {
        let mut net = Network {
            nodes: Vec::new(),
            free_nodes: Vec::new(),
            arcs: Vec::new(),
            free_arcs: Vec::new(),
            dummy: 0,
            potential: Vec::new(),
            parent: Vec::new(),
            parent_arc: Vec::new(),
            parent_forward: Vec::new(),
            depth: Vec::new(),
            adj: Vec::new(),
            queue: std::collections::VecDeque::new(),
            path_u: Vec::new(),
            path_v: Vec::new(),
            stamp: Vec::new(),
            cur_stamp: 0,
            needs_rebuild: false,
            dirty: true,
        };
        // The dummy node lives in slot 0 and has no penalty arc.
        net.dummy = net.raw_alloc_node(0, NONE);
        net
    }

    // --- slot allocation -------------------------------------------------

    fn raw_alloc_node(&mut self, supply: i64, penalty_arc: u32) -> u32 {
        if let Some(slot) = self.free_nodes.pop() {
            let n = &mut self.nodes[slot as usize];
            n.alive = true;
            n.generation += 1;
            n.supply = supply;
            n.penalty_arc = penalty_arc;
            slot
        } else {
            let slot = self.nodes.len() as u32;
            self.nodes.push(Node {
                alive: true,
                generation: 0,
                supply,
                penalty_arc,
            });
            self.potential.push(0.0);
            self.parent.push(NONE);
            self.parent_arc.push(NONE);
            self.parent_forward.push(true);
            self.depth.push(0);
            self.adj.push(Vec::new());
            self.stamp.push(0);
            slot
        }
    }

    fn raw_alloc_arc(&mut self, arc: Arc) -> u32 {
        if let Some(slot) = self.free_arcs.pop() {
            let generation = self.arcs[slot as usize].generation + 1;
            self.arcs[slot as usize] = Arc { generation, ..arc };
            slot
        } else {
            let slot = self.arcs.len() as u32;
            self.arcs.push(arc);
            slot
        }
    }

    fn node_slot(&self, id: NodeId) -> Option<usize> {
        let n = self.nodes.get(id.slot as usize)?;
        if n.alive && n.generation == id.generation {
            Some(id.slot as usize)
        } else {
            None
        }
    }

    fn arc_slot(&self, id: ArcId) -> Option<usize> {
        let a = self.arcs.get(id.slot as usize)?;
        if a.alive && a.generation == id.generation {
            Some(id.slot as usize)
        } else {
            None
        }
    }

    // --- public mutation API --------------------------------------------

    /// Add a node with the given signed `supply` and unmatched `penalty`.
    ///
    /// Incremental: attaches the node as a leaf off the dummy, preserving the
    /// current basis. Positive supply = source, negative = sink.
    pub fn add_node(&mut self, supply: i64, penalty: f64) -> NodeId {
        let slot = self.raw_alloc_node(supply, NONE);

        // Engine-managed penalty arc, oriented by supply sign and made basic so
        // the node is connected to the dummy (initially "unmatched").
        let (from, to, flow) = if supply >= 0 {
            (slot, self.dummy, supply)
        } else {
            (self.dummy, slot, -supply)
        };
        let arc_slot = self.raw_alloc_arc(Arc {
            alive: true,
            generation: 0,
            from,
            to,
            cost: penalty,
            basic: true,
            flow,
            is_penalty: true,
        });
        self.nodes[slot as usize].penalty_arc = arc_slot;

        self.dirty = true;
        NodeId {
            slot,
            generation: self.nodes[slot as usize].generation,
        }
    }

    /// Add a directed real arc `from -> to` with the given cost.
    ///
    /// Incremental: the arc enters non-basic, so the basis is unchanged.
    pub fn add_arc(&mut self, from: NodeId, to: NodeId, cost: f64) -> Option<ArcId> {
        let f = self.node_slot(from)? as u32;
        let t = self.node_slot(to)? as u32;
        let slot = self.raw_alloc_arc(Arc {
            alive: true,
            generation: 0,
            from: f,
            to: t,
            cost,
            basic: false,
            flow: 0,
            is_penalty: false,
        });
        self.dirty = true;
        Some(ArcId {
            slot,
            generation: self.arcs[slot as usize].generation,
        })
    }

    /// Update an arc's cost. Incremental (basis/flows preserved; potentials are
    /// recomputed on next solve).
    pub fn set_cost(&mut self, arc: ArcId, cost: f64) -> Option<()> {
        let s = self.arc_slot(arc)?;
        self.arcs[s].cost = cost;
        self.dirty = true;
        Some(())
    }

    /// Update a node's unmatched penalty (the cost of its penalty arc).
    pub fn set_penalty(&mut self, node: NodeId, penalty: f64) -> Option<()> {
        let s = self.node_slot(node)?;
        let arc = self.nodes[s].penalty_arc;
        if arc != NONE {
            self.arcs[arc as usize].cost = penalty;
            self.dirty = true;
        }
        Some(())
    }

    /// Change a node's supply. Falls back to a feasible rebuild (correct, not
    /// yet minimal — the dual pivot will make this incremental).
    pub fn set_supply(&mut self, node: NodeId, supply: i64) -> Option<()> {
        let s = self.node_slot(node)?;
        if self.nodes[s].supply != supply {
            self.nodes[s].supply = supply;
            self.needs_rebuild = true;
            self.dirty = true;
        }
        Some(())
    }

    /// Remove an arc. Incremental if non-basic; otherwise triggers a rebuild.
    pub fn remove_arc(&mut self, arc: ArcId) -> Option<()> {
        let s = self.arc_slot(arc)?;
        if self.arcs[s].is_penalty {
            return None; // penalty arcs are engine-managed
        }
        if self.arcs[s].basic {
            self.needs_rebuild = true;
        }
        self.arcs[s].alive = false;
        self.free_arcs.push(s as u32);
        self.dirty = true;
        Some(())
    }

    /// Remove a node and all its arcs. Triggers a rebuild if the node carried
    /// any real flow.
    pub fn remove_node(&mut self, node: NodeId) -> Option<()> {
        let s = self.node_slot(node)?;
        // Kill every arc incident to this node.
        for a in 0..self.arcs.len() {
            let arc = &self.arcs[a];
            if arc.alive && (arc.from as usize == s || arc.to as usize == s) {
                if arc.basic && !arc.is_penalty {
                    self.needs_rebuild = true;
                }
                self.arcs[a].alive = false;
                self.free_arcs.push(a as u32);
            }
        }
        self.nodes[s].alive = false;
        self.nodes[s].penalty_arc = NONE;
        self.free_nodes.push(s as u32);
        self.needs_rebuild = true;
        self.dirty = true;
        Some(())
    }

    // --- queries ---------------------------------------------------------

    /// Flow currently routed on an arc (0 if non-basic or unknown).
    pub fn flow(&self, arc: ArcId) -> i64 {
        match self.arc_slot(arc) {
            Some(s) if self.arcs[s].basic => self.arcs[s].flow,
            _ => 0,
        }
    }

    /// Iterate matched real arcs (basic, non-penalty, positive flow) as
    /// `(from, to, flow)` triples.
    pub fn matches(&self) -> impl Iterator<Item = (NodeId, NodeId, i64)> + '_ {
        self.arcs.iter().filter_map(move |a| {
            if a.alive && a.basic && !a.is_penalty && a.flow > 0 {
                Some((
                    NodeId {
                        slot: a.from,
                        generation: self.nodes[a.from as usize].generation,
                    },
                    NodeId {
                        slot: a.to,
                        generation: self.nodes[a.to as usize].generation,
                    },
                    a.flow,
                ))
            } else {
                None
            }
        })
    }

    /// Number of alive real nodes (excluding the dummy).
    pub fn node_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.alive).count() - 1
    }

    // --- persistence -----------------------------------------------------

    /// Capture the persistent basis state for caching (e.g. to disk between
    /// reconciliation runs). Transient solver buffers are not included; they
    /// are rebuilt on `restore`.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            nodes: self.nodes.clone(),
            free_nodes: self.free_nodes.clone(),
            arcs: self.arcs.clone(),
            free_arcs: self.free_arcs.clone(),
            dummy: self.dummy,
        }
    }

    /// Rebuild a network from a snapshot. Node/arc handles taken before the
    /// snapshot remain valid (slots and generations are preserved). The next
    /// `solve` refreshes potentials from the restored basis.
    pub fn restore(s: Snapshot) -> Self {
        let n = s.nodes.len();
        Network {
            nodes: s.nodes,
            free_nodes: s.free_nodes,
            arcs: s.arcs,
            free_arcs: s.free_arcs,
            dummy: s.dummy,
            potential: vec![0.0; n],
            parent: vec![NONE; n],
            parent_arc: vec![NONE; n],
            parent_forward: vec![true; n],
            depth: vec![0; n],
            adj: vec![Vec::new(); n],
            queue: std::collections::VecDeque::new(),
            path_u: Vec::new(),
            path_v: Vec::new(),
            stamp: vec![0; n],
            cur_stamp: 0,
            needs_rebuild: false,
            dirty: true,
        }
    }

    // --- solve -----------------------------------------------------------

    /// Re-optimize from the cached basis. Returns when optimal or capped.
    pub fn solve(&mut self) -> SolveStatus {
        if self.needs_rebuild {
            self.rebuild_star_basis();
            self.needs_rebuild = false;
        }
        if !self.dirty {
            return SolveStatus::Optimal;
        }

        let n_alive = self.nodes.iter().filter(|n| n.alive).count();
        let max_iterations = (n_alive * n_alive * 2).max(1000);
        let mut iterations = 0;
        let mut since_refactor = 0u32;
        let mut status = SolveStatus::Optimal;

        // One full refactorization to establish consistent adjacency and
        // potentials; subsequent pivots update both incrementally.
        self.rebuild_tree();

        loop {
            if iterations >= max_iterations {
                warn!("network simplex hit iteration cap ({max_iterations})");
                status = SolveStatus::IterationLimit;
                break;
            }
            iterations += 1;

            let Some((entering, rc)) = self.find_entering_arc() else {
                debug!("optimal after {iterations} iterations");
                break;
            };

            if !self.pivot(entering, rc) {
                warn!("degenerate/unbounded: no leaving arc");
                break;
            }

            // Periodic refactorization to flush accumulated float error.
            since_refactor += 1;
            if since_refactor >= 1024 {
                self.rebuild_tree();
                since_refactor = 0;
            }
        }

        self.dirty = false;
        status
    }

    /// Reset the basis to the feasible dummy-star: every node connected to the
    /// dummy by its penalty arc, all real arcs non-basic. Used after mutations
    /// that break the spanning tree.
    fn rebuild_star_basis(&mut self) {
        for a in &mut self.arcs {
            if a.alive {
                a.basic = false;
                a.flow = 0;
            }
        }
        for s in 0..self.nodes.len() {
            if !self.nodes[s].alive || s as u32 == self.dummy {
                continue;
            }
            let supply = self.nodes[s].supply;
            let arc = self.nodes[s].penalty_arc;
            if arc == NONE {
                continue;
            }
            let a = &mut self.arcs[arc as usize];
            if supply >= 0 {
                a.from = s as u32;
                a.to = self.dummy;
                a.flow = supply;
            } else {
                a.from = self.dummy;
                a.to = s as u32;
                a.flow = -supply;
            }
            a.basic = true;
        }
        self.dirty = true;
    }

    /// Recompute parents, depths, and potentials from the basic arcs via BFS.
    fn rebuild_tree(&mut self) {
        for a in &mut self.adj {
            a.clear();
        }
        for (idx, arc) in self.arcs.iter().enumerate() {
            if !arc.alive || !arc.basic {
                continue;
            }
            self.adj[arc.from as usize].push((arc.to, idx as u32, true));
            self.adj[arc.to as usize].push((arc.from, idx as u32, false));
        }

        let root = self.dummy as usize;
        self.queue.clear();
        // depth doubles as a visited marker via parent == NONE reset.
        for s in 0..self.nodes.len() {
            self.parent[s] = NONE;
        }
        self.parent[root] = root as u32;
        self.parent_arc[root] = NONE;
        self.depth[root] = 0;
        self.potential[root] = 0.0;
        self.queue.push_back(root as u32);

        while let Some(u) = self.queue.pop_front() {
            let u = u as usize;
            for i in 0..self.adj[u].len() {
                let (v, arc_idx, forward) = self.adj[u][i];
                let v = v as usize;
                if self.parent[v] != NONE || v == root {
                    continue;
                }
                self.parent[v] = u as u32;
                self.parent_arc[v] = arc_idx;
                self.parent_forward[v] = forward;
                self.depth[v] = self.depth[u] + 1;
                let cost = self.arcs[arc_idx as usize].cost;
                self.potential[v] = if forward {
                    self.potential[u] + cost
                } else {
                    self.potential[u] - cost
                };
                self.queue.push_back(v as u32);
            }
        }
    }

    /// Find a non-basic arc with negative reduced cost (Dantzig rule).
    fn find_entering_arc(&self) -> Option<(u32, f64)> {
        let mut best = None;
        let mut best_rc = PRICING_TOLERANCE;
        for (idx, arc) in self.arcs.iter().enumerate() {
            if !arc.alive || arc.basic {
                continue;
            }
            let rc = arc.cost - self.potential[arc.to as usize] + self.potential[arc.from as usize];
            if rc < best_rc {
                best_rc = rc;
                best = Some((idx as u32, rc));
            }
        }
        best
    }

    fn adj_add(&mut self, arc_idx: u32) {
        let (f, t) = {
            let a = &self.arcs[arc_idx as usize];
            (a.from, a.to)
        };
        self.adj[f as usize].push((t, arc_idx, true));
        self.adj[t as usize].push((f, arc_idx, false));
    }

    fn adj_remove(&mut self, arc_idx: u32) {
        let (f, t) = {
            let a = &self.arcs[arc_idx as usize];
            (a.from, a.to)
        };
        self.adj[f as usize].retain(|e| e.1 != arc_idx);
        self.adj[t as usize].retain(|e| e.1 != arc_idx);
    }

    /// Re-root subtree `S` (reachable from `x` without crossing to `y`) onto
    /// `y` via the entering arc, recomputing parents/depths and shifting all of
    /// `S`'s potentials by the constant `delta_pot`. O(|S|).
    fn bfs_reroot(&mut self, x: usize, y: usize, entering: u32, delta_pot: f64) {
        self.cur_stamp += 1;
        let s = self.cur_stamp;
        self.stamp[y] = s; // block the boundary node
        self.stamp[x] = s;

        let forward = self.arcs[entering as usize].from as usize == y;
        self.parent[x] = y as u32;
        self.parent_arc[x] = entering;
        self.parent_forward[x] = forward;
        self.depth[x] = self.depth[y] + 1;
        self.potential[x] += delta_pot;

        self.queue.clear();
        self.queue.push_back(x as u32);
        while let Some(cur) = self.queue.pop_front() {
            let cur = cur as usize;
            for i in 0..self.adj[cur].len() {
                let (nbr, arc_idx, fwd) = self.adj[cur][i];
                let nbr = nbr as usize;
                if self.stamp[nbr] == s {
                    continue;
                }
                self.stamp[nbr] = s;
                self.parent[nbr] = cur as u32;
                self.parent_arc[nbr] = arc_idx;
                self.parent_forward[nbr] = fwd;
                self.depth[nbr] = self.depth[cur] + 1;
                self.potential[nbr] += delta_pot;
                self.queue.push_back(nbr as u32);
            }
        }
    }

    /// Perform one pivot around the cycle created by `entering` (reduced cost
    /// `rc`). Updates flows, adjacency, and potentials incrementally. Returns
    /// false if no leaving arc exists (unbounded — should not happen).
    fn pivot(&mut self, entering: u32, rc: f64) -> bool {
        let (u, v) = {
            let a = &self.arcs[entering as usize];
            (a.from as usize, a.to as usize)
        };

        self.path_u.clear();
        self.path_v.clear();
        let mut cu = u;
        let mut cv = v;
        while cu != cv {
            if self.depth[cu] > self.depth[cv] {
                self.path_u.push(cu as u32);
                cu = self.parent[cu] as usize;
            } else if self.depth[cv] > self.depth[cu] {
                self.path_v.push(cv as u32);
                cv = self.parent[cv] as usize;
            } else {
                self.path_u.push(cu as u32);
                self.path_v.push(cv as u32);
                cu = self.parent[cu] as usize;
                cv = self.parent[cv] as usize;
            }
        }

        // Determine the maximum flow change theta and the leaving arc.
        // Entering arc u->v carries +theta. On the cycle, an arc "decreases"
        // when flow runs against its direction. Track which side (the subtree
        // that will detach) the leaving arc sits on.
        let mut min_theta = i64::MAX;
        let mut leaving = NONE;
        let mut leaving_on_v = true;

        for &w in &self.path_v {
            let idx = self.parent_arc[w as usize];
            if self.parent_forward[w as usize] {
                let flow = self.arcs[idx as usize].flow;
                if flow <= min_theta {
                    min_theta = flow;
                    leaving = idx;
                    leaving_on_v = true;
                }
            }
        }
        for &w in &self.path_u {
            let idx = self.parent_arc[w as usize];
            if !self.parent_forward[w as usize] {
                let flow = self.arcs[idx as usize].flow;
                if flow <= min_theta {
                    min_theta = flow;
                    leaving = idx;
                    leaving_on_v = false;
                }
            }
        }

        if leaving == NONE {
            return false;
        }
        let theta = min_theta;

        for &w in &self.path_v {
            let idx = self.parent_arc[w as usize] as usize;
            if self.parent_forward[w as usize] {
                self.arcs[idx].flow -= theta;
            } else {
                self.arcs[idx].flow += theta;
            }
        }
        for &w in &self.path_u {
            let idx = self.parent_arc[w as usize] as usize;
            if self.parent_forward[w as usize] {
                self.arcs[idx].flow += theta;
            } else {
                self.arcs[idx].flow -= theta;
            }
        }

        self.arcs[leaving as usize].basic = false;
        self.arcs[leaving as usize].flow = 0;
        self.arcs[entering as usize].basic = true;
        self.arcs[entering as usize].flow = theta;

        // Incremental structure + potential update. The detached subtree is the
        // one containing v (if leaving was on the v-side) or u (otherwise); it
        // re-roots onto the opposite endpoint and shifts potentials by rc.
        let (x, y, delta_pot) = if leaving_on_v {
            (v, u, rc)
        } else {
            (u, v, -rc)
        };
        self.adj_remove(leaving);
        self.adj_add(entering);
        self.bfs_reroot(x, y, entering, delta_pot);
        true
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn matched_pairs(net: &Network) -> Vec<(u32, u32, i64)> {
        let mut v: Vec<_> = net
            .matches()
            .map(|(a, b, f)| (a.slot, b.slot, f))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn simple_match() {
        let mut net = Network::new();
        let s = net.add_node(100, 1e6);
        let t = net.add_node(-100, 1e6);
        net.add_arc(s, t, 1.0).unwrap();
        assert_eq!(net.solve(), SolveStatus::Optimal);
        let m = matched_pairs(&net);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].2, 100);
    }

    #[test]
    fn partial_match() {
        let mut net = Network::new();
        let s = net.add_node(100, 1e6);
        let t = net.add_node(-50, 1e6);
        net.add_arc(s, t, 1.0).unwrap();
        net.solve();
        let m = matched_pairs(&net);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].2, 50);
    }

    #[test]
    fn picks_cheapest() {
        let mut net = Network::new();
        let s0 = net.add_node(100, 1e6);
        let s1 = net.add_node(50, 1e6);
        let t0 = net.add_node(-100, 1e6);
        let t1 = net.add_node(-50, 1e6);
        net.add_arc(s0, t0, 10.0);
        net.add_arc(s0, t1, 10.0);
        net.add_arc(s1, t0, 10.0);
        net.add_arc(s1, t1, 10.0);
        net.solve();
        // make (s0->t0) and (s1->t1) cheap, re-solve (warm start)
        // emulate by fresh costs
        let mut net = Network::new();
        let s0 = net.add_node(100, 1e6);
        let s1 = net.add_node(50, 1e6);
        let t0 = net.add_node(-100, 1e6);
        let t1 = net.add_node(-50, 1e6);
        net.add_arc(s0, t0, 1.0);
        net.add_arc(s1, t1, 1.0);
        net.add_arc(s0, t1, 10.0);
        net.add_arc(s1, t0, 10.0);
        net.solve();
        let m = matched_pairs(&net);
        assert_eq!(m.len(), 2);
        assert!(m.contains(&(s0.slot, t0.slot, 100)));
        assert!(m.contains(&(s1.slot, t1.slot, 50)));
    }

    #[test]
    fn warm_start_add_node() {
        let mut net = Network::new();
        let s = net.add_node(100, 1e6);
        let t = net.add_node(-100, 1e6);
        net.add_arc(s, t, 1.0);
        net.solve();
        assert_eq!(matched_pairs(&net).len(), 1);

        // Stream in a new pair; basis should warm-start.
        let s2 = net.add_node(40, 1e6);
        let t2 = net.add_node(-40, 1e6);
        net.add_arc(s2, t2, 1.0);
        net.solve();
        let m = matched_pairs(&net);
        assert_eq!(m.len(), 2);
        assert!(m.contains(&(s2.slot, t2.slot, 40)));
    }

    #[test]
    fn unmatched_when_too_costly() {
        let mut net = Network::new();
        let s = net.add_node(100, 1.0); // cheap to leave unmatched
        let t = net.add_node(-100, 1.0);
        net.add_arc(s, t, 1000.0); // expensive to match
        net.solve();
        assert_eq!(matched_pairs(&net).len(), 0);
    }

    #[test]
    fn remove_arc_rebuilds() {
        let mut net = Network::new();
        let s = net.add_node(100, 1000.0);
        let t = net.add_node(-100, 1000.0);
        let a = net.add_arc(s, t, 1.0).unwrap();
        net.solve();
        assert_eq!(matched_pairs(&net).len(), 1);

        net.remove_arc(a);
        net.solve();
        // 2000 unmatched penalty < no match? matching gone -> unmatched
        assert_eq!(matched_pairs(&net).len(), 0);
    }

    #[test]
    fn random_vs_brute_force() {
        // Small random assignment instances: equal unit supplies, full bipartite,
        // cheap-enough costs that a perfect matching always wins. Compare the
        // engine objective to the optimal assignment found by brute force.
        fn brute(costs: &[Vec<f64>], k: usize) -> f64 {
            let mut perm: Vec<usize> = (0..k).collect();
            let mut best = f64::MAX;
            permute(&mut perm, 0, costs, &mut best);
            best
        }
        fn permute(p: &mut [usize], i: usize, c: &[Vec<f64>], best: &mut f64) {
            if i == p.len() {
                let s: f64 = (0..p.len()).map(|r| c[r][p[r]]).sum();
                if s < *best {
                    *best = s;
                }
                return;
            }
            for j in i..p.len() {
                p.swap(i, j);
                permute(p, i + 1, c, best);
                p.swap(i, j);
            }
        }

        let mut seed: u64 = 0x1234_5678_9abc_def0;
        let mut rng = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for _ in 0..40 {
            let k = 2 + (rng() % 4) as usize; // 2..=5 per side
            let costs: Vec<Vec<f64>> =
                (0..k).map(|_| (0..k).map(|_| 1.0 + (rng() % 50) as f64).collect()).collect();

            let mut net = Network::new();
            let sources: Vec<NodeId> = (0..k).map(|_| net.add_node(1, 1e6)).collect();
            let sinks: Vec<NodeId> = (0..k).map(|_| net.add_node(-1, 1e6)).collect();
            let mut arc_of: Vec<Vec<ArcId>> = vec![vec![]; k];
            for r in 0..k {
                for cc in 0..k {
                    arc_of[r].push(net.add_arc(sources[r], sinks[cc], costs[r][cc]).unwrap());
                }
            }
            net.solve();
            let obj: f64 = (0..k)
                .flat_map(|r| (0..k).map(move |cc| (r, cc)))
                .map(|(r, cc)| if net.flow(arc_of[r][cc]) > 0 { costs[r][cc] } else { 0.0 })
                .sum();
            let opt = brute(&costs, k);
            assert!((obj - opt).abs() < 1e-6, "obj {obj} != opt {opt} (k={k})");
        }
    }

    #[test]
    #[allow(clippy::needless_range_loop)]
    fn warm_start_matches_cold() {
        // Drive a network through many cost mutations with warm re-solves and
        // verify the objective always equals a freshly cold-solved equivalent.
        let mut seed: u64 = 0xdead_beef_0000_0001;
        let mut rng = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let k = 6;

        let mut warm = Network::new();
        let ws: Vec<NodeId> = (0..k).map(|_| warm.add_node(1, 1e6)).collect();
        let wt: Vec<NodeId> = (0..k).map(|_| warm.add_node(-1, 1e6)).collect();
        let mut warc = vec![vec![]; k];
        for r in 0..k {
            for c in 0..k {
                warc[r].push(warm.add_arc(ws[r], wt[c], 0.0).unwrap());
            }
        }

        for round in 0..30 {
            let costs: Vec<Vec<f64>> =
                (0..k).map(|_| (0..k).map(|_| 1.0 + (rng() % 90) as f64).collect()).collect();
            for r in 0..k {
                for c in 0..k {
                    warm.set_cost(warc[r][c], costs[r][c]);
                }
            }
            warm.solve();
            let warm_obj: f64 = (0..k)
                .flat_map(|r| (0..k).map(move |c| (r, c)))
                .map(|(r, c)| if warm.flow(warc[r][c]) > 0 { costs[r][c] } else { 0.0 })
                .sum();

            // cold equivalent
            let mut cold = Network::new();
            let cs: Vec<NodeId> = (0..k).map(|_| cold.add_node(1, 1e6)).collect();
            let ct: Vec<NodeId> = (0..k).map(|_| cold.add_node(-1, 1e6)).collect();
            let mut carc = vec![vec![]; k];
            for r in 0..k {
                for c in 0..k {
                    carc[r].push(cold.add_arc(cs[r], ct[c], costs[r][c]).unwrap());
                }
            }
            cold.solve();
            let cold_obj: f64 = (0..k)
                .flat_map(|r| (0..k).map(move |c| (r, c)))
                .map(|(r, c)| if cold.flow(carc[r][c]) > 0 { costs[r][c] } else { 0.0 })
                .sum();

            assert!(
                (warm_obj - cold_obj).abs() < 1e-6,
                "round {round}: warm {warm_obj} != cold {cold_obj}"
            );
        }
    }

    #[test]
    fn snapshot_restore_preserves_basis() {
        let mut net = Network::new();
        let s = net.add_node(100, 1e6);
        let t = net.add_node(-100, 1e6);
        let a = net.add_arc(s, t, 1.0).unwrap();
        net.solve();
        assert_eq!(net.flow(a), 100);

        // Round-trip through a snapshot; handles stay valid, basis preserved.
        let snap = net.snapshot();
        let mut restored = Network::restore(snap);
        assert_eq!(restored.flow(a), 100);
        assert_eq!(restored.solve(), SolveStatus::Optimal);
        assert_eq!(matched_pairs(&restored), vec![(s.slot, t.slot, 100)]);

        // Warm-start the restored network with a new streamed pair.
        let s2 = restored.add_node(40, 1e6);
        let t2 = restored.add_node(-40, 1e6);
        restored.add_arc(s2, t2, 1.0);
        restored.solve();
        assert_eq!(matched_pairs(&restored).len(), 2);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn snapshot_serde_roundtrip() {
        let mut net = Network::new();
        let s = net.add_node(100, 1e6);
        let t = net.add_node(-100, 1e6);
        net.add_arc(s, t, 1.0);
        net.solve();

        let json = serde_json::to_string(&net.snapshot()).unwrap();
        let snap: Snapshot = serde_json::from_str(&json).unwrap();
        let mut restored = Network::restore(snap);
        restored.solve();
        assert_eq!(matched_pairs(&restored), vec![(s.slot, t.slot, 100)]);
    }

    #[test]
    fn stale_handle_rejected() {
        let mut net = Network::new();
        let s = net.add_node(100, 1e6);
        let t = net.add_node(-100, 1e6);
        let a = net.add_arc(s, t, 1.0).unwrap();
        net.remove_arc(a);
        // handle now stale
        assert_eq!(net.flow(a), 0);
        assert!(net.set_cost(a, 5.0).is_none());
    }
}
