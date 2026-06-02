//! Intercompany reconciliation expressed as a combinator pipeline.
//!
//!   partition_by(unit, partition_by(ccy, seq[
//!       agg_net,        // whole unit+currency nets at aggregate -> accept wholesale
//!       exact_1to1,     // clean opposite-sign pairs of equal native amount
//!       signal_group,   // reference bridge: shared token buckets that net
//!       flow(model),    // engine arbitrates the ambiguous remainder
//!   ]))
//!
//! Sharding by currency makes each sub-problem single-currency, so native amount
//! IS the canonical numeraire and FX never enters the flow.
//!
//! Run: cargo run --release --example interco [path]

use florecon::recon::Model;
use florecon::strategy::{Item, agg_net, exact_1to1, flow, partition_by, seq, signal_group};
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;
use std::collections::HashMap;
use std::fs::File;

#[derive(Clone)]
struct Tx {
    unit: u64,        // hashed unordered {company, icp}
    ccy: u64,         // hashed native currency (shard key; FX vanishes within)
    objsub: u64,      // hashed GL account (aggregation key)
    snative: i64,     // signed native amount, minor units (canonical per shard)
    gl_day: i64,
    tokens: Vec<u64>, // hashed reference tokens (the cross-book bridge)
}

#[derive(Clone)]
struct Interco {
    penalty: f64,
}
impl Model for Interco {
    type Tx = Tx;
    fn base_amount(&self, tx: &Tx) -> i64 {
        tx.snative // single currency per shard -> exact conservation, no FX
    }
    fn penalty(&self, _tx: &Tx) -> f64 {
        self.penalty
    }
    fn block_key(&self, tx: &Tx) -> i64 {
        tx.gl_day
    }
    fn window(&self) -> i64 {
        -1
    }
    fn match_keys(&self, tx: &Tx) -> Vec<u64> {
        let mut k = tx.tokens.clone();
        if tx.snative != 0 {
            k.push(fnv1a(&format!("AMT:{}", tx.snative.abs())));
        }
        k
    }
    fn cost(&self, a: &Tx, b: &Tx) -> Option<f64> {
        let ref_bridge = a.tokens.iter().any(|t| b.tokens.contains(t));
        let amt_match = a.snative.abs() == b.snative.abs() && a.snative != 0;
        let dd = (a.gl_day - b.gl_day).abs() as f64;
        let eps = 0.5;
        if ref_bridge {
            Some(1.0 + eps + dd * 0.002 + if amt_match { 0.0 } else { 0.5 })
        } else if amt_match {
            if dd > 92.0 {
                return None;
            }
            Some(4.0 + eps + dd * 0.02)
        } else {
            None
        }
    }
}

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
fn tokens(fields: &[&str]) -> Vec<u64> {
    let mut out = Vec::new();
    for field in fields {
        for raw in field.split_whitespace() {
            let t: String = raw
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_uppercase();
            if t.len() < 6 || t.len() > 40 || t == "OFFSETENTRY" || t.chars().all(|c| c.is_alphabetic()) {
                continue;
            }
            let h = fnv1a(&t);
            if !out.contains(&h) {
                out.push(h);
            }
        }
    }
    out
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "data/ledger.parquet".to_string());

    let t0 = std::time::Instant::now();
    let reader = SerializedFileReader::new(File::open(&path).expect("open")).expect("reader");

    let mut items: Vec<Item<Tx>> = Vec::new();
    let mut usd_by_id: Vec<i64> = Vec::new();
    for row in reader.get_row_iter(None).expect("rows") {
        let row = row.expect("row");
        let (mut co, mut icp, mut objsub) = (String::new(), String::new(), String::new());
        let (mut usd, mut gl) = (0.0, 0i64);
        let (mut bccy, mut tccy) = (String::new(), String::new());
        let (mut trx, mut fc) = (0.0, 0.0);
        let (mut refr, mut ref2, mut desc, mut remark, mut inv) = (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        );
        let mut is_off = false;
        for (n, f) in row.get_column_iter() {
            match n.as_str() {
                "company" => co = fstr(f),
                "icp" => icp = fstr(f),
                "objsub" => objsub = fstr(f),
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
        if is_off || co.is_empty() || icp.is_empty() || co == icp {
            continue;
        }
        let mut pair = [co.clone(), icp.clone()];
        pair.sort();
        // native amount: trx currency, falling back to base currency
        let (ccy_s, amt) = if trx.abs() >= 0.005 {
            (tccy.as_str(), trx)
        } else {
            (bccy.as_str(), fc)
        };
        let usd_cents = (usd * 100.0).round() as i64;
        let sign = usd_cents.signum();
        let snative = (amt.abs() * 100.0).round() as i64 * sign;
        let id = items.len() as u64;
        usd_by_id.push(usd_cents);
        items.push(Item {
            id,
            data: Tx {
                unit: fnv1a(&format!("{}|{}", pair[0], pair[1])),
                ccy: fnv1a(ccy_s),
                objsub: fnv1a(&objsub),
                snative,
                gl_day: gl,
                tokens: tokens(&[&refr, &ref2, &desc, &remark, &inv]),
            },
        });
    }
    eprintln!("read {} rows in {:.2?}", items.len(), t0.elapsed());

    // The pipeline.
    const TOL: i64 = 100; // 1.00 in native minor units
    const CAP: usize = 256;
    let pipeline = partition_by(
        |t: &Tx| t.unit,
        partition_by(
            |t: &Tx| t.ccy,
            seq(vec![
                agg_net(|t: &Tx| t.objsub, |t: &Tx| t.snative, TOL),
                exact_1to1(
                    |t: &Tx| if t.snative != 0 { Some(t.snative.unsigned_abs()) } else { None },
                    |t: &Tx| t.snative,
                ),
                signal_group(|t: &Tx| t.tokens.clone(), |t: &Tx| t.snative, TOL, CAP),
                flow(Interco { penalty: 1000.0 }),
            ]),
        ),
    );

    let total = items.len();
    let total_value: i64 = usd_by_id.iter().map(|v| v.abs()).sum();
    let t1 = std::time::Instant::now();
    let res = pipeline.run(items);
    let solve_time = t1.elapsed();

    // Tally.
    let mut by_origin: HashMap<&'static str, (usize, usize)> = HashMap::new(); // origin -> (groups, rows)
    let mut matched_rows = 0usize;
    let mut matched_value = 0i64;
    let mut clean = 0usize;
    for g in &res.groups {
        let e = by_origin.entry(g.origin).or_default();
        e.0 += 1;
        e.1 += g.members.len();
        matched_rows += g.members.len();
        matched_value += g.members.iter().map(|id| usd_by_id[*id as usize].abs()).sum::<i64>();
        if g.net.abs() <= TOL {
            clean += 1;
        }
    }

    println!("\n=== combinator pipeline ===");
    println!("  rows            : {total}");
    println!(
        "  matched rows    : {matched_rows} ({:.1}% by count)",
        100.0 * matched_rows as f64 / total.max(1) as f64
    );
    println!(
        "  matched value   : {:.0} of {:.0} usd ({:.1}% by value)",
        matched_value as f64 / 100.0,
        total_value as f64 / 100.0,
        100.0 * matched_value as f64 / total_value.max(1) as f64
    );
    println!("  groups          : {} ({} clean)", res.groups.len(), clean);
    let mut origins: Vec<_> = by_origin.iter().collect();
    origins.sort_by_key(|(k, _)| *k);
    for (origin, (groups, rows)) in origins {
        println!("    {origin:<13} {groups:>7} groups  {rows:>8} rows");
    }
    println!("  residual rows   : {}", res.residual.len());
    println!("  pipeline time   : {solve_time:.2?}");
}
