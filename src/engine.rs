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
//! ## Solving
//!
//! A bounded-variable network simplex with a cached basis. Each `solve`
//! dispatches on what changed since the last call:
//!
//! - **cost change** (dual-infeasible, primal-feasible) -> primal pricing with
//!   rolling **block (partial) pricing** so large edge sets aren't fully scanned
//!   every pivot;
//! - **supply / bound / removal** (primal-infeasible, dual-feasible) -> a
//!   **dual** repair phase whose entering search is localized to the cut via
//!   per-node incidence lists (no full edge scan per pivot);
//! - **both at once** (e.g. an FX reprice that moves cost and RHS together) ->
//!   a feasible rebuild + primal (the composite/dual-phase-1 case, left as a
//!   rebuild).
//!
//! Potentials and the tree adjacency are maintained incrementally across
//! `solve` calls (O(|subtree|) per pivot), with periodic refactorization for
//! float hygiene. `add_node`, `add_arc`, `set_cost`, `set_penalty`,
//! `set_supply`, `set_bounds` and `remove_arc` are all warm-started; only
//! `remove_node` falls back to a rebuild.
//!
//! At ~100k nodes / 10M arcs (block-diagonal), a cold solve is tens of seconds
//! while warm supply edits / removals / single cost changes re-solve in tens to
//! a few hundred milliseconds.

use log::{debug, warn};

const NONE: u32 = u32::MAX;
/// Sentinel for an uncapacitated upper bound.
const INF: i64 = i64::MAX;
const PRICING_TOLERANCE: f64 = -1e-9;
/// Block size for partial (rolling) pricing. Large problems only price a
/// window of arcs per pivot instead of the full edge set.
const PRICING_BLOCK: usize = 1 << 16;
/// Periodic refactorization interval (flush accumulated float error).
const REFACTOR_INTERVAL: u32 = 1024;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
enum ArcState {
    /// In the spanning-tree basis; `flow` is free between the bounds.
    Basic,
    /// Non-basic, resting at the lower bound (`flow == lower`).
    AtLower,
    /// Non-basic, resting at the upper bound (`flow == upper`).
    AtUpper,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
struct Arc {
    alive: bool,
    generation: u32,
    from: u32,
    to: u32,
    cost: f64,
    /// Inclusive flow bounds.
    lower: i64,
    upper: i64,
    /// Basis / bound state.
    state: ArcState,
    /// Flow carried (always within `[lower, upper]`).
    flow: i64,
    /// True for engine-managed penalty arcs (node <-> dummy).
    is_penalty: bool,
}

impl Arc {
    #[inline]
    fn is_basic(&self) -> bool {
        matches!(self.state, ArcState::Basic)
    }
}

/// Per-`solve` diagnostics, surfaced via [`Network::stats`]. Useful for
/// watching how much work a warm re-solve actually did.
#[derive(Clone, Copy, Debug, Default)]
pub struct SolveStats {
    /// Dual-repair pivots (RHS/bounds/removal fixes).
    pub dual_pivots: u64,
    /// Primal pricing pivots (cost-improvement steps).
    pub primal_pivots: u64,
    /// Total subtree nodes re-rooted across dual pivots (the dominant cost on
    /// deep trees).
    pub subtree_nodes: u64,
}

/// A chosen leaving arc for a dual pivot, with the data needed to apply it.
struct DualLeave {
    /// Basic arc leaving the tree.
    arc: u32,
    /// `+1` if its flow was below lower (rests at lower); `-1` if above upper.
    beta: i64,
    /// Exact flow change to bring it to the violated bound.
    theta: i64,
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
    /// All alive arc slots incident to each node (basic and non-basic). Lets
    /// the dual entering search scan only arcs touching the cut, not all edges.
    inc: Vec<Vec<u32>>,
    /// Scratch list of nodes in the most recently marked subtree.
    subtree_buf: Vec<u32>,
    queue: std::collections::VecDeque<u32>,
    path_u: Vec<u32>,
    path_v: Vec<u32>,
    /// O(1)-reset visited marker for subtree traversals.
    stamp: Vec<u32>,
    cur_stamp: u32,
    /// Rolling start index for block (partial) pricing.
    price_start: u32,
    /// Basic arcs capped to zero by `remove_arc`, awaiting pivot-out + kill.
    pending_kill: Vec<u32>,

    /// `adj`, `parent`, `depth` describe a valid spanning tree of the current
    /// basic arcs (so a full `rebuild_tree` can be skipped).
    tree_valid: bool,
    /// `potential[]` is consistent with the tree and current arc costs.
    pots_valid: bool,
    /// Dual feasibility may be broken (a cost changed): needs primal pricing.
    primal_dirty: bool,
    /// Primal feasibility may be broken (RHS/bounds changed): needs dual repair.
    dual_dirty: bool,

