//! Intercompany reconciliation as a florecon plugin.
//!
//! The host ships a columnar ledger (one Arrow table). This plugin bakes in the
//! preprocessing (FX selection, reference tokenization, identity) and the
//! matching cascade, then `export_plugin!` emits the self-describing wasm.
//!
//!   partition_by(unit, partition_by(ccy, seq[
//!       agg_net,       // whole unit+currency nets at aggregate
//!       exact_1to1,    // clean opposite-sign pairs of equal native amount
//!       signal_group,  // reference bridge: shared token buckets that net
//!       flow(model),   // engine arbitrates the ambiguous remainder
//!   ]))
//!
//! Sharding by currency makes each sub-problem single-currency, so the native
//! amount derived in `project` IS the conserved numeraire and FX never enters.

use florecon::export_plugin;
use florecon::sdk::{DescribeDoc, Field, Plugin, RowView};
use florecon::strategy::{
    Model, Strategy, agg_net, exact_1to1, flow, partition_by, seq, signal_group,
};
use florecon::token::fnv1a;

const TOL: i64 = 100; // 1.00 in native minor units
const CAP: usize = 256;

/// The typed match row, derived per ledger line.
#[derive(Clone)]
pub struct Row {
    unit: u64,        // hashed unordered {company, icp} (shard key)
    ccy: u64,         // hashed native currency (shard key; FX vanishes within)
    objsub: u64,      // hashed GL account (aggregation key)
    snative: i64,     // signed native amount, minor units (conserved per shard)
    gl_day: i64,      // GL date in epoch days (flow ordering)
    tokens: Vec<u64>, // hashed reference tokens (the cross-book bridge)
}

/// The flow arbiter for the ambiguous remainder.
#[derive(Clone)]
struct Interco {
    penalty: f64,
}

impl Model for Interco {
    type Tx = Row;
    fn base_amount(&self, tx: &Row) -> i64 {
        tx.snative
    }
    fn penalty(&self, _tx: &Row) -> f64 {
        self.penalty
    }
    fn block_key(&self, tx: &Row) -> i64 {
        tx.gl_day
    }
    fn window(&self) -> i64 {
        -1
    }
    fn match_keys(&self, tx: &Row) -> Vec<u64> {
        let mut k = tx.tokens.clone();
        if tx.snative != 0 {
            k.push(fnv1a(format!("AMT:{}", tx.snative.abs()).as_bytes()));
        }
        k
    }
    fn cost(&self, a: &Row, b: &Row) -> Option<f64> {
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

/// Reference tokenization: the cross-book bridge (filters boilerplate noise).
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
            let h = fnv1a(t.as_bytes());
            if !out.contains(&h) {
                out.push(h);
            }
        }
    }
    out
}

pub struct IntercoPlugin;

impl Plugin for IntercoPlugin {
    type Row = Row;

    fn new() -> Self {
        IntercoPlugin
    }

    fn describe() -> DescribeDoc {
        DescribeDoc::new("florecon.intercompany", "1.0.0").input(vec![
            Field::int("row_id"),
            Field::text("company"),
            Field::text("icp"),
            Field::text("objsub"),
            Field::float("indicative_usd_amt").primary(),
            Field::int("gl_date"),
            Field::text("base_currency"),
            Field::text("trx_currency"),
            Field::float("trx_amt"),
            Field::float("fc_amt"),
            Field::text("reference"),
            Field::text("reference2"),
            Field::text("description"),
            Field::text("name_remark_explanation"),
            Field::text("invoice_no"),
            Field::int("is_offset"),
        ])
    }

    fn id(&self, row: &RowView<'_>) -> u64 {
        row.i64("row_id") as u64 // the host's own stable ledger line id
    }

    fn project(&self, r: &RowView<'_>) -> Row {
        let co = r.str("company");
        let icp = r.str("icp");
        let inert = r.i64("is_offset") != 0 || co.is_empty() || icp.is_empty() || co == icp;

        // native amount: trx currency, falling back to base currency.
        let (trx, fc) = (r.f64("trx_amt"), r.f64("fc_amt"));
        let (ccy_s, amt) = if trx.abs() >= 0.005 {
            (r.str("trx_currency"), trx)
        } else {
            (r.str("base_currency"), fc)
        };
        let usd_cents = (r.f64("indicative_usd_amt") * 100.0).round() as i64;
        let sign = usd_cents.signum();
        let snative = if inert { 0 } else { (amt.abs() * 100.0).round() as i64 * sign };

        let mut pair = [co.to_string(), icp.to_string()];
        pair.sort();
        Row {
            unit: fnv1a(format!("{}|{}", pair[0], pair[1]).as_bytes()),
            ccy: fnv1a(ccy_s.as_bytes()),
            objsub: fnv1a(r.str("objsub").as_bytes()),
            snative,
            gl_day: r.i64("gl_date"),
            tokens: tokens(&[
                r.str("reference"),
                r.str("reference2"),
                r.str("description"),
                r.str("name_remark_explanation"),
                r.str("invoice_no"),
            ]),
        }
    }

