//! Golden wire vectors — the cross-language contract for the florecon ABI.
//!
//! This drives the **real** dispatch path (`sdk::abi::dispatch`, the same code
//! the wasm export wraps) over the interco plugin through a canonical command
//! script, and pins the exact `(cmd, arrow) -> envelope` triples under
//! `golden/`. The Rust side self-verifies here; the Python host replays the
//! identical fixtures (`hosts/python/golden_replay.py`) against the wasm. Any
//! drift in the wire shape — a renamed field, a changed status string, a moved
//! error code — breaks one of the two, so the contract cannot silently rot.
//!
//! Regenerate after an intentional wire change:
//!
//!     UPDATE_GOLDEN=1 cargo test -p interco-plugin --test golden
//!
//! The committed `golden/` is then the source of truth every host is checked
//! against.

use std::path::PathBuf;
use std::sync::Arc;

use arrow::array::{Float64Array, Int64Array, StringArray};
use arrow::datatypes::{DataType, Field as AField, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use serde_json::{Value, json};

use florecon::sdk::Session;
use interco_plugin::IntercoPlugin;

/// One raw ledger line, in the columns the plugin declares. The constant
/// columns (currencies, fc, free-text) are filled in by [`ipc`].
struct Row {
    id: i64,
    co: &'static str,
    icp: &'static str,
    objsub: &'static str,
    usd: f64,
    gl: i64,
    reference: &'static str,
}

fn row(
    id: i64,
    co: &'static str,
    icp: &'static str,
    objsub: &'static str,
    usd: f64,
    gl: i64,
    reference: &'static str,
) -> Row {
    Row {
        id,
        co,
        icp,
        objsub,
        usd,
        gl,
        reference,
    }
}

/// Encode rows as an Arrow IPC stream over the interco input schema.
fn ipc(rows: &[Row]) -> Vec<u8> {
    let n = rows.len();
    let blank = StringArray::from(vec![""; n]);
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
            Arc::new(Int64Array::from(
                rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.co).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.icp).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.objsub).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|r| r.usd).collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                rows.iter().map(|r| r.gl).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(vec!["USD"; n])),
            Arc::new(StringArray::from(vec!["USD"; n])),
            Arc::new(Float64Array::from(
                rows.iter().map(|r| r.usd.abs()).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(vec![0.0; n])),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.reference).collect::<Vec<_>>(),
            )),
            Arc::new(blank.clone()),
            Arc::new(blank.clone()),
            Arc::new(blank.clone()),
            Arc::new(blank),
            Arc::new(Int64Array::from(vec![0i64; n])),
        ],
    )
    .unwrap();

    let mut buf = Vec::new();
    let mut w = StreamWriter::try_new(&mut buf, &batch.schema()).unwrap();
    w.write(&batch).unwrap();
    w.finish().unwrap();
    buf
}

/// Sort a report so two hosts that build groups in a different internal order
/// still compare equal: groups by id, allocations by (group, id).
fn normalize(mut env: Value) -> Value {
    if let Some(report) = env.get_mut("report").and_then(|v| v.as_object_mut()) {
        if let Some(groups) = report.get_mut("groups").and_then(|v| v.as_array_mut()) {
            groups.sort_by_key(|g| g["group_id"].as_u64().unwrap_or(0));
        }
        if let Some(allocs) = report.get_mut("allocations").and_then(|v| v.as_array_mut()) {
            allocs.sort_by_key(|a| {
                (
                    a["group_id"].as_u64().unwrap_or(0),
                    a["id"].as_u64().unwrap_or(0),
                )
            });
        }
    }
    env
}

/// The group id currently holding row `id` (deterministic: `next_id` is monotonic).
fn gid_of(report: &Value, id: u64) -> u64 {
    report["report"]["allocations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"].as_u64() == Some(id))
        .map(|a| a["group_id"].as_u64().unwrap())
        .unwrap_or_else(|| panic!("row {id} not allocated in {report}"))
}

/// One recorded vector: the command, the arrow fixture file (if any), and the
/// normalized envelope the plugin returns.
struct Step {
    name: &'static str,
    cmd: Value,
    arrow: Option<Vec<u8>>,
    envelope: Value,
}

