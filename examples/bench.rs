//! Synthetic scale benchmark for the `net` engine.
//!
//! Builds a windowed transportation instance (each source connects to a band
//! of nearby sinks), then times a cold solve and several warm incremental
//! operations (supply edit, arc removal, single cost change).
//!
//! Run with: `cargo run --release --example bench -- [pairs] [degree]`
//! e.g. `cargo run --release --example bench -- 50000 100`  (~5M edges)
//!      `cargo run --release --example bench -- 50000 200`  (~10M edges)

use florecon::net::Network;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let pairs: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let degree: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(50);

    // Simple deterministic RNG.
    let mut seed: u64 = 0x1234_5678_9abc_def0;
    let mut rng = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    let t0 = Instant::now();
    let mut net = Network::new();
    let amount = 1_000i64;
    let penalty = 1e6;

    let sources: Vec<_> = (0..pairs).map(|_| net.add_node(amount, penalty)).collect();
    let sinks: Vec<_> = (0..pairs).map(|_| net.add_node(-amount, penalty)).collect();

    let mut arc_count = 0usize;
    let mut sample_arc = None;
    for (i, &s) in sources.iter().enumerate() {
        // Block-diagonal candidate structure: each source competes only within
        // its own block of `degree` sinks (like a time/counterparty window).
        // This yields a shallow forest of small components, as in real recon.
        let block = (i / degree) * degree;
        for d in 0..degree {
            let j = (block + d) % pairs;
            let cost = 1.0 + (rng() % 100) as f64;
            if let Some(a) = net.add_arc(s, sinks[j], cost) {
                arc_count += 1;
                if i == pairs / 2 && d == 0 {
                    sample_arc = Some(a);
                }
            }
        }
    }
    let build = t0.elapsed();
    println!(
        "built {pairs} sources + {pairs} sinks, {arc_count} arcs ({:.1}M) in {build:.2?}",
        arc_count as f64 / 1e6
    );

    // --- cold solve ---
    let t = Instant::now();
    let status = net.solve();
    let cold = t.elapsed();
    let matched: i64 = net.matches().map(|(_, _, f)| f).sum();
    println!("cold solve: {cold:.2?}  status={status:?}  matched_flow={matched}");

    // --- warm: single supply edit (dual repair) ---
    let t = Instant::now();
    net.set_supply(sources[pairs / 3], amount + 500);
    net.set_supply(sinks[pairs / 3], -(amount + 500));
    net.solve();
    let (d, p, st) = net.debug_counts();
    println!(
        "warm supply edit (x2) + solve: {:.2?}  (dual={d} primal={p} subtree_nodes={st})",
        t.elapsed()
    );

    // --- warm: remove a matched candidate arc (dual pivot-out) ---
    if let Some(a) = sample_arc {
        let t = Instant::now();
        net.remove_arc(a);
        net.solve();
        let (d, p, _) = net.debug_counts();
        println!("warm arc removal + solve: {:.2?}  (dual={d} primal={p})", t.elapsed());
    }

    // --- warm: single cost change (primal) ---
    let t = Instant::now();
    if let Some(&s) = sources.first() {
        // re-price one fresh arc
        if let Some(a) = net.add_arc(s, sinks[0], 0.5) {
            net.set_cost(a, 0.25);
        }
    }
    net.solve();
    println!("warm cost change + solve: {:.2?}", t.elapsed());
}
