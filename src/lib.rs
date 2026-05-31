use log::{debug, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------
const FLOW_THRESHOLD: f64 = 1e-9;
const PRICING_TOLERANCE: f64 = -1e-12;
const BIG_M_PENALTY_DELTA: f64 = 1000.0;
const PRICING_BLOCK_SIZE: usize = 65536;

// ---------------------------------------------------------------------------
// Sparse API Types
// ---------------------------------------------------------------------------

/// Result of the sparse reconciliation matching.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SparseMatch {
    /// Original index of the matched source node in the user's `supplies` array.
    pub source_idx: usize,
    /// Original index of the matched sink node in the user's `supplies` array.
    pub sink_idx: usize,
    /// The amount of flow (value) routed between these two nodes.
    pub flow: f64,
}

/// Unique identifier for each potential directed edge in the transport network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeId {
    /// A real flow-carrying edge from source to sink.
    Real { source: usize, sink: usize },
    /// Dummy edge from the dummy source to a sink (leaves a sink unmatched, penalized).
    DummySourceToSink { sink: usize },
    /// Dummy edge from a source to the dummy sink (leaves a source unmatched, penalized).
    SourceToDummySink { source: usize },
    /// Dummy edge connecting dummy source to dummy sink.
    DummySourceToDummySink,
}

impl EdgeId {
    /// Get the endpoints (from, to) of the directed edge.
    #[inline]
    pub fn endpoints(&self, dummy_source: usize, dummy_sink: usize) -> (usize, usize) {
        match *self {
            EdgeId::Real { source, sink } => (source, sink),
            EdgeId::DummySourceToSink { sink } => (dummy_source, sink),
            EdgeId::SourceToDummySink { source } => (source, dummy_sink),
            EdgeId::DummySourceToDummySink => (dummy_source, dummy_sink),
        }
    }
}

/// A basic edge in the spanning tree with its active flow and static cost.
#[derive(Debug, Clone)]
pub struct BasicEdge {
    pub id: EdgeId,
    pub flow: f64,
    pub cost: f64,
    pub candidate_idx: Option<usize>,
}

/// An entry in the adjacency list representing a spanning tree edge.
#[derive(Debug, Clone, Copy)]
pub struct AdjEntry {
    pub neighbor: usize,
    pub edge_idx: usize,
    pub is_forward_from_curr: bool,
}

/// A stateful sparse reconciler that expects user-specified candidate edges.
///
/// This maintains an optimal basis tree and flow solution from previous runs, allowing
/// incremental re-optimization (warm start) when given a new cost function.
pub struct SparseReconciler {
    supplies: Vec<f64>,
    unmatched_penalties: Vec<f64>,
    source_map: Vec<usize>,
    sink_map: Vec<usize>,

    // Spanning tree state
    basis_edges: Option<Vec<BasicEdge>>,
    potentials: Vec<f64>,
    parent: Vec<usize>,
    parent_edge_idx: Vec<usize>,
    parent_direction_forward: Vec<bool>,
    depth: Vec<usize>,

    // Reusable allocation-free buffers
    adj: Vec<Vec<AdjEntry>>,
    visited: Vec<bool>,
    queue: std::collections::VecDeque<usize>,
    source_to_dummy_sink_basic: Vec<bool>,
    dummy_source_to_sink_basic: Vec<bool>,
    dummy_source_to_dummy_sink_basic: bool,
    path_u: Vec<usize>,
    path_v: Vec<usize>,

    // Sparse custom-edge pricing state storing (u_internal, v_internal, cost)
    candidate_edges: Vec<(usize, usize, f64)>,
    is_candidate_basic: Vec<bool>,
    next_edge_to_scan: usize,
}