    needs_rebuild: bool,
    dirty: bool,
    /// Diagnostics of the most recent `solve`.
    dbg: SolveStats,
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
            inc: Vec::new(),
            subtree_buf: Vec::new(),
            queue: std::collections::VecDeque::new(),
            path_u: Vec::new(),
            path_v: Vec::new(),
            stamp: Vec::new(),
            cur_stamp: 0,
            price_start: 0,
            pending_kill: Vec::new(),
            tree_valid: false,
            pots_valid: false,
            primal_dirty: false,
            dual_dirty: false,
            needs_rebuild: false,
            dirty: true,
            dbg: SolveStats::default(),
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
            self.adj[slot as usize].clear(); // recycled slot: drop stale tree edges
            self.inc[slot as usize].clear();
            self.parent[slot as usize] = NONE;
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
            self.inc.push(Vec::new());
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
            lower: 0,
            upper: INF,
            state: ArcState::Basic,
            flow,
            is_penalty: true,
        });
        self.nodes[slot as usize].penalty_arc = arc_slot;
        self.inc[from as usize].push(arc_slot);
        self.inc[to as usize].push(arc_slot);

        // Keep the cached tree valid by attaching the penalty arc as a leaf off
        // the dummy (root). pot[dummy] == 0, so the leaf potential makes the
        // penalty arc's reduced cost zero, as required for a basic arc.
        if self.tree_valid {
            self.adj_add(arc_slot);
            let forward = from == self.dummy; // arc.from == parent(dummy)?
            self.parent[slot as usize] = self.dummy;
            self.parent_arc[slot as usize] = arc_slot;
            self.parent_forward[slot as usize] = forward;
            self.depth[slot as usize] = self.depth[self.dummy as usize] + 1;
            self.potential[slot as usize] = if forward { penalty } else { -penalty };
        }

        self.dirty = true;
        NodeId {
            slot,
            generation: self.nodes[slot as usize].generation,
        }
    }

    /// Add a directed real arc `from -> to` with the given cost, uncapacitated.
    ///
    /// Incremental: the arc enters non-basic, so the basis is unchanged.
    pub fn add_arc(&mut self, from: NodeId, to: NodeId, cost: f64) -> Option<ArcId> {
        self.add_arc_bounded(from, to, cost, 0, INF)
    }

    /// Add a directed real arc with explicit `[lower, upper]` flow bounds.
    pub fn add_arc_bounded(
        &mut self,
        from: NodeId,
        to: NodeId,
        cost: f64,
        lower: i64,
        upper: i64,
    ) -> Option<ArcId> {
        let f = self.node_slot(from)? as u32;
        let t = self.node_slot(to)? as u32;
        let slot = self.raw_alloc_arc(Arc {
            alive: true,
            generation: 0,
            from: f,
            to: t,
            cost,
            lower,
            upper,
            state: ArcState::AtLower,
            flow: lower,
            is_penalty: false,
        });
        self.inc[f as usize].push(slot);
        self.inc[t as usize].push(slot);
        // A new non-basic arc can have negative reduced cost: dual feasibility
        // may break, so a primal pricing pass is needed.
        self.primal_dirty = true;
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
        let basic = self.arcs[s].is_basic();
        self.arcs[s].cost = cost;
        // Changing a basic arc's cost shifts subtree potentials; a non-basic
        // change only alters that arc's reduced cost. Either way the basis may
        // become dual-infeasible -> primal pricing.
        if basic {
            self.pots_valid = false;
        }
        self.primal_dirty = true;
        self.dirty = true;
        Some(())
    }

    /// Update an arc's flow bounds. Incremental when feasible to repair via the
    /// dual; falls back to a rebuild only for awkward non-basic relocations.
    pub fn set_bounds(&mut self, arc: ArcId, lower: i64, upper: i64) -> Option<()> {
        let s = self.arc_slot(arc)?;
        let (state, flow) = (self.arcs[s].state, self.arcs[s].flow);
        self.arcs[s].lower = lower;
        self.arcs[s].upper = upper;
        match state {
            // A basic arc may now violate its bounds: dual repair handles it.
            ArcState::Basic => {
                self.dual_dirty = true;
            }
            // A non-basic arc rests on a bound; if that resting flow moves we
            // must re-balance conservation by pushing the delta around its tree
            // cycle, then let the dual clean up.
            ArcState::AtLower => {
                if flow != lower {
                    if self.tree_valid {
                        self.push_nonbasic_reset(s, lower);
                        self.dual_dirty = true;
                    } else {
                        self.needs_rebuild = true;
                    }
                }
            }
            ArcState::AtUpper => {
                if flow != upper {
                    if self.tree_valid {
                        self.push_nonbasic_reset(s, upper);
                        self.dual_dirty = true;
                    } else {
                        self.needs_rebuild = true;
                    }
                }
            }
        }
        self.dirty = true;
        Some(())
    }

    /// Update a node's unmatched penalty (the cost of its penalty arc).
    pub fn set_penalty(&mut self, node: NodeId, penalty: f64) -> Option<()> {
        let s = self.node_slot(node)?;
        let arc = self.nodes[s].penalty_arc;
        if arc != NONE {
            let basic = self.arcs[arc as usize].is_basic();
            self.arcs[arc as usize].cost = penalty;
            if basic {
                self.pots_valid = false;
            }
            self.primal_dirty = true;
            self.dirty = true;
        }
        Some(())
    }

    /// Change a node's supply. Incremental: pushes the delta along the tree to
    /// preserve conservation (creating primal infeasibility the dual repairs),
    /// keeping costs/potentials and thus dual feasibility intact.
    pub fn set_supply(&mut self, node: NodeId, supply: i64) -> Option<()> {
        let s = self.node_slot(node)?;
        let delta = supply - self.nodes[s].supply;
        if delta == 0 {
            return Some(());
        }
        self.nodes[s].supply = supply;
        if self.tree_valid {
            // Route `delta` from the node up to the dummy root along tree arcs.
            self.push_supply_to_root(s, delta);
            self.dual_dirty = true;
        } else {
            self.needs_rebuild = true;
        }
        self.dirty = true;
        Some(())
    }

    /// Remove an arc. Incremental if non-basic; a basic arc is capped to zero
    /// and resolved by the dual (degenerate pivot-out), preserving the basis.
    pub fn remove_arc(&mut self, arc: ArcId) -> Option<()> {
        let s = self.arc_slot(arc)?;
        if self.arcs[s].is_penalty {
            return None; // penalty arcs are engine-managed
        }
        if self.arcs[s].is_basic() {
            if self.tree_valid {
                // Cap to zero so the dual drains its flow, then pivot it out and
                // kill it once it carries nothing.
                self.arcs[s].lower = 0;
                self.arcs[s].upper = 0;
                self.pending_kill.push(s as u32);
                self.dual_dirty = true;
            } else {
                self.needs_rebuild = true;
                self.inc_remove(s as u32);
                self.arcs[s].alive = false;
                self.free_arcs.push(s as u32);
            }
        } else {
            // Non-basic: drop immediately. If it rested at a non-zero bound it
            // was carrying flow, so re-balance conservation first.
            if self.arcs[s].flow != 0 {
                if self.tree_valid {
                    self.push_nonbasic_reset(s, 0);
                    self.dual_dirty = true;
                } else {
                    self.needs_rebuild = true;
                }
            }
            self.inc_remove(s as u32);
            self.arcs[s].alive = false;
            self.free_arcs.push(s as u32);
        }
        self.dirty = true;
        Some(())
    }

    /// Remove a node and all its arcs. Structural surgery: triggers a feasible
    /// rebuild (rare in streaming workloads, which correct via `set_supply`).
    pub fn remove_node(&mut self, node: NodeId) -> Option<()> {
        let s = self.node_slot(node)?;
        // Kill every arc incident to this node, keeping incidence lists tidy.
        for a in 0..self.arcs.len() {
            let arc = &self.arcs[a];
            if arc.alive && (arc.from as usize == s || arc.to as usize == s) {
                self.inc_remove(a as u32);
                self.arcs[a].alive = false;
                self.free_arcs.push(a as u32);
            }
        }
        self.inc[s].clear();
        self.nodes[s].alive = false;
        self.nodes[s].penalty_arc = NONE;
        self.free_nodes.push(s as u32);
        self.needs_rebuild = true;
        self.dirty = true;
        Some(())
    }

    // --- queries ---------------------------------------------------------

    /// Flow currently routed on an arc (includes arcs saturated at their upper
    /// bound; 0 for unknown/stale handles).
    pub fn flow(&self, arc: ArcId) -> i64 {
        match self.arc_slot(arc) {
            Some(s) => self.arcs[s].flow,
            None => 0,
        }
    }

    /// Iterate matched real arcs (non-penalty arcs carrying positive flow,
    /// whether basic or saturated) as `(from, to, flow)` triples.
    pub fn matches(&self) -> impl Iterator<Item = (NodeId, NodeId, i64)> + '_ {
        self.arcs.iter().filter_map(move |a| {
            if a.alive && !a.is_penalty && a.flow > 0 {
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

    /// Total objective: sum of `cost * flow` over all alive arcs (matched flow
    /// plus unmatched penalties).
    pub fn total_cost(&self) -> f64 {
        self.arcs
            .iter()
            .filter(|a| a.alive)
            .map(|a| a.cost * a.flow as f64)
            .sum()
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
        let mut net = Network {
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
            inc: vec![Vec::new(); n],
            subtree_buf: Vec::new(),
            queue: std::collections::VecDeque::new(),
            path_u: Vec::new(),
            path_v: Vec::new(),
            stamp: vec![0; n],
            cur_stamp: 0,
            price_start: 0,
            pending_kill: Vec::new(),
            tree_valid: false,
            pots_valid: false,
            primal_dirty: false,
            dual_dirty: false,
            needs_rebuild: false,
            dirty: true,
            dbg: SolveStats::default(),
        };
        net.rebuild_inc();
        net
    }

    /// Rebuild per-node incidence lists from the alive arc set. O(E).
    fn rebuild_inc(&mut self) {
        for l in &mut self.inc {
            l.clear();
        }
        for (idx, arc) in self.arcs.iter().enumerate() {
            if arc.alive {
                self.inc[arc.from as usize].push(idx as u32);
                self.inc[arc.to as usize].push(idx as u32);
            }
        }
    }

    /// Remove an arc slot from its endpoints' incidence lists.
    fn inc_remove(&mut self, arc_slot: u32) {
        let (f, t) = {
            let a = &self.arcs[arc_slot as usize];
            (a.from as usize, a.to as usize)
        };
        self.inc[f].retain(|&x| x != arc_slot);
        if t != f {
            self.inc[t].retain(|&x| x != arc_slot);
        }
    }

    // --- solve -----------------------------------------------------------

    /// Re-optimize from the cached basis. Returns when optimal or capped.
    ///
    /// Diagnostics from the most recent [`Network::solve`] call.
    pub fn stats(&self) -> SolveStats {
        self.dbg
    }

    /// Re-optimize from the cached basis. Returns when optimal or capped.
    ///
    /// Dispatches on what changed since the last solve:
    /// - RHS/bounds/removal (primal-infeasible, dual-feasible) -> dual repair;
    /// - cost changes (dual-infeasible, primal-feasible) -> primal pricing;
    /// - both at once (e.g. an FX reprice) -> a feasible rebuild + primal.
    pub fn solve(&mut self) -> SolveStatus {
        if self.needs_rebuild {
            self.kill_pending();
            self.rebuild_star_basis();
            self.needs_rebuild = false;
            self.dual_dirty = false; // the star basis is primal-feasible
            self.primal_dirty = true; // ... but not yet optimal
        }

        // Establish a consistent tree + potentials before pivoting.
        if !self.tree_valid {
            self.rebuild_tree();
        } else if !self.pots_valid {
            self.recompute_potentials();
            self.pots_valid = true;
        }

        if !self.dirty {
            return SolveStatus::Optimal;
        }

        let n_alive = self.nodes.iter().filter(|n| n.alive).count();
        let max_iterations = (n_alive * n_alive * 2).max(1000);
        let mut status = SolveStatus::Optimal;
        self.dbg = SolveStats::default();

        if self.dual_dirty && self.primal_dirty {
            // Mixed cost+RHS change: neither phase can warm-start safely alone
            // (this is the composite / dual-phase-1 case, left as a rebuild).
            self.kill_pending();
            self.rebuild_star_basis();
            self.rebuild_tree();
            self.dual_dirty = false;
        } else if self.dual_dirty {
            // Phase 1: restore primal feasibility while holding dual feasibility.
            let s = self.dual_repair(max_iterations);
            if s != SolveStatus::Optimal {
                status = s;
            }
            self.dual_dirty = false;
        }

        // Pivot out / drop any basic arcs capped to zero for removal.
        self.flush_pending_kill();

        // Phase 2: restore dual feasibility / optimality via primal pricing.
        let s = self.primal_optimize(max_iterations);
        if s != SolveStatus::Optimal {
            status = s;
        }
        self.primal_dirty = false;

        self.dirty = false;
        status
    }

    /// Drop arcs queued for deletion without any pivoting (used before a full
    /// rebuild, which discards the basis anyway).
    fn kill_pending(&mut self) {
        for a in std::mem::take(&mut self.pending_kill) {
            if self.arcs[a as usize].alive {
                self.arcs[a as usize].alive = false;
                self.free_arcs.push(a);
            }
        }
    }

    /// Primal pricing loop (Dantzig within a rolling block). Restores dual
    /// feasibility from a primal-feasible basis.
    fn primal_optimize(&mut self, max_iterations: usize) -> SolveStatus {
        let mut iterations = 0;
        let mut since_refactor = 0u32;
        loop {
            if iterations >= max_iterations {
                warn!("network simplex hit iteration cap ({max_iterations})");
                return SolveStatus::IterationLimit;
            }
            iterations += 1;
            self.dbg.primal_pivots += 1;

            let Some((entering, rc, dir)) = self.find_entering_block() else {
                debug!("optimal after {iterations} primal iterations");
                return SolveStatus::Optimal;
            };

            if !self.pivot(entering, rc, dir) {
                warn!("degenerate/unbounded: no leaving arc");
                return SolveStatus::Optimal;
            }

            since_refactor += 1;
            if since_refactor >= REFACTOR_INTERVAL {
                self.rebuild_tree();
                since_refactor = 0;
            }
        }
    }

    /// Reset the basis to the feasible dummy-star: every node connected to the
    /// dummy by its penalty arc, all real arcs non-basic. Used after mutations
    /// that break the spanning tree.
    fn rebuild_star_basis(&mut self) {
        for a in &mut self.arcs {
            if a.alive {
                a.state = ArcState::AtLower;
                a.flow = a.lower;
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
            a.state = ArcState::Basic;
        }
        self.tree_valid = false;
        self.pots_valid = false;
        self.dirty = true;
    }

    /// Recompute parents, depths, and potentials from the basic arcs via BFS.
    fn rebuild_tree(&mut self) {
        for a in &mut self.adj {
            a.clear();
        }
        for (idx, arc) in self.arcs.iter().enumerate() {
            if !arc.alive || !arc.is_basic() {
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
        self.tree_valid = true;
        self.pots_valid = true;
        self.price_start = 0;
    }

    /// Recompute potentials over the existing (valid) tree. O(N): no arc scan,
    /// just a BFS over the tree adjacency applying the basis cost relation.
    fn recompute_potentials(&mut self) {
        let root = self.dummy as usize;
        self.cur_stamp += 1;
        let s = self.cur_stamp;
        self.queue.clear();
        self.stamp[root] = s;
        self.potential[root] = 0.0;
        self.queue.push_back(root as u32);
        while let Some(u) = self.queue.pop_front() {
            let u = u as usize;
            for i in 0..self.adj[u].len() {
                let (v, arc_idx, forward) = self.adj[u][i];
                let v = v as usize;
                if self.stamp[v] == s {
                    continue;
                }
                self.stamp[v] = s;
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

    /// Reduced cost of an arc under the current potentials.
    #[inline]
    fn reduced_cost(&self, idx: usize) -> f64 {
        let a = &self.arcs[idx];
        a.cost - self.potential[a.to as usize] + self.potential[a.from as usize]
    }

    /// Find a bound-violating non-basic arc using rolling block (partial)
    /// pricing: scan a window of `PRICING_BLOCK` arcs starting where the last
    /// search stopped; if the window holds an improving arc, return its best,
    /// otherwise advance. Falls back to a full Dantzig scan for small problems.
    fn find_entering_block(&mut self) -> Option<(u32, f64, i64)> {
        let n = self.arcs.len();
        if n == 0 {
            return None;
        }
        let mut best: Option<(u32, f64, i64)> = None;
        let mut best_viol = -PRICING_TOLERANCE;
        let mut i = (self.price_start as usize) % n;
        let mut scanned = 0usize;
        let mut in_block = 0usize;
        while scanned < n {
            let arc = &self.arcs[i];
            if arc.alive {
                let dir = match arc.state {
                    ArcState::AtLower => 1i64,
                    ArcState::AtUpper => -1i64,
                    ArcState::Basic => 0,
                };
                if dir != 0 {
                    let rc = self.reduced_cost(i);
                    let viol = if dir > 0 { -rc } else { rc };
                    if viol > best_viol {
                        best_viol = viol;
                        best = Some((i as u32, rc, dir));
                    }
                }
            }
            i = (i + 1) % n;
            scanned += 1;
            in_block += 1;
            if in_block >= PRICING_BLOCK {
                if best.is_some() {
                    self.price_start = i as u32;
                    return best;
                }
                in_block = 0;
                best_viol = -PRICING_TOLERANCE;
            }
        }
        self.price_start = i as u32;
        best
    }

    /// Fill `path_u`/`path_v` with the two tree paths from `u` and `v` up to
    /// their lowest common ancestor (the cycle the chord `u-v` closes).
    fn cycle_paths(&mut self, u: usize, v: usize) {
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
    }

    /// Apply a flow change of magnitude `theta` (signed) around the cycle in
    /// `path_u`/`path_v`, with the chord oriented so its flow moves in `dir`.
    /// Does not touch the chord arc itself.
    fn apply_cycle_delta(&mut self, dir: i64, theta: i64) {
        let eff_inc = |nominal_inc: bool| if dir > 0 { nominal_inc } else { !nominal_inc };
        for k in 0..self.path_v.len() {
            let w = self.path_v[k] as usize;
            let idx = self.parent_arc[w] as usize;
            if eff_inc(!self.parent_forward[w]) {
                self.arcs[idx].flow += theta;
            } else {
                self.arcs[idx].flow -= theta;
            }
        }
        for k in 0..self.path_u.len() {
            let w = self.path_u[k] as usize;
            let idx = self.parent_arc[w] as usize;
            if eff_inc(self.parent_forward[w]) {
                self.arcs[idx].flow += theta;
            } else {
                self.arcs[idx].flow -= theta;
            }
        }
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

    /// Perform one bounded pivot around the cycle created by `entering`
    /// (reduced cost `rc`, moving in `dir`). Handles capacity limits and
    /// bound-flips (no basis change). Updates flows, adjacency, and potentials
    /// incrementally. Returns false only if the problem is unbounded.
    fn pivot(&mut self, entering: u32, rc: f64, dir: i64) -> bool {
        let (u, v) = {
            let a = &self.arcs[entering as usize];
            (a.from as usize, a.to as usize)
        };
        self.cycle_paths(u, v);

        // Ratio test. The entering arc moves its flow by `dir * theta`; each
        // cycle arc then increases or decreases. theta is bounded by the first
        // arc to hit a bound (or the entering arc's own width => bound flip).
        let limit = |inc: bool, a: &Arc| -> i64 {
            if inc {
                if a.upper == INF { INF } else { a.upper - a.flow }
            } else {
                a.flow - a.lower
            }
        };
        let eff_inc = |nominal_inc: bool| if dir > 0 { nominal_inc } else { !nominal_inc };

        // Start with the entering arc's own bound-flip width.
        let mut best_theta = {
            let a = &self.arcs[entering as usize];
            if a.upper == INF { INF } else { a.upper - a.lower }
        };
        let mut leaving = NONE; // NONE => bound flip on the entering arc
        let mut leaving_inc = false;
        let mut leaving_on_v = false;

        for k in 0..self.path_v.len() {
            let w = self.path_v[k] as usize;
            let idx = self.parent_arc[w];
            let inc = eff_inc(!self.parent_forward[w]);
            let lim = limit(inc, &self.arcs[idx as usize]);
            if lim <= best_theta {
                best_theta = lim;
                leaving = idx;
                leaving_inc = inc;
                leaving_on_v = true;
            }
        }
        for k in 0..self.path_u.len() {
            let w = self.path_u[k] as usize;
            let idx = self.parent_arc[w];
            let inc = eff_inc(self.parent_forward[w]);
            let lim = limit(inc, &self.arcs[idx as usize]);
            if lim <= best_theta {
                best_theta = lim;
                leaving = idx;
                leaving_inc = inc;
                leaving_on_v = false;
            }
        }

        if best_theta == INF {
            return false; // unbounded
        }
        let theta = best_theta;

        self.apply_cycle_delta(dir, theta);
        self.arcs[entering as usize].flow += dir * theta;

        if leaving == NONE {
            // Bound flip: the entering arc moves to its opposite bound; the
            // basis (tree/potentials) is unchanged.
            self.arcs[entering as usize].state =
                if dir > 0 { ArcState::AtUpper } else { ArcState::AtLower };
            return true;
        }

        // Real pivot: leaving arc rests at the bound it hit; entering joins the
        // basis.
        self.arcs[leaving as usize].state =
            if leaving_inc { ArcState::AtUpper } else { ArcState::AtLower };
        self.arcs[entering as usize].state = ArcState::Basic;

        // Incremental structure + potential update. The detached subtree is the
        // one containing v (if leaving was on the v-side) or u (otherwise); it
        // re-roots onto the opposite endpoint and shifts potentials by ±rc.
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

    // --- dual simplex (RHS / bounds / removal repair) -------------------

    /// Push `delta` extra supply at `node` along the tree to the dummy root,
    /// preserving conservation. Some basic arcs may go out of bounds (primal
    /// infeasibility) for the dual phase to repair.
    fn push_supply_to_root(&mut self, node: usize, delta: i64) {
        let root = self.dummy as usize;
        let mut w = node;
        while w != root {
            let arc = self.parent_arc[w] as usize;
            if self.parent_forward[w] {
                self.arcs[arc].flow -= delta;
            } else {
                self.arcs[arc].flow += delta;
            }
            w = self.parent[w] as usize;
        }
    }

    /// Reset a non-basic arc's resting flow to `new_flow`, pushing the delta
    /// around its tree cycle so conservation still holds.
    fn push_nonbasic_reset(&mut self, arc: usize, new_flow: i64) {
        let (u, v, old) = {
            let a = &self.arcs[arc];
            (a.from as usize, a.to as usize, a.flow)
        };
        let delta = new_flow - old;
        if delta != 0 {
            self.cycle_paths(u, v);
            self.apply_cycle_delta(1, delta);
        }
        self.arcs[arc].flow = new_flow;
    }

    /// Mark the subtree rooted at `child` (the side that detaches if `child`'s
    /// parent arc leaves the basis) with a fresh stamp, collecting its nodes in
    /// `subtree_buf`. Returns the stamp.
    fn mark_subtree(&mut self, child: usize) -> u32 {
        self.cur_stamp += 1;
        let s = self.cur_stamp;
        let p = self.parent[child] as usize;
        self.stamp[child] = s;
        self.subtree_buf.clear();
        self.subtree_buf.push(child as u32);
        self.queue.clear();
        self.queue.push_back(child as u32);
        while let Some(cur) = self.queue.pop_front() {
            let cur = cur as usize;
            for i in 0..self.adj[cur].len() {
                let (nbr, _, _) = self.adj[cur][i];
                let nbr = nbr as usize;
                if nbr == p || self.stamp[nbr] == s {
                    continue; // never cross to the parent side
                }
                self.stamp[nbr] = s;
                self.subtree_buf.push(nbr as u32);
                self.queue.push_back(nbr as u32);
            }
        }
        s
    }

    /// Most primal-infeasible basic arc. Returns `(arc, beta, violation)` where
    /// `beta == +1` means its flow is below the lower bound (must increase) and
    /// `beta == -1` means above the upper bound (must decrease).
    fn find_leaving_dual(&self) -> Option<(u32, i64, i64)> {
        let mut best = None;
        let mut best_viol = 0i64;
        for (idx, arc) in self.arcs.iter().enumerate() {
            if !arc.alive || !arc.is_basic() {
                continue;
            }
            let (viol, beta) = if arc.flow > arc.upper {
                (arc.flow - arc.upper, -1i64)
            } else if arc.flow < arc.lower {
                (arc.lower - arc.flow, 1i64)
            } else {
                continue;
            };
            if viol > best_viol {
                best_viol = viol;
                best = Some((idx as u32, beta, viol));
            }
        }
        best
    }

    /// Dual ratio test: among non-basic arcs crossing the cut induced by the
    /// leaving arc, pick the eligible one with the smallest |reduced cost| (so
    /// dual feasibility is preserved). `child` must already be subtree-marked.
    /// Returns `(entering, rc, dir)`.
    fn find_entering_dual(&mut self, leaving: u32, child: usize, beta: i64) -> Option<(u32, f64, i64)> {
        let s = self.mark_subtree(child);
        // Orientation of the leaving arc across the cut (R -> S is +1).
        let cross_l = {
            let a = &self.arcs[leaving as usize];
            if self.stamp[a.to as usize] == s { 1i64 } else { -1i64 }
        };
        let target = -beta * cross_l;

        // Every arc crossing the cut has exactly one endpoint in the subtree, so
        // it is reached via that endpoint's incidence list. Iterating the
        // subtree's incidence visits all candidates without touching the full
        // edge set.
        let mut best: Option<(u32, f64, i64)> = None;
        let mut best_abs = f64::INFINITY;
        for si in 0..self.subtree_buf.len() {
            let node = self.subtree_buf[si] as usize;
            for ci in 0..self.inc[node].len() {
                let idx = self.inc[node][ci] as usize;
                let arc = &self.arcs[idx];
                if !arc.alive || arc.is_basic() {
                    continue;
                }
                let a_in = self.stamp[arc.from as usize] == s;
                let b_in = self.stamp[arc.to as usize] == s;
                if a_in == b_in {
                    continue; // does not cross the cut
                }
                let cross = if !a_in && b_in { 1i64 } else { -1i64 };
                let dir = match arc.state {
                    ArcState::AtLower => 1i64,
                    ArcState::AtUpper => -1i64,
                    ArcState::Basic => continue,
                };
                if dir * cross != target {
                    continue; // would push the leaving arc the wrong way
                }
                let abs = self.reduced_cost(idx).abs();
                if abs < best_abs {
                    best_abs = abs;
                    best = Some((idx as u32, self.reduced_cost(idx), dir));
                }
            }
        }
        best
    }

    /// Perform a dual pivot: `leaving` exits to the bound it violated, moving
    /// its flow by exactly `theta`; `entering` joins the basis. Other basic
    /// arcs may become infeasible (handled in later dual iterations).
    fn dual_pivot(&mut self, entering: u32, rc: f64, dir: i64, leaving: DualLeave) {
        let DualLeave {
            arc: leaving,
            beta,
            theta,
        } = leaving;
        let (u, v) = {
            let a = &self.arcs[entering as usize];
            (a.from as usize, a.to as usize)
        };
        self.cycle_paths(u, v);
        self.apply_cycle_delta(dir, theta);
        self.arcs[entering as usize].flow += dir * theta;

        // Leaving arc rests at the bound it hit (over upper => AtUpper).
        self.arcs[leaving as usize].state = if beta < 0 {
            ArcState::AtUpper
        } else {
            ArcState::AtLower
        };
        self.arcs[entering as usize].state = ArcState::Basic;

        // Re-root the detached subtree (the `child` side). The subtree stamp
        // from `find_entering_dual` is still current, so reuse it to find which
        // entering endpoint lies inside the subtree.
        let s = self.cur_stamp;
        let (x, y, delta_pot) = if self.stamp[v] == s {
            (v, u, rc)
        } else {
            (u, v, -rc)
        };
        self.adj_remove(leaving);
        self.adj_add(entering);
        self.bfs_reroot(x, y, entering, delta_pot);
    }

    /// The child endpoint of a basic tree arc (the node whose parent arc it is).
    fn arc_child(&self, arc: u32) -> usize {
        let (a, b) = {
            let x = &self.arcs[arc as usize];
            (x.from as usize, x.to as usize)
        };
        if self.parent_arc[a] == arc { a } else { b }
    }

    /// Dual simplex loop: repeatedly fix the most primal-infeasible basic arc
    /// while preserving dual feasibility. Terminates primal-feasible (optimal).
    fn dual_repair(&mut self, max_iterations: usize) -> SolveStatus {
        let mut iterations = 0;
        let mut since_refactor = 0u32;
        loop {
            if iterations >= max_iterations {
                warn!("dual simplex hit iteration cap ({max_iterations})");
                return SolveStatus::IterationLimit;
            }
            iterations += 1;
            self.dbg.dual_pivots += 1;

            let Some((leaving, beta, viol)) = self.find_leaving_dual() else {
                debug!("primal-feasible after {iterations} dual iterations");
                return SolveStatus::Optimal;
            };
            let child = self.arc_child(leaving);
            let Some((entering, rc, dir)) = self.find_entering_dual(leaving, child, beta) else {
                // No eligible entering arc: fall back to a feasible rebuild.
                warn!("dual: no entering arc; rebuilding");
                self.needs_rebuild = true;
                return SolveStatus::Optimal;
            };
            self.dbg.subtree_nodes += self.subtree_buf.len() as u64;
            self.dual_pivot(entering, rc, dir, DualLeave { arc: leaving, beta, theta: viol });

            since_refactor += 1;
            if since_refactor >= REFACTOR_INTERVAL {
                self.rebuild_tree();
                since_refactor = 0;
            }
        }
    }

    /// Drop arcs queued by `remove_arc`: each was capped to `[0, 0]`, so the
    /// dual has drained its flow. Pivot out any that are still basic (degenerate
    /// at zero), then kill them.
    fn flush_pending_kill(&mut self) {
        if self.pending_kill.is_empty() {
            return;
        }
        for s in std::mem::take(&mut self.pending_kill) {
            let s = s as usize;
            if !self.arcs[s].alive {
                continue;
            }
            if self.arcs[s].is_basic() {
                // Degenerate (flow 0) pivot-out, picking an entering arc that
                // keeps dual feasibility.
                let child = self.arc_child(s as u32);
                if let Some((entering, rc, dir)) = self.find_entering_dual(s as u32, child, 1) {
                    self.dual_pivot(
                        entering,
                        rc,
                        dir,
                        DualLeave { arc: s as u32, beta: 1, theta: 0 },
                    );
                } else {
                    self.needs_rebuild = true;
                    continue;
                }
            }
            self.arcs[s].alive = false;
            self.free_arcs.push(s as u32);
        }
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
    fn capacity_caps_flow() {
        // A single arc capped below the full amount: it should saturate at its
        // upper bound, leaving the remainder unmatched.
        let mut net = Network::new();
        let s = net.add_node(100, 1000.0);
        let t = net.add_node(-100, 1000.0);
        let a = net.add_arc_bounded(s, t, 1.0, 0, 70).unwrap();
        net.solve();
        assert_eq!(net.flow(a), 70);
    }

    #[test]
    fn capacity_splits_across_sinks() {
        // Source must split: cheap sink is capacity-limited, rest spills to the
        // pricier sink.
        let mut net = Network::new();
        let s = net.add_node(100, 1e6);
        let t0 = net.add_node(-50, 1e6);
        let t1 = net.add_node(-50, 1e6);
        let a0 = net.add_arc_bounded(s, t0, 1.0, 0, 30).unwrap(); // cheap but capped
        let a1 = net.add_arc(s, t1, 5.0).unwrap(); // pricier, uncapped
        net.solve();
        assert_eq!(net.flow(a0), 30);
        // 70 remain; 50 fill t1, 20 of source unmatched (t0 short by 20).
        assert_eq!(net.flow(a1), 50);
    }

    #[test]
    fn set_bounds_then_resolve() {
        let mut net = Network::new();
        let s = net.add_node(100, 1000.0);
        let t = net.add_node(-100, 1000.0);
        let a = net.add_arc(s, t, 1.0).unwrap();
        net.solve();
        assert_eq!(net.flow(a), 100);
        net.set_bounds(a, 0, 40);
        net.solve();
        assert_eq!(net.flow(a), 40);
    }

    #[test]
    #[allow(clippy::needless_range_loop)]
    fn capacitated_warm_matches_cold() {
        // Random capacitated transportation driven through cost+bound mutations
        // with warm re-solves; objective must match a cold build each round.
        let mut seed: u64 = 0x0bad_f00d_1234_5678;
        let mut rng = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let k = 4;
        let supply = 20i64;

        let mut warm = Network::new();
        let ws: Vec<NodeId> = (0..k).map(|_| warm.add_node(supply, 100.0)).collect();
        let wt: Vec<NodeId> = (0..k).map(|_| warm.add_node(-supply, 100.0)).collect();
        let mut warc = vec![vec![]; k];
        for r in 0..k {
            for c in 0..k {
                warc[r].push(warm.add_arc_bounded(ws[r], wt[c], 1.0, 0, supply).unwrap());
            }
        }

        for round in 0..25 {
            let costs: Vec<Vec<f64>> =
                (0..k).map(|_| (0..k).map(|_| 1.0 + (rng() % 40) as f64).collect()).collect();
            let caps: Vec<Vec<i64>> =
                (0..k).map(|_| (0..k).map(|_| (rng() % (supply as u64 + 1)) as i64).collect()).collect();
            for r in 0..k {
                for c in 0..k {
                    warm.set_cost(warc[r][c], costs[r][c]);
                    warm.set_bounds(warc[r][c], 0, caps[r][c]);
                }
            }
            warm.solve();
            let warm_cost = warm.total_cost();

            let mut cold = Network::new();
            let cs: Vec<NodeId> = (0..k).map(|_| cold.add_node(supply, 100.0)).collect();
            let ct: Vec<NodeId> = (0..k).map(|_| cold.add_node(-supply, 100.0)).collect();
            for r in 0..k {
                for c in 0..k {
                    cold.add_arc_bounded(cs[r], ct[c], costs[r][c], 0, caps[r][c]);
                }
            }
            cold.solve();
            let cold_cost = cold.total_cost();

            assert!(
                (warm_cost - cold_cost).abs() < 1e-6,
                "round {round}: warm {warm_cost} != cold {cold_cost}"
            );
        }
    }

    #[test]
    #[allow(clippy::needless_range_loop)]
    fn dual_warm_matches_cold() {
        // Stress the incremental dual/primal paths: random supply edits, arc
        // removals/adds and cost changes on a standing tree, each re-solved
        // warm and checked against a cold rebuild of the same shadow state.
        fn xorshift(seed: &mut u64) -> u64 {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            *seed
        }
      for trial in 0..12u64 {
        let mut seed = 0xfeed_dead_beef_cafe ^ trial.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let mut rng = || xorshift(&mut seed);
        let k = 4 + (trial as usize % 4); // 4..7
        let pen = 500.0;

        // Shadow state.
        let mut sup = vec![0i64; 2 * k]; // 0..k sources (+), k..2k sinks (-)
        for i in 0..k {
            sup[i] = 5 + (rng() % 10) as i64;
            sup[k + i] = -(5 + (rng() % 10) as i64);
        }
        let mut cost = vec![vec![0.0f64; k]; k];
        let mut present = vec![vec![false; k]; k];
        for r in 0..k {
            for c in 0..k {
                cost[r][c] = 1.0 + (rng() % 30) as f64;
                present[r][c] = rng() % 3 != 0;
            }
        }

        // Warm network mirroring the shadow.
        let mut warm = Network::new();
        let wsrc: Vec<NodeId> = (0..k).map(|i| warm.add_node(sup[i], pen)).collect();
        let wsnk: Vec<NodeId> = (0..k).map(|i| warm.add_node(sup[k + i], pen)).collect();
        let mut waid = vec![vec![None::<ArcId>; k]; k];
        for r in 0..k {
            for c in 0..k {
                if present[r][c] {
                    waid[r][c] = warm.add_arc(wsrc[r], wsnk[c], cost[r][c]);
                }
            }
        }
        warm.solve();

        let build_cold = |sup: &[i64], cost: &[Vec<f64>], present: &[Vec<bool>]| {
            let mut cold = Network::new();
            let s: Vec<NodeId> = (0..k).map(|i| cold.add_node(sup[i], pen)).collect();
            let t: Vec<NodeId> = (0..k).map(|i| cold.add_node(sup[k + i], pen)).collect();
            for r in 0..k {
                for c in 0..k {
                    if present[r][c] {
                        cold.add_arc(s[r], t[c], cost[r][c]);
                    }
                }
            }
            cold.solve();
            cold.total_cost()
        };

        for round in 0..200 {
            match rng() % 4 {
                0 => {
                    // Supply edit (keep sign so sources stay sources).
                    let i = (rng() % (2 * k) as u64) as usize;
                    let mag = 5 + (rng() % 10) as i64;
                    sup[i] = if i < k { mag } else { -mag };
                    let node = if i < k { wsrc[i] } else { wsnk[i - k] };
                    warm.set_supply(node, sup[i]);
                }
                1 => {
                    // Cost change on a present arc.
                    let r = (rng() % k as u64) as usize;
                    let c = (rng() % k as u64) as usize;
                    if present[r][c] {
                        cost[r][c] = 1.0 + (rng() % 30) as f64;
                        warm.set_cost(waid[r][c].unwrap(), cost[r][c]);
                    }
                }
                2 => {
                    // Remove a present arc.
                    let r = (rng() % k as u64) as usize;
                    let c = (rng() % k as u64) as usize;
                    if present[r][c] {
                        present[r][c] = false;
                        warm.remove_arc(waid[r][c].take().unwrap());
                    }
                }
                _ => {
                    // Add a missing arc.
                    let r = (rng() % k as u64) as usize;
                    let c = (rng() % k as u64) as usize;
                    if !present[r][c] {
                        present[r][c] = true;
                        cost[r][c] = 1.0 + (rng() % 30) as f64;
                        waid[r][c] = warm.add_arc(wsrc[r], wsnk[c], cost[r][c]);
                    }
                }
            }
            warm.solve();
            let warm_cost = warm.total_cost();
            let cold_cost = build_cold(&sup, &cost, &present);
            assert!(
                (warm_cost - cold_cost).abs() < 1e-6,
                "trial {trial} round {round}: warm {warm_cost} != cold {cold_cost}"
            );
        }
      }
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
