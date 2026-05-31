use log::{debug, info, trace, warn};

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

/// A high-performance, single-pass solver for transportation and matching problems.
///
/// Decouples data types and business logic from the optimization engine. It
/// operates entirely on raw indices, supplies, and costs.
pub struct SparseReconciler {
    supplies: Vec<f64>,
    edges: Vec<(usize, usize, f64)>,
    unmatched_penalties: Vec<f64>,
}

impl SparseReconciler {
    /// Create a new solver instance.
    ///
    /// - `supplies`: Supply or demand values for each node.
    ///   - `> 0.0` represents a source (supply, e.g., Accounts Receivable invoice).
    ///   - `< 0.0` represents a sink (demand, e.g., Accounts Payable receipt).
    ///   - Node indices correspond to positions in this slice.
    /// - `edges`: Candidate matches: `(source_node_idx, sink_node_idx, cost)`.
    /// - `unmatched_penalties`: The penalty for leaving each node unmatched.
    ///   Must be the same length as `supplies`.
    pub fn new(
        supplies: Vec<f64>,
        edges: Vec<(usize, usize, f64)>,
        unmatched_penalties: Vec<f64>,
    ) -> Self {
        assert_eq!(
            supplies.len(),
            unmatched_penalties.len(),
            "supplies and unmatched_penalties must have the same length"
        );
        let sanitized_edges = edges
            .into_iter()
            .map(|(from, to, cost)| (from, to, cost.max(0.0)))
            .collect();
        let sanitized_penalties = unmatched_penalties
            .into_iter()
            .map(|p| p.max(0.0))
            .collect();
        Self {
            supplies,
            edges: sanitized_edges,
            unmatched_penalties: sanitized_penalties,
        }
    }

