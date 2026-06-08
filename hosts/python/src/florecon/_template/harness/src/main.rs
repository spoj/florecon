//! The native author loop: load a CSV sample, run the plugin end to end, and
//! print the proposed groups plus a conservation check. No wasm, no Python —
//! this is the fast inner loop you iterate the strategy in.
//!
//!     cargo run --profile author -p harness -- data/sample.csv   (or: just author)
//!
//! It builds the same Arrow batch a real host ships and drives the plugin's
//! `describe()` + `project` + `strategy`, so what you see here is what the
//! shipped wasm produces (up to native-vs-wasm performance).

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, Int64Array, StringArray};
use arrow::datatypes::{DataType, Field as AField, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;

use florecon::Recon;
use florecon::report::Status;
use florecon::sdk::{FieldType, Plugin, Record, Table};

use solver::Solver;

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: harness <data.csv>");
        return ExitCode::FAILURE;
    };
    match run(&path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(path: &str) -> Result<(), String> {
    let ipc = csv_to_ipc::<Solver>(path)?;
    let doc = Solver::describe();
    let table = Table::from_ipc(&ipc, &doc)?;

    let plugin = Solver::new(Default::default());
    let mut recon = Recon::new(plugin.strategy(), Solver::primary);
    let mut input_net: i64 = 0;
    for i in 0..table.len() {
        let input = <Solver as Plugin>::Input::from_view(&table.row(i));
        let row = plugin.project(&input);
        input_net += Solver::primary(&row);
        recon.upsert(input.ext_id(), row);
    }
    // `solve` verifies conservation at the boundary and errors if it is ever
    // violated, so reaching past this line is itself the proof.
    recon.solve().map_err(|e| format!("solve failed: {e:?}"))?;
    let report = recon.report();

    // --- summarize -------------------------------------------------------
    let mut matched_groups = 0usize;
    let mut matched_rows = 0usize;
    let mut unmatched = 0usize;
    let mut by_origin: BTreeMap<String, (usize, usize)> = BTreeMap::new(); // origin -> (#groups, #rows)
    let mut allocated_net: i64 = 0;
    for g in &report.groups {
        let rows = g.size;
        let entry = by_origin.entry(g.origin.clone()).or_default();
        entry.0 += 1;
        entry.1 += rows;
        if g.status == Status::Proposed && g.size >= 2 {
            matched_groups += 1;
            matched_rows += rows;
        } else if g.size == 1 {
            unmatched += 1;
        }
    }
    for a in &report.allocations {
        allocated_net += a.amount;
    }

    println!("rows in:        {}", table.len());
    println!("matched:        {matched_groups} groups, {matched_rows} rows");
    println!("unmatched:      {unmatched} rows");
    println!("by origin:");
    for (origin, (groups, rows)) in &by_origin {
        println!("  {origin:<24} {groups:>4} groups  {rows:>5} rows");
    }
    let ok = input_net == allocated_net;
    println!(
        "conservation:   {} (input net {input_net} == allocated net {allocated_net})",
        if ok { "OK" } else { "VIOLATED" }
    );
    if !ok {
        return Err("conservation check failed".into());
    }
    Ok(())
}

/// Build an Arrow IPC stream from a CSV, typing each column by the plugin's
/// declared schema and matching CSV columns to declared fields **by name**.
fn csv_to_ipc<P: Plugin>(path: &str) -> Result<Vec<u8>, String> {
    let doc = P::describe();
    let mut rdr = csv::Reader::from_path(path).map_err(|e| format!("open {path}: {e}"))?;
    let headers = rdr.headers().map_err(|e| e.to_string())?.clone();
    let col_of = |name: &str| -> Result<usize, String> {
        headers
            .iter()
            .position(|h| h == name)
            .ok_or_else(|| format!("CSV is missing declared column {name:?}"))
    };
    let indices: Vec<usize> = doc
        .input
        .iter()
        .map(|f| col_of(&f.name))
        .collect::<Result<_, _>>()?;

    let records: Vec<csv::StringRecord> = rdr
        .records()
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;

    let mut fields = Vec::with_capacity(doc.input.len());
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(doc.input.len());
    for (f, &ci) in doc.input.iter().zip(&indices) {
        let cells = || records.iter().map(move |r| r.get(ci).unwrap_or(""));
        let (dt, arr): (DataType, ArrayRef) = match f.ty {
            FieldType::I64 => {
                let v: Vec<i64> = cells()
                    .map(|c| c.trim().parse::<i64>().unwrap_or(0))
                    .collect();
                (DataType::Int64, Arc::new(Int64Array::from(v)))
            }
            FieldType::F64 => {
                let v: Vec<f64> = cells()
                    .map(|c| c.trim().parse::<f64>().unwrap_or(0.0))
                    .collect();
                (DataType::Float64, Arc::new(Float64Array::from(v)))
            }
            FieldType::Utf8 => {
                let v: Vec<String> = cells().map(|c| c.to_string()).collect();
                (DataType::Utf8, Arc::new(StringArray::from(v)))
            }
        };
        fields.push(AField::new(&f.name, dt, true));
        arrays.push(arr);
    }

    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), arrays).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    {
        let mut w = StreamWriter::try_new(&mut buf, &schema).map_err(|e| e.to_string())?;
        w.write(&batch).map_err(|e| e.to_string())?;
        w.finish().map_err(|e| e.to_string())?;
    }
    Ok(buf)
}
