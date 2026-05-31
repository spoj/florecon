use log::{debug, warn};

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
    basis_edges: Vec<BasicEdge>,
    potentials: Vec<f64>,
    parent: Vec<usize>,
    parent_edge_idx: Vec<usize>,
    parent_direction_forward: Vec<bool>,
    depth: Vec<usize>,
    children: Vec<Vec<usize>>,
    
    // Pricing state
    has_run: bool,

    // Reusable allocation-free buffers
    adj: Vec<Vec<AdjEntry>>,
    visited: Vec<bool>,
    queue: std::collections::VecDeque<usize>,
    basic_sinks: Vec<Vec<usize>>,
    source_to_dummy_sink_basic: Vec<bool>,
    dummy_source_to_sink_basic: Vec<bool>,
    dummy_source_to_dummy_sink_basic: bool,
    path_u: Vec<usize>,
    path_v: Vec<usize>,

    // Sparse custom-edge pricing state storing (u_internal, v_internal, cost)
    candidate_edges: Vec<(usize, usize, f64)>,
    next_edge_to_scan: usize,
}

impl Default for SparseReconciler {
    fn default() -> Self {
        Self::new()
    }
}

impl SparseReconciler {
    /// Create a new empty stateful solver instance.
    pub fn new() -> Self {
        Self {
            supplies: Vec::new(),
            unmatched_penalties: Vec::new(),
            source_map: Vec::new(),
            sink_map: Vec::new(),
            basis_edges: Vec::new(),
            potentials: Vec::new(),
            parent: Vec::new(),
            parent_edge_idx: Vec::new(),
            parent_direction_forward: Vec::new(),
            depth: Vec::new(),
            children: Vec::new(),
            has_run: false,
            adj: Vec::new(),
            visited: Vec::new(),
            queue: std::collections::VecDeque::new(),
            basic_sinks: Vec::new(),
            source_to_dummy_sink_basic: Vec::new(),
            dummy_source_to_sink_basic: Vec::new(),
            dummy_source_to_dummy_sink_basic: false,
            path_u: Vec::new(),
            path_v: Vec::new(),
            candidate_edges: Vec::new(),
            next_edge_to_scan: 0,
        }
    }

