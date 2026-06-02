//! Intercompany reconciliation on the real parquet extract.
//!
//! Pipeline:
//!   1. read rows, drop offset entries;
//!   2. shard by bilateral pair {company, icp} (unordered);
//!   3. per unit, feed the `recon` engine with reference-token match keys
//!      (the cross-book bridge) and amount/date-aware costs;
//!   4. solve and report matched groups, 1-to-many structure, and residual.
//!
//! Run: cargo run --release --example interco [path] [--unit AAAAA BBBBB]

use florecon::recon::{ExtId, Model, Reconciler};
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;
use std::collections::HashMap;
use std::fs::File;

// ---------------------------------------------------------------------------
// Domain
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Tx {
    usd_cents: i64, // signed numeraire the engine conserves
    gl_day: i64,
    ccy: u64,       // hashed native currency
    amt_cents: i64, // |native amount| (trx, or base as fallback)
    tokens: Vec<u64>, // hashed reference tokens (cross-book bridge keys)
}

struct Interco {
    penalty: f64,
}

impl Model for Interco {
    type Tx = Tx;
    fn base_amount(&self, tx: &Tx) -> i64 {
        tx.usd_cents
    }
    fn penalty(&self, _tx: &Tx) -> f64 {
        self.penalty
    }
    fn block_key(&self, tx: &Tx) -> i64 {
        tx.gl_day
    }
    fn window(&self) -> i64 {
        -1 // disable proximity generation; candidacy comes from match keys
    }
    fn match_keys(&self, tx: &Tx) -> Vec<u64> {
        // Reference tokens (the strong cross-book bridge) plus a native
        // (currency, amount) key so unbridged rows -- GA postings with no
        // reference -- can still pair on an exact amount.
        let mut k = tx.tokens.clone();
        if tx.amt_cents > 0 {
            k.push(fnv1a(&format!("AMT:{}:{}", tx.ccy, tx.amt_cents)));
        }
        k
    }
    fn cost(&self, a: &Tx, b: &Tx) -> Option<f64> {
        // Tier by confidence. A shared reference token is the trustworthy
        // signal (it survives 1-to-many splits); an exact native amount with
        // no reference is weaker and only allowed within a date window.
        let ref_bridge = a.tokens.iter().any(|t| b.tokens.contains(t));
        let amt_match = a.ccy == b.ccy && a.amt_cents == b.amt_cents && a.amt_cents > 0;
        let dd = (a.gl_day - b.gl_day).abs() as f64;
        let eps = 0.5; // per-leg activation: discourage sprawl
        if ref_bridge {
            // Trusted: cheapest when the amount also agrees (clean 1-to-1).
            Some(1.0 + eps + dd * 0.002 + if amt_match { 0.0 } else { 0.5 })
        } else if amt_match {
            if dd > 92.0 {
                return None; // amount-only across distant dates: coincidence
            }
            Some(4.0 + eps + dd * 0.02)
        } else {
            None // no signal
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

fn fstr(f: &Field) -> String {
    match f {
        Field::Str(v) => v.clone(),
        _ => String::new(),
    }
}
fn fdouble(f: &Field) -> f64 {
    match f {
        Field::Double(v) => *v,
        Field::Float(v) => *v as f64,
        _ => 0.0,
    }
}
fn fday(f: &Field) -> i64 {
    match f {
        Field::Date(v) => *v as i64,
        _ => 0,
    }
}
fn fbool(f: &Field) -> bool {
    matches!(f, Field::Bool(true))
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Reference-like tokens from a row's text fields: whitespace-split, strip
/// non-alphanumerics, uppercase, keep medium-length tokens, drop known junk.
fn tokens(fields: &[&str]) -> Vec<u64> {
    let mut out = Vec::new();
    for field in fields {
        for raw in field.split_whitespace() {
            let t: String = raw.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_uppercase();
            if t.len() < 6 || t.len() > 40 {
                continue;
            }
            if t == "OFFSETENTRY" || t.chars().all(|c| c.is_alphabetic()) {
                continue; // pure words carry no doc identity
            }
            let h = fnv1a(&t);
            if !out.contains(&h) {
                out.push(h);
            }
        }
    }
    out
}

struct Raw {
    unit: (String, String), // unordered {company, icp}
    tx: Tx,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .get(1)
        .map(|s| s.as_str())
        .unwrap_or("data/ledger.parquet");
    let only_unit: Option<(String, String)> = match (args.iter().position(|a| a == "--unit"), &args) {
        (Some(i), a) if a.len() > i + 2 => {
            let mut u = [a[i + 1].clone(), a[i + 2].clone()];
            u.sort();
            Some((u[0].clone(), u[1].clone()))
        }
        _ => None,
    };

    let t0 = std::time::Instant::now();
    let file = File::open(path).expect("open parquet");
    let reader = SerializedFileReader::new(file).expect("reader");

    let mut raws: Vec<Raw> = Vec::new();
    for row in reader.get_row_iter(None).expect("rows") {
        let row = row.expect("row");
        let (mut co, mut icp) = (String::new(), String::new());
        let (mut usd, mut gl) = (0.0, 0i64);
        let (mut bccy, mut tccy) = (String::new(), String::new());
        let (mut trx, mut fc) = (0.0, 0.0);
        let (mut refr, mut ref2, mut desc, mut remark, mut inv) =
            (String::new(), String::new(), String::new(), String::new(), String::new());
        let mut is_off = false;
        for (n, f) in row.get_column_iter() {
            match n.as_str() {
                "company" => co = fstr(f),
                "icp" => icp = fstr(f),
                "indicative_usd_amt" => usd = fdouble(f),
                "gl_date" => gl = fday(f),
                "base_currency" => bccy = fstr(f),
                "trx_currency" => tccy = fstr(f),
                "trx_amt" => trx = fdouble(f),
                "fc_amt" => fc = fdouble(f),
                "reference" => refr = fstr(f),
                "reference2" => ref2 = fstr(f),
                "description" => desc = fstr(f),
                "name_remark_explanation" => remark = fstr(f),
                "invoice_no" => inv = fstr(f),
                "is_offset" => is_off = fbool(f),
                _ => {}
            }
        }
        if is_off || co.is_empty() || icp.is_empty() {
            continue;
        }
        let mut unit = [co.clone(), icp.clone()];
        unit.sort();
        let unit = (unit[0].clone(), unit[1].clone());
        if let Some(u) = &only_unit
            && &unit != u
        {
            continue;
        }
        // Native amount: transaction currency, falling back to base when the
        // transaction amount is blank.
        let (ccy_s, amt) = if trx.abs() >= 0.005 {
            (tccy.as_str(), trx)
        } else {
            (bccy.as_str(), fc)
        };
        let toks = tokens(&[&refr, &ref2, &desc, &remark, &inv]);
        raws.push(Raw {
            unit,
            tx: Tx {
                usd_cents: (usd * 100.0).round() as i64,
                gl_day: gl,
                ccy: fnv1a(ccy_s),
                amt_cents: (amt.abs() * 100.0).round() as i64,
                tokens: toks,
            },
        });
    }
    eprintln!("read {} real rows in {:.2?}", raws.len(), t0.elapsed());

    // Shard by bilateral unit.
    let mut units: HashMap<(String, String), Vec<Tx>> = HashMap::new();
    for r in raws {
        units.entry(r.unit).or_default().push(r.tx);
    }

    // Aggregate stats.
    let mut agg = Stats::default();
    let mut per_unit: Vec<(String, usize, usize, usize, i64)> = Vec::new();
    let mut rate_buckets = [0usize; 5]; // <20,<50,<80,<95,>=95 % matched
    let mut one_sided = 0usize; // units with rows on only one side (unmatchable)
    let mut unit_net_abs = 0i64; // sum of |unit net| = irreducible residual
    let t1 = std::time::Instant::now();
    for (unit, txs) in &units {
        if unit.0 == unit.1 {
            continue; // self-pair (icp == company): not a counterparty
        }
        let pos = txs.iter().filter(|t| t.usd_cents > 0).count();
        if pos == 0 || pos == txs.len() {
            one_sided += 1;
            continue; // nothing to reconcile against
        }
        let pos = txs.iter().filter(|t| t.usd_cents > 0).count();
        if pos == 0 || pos == txs.len() {
            one_sided += 1;
            continue; // nothing to reconcile against
        }
        // Irreducible residual: a unit's total net is what stays unreconciled
        // no matter how we group (conservation).
        unit_net_abs += txs.iter().map(|t| t.usd_cents).sum::<i64>().abs();
        let s = run_unit(txs);
        let rate = s.matched_rows as f64 / txs.len().max(1) as f64;
        rate_buckets[match rate {
            r if r < 0.20 => 0,
            r if r < 0.50 => 1,
            r if r < 0.80 => 2,
            r if r < 0.95 => 3,
            _ => 4,
        }] += 1;
        agg.add(&s);
        per_unit.push((
            format!("{}<->{}", unit.0, unit.1),
            txs.len(),
            s.matched_rows,
            s.one_to_many,
            s.residual_abs,
        ));
    }
    let solve_time = t1.elapsed();
    eprintln!("solved {} units in {:.2?}", units.len(), solve_time);

    per_unit.sort_by_key(|x| std::cmp::Reverse(x.1));
    println!("\n=== largest units (rows | matched | 1-to-many groups | |residual| usd) ===");
    for (name, rows, matched, otm, resid) in per_unit.iter().take(15) {
        println!(
            "  {name:<16} {rows:>7} | {matched:>7} | {otm:>6} | {:>14.2}",
            *resid as f64 / 100.0
        );
    }

    println!("\n=== totals over {} units ===", units.len());
    println!("  rows                : {}", agg.rows);
    println!(
        "  matched rows        : {} ({:.1}% by count)",
        agg.matched_rows,
        100.0 * agg.matched_rows as f64 / agg.rows.max(1) as f64
    );
    println!(
        "  matched value       : {:.0} of {:.0} usd ({:.1}% by value)",
        agg.matched_value as f64 / 100.0,
        agg.total_value as f64 / 100.0,
        100.0 * agg.matched_value as f64 / agg.total_value.max(1) as f64
    );
    println!("  groups              : {}", agg.groups);
    println!("    1-to-1            : {}", agg.one_to_one);
    println!("    1-to-many         : {}", agg.one_to_many);
    println!("  clean groups (<=$1) : {}", agg.clean_groups);
    println!(
        "  group gross residual: {:.2} usd (incl. genuine large-value discrepancies)",
        agg.residual_abs as f64 / 100.0
    );
    println!(
        "  irreducible residual: {:.2} usd (sum of |unit net| -- the real unreconciled total)",
        unit_net_abs as f64 / 100.0
    );
    println!(
        "  unit matched-rate   : <20%={} <50%={} <80%={} <95%={} >=95%={}",
        rate_buckets[0], rate_buckets[1], rate_buckets[2], rate_buckets[3], rate_buckets[4]
    );
    println!("  one-sided units     : {} (no counterparty rows; excluded above)", one_sided);
    println!("  solve wall time     : {:.2?}", solve_time);
}

#[derive(Default)]
struct Stats {
    rows: usize,
    matched_rows: usize,
    total_value: i64,
    matched_value: i64,
    groups: usize,
    one_to_one: usize,
    one_to_many: usize,
    clean_groups: usize,
    residual_abs: i64,
}
impl Stats {
    fn add(&mut self, o: &Stats) {
        self.rows += o.rows;
        self.matched_rows += o.matched_rows;
        self.total_value += o.total_value;
        self.matched_value += o.matched_value;
        self.groups += o.groups;
        self.one_to_one += o.one_to_one;
        self.one_to_many += o.one_to_many;
        self.clean_groups += o.clean_groups;
        self.residual_abs += o.residual_abs;
    }
}

fn run_unit(txs: &[Tx]) -> Stats {
    let mut r = Reconciler::new(Interco { penalty: 1000.0 });
    for (i, tx) in txs.iter().enumerate() {
        r.upsert(i as ExtId, tx.clone());
    }
    r.solve();
    let groups = r.groups();
    let mut s = Stats {
        rows: txs.len(),
        total_value: txs.iter().map(|t| t.usd_cents.abs()).sum(),
        ..Default::default()
    };
    for g in &groups {
        s.groups += 1;
        s.matched_rows += g.members.len();
        s.matched_value += g.members.iter().map(|id| txs[*id as usize].usd_cents.abs()).sum::<i64>();
        if g.members.len() <= 2 {
            s.one_to_one += 1;
        } else {
            s.one_to_many += 1;
        }
        if g.net_base.abs() <= 100 {
            s.clean_groups += 1;
        } else {
            s.residual_abs += g.net_base.abs();
        }
    }
    s
}