    fn primary(row: &Row) -> i64 {
        row.snative // single currency per shard -> exact conservation, no FX
    }

    fn strategy(&self) -> Box<dyn Strategy<Row>> {
        partition_by(
            |t: &Row| t.unit,
            || {
                partition_by(
                    |t: &Row| t.ccy,
                    || {
                        seq(vec![
                            agg_net(|t: &Row| t.objsub, TOL),
                            exact_1to1(|_t: &Row| Some(0)),
                            signal_group(|t: &Row| t.tokens.clone(), TOL, CAP),
                            flow(Interco { penalty: 1000.0 }),
                        ])
                    },
                )
            },
        )
    }
}

export_plugin!(IntercoPlugin);

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Float64Array, Int64Array, StringArray};
    use arrow::datatypes::{DataType, Field as AField, Schema};
    use arrow::ipc::writer::StreamWriter;
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    fn sample_ipc() -> Vec<u8> {
        // Two clean opposite-sign intercompany pairs sharing a reference token.
        let row_id = Int64Array::from(vec![1, 2, 3, 4]);
        let company = StringArray::from(vec!["A", "B", "A", "B"]);
        let icp = StringArray::from(vec!["B", "A", "B", "A"]);
        let objsub = StringArray::from(vec!["100", "100", "200", "200"]);
        let usd = Float64Array::from(vec![100.0, -100.0, 50.0, -50.0]);
        let gl = Int64Array::from(vec![10, 11, 20, 22]);
        let bccy = StringArray::from(vec!["USD", "USD", "USD", "USD"]);
        let tccy = StringArray::from(vec!["USD", "USD", "USD", "USD"]);
        let trx = Float64Array::from(vec![100.0, 100.0, 50.0, 50.0]);
        let fc = Float64Array::from(vec![0.0, 0.0, 0.0, 0.0]);
        let reference = StringArray::from(vec!["INV-AAAA-1", "INV-AAAA-1", "INV-BBBB-2", "INV-BBBB-2"]);
        let blank = StringArray::from(vec!["", "", "", ""]);
        let is_off = Int64Array::from(vec![0, 0, 0, 0]);

        let schema = Schema::new(vec![
            AField::new("row_id", DataType::Int64, false),
            AField::new("company", DataType::Utf8, false),
            AField::new("icp", DataType::Utf8, false),
            AField::new("objsub", DataType::Utf8, false),
            AField::new("indicative_usd_amt", DataType::Float64, false),
            AField::new("gl_date", DataType::Int64, false),
            AField::new("base_currency", DataType::Utf8, false),
            AField::new("trx_currency", DataType::Utf8, false),
            AField::new("trx_amt", DataType::Float64, false),
            AField::new("fc_amt", DataType::Float64, false),
            AField::new("reference", DataType::Utf8, false),
            AField::new("reference2", DataType::Utf8, false),
            AField::new("description", DataType::Utf8, false),
            AField::new("name_remark_explanation", DataType::Utf8, false),
            AField::new("invoice_no", DataType::Utf8, false),
            AField::new("is_offset", DataType::Int64, false),
        ]);
        let batch = RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(row_id),
                Arc::new(company),
                Arc::new(icp),
                Arc::new(objsub),
                Arc::new(usd),
                Arc::new(gl),
                Arc::new(bccy),
                Arc::new(tccy),
                Arc::new(trx),
                Arc::new(fc),
                Arc::new(reference),
                Arc::new(blank.clone()),
                Arc::new(blank.clone()),
                Arc::new(blank.clone()),
                Arc::new(blank),
                Arc::new(is_off),
            ],
        )
        .unwrap();

        let mut buf = Vec::new();
        {
            let mut w = StreamWriter::try_new(&mut buf, &batch.schema()).unwrap();
            w.write(&batch).unwrap();
            w.finish().unwrap();
        }
        buf
    }

    #[test]
    fn conforms() {
        florecon::sdk::conformance::assert_conformance::<IntercoPlugin>(&sample_ipc());
    }

    #[test]
    fn pairs_net_clean() {
        use florecon::recon::Recon;
        use florecon::sdk::Table;
        let table = Table::from_ipc(&sample_ipc()).unwrap();
        let p = IntercoPlugin::new();
        let mut r = Recon::new(p.strategy(), IntercoPlugin::primary);
        for i in 0..table.len() {
            let rv = table.row(i);
            r.upsert(p.id(&rv), p.project(&rv));
        }
        r.solve().unwrap();
        let rep = r.report();
        // All four rows net to zero across two clean groups.
        let clean: i64 = rep.groups.iter().filter(|g| g.net == 0).map(|g| g.size as i64).sum();
        assert_eq!(clean, 4, "expected all four rows in net-zero groups: {rep:?}");
    }
}