impl SparseReconciler {
    /// Create a new stateful solver instance initialized with the node supplies/demands.
    pub fn new(supplies: Vec<f64>) -> Self {
        let mut source_map = Vec::new();
        let mut sink_map = Vec::new();
        for (idx, &val) in supplies.iter().enumerate() {
            if val > 0.0 {
                source_map.push(idx);
            } else if val < 0.0 {
                sink_map.push(idx);
            }
        }

        let m = source_map.len();
        let n = sink_map.len();
        let num_nodes = m + n + 2;

        let potentials = vec![0.0; num_nodes];
        let parent = vec![0; num_nodes];
        let parent_edge_idx = vec![0; num_nodes];
        let parent_direction_forward = vec![true; num_nodes];
        let depth = vec![0; num_nodes];

        let adj = vec![Vec::new(); num_nodes];
        let visited = vec![false; num_nodes];
        let queue = std::collections::VecDeque::with_capacity(num_nodes);
        let source_to_dummy_sink_basic = vec![false; m];
        let dummy_source_to_sink_basic = vec![false; n];
        let path_u = Vec::with_capacity(num_nodes);
        let path_v = Vec::with_capacity(num_nodes);
        let unmatched_penalties = vec![0.0; supplies.len()];

        Self {
            supplies,
            unmatched_penalties,
            source_map,
            sink_map,
            basis_edges: None,
            potentials,
            parent,
            parent_edge_idx,
            parent_direction_forward,
            depth,
            adj,
            visited,
            queue,
            source_to_dummy_sink_basic,
            dummy_source_to_sink_basic,
            dummy_source_to_dummy_sink_basic: false,
            path_u,
            path_v,
            candidate_edges: Vec::new(),
            is_candidate_basic: Vec::new(),
            next_edge_to_scan: 0,
        }
    }

    /// Safely updates the unmatched penalties and candidate costs in one atomic, validated operation.
    /// Returns Some(()) if valid, or None if any input invariant is violated.
    pub fn update_costs(&mut self, penalties: &[f64], costs: &[(usize, usize, f64)]) -> Option<()> {
        // 1. INVARIANT CHECK: Penalties must be equal in length to supplies
        if penalties.len() != self.supplies.len() {
            return None;
        }

        // 2. INVARIANT CHECK: Validate all user-facing edge indices and signs
        let n_nodes = self.supplies.len();
        for &(u_user, v_user, _cost) in costs {
            if u_user >= n_nodes || v_user >= n_nodes {
                return None; // Out of bounds
            }
            if self.supplies[u_user] <= 0.0 || self.supplies[v_user] >= 0.0 {
                return None; // u_user must be a source, v_user must be a sink
            }
        }

        // 3. Update local unmatched penalties buffer
        self.unmatched_penalties.clear();
        self.unmatched_penalties
            .extend(penalties.iter().map(|&p| p.max(0.0)));

        // 4. Map and cache new candidate edges
        self.candidate_edges.clear();
        for &(u_user, v_user, cost) in costs {
            let u = self.source_map.binary_search(&u_user).unwrap();
            let v = self.sink_map.binary_search(&v_user).unwrap();
            self.candidate_edges.push((u, v, cost));
        }
        self.is_candidate_basic.clear();
        self.is_candidate_basic
            .resize(self.candidate_edges.len(), false);
        self.next_edge_to_scan = 0;

        // 5. WARM-START REPAIR (If basis is already initialized)
        if let Some(basis_edges) = &mut self.basis_edges {
            let m = self.source_map.len();

            self.source_to_dummy_sink_basic.fill(false);
            self.dummy_source_to_sink_basic.fill(false);
            self.dummy_source_to_dummy_sink_basic = false;

            // Re-map basis edge costs using the new static candidate costs
            for edge in basis_edges {
                match edge.id {
                    EdgeId::Real { source, sink } => {
                        if let Some(cand_idx) = self
                            .candidate_edges
                            .iter()
                            .position(|&(u, v, _)| u == source && v == sink - m)
                        {
                            edge.cost = self.candidate_edges[cand_idx].2;
                            edge.candidate_idx = Some(cand_idx);
                            self.is_candidate_basic[cand_idx] = true;
                        } else {
                            // Apply Big-M cost to smoothly pivot out removed edges
                            let u_user = self.source_map[source];
                            let v_user = self.sink_map[sink - m];
                            edge.cost = self.unmatched_penalties[u_user]
                                + self.unmatched_penalties[v_user]
                                + BIG_M_PENALTY_DELTA;
                            edge.candidate_idx = None;
                        }
                    }
                    EdgeId::DummySourceToSink { sink } => {
                        let v_user = self.sink_map[sink - m];
                        edge.cost = self.unmatched_penalties[v_user];
                        edge.candidate_idx = None;
                        self.dummy_source_to_sink_basic[sink - m] = true;
                    }
                    EdgeId::SourceToDummySink { source } => {
                        let u_user = self.source_map[source];
                        edge.cost = self.unmatched_penalties[u_user];
                        edge.candidate_idx = None;
                        self.source_to_dummy_sink_basic[source] = true;
                    }
                    EdgeId::DummySourceToDummySink => {
                        edge.cost = 0.0;
                        edge.candidate_idx = None;
                        self.dummy_source_to_dummy_sink_basic = true;
                    }
                }
            }
        }

        Some(())
    }

