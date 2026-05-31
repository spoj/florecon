//! Intercompany reconciliation example — LF Centennial (288) vs Mighty Hurricane (236)
//!
//! Sparse API version. This shows how a client uses the core `SparseReconciler`
//! by:
//! 1. Loading data from CSV.
//! 2. Building supplies and unmatched penalty arrays.
//! 3. Creating a **Reference and Proximity Blocker** to generate sparse candidate
//!    edges, avoiding the O(N^2) fully dense scaling wall.
//! 4. Running the solver in milliseconds.
//! 5. Mapping indices back to print a detailed business report.

use florecon::SparseReconciler;
use std::collections::{HashMap, HashSet};
use std::env;
use std::time::Instant;

// ---------------------------------------------------------------------------
// Client-side Data Model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct IntercoTx {
    row: usize,
    policy: Policy,
    co: String,
    objsub: String,
    icp: String,
    reference: String,
    description: String,
    business_unit: String,
    gl_date_days: i64,
    usd_amt: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Policy {
    AR,
    AP,
    GA,
}

impl IntercoTx {
    fn from_csv_record(row: usize, record: &csv::StringRecord) -> Option<Self> {
        let policy = match record.get(8)? {
            "AR" => Policy::AR,
            "AP" => Policy::AP,
            "GA" => Policy::GA,
            _ => return None,
        };

        let co = record.get(3)?.trim().to_string();
        let objsub = record.get(6)?.trim().to_string();
        let icp = record.get(13)?.trim().to_string();
        let reference = record.get(20)?.trim().to_string();
        let description = record.get(22)?.trim().to_string();
        let business_unit = record.get(37)?.trim().to_string();

        let fc_amt: f64 = record.get(27)?.parse().ok()?;
        let usd_amt: f64 = record.get(43)?.parse().ok()?;

        let signed_usd = match policy {
            Policy::AR => usd_amt.abs(),
            Policy::AP => -usd_amt.abs(),
            Policy::GA => {
                if fc_amt >= 0.0 {
                    usd_amt.abs()
                } else {
                    -usd_amt.abs()
                }
            }
        };

        let gl_date_str = record.get(28)?;
        let gl_date_days = parse_date(gl_date_str).unwrap_or(0);

        Some(IntercoTx {
            row,
            policy,
            co,
            objsub,
            icp,
            reference,
            description,
            business_unit,
            gl_date_days,
            usd_amt: signed_usd,
        })
    }
}

