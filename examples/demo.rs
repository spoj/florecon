//! Simplified Sparse API Demo
//!
//! Demonstrates the raw high-performance sparse-graph solver interface.

use florecon::SparseReconciler;

fn main() {
    println!("=== Sparse API Demo ===");

    // Nodes:
    //  0: Invoice A (+100.0) — Source
    //  1: Invoice B (+50.0)  — Source
    //  2: Receipt 1 (-100.0) — Sink
    //  3: Receipt 2 (-50.0)  — Sink
    let supplies = vec![100.0, 50.0, -100.0, -50.0];

    // Global unmatched penalties for each node (e.g. $10,000)
    let penalties = vec![10000.0; 4];

    // Solve the transportation problem
    let mut reconciler = SparseReconciler::new(supplies, penalties);
    let matches = reconciler.solve(|i, j| {
        if i == 0 && j == 2 {
            1.0
        } else if i == 0 && j == 3 {
            10.0
        } else if i == 1 && j == 2 {
            10.0
        } else if i == 1 && j == 3 {
            1.0
        } else {
            1000.0
        }
    });

    println!("\nMatches found: {}", matches.len());
    for m in matches {
        println!(
            "  Node {} matched Node {} | flow = {} units",
            m.source_idx, m.sink_idx, m.flow
        );
    }
}