    /// Returns the original user-facing indices of all registered source nodes.
    pub fn sources(&self) -> &[usize] {
        &self.source_map
    }

    /// Returns the original user-facing indices of all registered sink nodes.
    pub fn sinks(&self) -> &[usize] {
        &self.sink_map
    }

    #[inline]
    fn dummy_source(&self) -> usize {
        self.source_map.len() + self.sink_map.len()
    }

    #[inline]
    fn dummy_sink(&self) -> usize {
        self.source_map.len() + self.sink_map.len() + 1
    }

    /// Rebuilds parents, depths, and potentials from `basis_edges` using BFS.
    ///
    /// This runs completely allocation-free and uses static stored costs inside `basis_edges`.
    fn rebuild_tree(&mut self) {
        let root = self.dummy_source();

        for a in &mut self.adj {
            a.clear();
        }

        let basis_edges = self
            .basis_edges
            .as_ref()
            .expect("basis_edges must be initialized");
        for (idx, edge) in basis_edges.iter().enumerate() {
            let (from, to) = edge.id.endpoints(self.dummy_source(), self.dummy_sink());
            self.adj[from].push(AdjEntry {
                neighbor: to,
                edge_idx: idx,
                is_forward_from_curr: true,
            });
            self.adj[to].push(AdjEntry {
                neighbor: from,
                edge_idx: idx,
                is_forward_from_curr: false,
            });
        }

        self.visited.fill(false);
        self.queue.clear();

        self.visited[root] = true;
        self.depth[root] = 0;
        self.potentials[root] = 0.0;
        self.parent[root] = root;
        self.parent_edge_idx[root] = usize::MAX;
        self.parent_direction_forward[root] = true;

        self.queue.push_back(root);

        while let Some(u) = self.queue.pop_front() {
            for i in 0..self.adj[u].len() {
                let entry = self.adj[u][i];
                let v = entry.neighbor;
                if !self.visited[v] {
                    self.visited[v] = true;
                    self.parent[v] = u;
                    self.parent_edge_idx[v] = entry.edge_idx;
                    self.parent_direction_forward[v] = entry.is_forward_from_curr;
                    self.depth[v] = self.depth[u] + 1;

                    let cost = basis_edges[entry.edge_idx].cost;
                    if entry.is_forward_from_curr {
                        self.potentials[v] = self.potentials[u] + cost;
                    } else {
                        self.potentials[v] = self.potentials[u] - cost;
                    }

                    self.queue.push_back(v);
                }
            }
        }
    }