/// Drive the canonical script against a fresh session, recording every step.
fn run_script() -> Vec<Step> {
    let mut slot: Option<Session<IntercoPlugin>> = None;
    let mut steps = Vec::new();
    let mut last = Value::Null;

    let mut step = |slot: &mut Option<Session<IntercoPlugin>>,
                    last: &mut Value,
                    name: &'static str,
                    cmd: Value,
                    arrow: Option<Vec<u8>>| {
        let cmd_bytes = serde_json::to_vec(&cmd).unwrap();
        let arrow_bytes = arrow.clone().unwrap_or_default();
        let out = florecon::sdk::abi::dispatch::<IntercoPlugin>(slot, &cmd_bytes, &arrow_bytes);
        let env = normalize(serde_json::from_slice(&out).unwrap());
        if env.get("report").is_some() {
            *last = env.clone();
        }
        steps.push(Step {
            name,
            cmd,
            arrow,
            envelope: env,
        });
    };

    let r = |id, co, icp, obj, usd, gl, rf| row(id, co, icp, obj, usd, gl, rf);

    step(
        &mut slot,
        &mut last,
        "init",
        json!({"op": "init"}),
        Some(ipc(&[
            r(1, "A", "B", "100", 100.0, 10, "INV-AAAA-1"),
            r(2, "B", "A", "100", -100.0, 11, "INV-AAAA-1"),
        ])),
    );
    step(&mut slot, &mut last, "solve", json!({"op": "solve"}), None);
    step(
        &mut slot,
        &mut last,
        "upsert",
        json!({"op": "upsert"}),
        Some(ipc(&[
            r(3, "A", "B", "200", 50.0, 20, "INV-BBBB-2"),
            r(4, "B", "A", "200", -50.0, 22, "INV-BBBB-2"),
        ])),
    );
    step(&mut slot, &mut last, "solve", json!({"op": "solve"}), None);
    step(
        &mut slot,
        &mut last,
        "remove",
        json!({"op": "remove", "ids": [4]}),
        None,
    );
    step(&mut slot, &mut last, "solve", json!({"op": "solve"}), None);
    step(
        &mut slot,
        &mut last,
        "pin_clean",
        json!({"op": "pin", "by": "clean", "tol": 0}),
        None,
    );
    step(&mut slot, &mut last, "solve", json!({"op": "solve"}), None);

    // The typed-error vector: dissolving a pinned group must be refused.
    let pinned = gid_of(&last, 1);
    step(
        &mut slot,
        &mut last,
        "dissolve_pinned_err",
        json!({"op": "dissolve", "group_id": pinned}),
        None,
    );

    step(
        &mut slot,
        &mut last,
        "readd",
        json!({"op": "upsert"}),
        Some(ipc(&[r(4, "B", "A", "200", -50.0, 22, "INV-BBBB-2")])),
    );
    step(&mut slot, &mut last, "solve", json!({"op": "solve"}), None);
    let g34 = gid_of(&last, 3);
    step(
        &mut slot,
        &mut last,
        "dissolve",
        json!({"op": "dissolve", "group_id": g34}),
        None,
    );
    step(
        &mut slot,
        &mut last,
        "report",
        json!({"op": "report"}),
        None,
    );

    steps
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../golden")
}

fn pretty(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).unwrap();
    s.push('\n');
    s
}

#[test]
fn golden_vectors() {
    let dir = golden_dir();
    let steps = run_script();
    let describe: Value =
        serde_json::from_slice(&florecon::sdk::abi::describe_json::<IntercoPlugin>()).unwrap();

    // The manifest: every step's command, arrow fixture name, and expected
    // normalized envelope. Arrow payloads are written beside it as binaries.
    let manifest = json!({
        "abi_version": 1,
        "describe": describe,
        "steps": steps.iter().enumerate().map(|(i, s)| {
            let arrow = s.arrow.as_ref().map(|_| format!("step-{i:02}.arrow"));
            json!({"name": s.name, "cmd": s.cmd, "arrow": arrow, "expect": s.envelope})
        }).collect::<Vec<_>>(),
    });

    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::create_dir_all(&dir).unwrap();
        // Clear stale arrow fixtures so a shorter script never leaves orphans.
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            if e.path().extension().is_some_and(|x| x == "arrow") {
                std::fs::remove_file(e.path()).unwrap();
            }
        }
        for (i, s) in steps.iter().enumerate() {
            if let Some(bytes) = &s.arrow {
                std::fs::write(dir.join(format!("step-{i:02}.arrow")), bytes).unwrap();
            }
        }
        std::fs::write(dir.join("vectors.json"), pretty(&manifest)).unwrap();
        eprintln!("wrote {} golden vectors to {}", steps.len(), dir.display());
        return;
    }

    let want: Value = serde_json::from_slice(
        &std::fs::read(dir.join("vectors.json")).unwrap_or_else(|_| {
            panic!("missing golden/vectors.json — regenerate with UPDATE_GOLDEN=1")
        }),
    )
    .unwrap();
    assert_eq!(
        want, manifest,
        "wire drift vs committed golden; regenerate with UPDATE_GOLDEN=1 if intentional"
    );

    // The committed arrow fixtures must be exactly what the script ships, since
    // the Python host replays those bytes, not freshly-built ones.
    for (i, s) in steps.iter().enumerate() {
        if let Some(bytes) = &s.arrow {
            let on_disk = std::fs::read(dir.join(format!("step-{i:02}.arrow"))).unwrap();
            assert_eq!(&on_disk, bytes, "arrow fixture step-{i:02} drifted");
        }
    }
}
