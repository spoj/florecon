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

#[allow(dead_code)]
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
    co_cleaned: String,
    icp_cleaned: String,
    objsub_cleaned: String,
    ref_cleaned: String,
    desc_cleaned: String,
    log_value: f64,
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

        let co_cleaned = co.trim().trim_start_matches('0').to_string();
        let icp_cleaned = icp.trim().trim_start_matches('0').to_string();
        let objsub_cleaned = objsub.trim().to_string();
        let ref_cleaned = reference.trim().to_string();
        let desc_cleaned = description.trim().to_string();
        let log_value = signed_usd.abs().ln_1p();

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
            co_cleaned,
            icp_cleaned,
            objsub_cleaned,
            ref_cleaned,
            desc_cleaned,
            log_value,
        })
    }

    fn from_parquet_row(row_idx: usize, row: &parquet::record::Row) -> Option<Self> {
        let mut policy = None;
        let mut co = String::new();
        let mut objsub = String::new();
        let mut icp = String::new();
        let mut reference = String::new();
        let mut description = String::new();
        let mut business_unit = String::new();
        let mut fc_amt = 0.0;
        let mut usd_amt = 0.0;
        let mut gl_date_days = 0;

        for (name, field) in row.get_column_iter() {
            match name.as_str() {
                "source_policy" => {
                    if let parquet::record::Field::Str(s) = field {
                        policy = match s.as_str() {
                            "AR" => Some(Policy::AR),
                            "AP" => Some(Policy::AP),
                            "GA" => Some(Policy::GA),
                            _ => None,
                        };
                    }
                }
                "company" => {
                    if let parquet::record::Field::Str(s) = field {
                        co = s.trim().to_string();
                    }
                }
                "objsub" => {
                    if let parquet::record::Field::Str(s) = field {
                        objsub = s.trim().to_string();
                    }
                }
                "icp" => {
                    if let parquet::record::Field::Str(s) = field {
                        icp = s.trim().to_string();
                    }
                }
                "reference" => {
                    if let parquet::record::Field::Str(s) = field {
                        reference = s.trim().to_string();
                    }
                }
                "description" => {
                    if let parquet::record::Field::Str(s) = field {
                        description = s.trim().to_string();
                    }
                }
                "business_unit" => {
                    if let parquet::record::Field::Str(s) = field {
                        business_unit = s.trim().to_string();
                    }
                }
                "fc_amt" => {
                    fc_amt = match field {
                        parquet::record::Field::Double(d) => *d,
                        parquet::record::Field::Float(f) => *f as f64,
                        parquet::record::Field::Int(i) => *i as f64,
                        parquet::record::Field::Long(l) => *l as f64,
                        _ => 0.0,
                    };
                }
                "indicative_usd_amt" => {
                    usd_amt = match field {
                        parquet::record::Field::Double(d) => *d,
                        parquet::record::Field::Float(f) => *f as f64,
                        parquet::record::Field::Int(i) => *i as f64,
                        parquet::record::Field::Long(l) => *l as f64,
                        _ => 0.0,
                    };
                }
                "gl_date" => {
                    if let parquet::record::Field::Date(days) = field {
                        gl_date_days = *days as i64;
                    }
                }
                _ => {}
            }
        }

        let policy = policy?;

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

        let co_cleaned = co.trim().trim_start_matches('0').to_string();
        let icp_cleaned = icp.trim().trim_start_matches('0').to_string();
        let objsub_cleaned = objsub.trim().to_string();
        let ref_cleaned = reference.trim().to_string();
        let desc_cleaned = description.trim().to_string();

        Some(IntercoTx {
            row: row_idx + 2,
            policy,
            co,
            objsub,
            icp,
            reference,
            description,
            business_unit,
            gl_date_days,
            usd_amt: signed_usd,
            co_cleaned,
            icp_cleaned,
            objsub_cleaned,
            ref_cleaned,
            desc_cleaned,
            log_value: signed_usd.abs().ln_1p(),
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
    let exp_a = ((a.abs().max(0.01).to_bits() >> 52) & 0x7FF) as i32;
    let exp_b = ((b.abs().max(0.01).to_bits() >> 52) & 0x7FF) as i32;
    (exp_a - exp_b).abs() as f64
}

fn compute_cost(a: &IntercoTx, b: &IntercoTx) -> f64 {
    // Only connect opposite signs
    if a.usd_amt.signum() == b.usd_amt.signum() {
        return f64::MAX;
    }

    // Start with baseline metadata cost
    let mut metadata_cost = 100.0;

    // 1. if (co,icp) on one side = (icp,co) on the other side, high prior
    if !a.co_cleaned.is_empty() && !a.icp_cleaned.is_empty() && a.co_cleaned == b.icp_cleaned && a.icp_cleaned == b.co_cleaned {
        metadata_cost *= 0.05; // 95% discount
    }

    // 2. if (objsub) = (objsub), high prior
    if !a.objsub_cleaned.is_empty() && a.objsub_cleaned == b.objsub_cleaned {
        metadata_cost *= 0.1; // 90% discount
    }

    // 3. if reference equal, high prior (but less so than the above)
    let is_meaningful = |r: &str| !r.is_empty() && r != "nan" && r != "AGGREGATED OPENING BALANCE";
    if is_meaningful(&a.ref_cleaned) && is_meaningful(&b.ref_cleaned) && a.ref_cleaned == b.ref_cleaned {
        metadata_cost *= 0.2; // 80% discount
    }

    // 4. if (reference) = (description), high prior
    if (is_meaningful(&a.ref_cleaned) && !b.desc_cleaned.is_empty() && a.ref_cleaned == b.desc_cleaned) ||
       (is_meaningful(&b.ref_cleaned) && !a.desc_cleaned.is_empty() && b.ref_cleaned == a.desc_cleaned) {
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

    let mut matched_set = HashSet::new();
    for m in matches {
        matched_set.insert(m.source_idx);
        matched_set.insert(m.sink_idx);
    }
    let matched_rows_count = matched_set.len();
    let unmatched_rows_count = txs.len() - matched_rows_count;

    println!("\n  📊 SUMMARY");
    println!("  ──────────────────────────────────────────────────────────────────────────");
    println!("  Total input rows:        {:>5}", txs.len());
    println!(
        "  Matched rows:            {:>5}  ({:.1}%)",
        matched_rows_count,
        100.0 * matched_rows_count as f64 / txs.len() as f64
    );
    println!(
        "  Unmatched rows:          {:>5}  ({:.1}%)",
        unmatched_rows_count,
        100.0 * unmatched_rows_count as f64 / txs.len() as f64
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
    let filter_co = args.get(2).cloned();
    let row_offset: usize = args
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);

    let filter_cos: Option<Vec<String>> = filter_co.as_ref().map(|s| {
        s.split(',')
            .map(|part| part.trim().to_string())
            .collect()
    });

    if let Some(ref filter) = filter_co {
        println!("Loading intercompany data from: {} (filtered by company list: {}, row offset: {})", path, filter, row_offset);
    } else {
        println!("Loading intercompany data from: {} (row offset: {})", path, row_offset);
    }

    let mut transactions: Vec<IntercoTx> = Vec::new();

    if path.to_lowercase().ends_with(".parquet") {
        use parquet::file::reader::FileReader;
        let file = std::fs::File::open(path).expect("Failed to open Parquet file");
        let reader = parquet::file::reader::SerializedFileReader::new(file)
            .expect("Failed to create Parquet reader");
        let row_iter = reader.get_row_iter(None).expect("Failed to get row iterator");
        
        for (row_idx, row_res) in row_iter.enumerate() {
            let row = row_res.expect("Failed to read Parquet row");
            if let Some(tx) = IntercoTx::from_parquet_row(row_idx, &row)
                && tx.usd_amt.abs() > 0.01 {
                    if let Some(ref filters) = filter_cos {
                        let co_matched = filters.iter().any(|filter| {
                            let filter_clean = filter.trim_start_matches('0');
                            tx.co_cleaned == filter_clean || tx.co.trim() == filter.as_str()
                        });
                        let icp_matched = filters.iter().any(|filter| {
                            let filter_clean = filter.trim_start_matches('0');
                            tx.icp_cleaned == filter_clean || tx.icp.trim() == filter.as_str()
                        });
                        let matched = if filters.len() > 1 {
                            co_matched && icp_matched
                        } else {
                            co_matched
                        };
                        if matched {
                            transactions.push(tx);
                        }
                    } else {
                        transactions.push(tx);
                    }
                }
        }
    } else {
        let mut rdr = csv::Reader::from_path(path).expect("Failed to open CSV");
        for (row_idx, result) in rdr.records().enumerate() {
            let record = result.expect("Failed to read CSV record");
            if let Some(tx) = IntercoTx::from_csv_record(row_idx + 2, &record)
                && tx.usd_amt.abs() > 0.01 {
                    if let Some(ref filters) = filter_cos {
                        let co_matched = filters.iter().any(|filter| {
                            let filter_clean = filter.trim_start_matches('0');
                            tx.co_cleaned == filter_clean || tx.co.trim() == filter.as_str()
                        });
                        let icp_matched = filters.iter().any(|filter| {
                            let filter_clean = filter.trim_start_matches('0');
                            tx.icp_cleaned == filter_clean || tx.icp.trim() == filter.as_str()
                        });
                        let matched = if filters.len() > 1 {
                            co_matched && icp_matched
                        } else {
                            co_matched
                        };
                        if matched {
                            transactions.push(tx);
                        }
                    } else {
                        transactions.push(tx);
                    }
                }
        }
    }

    let n = transactions.len();
    println!("Loaded {} transactions", n);

    // 1. Sort the transactions by gl_date_days, and secondarily by signed usd_amt.
    transactions.sort_by(|a, b| {
        a.gl_date_days.cmp(&b.gl_date_days)
            .then_with(|| {
                a.usd_amt.partial_cmp(&b.usd_amt).unwrap_or(std::cmp::Ordering::Equal)
            })
    });

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

    let max_date_diff = 90;
    let max_value_ratio = 10.0;

    // Supplies and Unmatched Penalties
    let supplies: Vec<f64> = transactions.iter().map(|t| t.usd_amt).collect();
    let penalties = vec![1_000_000.0; n];

    // 2. Generate sparse candidate edges within configurable row offset with pre-computed costs
    let mut candidate_edges = Vec::new();
    for i in 0..n {
        if transactions[i].usd_amt > 0.0 {
            let start = i.saturating_sub(row_offset);
            let end = (i + row_offset).min(n - 1);
            for j in start..=end {
                if transactions[j].usd_amt < 0.0 {
                    let s = &transactions[i];
                    let t = &transactions[j];

                    // Compute cost
                    let mut cost = compute_cost(s, t);

                    // Apply date-diff and value-ratio filters
                    if (s.gl_date_days - t.gl_date_days).abs() > max_date_diff {
                        cost = 1_000_000_000.0;
                    }
                    let ratio = s.usd_amt.abs().max(t.usd_amt.abs())
                        / s.usd_amt.abs().min(t.usd_amt.abs()).max(0.01);
                    if ratio > max_value_ratio {
                        cost = 1_000_000_000.0;
                    }

                    candidate_edges.push((i, j, cost));
                }
            }
        }
    }

    // --- Solve ---
    println!("  Solving transportation sparse graph with Network Simplex (edges limit = {}, row offset = {})...", candidate_edges.len(), row_offset);
    let t_solve_start = Instant::now();
    let mut reconciler = SparseReconciler::new();
    reconciler.update(&supplies, &penalties, &candidate_edges).unwrap();
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