    /// Finds an entering arc using rolling block partial-pricing over user-specified sparse candidate edges.
    fn find_entering_arc(&mut self) -> Option<(EdgeId, f64, Option<usize>)> {
        let m = self.source_map.len();
        let n = self.sink_map.len();
        let dummy_src = self.dummy_source();
        let dummy_snk = self.dummy_sink();

        let num_candidates = self.candidate_edges.len();
        let mut best_edge = None;
        let mut best_rc = PRICING_TOLERANCE;

        if num_candidates > 0 {
            let block_size = PRICING_BLOCK_SIZE.min(num_candidates);
            let mut edges_scanned = 0;
            while edges_scanned < num_candidates {
                let start = self.next_edge_to_scan;
                let remaining_to_scan = num_candidates - edges_scanned;
                let current_block_size = block_size.min(remaining_to_scan);
                let end = (start + current_block_size).min(num_candidates);
                let chunk_len = end - start;

                for k in start..end {
                    if self.is_candidate_basic[k] {
                        continue;
                    }
                    let (u, v, cost) = self.candidate_edges[k];
                    let sink_node = m + v;
                    let rc = cost - self.potentials[sink_node] + self.potentials[u];
                    if rc < best_rc {
                        best_rc = rc;
                        best_edge = Some((
                            EdgeId::Real {
                                source: u,
                                sink: sink_node,
                            },
                            cost,
                            Some(k),
                        ));
                    }
                }

                self.next_edge_to_scan = if end == num_candidates { 0 } else { end };
                edges_scanned += chunk_len;

                if best_edge.is_some() {
                    return best_edge;
                }
            }
        }

        for v in 0..n {
            let sink_node = m + v;
            if !self.dummy_source_to_sink_basic[v] {
                let penalty = self.unmatched_penalties[self.sink_map[v]];
                let rc = penalty - self.potentials[sink_node] + self.potentials[dummy_src];
                if rc < best_rc {
                    best_rc = rc;
                    best_edge =
                        Some((EdgeId::DummySourceToSink { sink: sink_node }, penalty, None));
                }
            }
        }

        for u in 0..m {
            if !self.source_to_dummy_sink_basic[u] {
                let penalty = self.unmatched_penalties[self.source_map[u]];
                let rc = penalty - self.potentials[dummy_snk] + self.potentials[u];
                if rc < best_rc {
                    best_rc = rc;
                    best_edge = Some((EdgeId::SourceToDummySink { source: u }, penalty, None));
                }
            }
        }

        if !self.dummy_source_to_dummy_sink_basic {
            let rc = 0.0 - self.potentials[dummy_snk] + self.potentials[dummy_src];
            if rc < best_rc {
                best_edge = Some((EdgeId::DummySourceToDummySink, 0.0, None));
            }
        }

        best_edge
    }