    /// Solves the transportation problem using the Network Simplex method.
    ///
    /// Returns a list of optimal matches with positive flows between real nodes.
    pub fn solve(&self) -> Vec<SparseMatch> {
        let n_items = self.supplies.len();

        // 1. Partition user nodes into sources and sinks
        let mut source_map = Vec::new(); // index in sources -> user_idx
        let mut sink_map = Vec::new(); // index in sinks -> user_idx
        let mut user_to_internal = vec![0usize; n_items];

        for (idx, &val) in self.supplies.iter().enumerate() {
            if val > 0.0 {
                let si = source_map.len();
                source_map.push(idx);
                user_to_internal[idx] = si;
            } else if val < 0.0 {
                let ti = sink_map.len();
                sink_map.push(idx);
                user_to_internal[idx] = ti; // temporary, mapped later
            }
        }

        let m = source_map.len();
        let n = sink_map.len();

        if m == 0 || n == 0 {
            info!("No matching possible: 0 sources or 0 sinks");
            return Vec::new();
        }

        // Finalize mapping for sinks (offset by m)
        for &idx in &sink_map {
            user_to_internal[idx] += m;
        }

        let dummy_source = m + n;
        let dummy_sink = m + n + 1;
        let num_nodes = m + n + 2;

        // Supplies and demands
        let total_source_value: f64 = source_map.iter().map(|&i| self.supplies[i]).sum();
        let total_sink_abs_value: f64 = sink_map.iter().map(|&i| self.supplies[i].abs()).sum();

        let mut internal_supplies = vec![0f64; num_nodes];
        for (si, &idx) in source_map.iter().enumerate() {
            internal_supplies[si] = self.supplies[idx];
        }
        for (ti, &idx) in sink_map.iter().enumerate() {
            internal_supplies[m + ti] = -self.supplies[idx].abs();
        }
        internal_supplies[dummy_source] = total_sink_abs_value;
        internal_supplies[dummy_sink] = -total_source_value;

        // --- Build edges ---
        // We pre-allocate space: user candidate edges + dummy edges (m + n + 1)
        let mut edges: Vec<Edge> = Vec::with_capacity((self.edges.len() + m + n + 1) * 2);

        let mut add_edge = |from: usize, to: usize, cost: f64| {
            edges.push(Edge {
                from,
                to,
                cost,
                flow: 0.0,
            });
            edges.push(Edge {
                from: to,
                to: from,
                cost: -cost,
                flow: 0.0,
            });
        };

        // User real edges (automatically oriented from source to sink)
        let mut ignored_edges = 0;
        for &(node_a, node_b, cost) in &self.edges {
            let val_a = self.supplies[node_a];
            let val_b = self.supplies[node_b];

            if val_a > 0.0 && val_b < 0.0 {
                let si = user_to_internal[node_a];
                let ti = user_to_internal[node_b];
                add_edge(si, ti, cost);
            } else if val_a < 0.0 && val_b > 0.0 {
                let si = user_to_internal[node_b];
                let ti = user_to_internal[node_a];
                add_edge(si, ti, cost);
            } else {
                ignored_edges += 1;
                trace!(
                    "Ignored candidate edge between same-sign nodes: {} ({:.2}) and {} ({:.2})",
                    node_a, val_a, node_b, val_b
                );
            }
        }

        if ignored_edges > 0 {
            debug!(
                "Ignored {} candidate edges that connected same-sign nodes (e.g. source to source)",
                ignored_edges
            );
        }

        // Dummy source → all real sinks (allows leaving sinks unmatched)
        for (ti, &user_snk) in sink_map.iter().enumerate() {
            let penalty = self.unmatched_penalties[user_snk];
            add_edge(dummy_source, m + ti, penalty);
        }

        // All real sources → dummy sink (allows leaving sources unmatched)
        for (si, &user_src) in source_map.iter().enumerate() {
            let penalty = self.unmatched_penalties[user_src];
            add_edge(si, dummy_sink, penalty);
        }

        // Dummy source → dummy sink edge (structural)
        add_edge(dummy_source, dummy_sink, 0.0);

        trace!(
            "Built balanced network: {} nodes, {} variables",
            num_nodes,
            edges.len() / 2
        );

        // Solve using the optimized Network Simplex
        let network = TransportationNetwork {
            num_nodes,
            num_sources: m,
            num_sinks: n,
            edges,
            dummy_source,
            dummy_sink,
            supplies: internal_supplies,
        };

        let flows = network.solve();

        // 3. Map flows back to original user indices
        let mut matches = Vec::new();
        for (u, v, flow) in flows {
            if u < m && v >= m && v < m + n {
                // Real edge flow — find the original edge, allowing any orientation
                let real_edge_idx = self.edges.iter().position(|&(node_a, node_b, _)| {
                    let ua = user_to_internal[node_a];
                    let ub = user_to_internal[node_b];
                    (ua == u && ub == v) || (ub == u && ua == v)
                });
                if let Some(edge_idx) = real_edge_idx {
                    let (node_a, node_b, _) = self.edges[edge_idx];
                    let (source, sink) = if self.supplies[node_a] > 0.0 {
                        (node_a, node_b)
                    } else {
                        (node_b, node_a)
                    };
                    matches.push(SparseMatch {
                        source_idx: source,
                        sink_idx: sink,
                        flow,
                    });
                }
            }
        }

        matches
    }
}

// ---------------------------------------------------------------------------
// Internal Solver Logic (Network Simplex)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Edge {
    from: usize,
    to: usize,
    cost: f64,
    flow: f64,
}

struct TransportationNetwork {
    num_nodes: usize,
    num_sources: usize,
    num_sinks: usize,
    edges: Vec<Edge>,
    dummy_source: usize,
    dummy_sink: usize,
    supplies: Vec<f64>,
}

impl TransportationNetwork {
    fn solve(self) -> Vec<(usize, usize, f64)> {
        let mut state = SolverState::new(self);

        if let Err(e) = state.initial_feasible_flow() {
            warn!("Initial flow failed: {}", e);
            return Vec::new();
        }

        state.build_initial_basis();

        let max_iterations = state.network.num_nodes * state.network.num_nodes * 2;
        let mut iterations = 0;

        loop {
            if iterations >= max_iterations {
                warn!("Max iterations reached");
                break;
            }
            iterations += 1;

            state.compute_potentials();

            let entering = state.find_entering_arc();
            if entering.is_none() {
                debug!("Optimal after {} iterations", iterations);
                break;
            }
            let entering = entering.unwrap();

            let leaving = state.find_leaving_arc(entering);
            if leaving.is_none() {
                state.add_to_basis(entering);
                continue;
            }
            let (leaving, theta) = leaving.unwrap();

            state.pivot(entering, leaving, theta);
        }

        info!("Converged after {} simplex iterations", iterations);

        state
            .network
            .edges
            .iter()
            .step_by(2)
            .filter(|e| e.flow > 1e-9)
            .map(|e| (e.from, e.to, e.flow))
            .collect()
    }
}