    /// Safely updates the entire solver input dataset in one atomic, validated operation.
    /// Returns Some(()) if valid, or None if any input invariant is violated.
    pub fn update(
        &mut self,
        supplies: &[f64],
        penalties: &[f64],
        edges: &[(usize, usize, f64)],
    ) -> Option<()> {
        // 1. INVARIANT CHECK: Supplies and penalties must be equal in length
        if supplies.len() != penalties.len() {
            return None;
        }

        // 2. INVARIANT CHECK: Validate all user-facing edge indices and signs
        let n_nodes = supplies.len();
        for &(u_user, v_user, _cost) in edges {
            if u_user >= n_nodes || v_user >= n_nodes {
                return None; // Out of bounds
            }
            if supplies[u_user] <= 0.0 || supplies[v_user] >= 0.0 {
                return None; // u_user must be a source, v_user must be a sink
            }
        }

        // 3. Build new node mappings based on the new supplies
        let mut new_source_map = Vec::new();
        let mut new_sink_map = Vec::new();
        for (idx, &val) in supplies.iter().enumerate() {
            if val > 0.0 {
                new_source_map.push(idx);
            } else if val < 0.0 {
                new_sink_map.push(idx);
            }
        }

        // 4. Check if the active node structure has changed
        let node_mapping_identical = (new_source_map == self.source_map) && (new_sink_map == self.sink_map);
        
        // Check if supply values actually changed
        let supplies_changed = self.supplies != supplies;

        if !node_mapping_identical || supplies_changed {
            // A change in node structure or supplies requires rebuilding the initial basis tree (cold start)
            self.has_run = false;
        }

        if !node_mapping_identical {
            self.source_map = new_source_map;
            self.sink_map = new_sink_map;

            // Re-allocate / resize internal buffers to match the new dimensions
            let m = self.source_map.len();
            let n = self.sink_map.len();
            let num_nodes = m + n + 2;

            self.potentials.resize(num_nodes, 0.0);
            self.parent.resize(num_nodes, 0);
            self.parent_edge_idx.resize(num_nodes, 0);
            self.parent_direction_forward.resize(num_nodes, true);
            self.depth.resize(num_nodes, 0);
            self.children.resize(num_nodes, Vec::new());

            self.adj.resize(num_nodes, Vec::new());
            self.visited.resize(num_nodes, false);
            self.queue = std::collections::VecDeque::with_capacity(num_nodes);
            self.basic_sinks.resize(m, Vec::new());
            self.source_to_dummy_sink_basic.resize(m, false);
            self.dummy_source_to_sink_basic.resize(n, false);
            self.path_u = Vec::with_capacity(num_nodes);
            self.path_v = Vec::with_capacity(num_nodes);
        }

        // 5. Update local buffers (zero-allocation copy)
        self.supplies.clear();
        self.supplies.extend_from_slice(supplies);

        self.unmatched_penalties.clear();
        let sanitized_penalties: Vec<f64> = penalties
            .iter()
            .map(|&p| p.max(0.0))
            .collect();
        self.unmatched_penalties.extend_from_slice(&sanitized_penalties);

        // 6. Map and cache new candidate edges
        self.candidate_edges.clear();
        for &(u_user, v_user, cost) in edges {
            let u = self.source_map.binary_search(&u_user).unwrap();
            let v = self.sink_map.binary_search(&v_user).unwrap();
            self.candidate_edges.push((u, v, cost));
        }
        self.next_edge_to_scan = 0;

        // 7. WARM-START REPAIR (If node mapping did not change and we are reusing basis)
        if node_mapping_identical && self.has_run {
            let m = self.source_map.len();
            
            // Re-map basis edge costs using the new static candidate costs
            for edge in &mut self.basis_edges {
                match edge.id {
                    EdgeId::Real { source, sink } => {
                        if let Some(cand_idx) = self.candidate_edges.iter().position(|&(u, v, _)| u == source && v == sink - m) {
                            edge.cost = self.candidate_edges[cand_idx].2;
                        }
                    }
                    EdgeId::DummySourceToSink { sink } => {
                        let v_user = self.sink_map[sink - m];
                        edge.cost = self.unmatched_penalties[v_user];
                    }
                    EdgeId::SourceToDummySink { source } => {
                        let u_user = self.source_map[source];
                        edge.cost = self.unmatched_penalties[u_user];
                    }
                    EdgeId::DummySourceToDummySink => {
                        edge.cost = 0.0;
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

    #[inline]
    fn edge_cost(&self, id: EdgeId) -> f64 {
        let m = self.source_map.len();
        match id {
            EdgeId::Real { source, sink } => {
                if let Some(pos) = self.candidate_edges.iter().position(|&(u, v, _)| u == source && v == sink) {
                    self.candidate_edges[pos].2
                } else {
                    0.0
                }
            }
            EdgeId::DummySourceToSink { sink } => {
                let v_user = self.sink_map[sink - m];
                self.unmatched_penalties[v_user]
            }
            EdgeId::SourceToDummySink { source } => {
                let u_user = self.source_map[source];
                self.unmatched_penalties[u_user]
            }
            EdgeId::DummySourceToDummySink => 0.0,
        }
    }

    /// Rebuilds parents, depths, potentials, and child lists from `basis_edges` using BFS.
    ///
    /// This runs completely allocation-free and uses static stored costs inside `basis_edges`.
    fn rebuild_tree(&mut self) {
        let root = self.dummy_source();

        for child_list in &mut self.children {
            child_list.clear();
        }

        for a in &mut self.adj {
            a.clear();
        }

        for (idx, edge) in self.basis_edges.iter().enumerate() {
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
                    self.children[u].push(v);

                    let cost = self.basis_edges[entry.edge_idx].cost;
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
    fn find_entering_arc(&mut self) -> Option<EdgeId> {
        let m = self.source_map.len();
        let n = self.sink_map.len();
        let dummy_src = self.dummy_source();
        let dummy_snk = self.dummy_sink();

        for bs in &mut self.basic_sinks {
            bs.clear();
        }
        self.source_to_dummy_sink_basic.fill(false);
        self.dummy_source_to_sink_basic.fill(false);
        self.dummy_source_to_dummy_sink_basic = false;

        for edge in &self.basis_edges {
            match edge.id {
                EdgeId::Real { source, sink } => {
                    self.basic_sinks[source].push(sink);
                }
                EdgeId::DummySourceToSink { sink } => {
                    self.dummy_source_to_sink_basic[sink - m] = true;
                }
                EdgeId::SourceToDummySink { source } => {
                    self.source_to_dummy_sink_basic[source] = true;
                }
                EdgeId::DummySourceToDummySink => {
                    self.dummy_source_to_dummy_sink_basic = true;
                }
            }
        }

        let num_candidates = self.candidate_edges.len();
        let mut best_edge = None;
        let mut best_rc = -1e-12;

        if num_candidates > 0 {
            let block_size = 256.min(num_candidates);
            let mut edges_scanned = 0;
            while edges_scanned < num_candidates {
                let start = self.next_edge_to_scan;
                let end = (start + block_size).min(num_candidates);
                let chunk_len = end - start;

                for k in start..end {
                    let (u, v, cost) = self.candidate_edges[k];
                    let sink_node = m + v;
                    if self.basic_sinks[u].contains(&sink_node) {
                        continue;
                    }
                    let rc = cost - self.potentials[sink_node] + self.potentials[u];
                    if rc < best_rc {
                        best_rc = rc;
                        best_edge = Some(EdgeId::Real { source: u, sink: sink_node });
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
                    best_edge = Some(EdgeId::DummySourceToSink { sink: sink_node });
                }
            }
        }

        for u in 0..m {
            if !self.source_to_dummy_sink_basic[u] {
                let penalty = self.unmatched_penalties[self.source_map[u]];
                let rc = penalty - self.potentials[dummy_snk] + self.potentials[u];
                if rc < best_rc {
                    best_rc = rc;
                    best_edge = Some(EdgeId::SourceToDummySink { source: u });
                }
            }
        }

        if !self.dummy_source_to_dummy_sink_basic {
            let rc = 0.0 - self.potentials[dummy_snk] + self.potentials[dummy_src];
            if rc < best_rc {
                best_edge = Some(EdgeId::DummySourceToDummySink);
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

        // Safety check: If solver has never run, or node mapping changed, we must build the initial basis tree
        if !self.has_run {
            let mut basis_edges = Vec::with_capacity(m + n + 1);
            
            for u in 0..m {
                let user_idx = self.source_map[u];
                let val = self.supplies[user_idx];
                let penalty = self.unmatched_penalties[user_idx];
                basis_edges.push(BasicEdge {
                    id: EdgeId::SourceToDummySink { source: u },
                    flow: val,
                    cost: penalty,
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
                });
            }
            
            basis_edges.push(BasicEdge {
                id: EdgeId::DummySourceToDummySink,
                flow: 0.0,
                cost: 0.0,
            });
            
            self.basis_edges = basis_edges;
            self.has_run = true;
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

            let entering = self.find_entering_arc();
            if entering.is_none() {
                debug!("Optimal after {} iterations (incremental)", iterations);
                break;
            }
            let entering = entering.unwrap();

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

            for i in 0..self.path_v.len() {
                let w = self.path_v[i];
                let idx = self.parent_edge_idx[w];
                if self.parent_direction_forward[w] {
                    let flow = self.basis_edges[idx].flow;
                    if flow < min_theta {
                        min_theta = flow;
                        leaving_edge_basis_idx = Some(idx);
                    }
                }
            }

            for i in 0..self.path_u.len() {
                let w = self.path_u[i];
                let idx = self.parent_edge_idx[w];
                if !self.parent_direction_forward[w] {
                    let flow = self.basis_edges[idx].flow;
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
                println!("Iter {}: Entering={:?}, theta={}", iterations, entering, theta);
            }

            for i in 0..self.path_v.len() {
                let w = self.path_v[i];
                let idx = self.parent_edge_idx[w];
                if self.parent_direction_forward[w] {
                    self.basis_edges[idx].flow -= theta;
                } else {
                    self.basis_edges[idx].flow += theta;
                }
            }

            for i in 0..self.path_u.len() {
                let w = self.path_u[i];
                let idx = self.parent_edge_idx[w];
                if self.parent_direction_forward[w] {
                    self.basis_edges[idx].flow += theta;
                } else {
                    self.basis_edges[idx].flow -= theta;
                }
            }

            let entering_cost = self.edge_cost(entering);
            self.basis_edges[lei] = BasicEdge {
                id: entering,
                flow: theta,
                cost: entering_cost,
            };
        }

        let mut matches = Vec::new();
        for edge in &self.basis_edges {
            if edge.flow > 1e-9
                && let EdgeId::Real { source, sink } = edge.id {
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

        let mut recon = SparseReconciler::new();
        recon.update(&supplies, &penalties, &[(0, 1, 1.0)]).unwrap();
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

        let mut recon = SparseReconciler::new();
        recon.update(&supplies, &penalties, &[(0, 1, 1.0)]).unwrap();
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

        let mut recon = SparseReconciler::new();
        let all_edges = vec![(0, 2, 10.0), (0, 3, 10.0), (1, 2, 10.0), (1, 3, 10.0)];
        recon.update(&supplies, &penalties, &all_edges).unwrap();
        
        let matches1 = recon.solve();
        assert!(matches1.len() == 2 || matches1.len() == 3);

        let new_edges = vec![(0, 2, 1.0), (0, 3, 10.0), (1, 2, 10.0), (1, 3, 1.0)];
        recon.update(&supplies, &penalties, &new_edges).unwrap();
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

        let mut recon = SparseReconciler::new();
        let allowed_edges = vec![(0, 2, 10.0), (1, 3, 10.0)];
        recon.update(&supplies, &penalties, &allowed_edges).unwrap();
        
        let matches = recon.solve();
        
        assert_eq!(matches.len(), 2);
        
        let mut match_map = std::collections::HashMap::new();
        for m in matches {
            match_map.insert(m.source_idx, m.sink_idx);
        }
        assert_eq!(match_map.get(&0), Some(&2));
        assert_eq!(match_map.get(&1), Some(&3));
    }
}
