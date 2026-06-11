# netsimplex

A small, domain-agnostic **min-cost transportation solver** — a bounded-variable
network simplex over a node/arc graph with per-node supplies and unmatched
penalties.

```rust
use netsimplex::Network;

let mut net = Network::new();
let a = net.add_node(10, 1000.0);   // supply +10, penalty for leaving it unmet
let b = net.add_node(-10, 1000.0);  // demand 10
net.add_arc(a, b, 1.0);             // unit cost to ship a -> b
net.solve();
for (from, to, flow) in net.matches() {
    println!("{from:?} -> {to:?}: {flow}");
}
```

It is the engine behind [`florecon`](https://github.com/spoj/florecon)'s `flow`
leaf, but carries no reconciliation concepts of its own. It also supports
incremental edits (set supply/cost/bounds, add/remove arcs) and warm-started
re-solves via snapshots — more than `florecon` currently uses, but useful on its
own.

Enable the `serde` feature to (de)serialize a solved network `Snapshot`.