struct SolverState {
    network: TransportationNetwork,
    in_basis: Vec<bool>,
    potentials: Vec<f64>,
    parent: Vec<usize>,
    parent_edge: Vec<usize>,
    children: Vec<Vec<usize>>,
    depth: Vec<usize>,
}

impl SolverState {
    fn new(network: TransportationNetwork) -> Self {
        let n_edges_fwd = network.edges.len() / 2;
        Self {
            network,
            in_basis: vec![false; n_edges_fwd],
            potentials: Vec::new(),
            parent: Vec::new(),
            parent_edge: Vec::new(),
            children: Vec::new(),
            depth: Vec::new(),
        }
    }

    fn initial_feasible_flow(&mut self) -> Result<(), &'static str> {
        let m = self.network.num_sources;
        let n = self.network.num_sinks;
        let ds = self.network.dummy_source;
        let dt = self.network.dummy_sink;

        let mut rem = self.network.supplies.clone();

        // Greedy allocation on real edges first
        let mut real_edges: Vec<(usize, f64)> = Vec::new();
        for (ei, e) in self.network.edges.iter().step_by(2).enumerate() {
            if e.from < m && e.to >= m && e.to < m + n {
                real_edges.push((ei, e.cost));
            }
        }
        real_edges.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

        for &(ei, _) in &real_edges {
            let fwd = &self.network.edges[ei * 2];
            let u = fwd.from;
            let v = fwd.to;
            if rem[u] <= 1e-12 || rem[v] >= -1e-12 {
                continue;
            }
            let push = rem[u].min(-rem[v]);
            self.add_flow(ei, push);
            rem[u] -= push;
            rem[v] += push;
        }

        // Route remaining imbalances through dummy edges
        for si in 0..m {
            if rem[si] > 1e-12 {
                let ei = self.find_forward_edge(si, dt).ok_or("missing edge")?;
                let push = rem[si];
                self.add_flow(ei, push);
                rem[si] -= push;
                rem[dt] += push;
            }
        }
        for ti in 0..n {
            let node = m + ti;
            if rem[node] < -1e-12 {
                let ei = self.find_forward_edge(ds, node).ok_or("missing edge")?;
                let push = -rem[node];
                self.add_flow(ei, push);
                rem[ds] -= push;
                rem[node] += push;
            }
        }
        if rem[ds] > 1e-12 && rem[dt] < -1e-12 {
            let push = rem[ds].min(-rem[dt]);
            let ei = self.find_forward_edge(ds, dt).ok_or("missing ds->dt")?;
            self.add_flow(ei, push);
            rem[ds] -= push;
            rem[dt] += push;
        }