fn parse_date(s: &str) -> Option<i64> {
    if s.len() >= 9 && s.contains('-') && !s.contains(':') {
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() == 3 {
            let day: i64 = parts[0].parse().ok()?;
            let month = match parts[1].to_lowercase().as_str() {
                "jan" => 1,
                "feb" => 2,
                "mar" => 3,
                "apr" => 4,
                "may" => 5,
                "jun" => 6,
                "jul" => 7,
                "aug" => 8,
                "sep" => 9,
                "oct" => 10,
                "nov" => 11,
                "dec" => 12,
                _ => return None,
            };
            let year: i64 = parts[2].parse().ok()?;
            let year = if year < 50 { 2000 + year } else { 1900 + year };
            let days_since_2020 = (year - 2020) * 365 + (month - 1) * 30 + day;
            return Some(days_since_2020);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Client-side Cost Function
// ---------------------------------------------------------------------------

fn log_value_difference(a: f64, b: f64) -> f64 {
    (a.abs().ln_1p() - b.abs().ln_1p()).abs()
}

fn compute_cost(a: &IntercoTx, b: &IntercoTx) -> f64 {
    // Only connect opposite signs
    if a.usd_amt.signum() == b.usd_amt.signum() {
        return f64::MAX;
    }

    // Start with baseline metadata cost
    let mut metadata_cost = 100.0;

    // 1. if (co,icp) on one side = (icp,co) on the other side, high prior
    // Normalize by trimming and stripping leading zeroes to ensure robust match (e.g. "1" vs "001")
    let co_a = a.co.trim().trim_start_matches('0');
    let icp_a = a.icp.trim().trim_start_matches('0');
    let co_b = b.co.trim().trim_start_matches('0');
    let icp_b = b.icp.trim().trim_start_matches('0');
    if !co_a.is_empty() && !icp_a.is_empty() && co_a == icp_b && icp_a == co_b {
        metadata_cost *= 0.05; // 95% discount
    }

    // 2. if (objsub) = (objsub), high prior
    let objsub_a = a.objsub.trim();
    let objsub_b = b.objsub.trim();
    if !objsub_a.is_empty() && objsub_a == objsub_b {
        metadata_cost *= 0.1; // 90% discount
    }

    // 3. if reference equal, high prior (but less so than the above)
    let ref_a = a.reference.trim();
    let ref_b = b.reference.trim();
    let is_meaningful = |r: &str| !r.is_empty() && r != "nan" && r != "AGGREGATED OPENING BALANCE";
    if is_meaningful(ref_a) && is_meaningful(ref_b) && ref_a == ref_b {
        metadata_cost *= 0.2; // 80% discount
    }

    // 4. if (reference) = (description), high prior
    let desc_a = a.description.trim();
    let desc_b = b.description.trim();
    if (is_meaningful(ref_a) && !desc_b.is_empty() && ref_a == desc_b) ||
       (is_meaningful(ref_b) && !desc_a.is_empty() && ref_b == desc_a) {
        metadata_cost *= 0.2; // 80% discount
    }

    // Value based costs: simple log based
    let value_cost = log_value_difference(a.usd_amt.abs(), b.usd_amt.abs()) * 10.0;

    // Date proximity
    let date_diff = (a.gl_date_days - b.gl_date_days).abs() as f64;
    let date_cost = date_diff.min(365.0);

    // Final cost = metadata_cost (multiplicative) + value_cost + date_cost
    (metadata_cost + value_cost + date_cost).max(0.0)
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

fn print_report(matches: &[florecon::SparseMatch], txs: &[IntercoTx], unmatched_indices: &[usize]) {
    println!("\n{}", "=".repeat(90));
    println!("  SPARSE RECONCILIATION REPORT");
    println!("{}", "=".repeat(90));

    let total_matched_abs: f64 = matches.iter().map(|m| m.flow).sum::<f64>() * 2.0; // flow counts on both sides
    let total_unmatched_abs: f64 = unmatched_indices
        .iter()
        .map(|&i| txs[i].usd_amt.abs())
        .sum();
    let total_input_abs: f64 = txs.iter().map(|t| t.usd_amt.abs()).sum();

    println!("\n  📊 SUMMARY");
    println!("  ──────────────────────────────────────────────────────────────────────────");
    println!("  Total input rows:        {:>5}", txs.len());
    println!(
        "  Matched rows:            {:>5}  ({:.1}%)",
        matches.len() * 2,
        100.0 * (matches.len() * 2) as f64 / txs.len() as f64
    );
    println!(
        "  Unmatched rows:          {:>5}  ({:.1}%)",
        unmatched_indices.len(),
        100.0 * unmatched_indices.len() as f64 / txs.len() as f64
    );
    println!("  Total matches found:     {:>5}", matches.len());
    println!(
        "  Total value matched:     {:>15.2}  ({:.1}% of {:>15.2})",
        total_matched_abs / 2.0,
        100.0 * (total_matched_abs / 2.0) / total_input_abs,
        total_input_abs
    );
    println!("  Total value unmatched:   {:>15.2}", total_unmatched_abs);

    // Grouping matches into components for display
    let mut adj = vec![Vec::new(); txs.len()];
    for m in matches {
        adj[m.source_idx].push(m.sink_idx);
        adj[m.sink_idx].push(m.source_idx);
    }

    let mut visited = vec![false; txs.len()];
    let mut groups = Vec::new();

    for start in 0..txs.len() {
        if visited[start] || adj[start].is_empty() {
            continue;
        }
        let mut group = Vec::new();
        let mut stack = vec![start];
        visited[start] = true;
        while let Some(node) = stack.pop() {
            group.push(node);
            for &nb in &adj[node] {
                if !visited[nb] {
                    visited[nb] = true;
                    stack.push(nb);
                }
            }
        }
        groups.push(group);
    }

    // Group size distribution
    let mut size_counts = HashMap::new();
    for g in &groups {
        *size_counts.entry(g.len()).or_insert(0) += 1;
    }
    let mut sizes: Vec<_> = size_counts.iter().collect();
    sizes.sort_by_key(|(k, _)| *k);

    println!("\n  📐 GROUP SIZE DISTRIBUTION");
    println!("  ──────────────────────────────────────────────────────────────────────────");
    for (size, count) in sizes {
        println!(
            "    Size {:>2}: {:>4} groups  {}",
            size,
            count,
            "█".repeat((*count).min(50))
        );
    }

    // Clean matches
    println!("\n  🟢 CLEAN MATCHES (net ≈ 0, same reference) — first 10");
    println!("  ──────────────────────────────────────────────────────────────────────────");
    let clean_groups: Vec<_> = groups
        .iter()
        .filter(|g| {
            let net: f64 = g.iter().map(|&idx| txs[idx].usd_amt).sum();
            net.abs() < 1.0
        })
        .filter(|g| {
            let refs: Vec<&str> = g
                .iter()
                .map(|&idx| txs[idx].reference.as_str())
                .filter(|r| !r.is_empty())
                .collect();
            refs.len() >= 2 && refs.iter().all(|r| *r == refs[0])
        })
        .collect();

    for (i, g) in clean_groups.iter().take(10).enumerate() {
        let ar: f64 = g
            .iter()
            .filter(|&&idx| txs[idx].policy == Policy::AR)
            .map(|&idx| txs[idx].usd_amt)
            .sum();
        let ap: f64 = g
            .iter()
            .filter(|&&idx| txs[idx].policy == Policy::AP)
            .map(|&idx| txs[idx].usd_amt)
            .sum();
        let ga: f64 = g
            .iter()
            .filter(|&&idx| txs[idx].policy == Policy::GA)
            .map(|&idx| txs[idx].usd_amt)
            .sum();
        let ref_name = txs[g[0]].reference.clone();
        let net: f64 = g.iter().map(|&idx| txs[idx].usd_amt).sum();
        println!("\n    Group {}: {} txns | ref={}", i + 1, g.len(), ref_name);
        println!(
            "      AR={:>12.2}  AP={:>12.2}  GA={:>12.2}  NET={:>10.2}",
            ar, ap, ga, net
        );

        // Show link costs
        for ii in 0..g.len() {
            for jj in ii + 1..g.len() {
                let a = &txs[g[ii]];
                let b = &txs[g[jj]];
                if a.usd_amt * b.usd_amt < 0.0 {
                    println!(
                        "        link cost [{:?}↔{:?}]: {:>8.2}  ({:>10.2} vs {:>10.2})",
                        a.policy,
                        b.policy,
                        compute_cost(a, b),
                        a.usd_amt,
                        b.usd_amt
                    );
                }
            }
        }
        for &idx in *g {
            let t = &txs[idx];
            println!(
                "        [{:>3}] {:?}  {:>12.2}  bu={}",
                t.row, t.policy, t.usd_amt, t.business_unit
            );
        }
    }

    // Residual groups
    let residual_groups: Vec<_> = groups
        .iter()
        .filter(|g| {
            let net: f64 = g.iter().map(|&idx| txs[idx].usd_amt).sum();
            net.abs() >= 1.0
        })
        .collect();

    if !residual_groups.is_empty() {
        println!(
            "\n  🟡 RESIDUAL GROUPS (net ≠ 0) — all {} shown",
            residual_groups.len()
        );
        println!("  ──────────────────────────────────────────────────────────────────────────");
        for (i, g) in residual_groups.iter().enumerate() {
            let ar: f64 = g
                .iter()
                .filter(|&&idx| txs[idx].policy == Policy::AR)
                .map(|&idx| txs[idx].usd_amt)
                .sum();
            let ap: f64 = g
                .iter()
                .filter(|&&idx| txs[idx].policy == Policy::AP)
                .map(|&idx| txs[idx].usd_amt)
                .sum();
            let ga: f64 = g
                .iter()
                .filter(|&&idx| txs[idx].policy == Policy::GA)
                .map(|&idx| txs[idx].usd_amt)
                .sum();
            let net: f64 = g.iter().map(|&idx| txs[idx].usd_amt).sum();

            let refs: HashSet<&str> = g
                .iter()
                .map(|&idx| txs[idx].reference.as_str())
                .filter(|r| !r.is_empty() && *r != "nan")
                .collect();

            println!(
                "\n    Group {}: {} txns | refs={:?} | NET={:>12.2}",
                i + 1,
                g.len(),
                refs,
                net
            );
            println!("      AR={:>12.2}  AP={:>12.2}  GA={:>12.2}", ar, ap, ga);
            for &idx in *g {
                let t = &txs[idx];
                println!(
                    "        [{:>3}] {:?}  {:>12.2}  ref={:<20}  bu={}",
                    t.row, t.policy, t.usd_amt, t.reference, t.business_unit
                );
            }
        }
    }

    // Unmatched
    if !unmatched_indices.is_empty() {
        println!("\n  🔴 UNMATCHED TRANSACTIONS — first 10");
        println!("  ──────────────────────────────────────────────────────────────────────────");
        for &idx in unmatched_indices.iter().take(10) {
            let t = &txs[idx];
            println!(
                "    [{:>3}] {:?}  {:>12.2}  ref={:<20}  bu={}",
                t.row, t.policy, t.usd_amt, t.reference, t.business_unit
            );
        }
        if unmatched_indices.len() > 10 {
            println!(
                "    ... and {} more unmatched",
                unmatched_indices.len() - 10
            );
        }
    }

    println!("\n{}", "=".repeat(90));
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = env::args().collect();
    let path = args
        .get(1)
        .map(|s| s.as_str())
        .unwrap_or("data/ledger.csv");

    println!("Loading intercompany data from: {}", path);

    let mut rdr = csv::Reader::from_path(path).expect("Failed to open CSV");
    let mut transactions: Vec<IntercoTx> = Vec::new();

    for (row_idx, result) in rdr.records().enumerate() {
        let record = result.expect("Failed to read CSV record");
        if let Some(tx) = IntercoTx::from_csv_record(row_idx + 2, &record)
            && tx.usd_amt.abs() > 0.01
        {
            transactions.push(tx);
        }
    }

    let n = transactions.len();
    println!("Loaded {} transactions", n);

    let ar_count = transactions
        .iter()
        .filter(|t| t.policy == Policy::AR)
        .count();
    let ap_count = transactions
        .iter()
        .filter(|t| t.policy == Policy::AP)
        .count();
    let ga_count = transactions
        .iter()
        .filter(|t| t.policy == Policy::GA)
        .count();
    let ar_sum: f64 = transactions
        .iter()
        .filter(|t| t.policy == Policy::AR)
        .map(|t| t.usd_amt)
        .sum();
    let ap_sum: f64 = transactions
        .iter()
        .filter(|t| t.policy == Policy::AP)
        .map(|t| t.usd_amt)
        .sum();
    let ga_sum: f64 = transactions
        .iter()
        .filter(|t| t.policy == Policy::GA)
        .map(|t| t.usd_amt)
        .sum();

    println!("  AR: {:>3} rows, total = {:>15.2}", ar_count, ar_sum);
    println!("  AP: {:>3} rows, total = {:>15.2}", ap_count, ap_sum);
    println!("  GA: {:>3} rows, total = {:>15.2}", ga_count, ga_sum);
    println!("  Net imbalance: {:>15.2}", ar_sum + ap_sum + ga_sum);

    // --- SOTA Sparse Blocker (Candidate Generation) ---
    // Only generate edges if:
    // 1. They are opposite signs.
    // 2. Either they share a reference prefix, OR they are within 90 days
    //    and have a similar amount (max value ratio <= 10.0).
    println!("\n  Building sparse match candidates...");
    let t_start = Instant::now();

    let mut edges = Vec::new();
    let mut sources = Vec::new();
    let mut sinks = Vec::new();

    for (idx, tx) in transactions.iter().enumerate() {
        if tx.usd_amt > 0.0 {
            sources.push(idx);
        } else {
            sinks.push(idx);
        }
    }

    let max_date_diff = 90;
    let max_value_ratio = 10.0;
    let mut skipped = 0;

    for &si in &sources {
        let s = &transactions[si];
        for &ti in &sinks {
            let t = &transactions[ti];

            // Prune by date
            if (s.gl_date_days - t.gl_date_days).abs() > max_date_diff {
                skipped += 1;
                continue;
            }

            // Prune by value ratio
            let ratio = s.usd_amt.abs().max(t.usd_amt.abs())
                / s.usd_amt.abs().min(t.usd_amt.abs()).max(0.01);
            if ratio > max_value_ratio {
                skipped += 1;
                continue;
            }

            // Plausible candidate!
            let cost = compute_cost(s, t);
            edges.push((si, ti, cost));
        }
    }

    let t_block = t_start.elapsed();
    println!(
        "  Pruned candidate edges: {} ({}% skipped)",
        edges.len(),
        100 * skipped / (sources.len() * sinks.len())
    );
    println!("  Candidate generation took: {:?}", t_block);

    // Supplies and Unmatched Penalties
    let supplies: Vec<f64> = transactions.iter().map(|t| t.usd_amt).collect();
    let penalties = vec![1_000_000.0; n];

    // --- Solve ---
    println!("  Solving transportation sparse graph with Network Simplex...");
    let t_solve_start = Instant::now();
    let mut reconciler = SparseReconciler::new(supplies, edges, penalties);
    let matches = reconciler.solve();
    let t_solve = t_solve_start.elapsed();
    println!("  Solve complete! Took: {:?}", t_solve);

    // Identify unmatched indices
    let mut matched_indices = HashSet::new();
    for m in &matches {
        matched_indices.insert(m.source_idx);
        matched_indices.insert(m.sink_idx);
    }
    let unmatched_indices: Vec<usize> = (0..n)
        .filter(|idx| !matched_indices.contains(idx))
        .collect();

    print_report(&matches, &transactions, &unmatched_indices);
}