    /// Solves the transportation problem statefully based on the current validated input dataset.
    pub fn solve(&mut self) -> Vec<SparseMatch> {
        let m = self.source_map.len();
        let n = self.sink_map.len();

        if m == 0 || n == 0 {
            return Vec::new();
        }

        // Safety check: If basis has not been built yet, we must build the initial basis tree (cold start)
        if self.basis_edges.is_none() {
            let mut basis_edges = Vec::with_capacity(m + n + 1);

            for u in 0..m {
                let user_idx = self.source_map[u];
                let val = self.supplies[user_idx];
                let penalty = self.unmatched_penalties[user_idx];
                basis_edges.push(BasicEdge {
                    id: EdgeId::SourceToDummySink { source: u },
                    flow: val,
                    cost: penalty,
                    candidate_idx: None,
                });
            }

            for v in 0..n {
                let user_idx = self.sink_map[v];
                let val = self.supplies[user_idx].abs();
                let penalty = self.unmatched_penalties[user_idx];
                basis_edges.push(BasicEdge {
                    id: EdgeId::DummySourceToSink { sink: m + v },
                    flow: val,
                    cost: penalty,
                    candidate_idx: None,
                });
            }

            basis_edges.push(BasicEdge {
                id: EdgeId::DummySourceToDummySink,
                flow: 0.0,
                cost: 0.0,
                candidate_idx: None,
            });

            self.basis_edges = Some(basis_edges);

            // --- NEW: Initialize tracking arrays for cold start ---
            self.source_to_dummy_sink_basic.fill(true);
            self.dummy_source_to_sink_basic.fill(true);
            self.dummy_source_to_dummy_sink_basic = true;
        }

        let dummy_src = self.dummy_source();
        let dummy_snk = self.dummy_sink();
        let max_iterations = (m + n + 2) * (m + n + 2) * 2;
        let mut iterations = 0;

        loop {
            if iterations >= max_iterations {
                warn!("Max iterations reached");
                break;
            }
            iterations += 1;

            self.rebuild_tree();

            let entering_info = self.find_entering_arc();
            if entering_info.is_none() {
                debug!("Optimal after {} iterations (incremental)", iterations);
                break;
            }
            let (entering, entering_cost, candidate_idx) = entering_info.unwrap();

            let (u, v) = entering.endpoints(dummy_src, dummy_snk);

            self.path_u.clear();
            self.path_v.clear();

            let mut curr_u = u;
            let mut curr_v = v;

            while curr_u != curr_v {
                if self.depth[curr_u] > self.depth[curr_v] {
                    self.path_u.push(curr_u);
                    curr_u = self.parent[curr_u];
                } else if self.depth[curr_v] > self.depth[curr_u] {
                    self.path_v.push(curr_v);
                    curr_v = self.parent[curr_v];
                } else {
                    self.path_u.push(curr_u);
                    self.path_v.push(curr_v);
                    curr_u = self.parent[curr_u];
                    curr_v = self.parent[curr_v];
                }
            }
            let _lca = curr_u;

            let mut min_theta = f64::MAX;
            let mut leaving_edge_basis_idx = None;

            let basis_edges = self
                .basis_edges
                .as_mut()
                .expect("basis_edges must be initialized");

            for &w in &self.path_v {
                let idx = self.parent_edge_idx[w];
                if self.parent_direction_forward[w] {
                    let flow = basis_edges[idx].flow;
                    if flow < min_theta {
                        min_theta = flow;
                        leaving_edge_basis_idx = Some(idx);
                    }
                }
            }

            for &w in &self.path_u {
                let idx = self.parent_edge_idx[w];
                if !self.parent_direction_forward[w] {
                    let flow = basis_edges[idx].flow;
                    if flow < min_theta {
                        min_theta = flow;
                        leaving_edge_basis_idx = Some(idx);
                    }
                }
            }

            if leaving_edge_basis_idx.is_none() {
                warn!("No leaving edge found in cycle");
                break;
            }
            let lei = leaving_edge_basis_idx.unwrap();
            let theta = min_theta;

            if iterations % 1000 == 0 {
                println!(
                    "Iter {}: Entering={:?}, theta={}",
                    iterations, entering, theta
                );
            }

            for &w in &self.path_v {
                let idx = self.parent_edge_idx[w];
                if self.parent_direction_forward[w] {
                    basis_edges[idx].flow -= theta;
                } else {
                    basis_edges[idx].flow += theta;
                }
            }

            for &w in &self.path_u {
                let idx = self.parent_edge_idx[w];
                if self.parent_direction_forward[w] {
                    basis_edges[idx].flow += theta;
                } else {
                    basis_edges[idx].flow -= theta;
                }
            }

            let leaving_id = basis_edges[lei].id;
            let leaving_cand_idx = basis_edges[lei].candidate_idx;

            // 1. Remove the leaving edge from the boolean trackers
            match leaving_id {
                EdgeId::Real { .. } => {
                    if let Some(idx) = leaving_cand_idx {
                        self.is_candidate_basic[idx] = false;
                    }
                }
                EdgeId::SourceToDummySink { source } => self.source_to_dummy_sink_basic[source] = false,
                EdgeId::DummySourceToSink { sink } => self.dummy_source_to_sink_basic[sink - m] = false,
                EdgeId::DummySourceToDummySink => self.dummy_source_to_dummy_sink_basic = false,
            }

            // 2. Add the newly entering edge to the boolean trackers
            match entering {
                EdgeId::Real { .. } => {
                    if let Some(idx) = candidate_idx {
                        self.is_candidate_basic[idx] = true;
                    }
                }
                EdgeId::SourceToDummySink { source } => self.source_to_dummy_sink_basic[source] = true,
                EdgeId::DummySourceToSink { sink } => self.dummy_source_to_sink_basic[sink - m] = true,
                EdgeId::DummySourceToDummySink => self.dummy_source_to_dummy_sink_basic = true,
            }

            // 3. Update the tree
            basis_edges[lei] = BasicEdge {
                id: entering,
                flow: theta,
                cost: entering_cost,
                candidate_idx,
            };
        }

        let mut matches = Vec::new();
        let basis_edges = self
            .basis_edges
            .as_ref()
            .expect("basis_edges must be initialized");
        for edge in basis_edges {
            if edge.flow > FLOW_THRESHOLD
                && let EdgeId::Real { source, sink } = edge.id
            {
                let source_idx = self.source_map[source];
                let sink_idx = self.sink_map[sink - m];
                matches.push(SparseMatch {
                    source_idx,
                    sink_idx,
                    flow: edge.flow,
                });
            }
        }

        matches
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_match() {
        let supplies = vec![100.0, -100.0];
        let penalties = vec![1e6, 1e6];

        let mut recon = SparseReconciler::new(supplies);
        recon.update_costs(&penalties, &[(0, 1, 1.0)]).unwrap();
        let matches = recon.solve();

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].source_idx, 0);
        assert_eq!(matches[0].sink_idx, 1);
        assert!((matches[0].flow - 100.0).abs() < 1e-6);
    }

    #[test]
    fn test_unmatched() {
        let supplies = vec![100.0, -50.0];
        let penalties = vec![1e6, 1e6];

        let mut recon = SparseReconciler::new(supplies);
        recon.update_costs(&penalties, &[(0, 1, 1.0)]).unwrap();
        let matches = recon.solve();

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].source_idx, 0);
        assert_eq!(matches[0].sink_idx, 1);
        assert!((matches[0].flow - 50.0).abs() < 1e-6);
    }

    #[test]
    fn test_stateful_reconciler_incremental() {
        let supplies = vec![100.0, 50.0, -100.0, -50.0];
        let penalties = vec![10000.0; 4];

        let mut recon = SparseReconciler::new(supplies);
        let all_edges = vec![(0, 2, 10.0), (0, 3, 10.0), (1, 2, 10.0), (1, 3, 10.0)];
        recon.update_costs(&penalties, &all_edges).unwrap();

        let matches1 = recon.solve();
        assert!(matches1.len() == 2 || matches1.len() == 3);

        let new_edges = vec![(0, 2, 1.0), (0, 3, 10.0), (1, 2, 10.0), (1, 3, 1.0)];
        recon.update_costs(&penalties, &new_edges).unwrap();
        let matches2 = recon.solve();
        println!("matches2: {:?}", matches2);
        assert_eq!(matches2.len(), 2);

        let mut match_map = std::collections::HashMap::new();
        for m in matches2 {
            match_map.insert(m.source_idx, m.sink_idx);
        }
        assert_eq!(match_map.get(&0), Some(&2));
        assert_eq!(match_map.get(&1), Some(&3));
    }

    #[test]
    fn test_solve_sparse() {
        let supplies = vec![100.0, 50.0, -100.0, -50.0];
        let penalties = vec![10000.0; 4];

        let mut recon = SparseReconciler::new(supplies);
        let allowed_edges = vec![(0, 2, 10.0), (1, 3, 10.0)];
        recon.update_costs(&penalties, &allowed_edges).unwrap();

        let matches = recon.solve();

        assert_eq!(matches.len(), 2);

        let mut match_map = std::collections::HashMap::new();
        for m in matches {
            match_map.insert(m.source_idx, m.sink_idx);
        }
        assert_eq!(match_map.get(&0), Some(&2));
        assert_eq!(match_map.get(&1), Some(&3));
    }

    #[test]
    fn test_warm_start_removed_edge() {
        let supplies = vec![100.0, -100.0];
        let penalties = vec![1000.0, 1000.0];

        let mut recon = SparseReconciler::new(supplies);
        // First run with edge (0, 1, 1.0)
        recon.update_costs(&penalties, &[(0, 1, 1.0)]).unwrap();
        let matches = recon.solve();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].source_idx, 0);
        assert_eq!(matches[0].sink_idx, 1);
        assert!((matches[0].flow - 100.0).abs() < 1e-6);

        // Second run: remove edge (0, 1) entirely.
        // It should be pivoted out since unmatched penalties are 1000.0 + 1000.0 = 2000.0,
        // and unmatched is cheaper than the Big-M penalty (2000.0 + 1000.0 = 3000.0).
        recon.update_costs(&penalties, &[]).unwrap();
        let matches2 = recon.solve();
        assert_eq!(matches2.len(), 0);
    }
}