        for &r in &rem {
            if r.abs() > 1e-6 {
                return Err("Unbalanced network supply/demand");
            }
        }
        Ok(())
    }

    fn find_forward_edge(&self, from: usize, to: usize) -> Option<usize> {
        self.network
            .edges
            .iter()
            .step_by(2)
            .position(|e| e.from == from && e.to == to)
    }

    fn add_flow(&mut self, fwd_idx: usize, delta: f64) {
        let fwd = fwd_idx * 2;
        let rev = fwd + 1;
        self.network.edges[fwd].flow += delta;
        self.network.edges[rev].flow -= delta;
    }

    fn build_initial_basis(&mut self) {
        let num_nodes = self.network.num_nodes;
        let ds = self.network.dummy_source;
        let dt = self.network.dummy_sink;
        let target_basis_size = num_nodes - 1;

        for (ei, e) in self.network.edges.iter().step_by(2).enumerate() {
            if e.flow > 1e-12 {
                self.in_basis[ei] = true;
            }
        }

        let mut uf = UnionFind::new(num_nodes);
        let mut basis_edges = Vec::new();

        let ds_dt_ei = self.find_forward_edge(ds, dt).unwrap();
        if self.in_basis[ds_dt_ei] {
            uf.union(ds, dt);
            basis_edges.push(ds_dt_ei);
        }

        let mut candidates: Vec<usize> = (0..self.in_basis.len())
            .filter(|&ei| ei != ds_dt_ei && self.in_basis[ei])
            .collect();
        candidates.sort_by_key(|&ei| {
            let e = &self.network.edges[ei * 2];
            if e.from == ds || e.to == dt {
                1 // dummy
            } else {
                0 // real
            }
        });

        for ei in candidates {
            let e = &self.network.edges[ei * 2];
            if uf.find(e.from) != uf.find(e.to) {
                uf.union(e.from, e.to);
                basis_edges.push(ei);
            } else {
                self.in_basis[ei] = false;
            }
        }

        let mut extra: Vec<usize> = (0..self.in_basis.len())
            .filter(|&ei| !self.in_basis[ei])
            .collect();
        extra.sort_by_key(|&ei| {
            let e = &self.network.edges[ei * 2];
            if e.from == ds && e.to == dt {
                0
            } else if e.from == ds || e.to == dt {
                1
            } else {
                2
            }
        });

        for ei in extra {
            if basis_edges.len() >= target_basis_size {
                break;
            }
            let e = &self.network.edges[ei * 2];
            if uf.find(e.from) != uf.find(e.to) {
                uf.union(e.from, e.to);
                basis_edges.push(ei);
                self.in_basis[ei] = true;
            }
        }
    }

    /// SOTA BFS Potential Propagation — O(N) instead of O(N × E)
    fn compute_potentials(&mut self) {
        let n = self.network.num_nodes;
        self.potentials = vec![0.0; n];
        self.parent = vec![n; n];
        self.parent_edge = vec![n; n];
        self.children = vec![Vec::new(); n];
        self.depth = vec![0; n];

        let mut adj: Vec<Vec<(usize, usize, f64)>> = vec![Vec::new(); n];
        for (ei, e) in self.network.edges.iter().step_by(2).enumerate() {
            if self.in_basis[ei] {
                adj[e.from].push((e.to, ei, e.cost));
                adj[e.to].push((e.from, ei, -e.cost));
            }
        }

        let root = self.network.dummy_source;
        self.parent[root] = root;

        let mut queue = std::collections::VecDeque::new();
        queue.push_back(root);

        while let Some(u) = queue.pop_front() {
            for &(v, ei, cost_delta) in &adj[u] {
                if self.parent[v] == n {
                    self.parent[v] = u;
                    self.parent_edge[v] = ei;
                    self.children[u].push(v);
                    self.depth[v] = self.depth[u] + 1;
                    self.potentials[v] = self.potentials[u] + cost_delta;
                    queue.push_back(v);
                }
            }
        }
    }

    fn find_entering_arc(&self) -> Option<usize> {
        let mut best_ei = None;
        let mut best_rc = -1e-12;

        for (ei, e) in self.network.edges.iter().step_by(2).enumerate() {
            if self.in_basis[ei] {
                continue;
            }
            let rc = e.cost - self.potentials[e.from] + self.potentials[e.to];
            if rc < best_rc {
                best_rc = rc;
                best_ei = Some(ei);
            }
        }
        best_ei
    }

    fn find_leaving_arc(&self, entering: usize) -> Option<(usize, f64)> {
        let e = &self.network.edges[entering * 2];
        let u = e.from;
        let v = e.to;

        let path_u = self.path_to_root(u);
        let path_v = self.path_to_root(v);

        let mut pu = path_u.len();
        let mut pv = path_v.len();
        while pu > 0 && pv > 0 && path_u[pu - 1] == path_v[pv - 1] {
            pu -= 1;
            pv -= 1;
        }

        let mut min_theta = f64::MAX;
        let mut leaving_ei = None;

        for i in 0..pu {
            let node = path_u[i];
            let parent_n = path_u[i + 1];
            let ei = self.parent_edge[node];
            let edge = &self.network.edges[ei * 2];
            if (edge.from != node || edge.to != parent_n) && edge.flow < min_theta {
                min_theta = edge.flow;
                leaving_ei = Some(ei);
            }
        }

        for i in (1..=pv).rev() {
            let parent_n = path_v[i];
            let child = path_v[i - 1];
            let ei = self.parent_edge[child];
            let edge = &self.network.edges[ei * 2];
            if (edge.from != parent_n || edge.to != child) && edge.flow < min_theta {
                min_theta = edge.flow;
                leaving_ei = Some(ei);
            }
        }

        if min_theta < 1e-12 {
            return None;
        }
        leaving_ei.map(|lei| (lei, min_theta))
    }

    fn path_to_root(&self, mut node: usize) -> Vec<usize> {
        let mut path = vec![node];
        while node != self.network.dummy_source {
            node = self.parent[node];
            path.push(node);
        }
        path
    }

    fn pivot(&mut self, entering: usize, leaving: usize, theta: f64) {
        let e = &self.network.edges[entering * 2];
        let u = e.from;
        let v = e.to;

        self.add_flow(entering, theta);

        let path_u = self.path_to_root(u);
        let path_v = self.path_to_root(v);
        let mut pu = path_u.len();
        let mut pv = path_v.len();
        while pu > 0 && pv > 0 && path_u[pu - 1] == path_v[pv - 1] {
            pu -= 1;
            pv -= 1;
        }

        for i in 0..pu {
            let node = path_u[i];
            let parent_n = path_u[i + 1];
            let ei = self.parent_edge[node];
            let edge = &self.network.edges[ei * 2];
            if edge.from == node && edge.to == parent_n {
                self.add_flow(ei, theta);
            } else {
                self.add_flow(ei, -theta);
            }
        }

        for i in (1..=pv).rev() {
            let parent_n = path_v[i];
            let child = path_v[i - 1];
            let ei = self.parent_edge[child];
            let edge = &self.network.edges[ei * 2];
            if edge.from == parent_n && edge.to == child {
                self.add_flow(ei, theta);
            } else {
                self.add_flow(ei, -theta);
            }
        }

        self.in_basis[entering] = true;
        self.in_basis[leaving] = false;
    }

    fn add_to_basis(&mut self, entering: usize) {
        self.in_basis[entering] = true;
    }
}

