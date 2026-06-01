//! Intercompany reconciliation — ported to the `recon` facade.
//!
//! Shows the ergonomic path: implement `Model` once (numeraire = USD cents,
//! proximity key = GL date, cost = matching quality), then stream transactions
//! in via `upsert`, `solve`, and read back netted groups.

use florecon::recon::{Model, Reconciler};
use std::collections::HashMap;
use std::env;
use std::time::Instant;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct IntercoTx {
    row: usize,
    policy: Policy,
    co: String,
    icp: String,
    reference: String,
    business_unit: String,
    gl_date_days: i64,
    usd_amt: f64,
    co_cleaned: String,
    icp_cleaned: String,
    ref_cleaned: String,
    trx_currency: String,
    trx_amt: f64,
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
        let _ = objsub;
        let icp = record.get(13)?.trim().to_string();
        let reference = record.get(20)?.trim().to_string();
        let business_unit = record.get(37)?.trim().to_string();
        let fc_amt: f64 = record.get(27)?.parse().ok()?;
        let usd_amt: f64 = record.get(43)?.parse().ok()?;
        let gl_date_days = parse_date(record.get(28)?).unwrap_or(0);
        let trx_currency = record.get(25).map(|s| s.trim().to_string()).unwrap_or_default();
        let trx_amt: f64 = record.get(26).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        Some(Self::build(
            row,
            policy,
            co,
            icp,
            reference,
            business_unit,
            gl_date_days,
            fc_amt,
            usd_amt,
            trx_currency,
            trx_amt,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        row: usize,
        policy: Policy,
        co: String,
        icp: String,
        reference: String,
        business_unit: String,
        gl_date_days: i64,
        fc_amt: f64,
        usd_amt: f64,
        trx_currency: String,
        trx_amt: f64,
    ) -> Self {
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
        let ref_cleaned = reference.trim().to_string();
        IntercoTx {
            row,
            policy,
            co,
            icp,
            reference,
            business_unit,
            gl_date_days,
            usd_amt: signed_usd,
            co_cleaned,
            icp_cleaned,
            ref_cleaned,
            trx_currency,
            trx_amt,
        }
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
            return Some((year - 2020) * 365 + (month - 1) * 30 + day);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The Model
// ---------------------------------------------------------------------------

struct IntercoModel {
    ref_counts: HashMap<String, usize>,
    total_txs: usize,
    window_days: i64,
    max_value_ratio: f64,
}

impl Model for IntercoModel {
    type Tx = IntercoTx;

    fn base_amount(&self, tx: &IntercoTx) -> i64 {
        (tx.usd_amt * 100.0).round() as i64
    }

    fn penalty(&self, _tx: &IntercoTx) -> f64 {
        1_000_000.0
    }

    fn block_key(&self, tx: &IntercoTx) -> i64 {
        tx.gl_date_days
    }

    fn window(&self) -> i64 {
        self.window_days
    }

    fn cost(&self, a: &IntercoTx, b: &IntercoTx) -> Option<f64> {
        // Value-ratio gate.
        let ratio =
            a.usd_amt.abs().max(b.usd_amt.abs()) / a.usd_amt.abs().min(b.usd_amt.abs()).max(0.01);
        if ratio > self.max_value_ratio {
            return None;
        }

        let exact_val = (a.usd_amt + b.usd_amt).abs() < 0.01;
        let close_val = (a.usd_amt + b.usd_amt) / (a.usd_amt.abs() + b.usd_amt.abs()) < 0.001;
        let same_ccy = !a.trx_currency.is_empty() && a.trx_currency == b.trx_currency;
        let exact_trx = same_ccy && (a.trx_amt + b.trx_amt).abs() < 0.01;
        let close_trx = same_ccy && (a.trx_amt + b.trx_amt) / (a.trx_amt.abs() + b.trx_amt.abs()) < 0.001;
        let good_val = exact_val || close_val || exact_trx || close_trx;
        let date_within_3 = (a.gl_date_days - b.gl_date_days).abs() <= 3;

        // IDF quality of the reference.
        let ref_str = &a.ref_cleaned;
        let count = *self.ref_counts.get(ref_str).unwrap_or(&1);
        let idf = (self.total_txs as f64 / count as f64).ln().max(0.0);
        let max_idf = (self.total_txs as f64 / 2.0).max(2.0).ln();
        let ref_quality = (idf / max_idf).clamp(0.0, 1.0);
        let exact_ref = !ref_str.is_empty();

        let base_cost = if good_val && exact_ref && date_within_3 {
            1.0 + (1.0 - ref_quality) * 9.0
        } else if exact_ref {
            5.0 + (1.0 - ref_quality) * 45.0
        } else if good_val && date_within_3 {
            10.0
        } else if good_val {
            50.0
        } else {
            200.0
        };

        let val_diff = (a.usd_amt.abs() - b.usd_amt.abs()).abs();
        let date_diff = (a.gl_date_days - b.gl_date_days).abs() as f64;
        Some(base_cost + val_diff * 0.10 + date_diff * 0.5)
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = env::args().collect();
    let path = args.get(1).map(|s| s.as_str()).unwrap_or("data/ledger.csv");
    let filter_co = args.get(2).cloned();
    let window_days: i64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(90);

    let filter_cos: Option<Vec<String>> = filter_co
        .as_ref()
        .map(|s| s.split(',').map(|p| p.trim().to_string()).collect());

    println!("Loading {path} (window = {window_days} days)");

    let mut rdr = csv::Reader::from_path(path).expect("Failed to open CSV");
    let mut txs: Vec<IntercoTx> = Vec::new();
    for (i, result) in rdr.records().enumerate() {
        let record = result.expect("bad record");
        let Some(tx) = IntercoTx::from_csv_record(i + 2, &record) else {
            continue;
        };
        if tx.usd_amt.abs() <= 0.01 {
            continue;
        }
        if let Some(filters) = &filter_cos {
            let matches = |a: &str, b: &str| {
                filters
                    .iter()
                    .any(|f| a == f.trim_start_matches('0') || b == f.as_str())
            };
            let co_ok = matches(&tx.co_cleaned, tx.co.trim());
            let icp_ok = matches(&tx.icp_cleaned, tx.icp.trim());
            let keep = if filters.len() > 1 { co_ok && icp_ok } else { co_ok };
            if !keep {
                continue;
            }
        }
        txs.push(tx);
    }

    let n = txs.len();
    println!("Loaded {n} transactions");

    let mut ref_counts = HashMap::new();
    for tx in &txs {
        if !tx.ref_cleaned.is_empty() {
            *ref_counts.entry(tx.ref_cleaned.clone()).or_insert(0) += 1;
        }
    }

    let model = IntercoModel {
        ref_counts,
        total_txs: n,
        window_days,
        max_value_ratio: 10.0,
    };

    let t0 = Instant::now();
    let mut recon = Reconciler::new(model);
    for (i, tx) in txs.iter().enumerate() {
        recon.upsert(i as u64, tx.clone());
    }
    let status = recon.solve();
    let elapsed = t0.elapsed();
    println!("Solve {status:?} in {elapsed:?}");

    let groups = recon.groups();
    let unmatched = recon.unmatched();

    let matched_rows: usize = groups.iter().map(|g| g.members.len()).sum();
    let clean = groups.iter().filter(|g| g.clean).count();
    println!("\n  SUMMARY");
    println!("  total rows:    {n}");
    println!("  matched rows:  {matched_rows}");
    println!("  unmatched:     {}", unmatched.len());
    println!("  groups:        {} ({clean} clean, {} residual)", groups.len(), groups.len() - clean);

    // group size distribution
    let mut sizes: HashMap<usize, usize> = HashMap::new();
    for g in &groups {
        *sizes.entry(g.members.len()).or_insert(0) += 1;
    }
    let mut sizes: Vec<_> = sizes.into_iter().collect();
    sizes.sort();
    println!("\n  GROUP SIZE DISTRIBUTION");
    for (size, count) in sizes {
        println!("    size {size:>2}: {count:>4} {}", "#".repeat(count.min(50)));
    }

    println!("\n  RESIDUAL GROUPS (net != 0) — first 10");
    for g in groups.iter().filter(|g| !g.clean).take(10) {
        println!("    net={:>12.2}  members={:?}", g.net_base as f64 / 100.0, g.members);
        for &id in &g.members {
            let t = &txs[id as usize];
            println!("      [{:>3}] {:?} {:>12.2} ref={}", t.row, t.policy, t.usd_amt, t.reference);
        }
    }
}
