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
        -1 // disable proximity generation; candidacy comes from reference tokens
    }
    fn match_keys(&self, tx: &Tx) -> Vec<u64> {
        tx.tokens.clone()
    }
    fn cost(&self, a: &Tx, b: &Tx) -> Option<f64> {
        // Candidacy already implies a shared reference token. Score by date
        // proximity, plus a fixed per-leg activation cost (epsilon) so the
        // solver prefers tight 1-to-1 / small groups over sprawl.
        let dd = (a.gl_day - b.gl_day).abs() as f64;
        Some(1.0 + dd * 0.002 + 0.5)
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
        let (mut refr, mut ref2, mut desc, mut remark, mut inv) =
            (String::new(), String::new(), String::new(), String::new(), String::new());
        let mut is_off = false;
        for (n, f) in row.get_column_iter() {
            match n.as_str() {
                "company" => co = fstr(f),
                "icp" => icp = fstr(f),
                "indicative_usd_amt" => usd = fdouble(f),
                "gl_date" => gl = fday(f),
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
        let toks = tokens(&[&refr, &ref2, &desc, &remark, &inv]);
        raws.push(Raw {
            unit,
            tx: Tx {
                usd_cents: (usd * 100.0).round() as i64,
                gl_day: gl,
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
    let t1 = std::time::Instant::now();
    for (unit, txs) in &units {
        if unit.0 == unit.1 {
            continue; // self-pair (icp == company): not a counterparty
        }
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
    println!("  matched rows        : {} ({:.1}%)", agg.matched_rows, 100.0 * agg.matched_rows as f64 / agg.rows.max(1) as f64);
    println!("  groups              : {}", agg.groups);
    println!("    1-to-1            : {}", agg.one_to_one);
    println!("    1-to-many         : {}", agg.one_to_many);
    println!("  clean groups (<=$1) : {}", agg.clean_groups);
    println!("  sum |residual| usd  : {:.2}", agg.residual_abs as f64 / 100.0);
    println!(
        "  unit matched-rate   : <20%={} <50%={} <80%={} <95%={} >=95%={}",
        rate_buckets[0], rate_buckets[1], rate_buckets[2], rate_buckets[3], rate_buckets[4]
    );
    println!("  solve wall time     : {:.2?}", solve_time);
}

#[derive(Default)]
struct Stats {
    rows: usize,
    matched_rows: usize,
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
        ..Default::default()
    };
    for g in &groups {
        s.groups += 1;
        s.matched_rows += g.members.len();
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