// ---------------------------------------------------------------------------
// Union-Find (Disjoint Set Union)
// ---------------------------------------------------------------------------

struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            self.parent[x] = self.find(self.parent[x]);
        }
        self.parent[x]
    }

    fn union(&mut self, x: usize, y: usize) {
        let rx = self.find(x);
        let ry = self.find(y);
        if rx == ry {
            return;
        }
        if self.rank[rx] < self.rank[ry] {
            self.parent[rx] = ry;
        } else if self.rank[rx] > self.rank[ry] {
            self.parent[ry] = rx;
        } else {
            self.parent[ry] = rx;
            self.rank[rx] += 1;
        }
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
        // Pass the edge in reverse order: (sink, source, cost)
        // The library should automatically orient it.
        let edges = vec![(1, 0, 1.0)];
        let penalties = vec![1e6, 1e6];

        let recon = SparseReconciler::new(supplies, edges, penalties);
        let matches = recon.solve();

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].source_idx, 0);
        assert_eq!(matches[0].sink_idx, 1);
        assert!((matches[0].flow - 100.0).abs() < 1e-6);
    }

    #[test]
    fn test_unmatched() {
        // Source is 100, sink is 50. Flow should route 50, leaving remaining 50 unmatched
        let supplies = vec![100.0, -50.0];
        let edges = vec![(0, 1, 1.0)];
        let penalties = vec![1e6, 1e6];

        let recon = SparseReconciler::new(supplies, edges, penalties);
        let matches = recon.solve();

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].source_idx, 0);
        assert_eq!(matches[0].sink_idx, 1);
        assert!((matches[0].flow - 50.0).abs() < 1e-6);
    }

    #[test]
    fn test_negative_costs_clamped() {
        let supplies = vec![100.0, -100.0];
        let edges = vec![(0, 1, -5.0)]; // negative cost should be clamped to 0.0
        let penalties = vec![-10.0, -10.0]; // negative penalties should be clamped to 0.0

        let recon = SparseReconciler::new(supplies, edges, penalties);
        let matches = recon.solve();

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].source_idx, 0);
        assert_eq!(matches[0].sink_idx, 1);
        assert!((matches[0].flow - 100.0).abs() < 1e-6);
    }
}
