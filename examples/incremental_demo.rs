//! Incremental (Warm-start) Solver Demo
//!
//! Demonstrates how to adjust cost pairs and quickly rerun the solver
//! without rebuilding the network simplex graph from scratch.

use florecon::SparseReconciler;
use std::time::Instant;

fn main() {
    println!("=== Incremental (Warm-start) Demo ===\n");

    // Nodes:
    //  0: Invoice A (+100.0) — Source
    //  1: Invoice B (+50.0)  — Source
    //  2: Receipt 1 (-100.0) — Sink
    //  3: Receipt 2 (-50.0)  — Sink
    let supplies = vec![100.0, 50.0, -100.0, -50.0];
    let penalties = vec![10000.0; 4];

    // Create the stateful solver instance
    println!("Initializing stateful solver...");
    let mut recon = SparseReconciler::new();

    // 1. Initial run: all real edges are expensive ($10.0 cost)
    let t1 = Instant::now();
    let initial_edges = vec![
        (0, 2, 10.0),
        (0, 3, 10.0),
        (1, 2, 10.0),
        (1, 3, 10.0),
    ];
    recon.update(&supplies, &penalties, &initial_edges).unwrap();
    let matches1 = recon.solve();
    let duration1 = t1.elapsed();

    println!("Run 1 completed in {:?}", duration1);
    println!("Matches found (Run 1):");
    for m in &matches1 {
        println!(
            "  Node {} matched Node {} | flow = {} units",
            m.source_idx, m.sink_idx, m.flow
        );
    }

    // 2. Adjust cost pairs and quickly rerun!
    // Let's make:
    // - (0, 2) perfect (cost 1.0)
    // - (1, 3) perfect (cost 1.0)
    println!("\nAdjusting cost pairs dynamically...");

    println!("Rerunning stateful solver incrementally (warm start)...");
    let t2 = Instant::now();
    let updated_edges = vec![
        (0, 2, 1.0),
        (0, 3, 10.0),
        (1, 2, 10.0),
        (1, 3, 1.0),
    ];
    recon.update(&supplies, &penalties, &updated_edges).unwrap();
    let matches2 = recon.solve();
    let duration2 = t2.elapsed();

    println!("Incremental Run 2 completed in {:?}", duration2);
    println!("Matches found (Run 2):");
    for m in &matches2 {
        println!(
            "  Node {} matched Node {} | flow = {} units",
            m.source_idx, m.sink_idx, m.flow
        );
    }
}
