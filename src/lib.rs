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

/// A stateful dense/all-to-all reconciler that expects a cost closure.
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
    next_source_to_scan: usize,
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
}

impl SparseReconciler {
    /// Create a new stateful solver instance.
    ///
    /// - `supplies`: Supply or demand values for each node.
    /// - `unmatched_penalties`: The penalty for leaving each node unmatched.
    pub fn new(
        supplies: Vec<f64>,
        unmatched_penalties: Vec<f64>,
    ) -> Self {
        assert_eq!(
            supplies.len(),
            unmatched_penalties.len(),
            "supplies and unmatched_penalties must have the same length"
        );
        let sanitized_penalties: Vec<f64> = unmatched_penalties
            .iter()
            .map(|&p| p.max(0.0))
            .collect();

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
        let children = vec![Vec::new(); num_nodes];

        let adj = vec![Vec::new(); num_nodes];
        let visited = vec![false; num_nodes];
        let queue = std::collections::VecDeque::with_capacity(num_nodes);
        let basic_sinks = vec![Vec::new(); m];
        let source_to_dummy_sink_basic = vec![false; m];
        let dummy_source_to_sink_basic = vec![false; n];
        let path_u = Vec::with_capacity(num_nodes);
        let path_v = Vec::with_capacity(num_nodes);

        Self {
            supplies,
            unmatched_penalties: sanitized_penalties,
            source_map,
            sink_map,
            basis_edges: Vec::new(),
            potentials,
            parent,
            parent_edge_idx,
            parent_direction_forward,
            depth,
            children,
            next_source_to_scan: 0,
            has_run: false,
            adj,
            visited,
            queue,
            basic_sinks,
            source_to_dummy_sink_basic,
            dummy_source_to_sink_basic,
            dummy_source_to_dummy_sink_basic: false,
            path_u,
            path_v,
        }
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
    fn edge_cost<F>(&self, id: EdgeId, cost_fn: &F) -> f64
    where
        F: Fn(usize, usize) -> f64,
    {
        let m = self.source_map.len();
        match id {
            EdgeId::Real { source, sink } => {
                let u_user = self.source_map[source];
                let v_user = self.sink_map[sink - m];
                cost_fn(u_user, v_user)
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

    /// Finds an entering arc using rolling block partial-pricing.
    fn find_entering_arc<F>(&mut self, cost_fn: &F) -> Option<EdgeId>
    where
        F: Fn(usize, usize) -> f64,
    {
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

        let block_size = (65_536 / n).max(1).min(16).min(m);
        let mut best_edge = None;
        let mut best_rc = -1e-12;

        let gcd = |mut x: usize, mut y: usize| {
            while y != 0 {
                let temp = y;
                y = x % y;
                x = temp;
            }
            x
        };

        let mut a_prime = 999983;
        while gcd(a_prime, m) != 1 {
            a_prime += 1;
        }

        let mut sources_scanned = 0;
        while sources_scanned < m {
            let start = self.next_source_to_scan;
            let end = (start + block_size).min(m);
            let chunk_len = end - start;

            for k in start..end {
                let u = (k * a_prime) % m;
                let u_user = self.source_map[u];
                for v in 0..n {
                    let sink_node = m + v;
                    if self.basic_sinks[u].contains(&sink_node) {
                        continue;
                    }
                    let pot_diff = self.potentials[sink_node] - self.potentials[u];
                    if pot_diff <= -best_rc {
                        continue;
                    }
                    let cost = cost_fn(u_user, self.sink_map[v]);
                    let rc = cost - pot_diff;
                    if rc < best_rc {
                        best_rc = rc;
                        best_edge = Some(EdgeId::Real { source: u, sink: sink_node });
                    }
                }
            }

            self.next_source_to_scan = if end == m { 0 } else { end };
            sources_scanned += chunk_len;

            if best_edge.is_some() {
                return best_edge;
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

    /// Solves or re-solves the transportation problem.
    ///
    /// Uses the warm start solution if it was already run.
    pub fn solve<F>(&mut self, cost_fn: F) -> Vec<SparseMatch>
    where
        F: Fn(usize, usize) -> f64,
    {
        let m = self.source_map.len();
        let n = self.sink_map.len();

        if m == 0 || n == 0 {
            return Vec::new();
        }

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

            let entering = self.find_entering_arc(&cost_fn);
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

            let entering_cost = self.edge_cost(entering, &cost_fn);
            self.basis_edges[lei] = BasicEdge {
                id: entering,
                flow: theta,
                cost: entering_cost,
            };
        }

        let mut matches = Vec::new();
        for edge in &self.basis_edges {
            if edge.flow > 1e-9 {
                if let EdgeId::Real { source, sink } = edge.id {
                    let source_idx = self.source_map[source];
                    let sink_idx = self.sink_map[sink - m];
                    matches.push(SparseMatch {
                        source_idx,
                        sink_idx,
                        flow: edge.flow,
                    });
                }
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

        let mut recon = SparseReconciler::new(supplies, penalties);
        let matches = recon.solve(|_i, _j| 1.0);

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].source_idx, 0);
        assert_eq!(matches[0].sink_idx, 1);
        assert!((matches[0].flow - 100.0).abs() < 1e-6);
    }

    #[test]
    fn test_unmatched() {
        let supplies = vec![100.0, -50.0];
        let penalties = vec![1e6, 1e6];

        let mut recon = SparseReconciler::new(supplies, penalties);
        let matches = recon.solve(|_i, _j| 1.0);

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].source_idx, 0);
        assert_eq!(matches[0].sink_idx, 1);
        assert!((matches[0].flow - 50.0).abs() < 1e-6);
    }

    #[test]
    fn test_stateful_reconciler_incremental() {
        let supplies = vec![100.0, 50.0, -100.0, -50.0];
        let penalties = vec![10000.0; 4];

        let mut recon = SparseReconciler::new(supplies, penalties);
        
        let matches1 = recon.solve(|_i, _j| 10.0);
        assert!(matches1.len() == 2 || matches1.len() == 3);

        let matches2 = recon.solve(|i, j| {
            if (i == 0 && j == 2) || (i == 1 && j == 3) {
                1.0
            } else {
                10.0
            }
        });
        assert_eq!(matches2.len(), 2);
        
        let mut match_map = std::collections::HashMap::new();
        for m in matches2 {
            match_map.insert(m.source_idx, m.sink_idx);
        }
        assert_eq!(match_map.get(&0), Some(&2));
        assert_eq!(match_map.get(&1), Some(&3));
    }
}
