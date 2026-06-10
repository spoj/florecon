//! Intercompany reconciliation as a florecon plugin.
//!
//! The host ships a columnar ledger (one Arrow table). The `Ledger` record —
//! `#[derive(Record)]` — is the single source of truth for the input schema,
//! the typed projection, and identity. This plugin bakes in the preprocessing
//! (FX selection, reference tokenization) and the matching cascade, then
//! `export_plugin!` emits the self-describing wasm.
//!
//!   partition_by(unit, partition_by(ccy, seq[
//!       agg_net,       // whole unit+currency nets at aggregate
//!       exact_1to1,    // clean opposite-sign pairs of equal native amount
//!       signal_group,  // reference bridge: shared token buckets that net
//!       flow(spec),    // engine arbitrates the ambiguous remainder
//!   ]))
//!
//! Sharding by currency makes each sub-problem single-currency, so the native
//! amount derived in `project` IS the conserved numeraire and FX never enters.

use florecon::export_plugin;
use florecon::sdk::{Domain, Plugin, Record};
use florecon::strategy::{
    FlowSpec, Item, Strategy, agg_net, coalesce, exact_1to1, flow, partition_by, seq, signal_group,
};
use florecon::token::fnv1a;

const TOL: i64 = 100; // 1.00 in native minor units
const CAP: usize = 256;

/// The raw ledger line the host ships: schema + projection + identity in one.
#[derive(Record)]
pub struct Ledger {
    #[record(id)]
    row_id: i64,
    company: String,
    icp: String,
    objsub: String,
    #[record(amount)]
    indicative_usd_amt: f64,
    gl_date: i64,
    base_currency: String,
    trx_currency: String,
    trx_amt: f64,
    fc_amt: f64,
    reference: String,
    reference2: String,
    description: String,
    name_remark_explanation: String,
    invoice_no: String,
    is_offset: i64,
}

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

/// The flow arbiter for the ambiguous remainder, as a [`FlowSpec`].
fn interco_spec(penalty: f64) -> FlowSpec<Row> {
    FlowSpec::new()
        .window(-1)
        .penalty(penalty)
        .block_key(|tx: &Row| tx.gl_day)
        .match_keys(|tx: &Row| {
            let mut k = tx.tokens.clone();
            if tx.snative != 0 {
                k.push(fnv1a(format!("AMT:{}", tx.snative.abs()).as_bytes()));
            }
            k
        })
        .cost(|a: &Row, b: &Row| {
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
        })
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
            if t.len() < 6
                || t.len() > 40
                || t == "OFFSETENTRY"
                || t.chars().all(|c| c.is_alphabetic())
            {
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
    type Input = Ledger;
    type Row = Row;
    type Config = ();

    fn domain() -> Domain {
        Domain::new("florecon.intercompany", "1.0.0")
    }

    fn new(_config: ()) -> Self {
        IntercoPlugin
    }

    fn project(&self, r: &Ledger) -> Row {
        let inert =
            r.is_offset != 0 || r.company.is_empty() || r.icp.is_empty() || r.company == r.icp;

        // native amount: trx currency, falling back to base currency.
        let (trx, fc) = (r.trx_amt, r.fc_amt);
        let (ccy_s, amt) = if trx.abs() >= 0.005 {
            (r.trx_currency.as_str(), trx)
        } else {
            (r.base_currency.as_str(), fc)
        };
        let usd_cents = (r.indicative_usd_amt * 100.0).round() as i64;
        let sign = usd_cents.signum();
        let snative = if inert {
            0
        } else {
            (amt.abs() * 100.0).round() as i64 * sign
        };

        let mut pair = [r.company.clone(), r.icp.clone()];
        pair.sort();
        Row {
            unit: fnv1a(format!("{}|{}", pair[0], pair[1]).as_bytes()),
            ccy: fnv1a(ccy_s.as_bytes()),
            objsub: fnv1a(r.objsub.as_bytes()),
            snative,
            gl_day: r.gl_date,
            tokens: tokens(&[
                r.reference.as_str(),
                r.reference2.as_str(),
                r.description.as_str(),
                r.name_remark_explanation.as_str(),
                r.invoice_no.as_str(),
            ]),
        }
    }

    fn primary(row: &Row) -> i64 {
        row.snative // single currency per shard -> exact conservation, no FX
    }

    fn strategy(&self) -> Box<dyn Strategy<Row>> {
        partition_by(
            |t: &Item<Row>| t.data.unit,
            |_| {
                partition_by(
                    |t: &Item<Row>| t.data.ccy,
                    |_| {
                        seq(vec![
                            agg_net(|t: &Item<Row>| t.data.objsub, |g| g.net().abs() <= TOL),
                            exact_1to1(|_t: &Item<Row>| Some(0)),
                            signal_group(
                                |t: &Item<Row>| t.data.tokens.clone(),
                                |g| g.net().abs() <= TOL,
                                CAP,
                            ),
                            coalesce("flow", flow(interco_spec(1000.0))),
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
    use florecon::sdk::{DescribeDoc, Field, Table};
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
        let reference =
            StringArray::from(vec!["INV-AAAA-1", "INV-AAAA-1", "INV-BBBB-2", "INV-BBBB-2"]);
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
        let table = Table::from_ipc(&sample_ipc(), &IntercoPlugin::describe()).unwrap();
        let p = IntercoPlugin::new(());
        let mut r = Recon::new(p.strategy(), IntercoPlugin::primary);
        for i in 0..table.len() {
            let input = Ledger::from_view(&table.row(i));
            r.upsert(input.ext_id(), p.project(&input));
        }
        r.solve().unwrap();
        let rep = r.report();
        // All four rows net to zero across two clean groups.
        let clean: i64 = rep
            .groups
            .iter()
            .filter(|g| g.net == 0)
            .map(|g| g.size as i64)
            .sum();
        assert_eq!(
            clean, 4,
            "expected all four rows in net-zero groups: {rep:?}"
        );
    }

    #[test]
    fn missing_declared_column_errors() {
        let doc = DescribeDoc::new("x", "1").input(vec![Field::int("does_not_exist")]);
        assert!(Table::from_ipc(&sample_ipc(), &doc).is_err());
    }

    #[test]
    fn wrong_typed_column_errors() {
        // row_id is shipped as Int64; declaring it text must fail at ingest.
        let doc = DescribeDoc::new("x", "1").input(vec![Field::text("row_id")]);
        assert!(Table::from_ipc(&sample_ipc(), &doc).is_err());
    }

    #[test]
    #[should_panic(expected = "undeclared column")]
    fn undeclared_access_panics() {
        let doc = DescribeDoc::new("x", "1").input(vec![Field::int("row_id")]);
        let t = Table::from_ipc(&sample_ipc(), &doc).unwrap();
        let _ = t.row(0).i64("company"); // declared nowhere -> loud panic, not 0
    }
}
