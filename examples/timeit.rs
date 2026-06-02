//! Throwaway profiler: replay a JSON SolveRequest (the web wire payload) and
//! time the cascade. Run: FLORECON_TIME=1 cargo run --release --example timeit -- web/data.json
use florecon::plan::SolveRequest;
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "web/data.json".into());
    let raw = std::fs::read_to_string(&path).expect("read json");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("parse json");
    // web/data.json wraps schema/plan/rows alongside display; lift them out.
    let req = serde_json::json!({
        "schema": v["schema"], "plan": v["plan"], "rows": v["rows"],
    });
    let req: SolveRequest = serde_json::from_value(req).expect("SolveRequest");
    let n = req.rows.len();
    // warm any allocator effects, then take the best of a few runs
    let mut best = f64::INFINITY;
    let mut report = None;
    for _ in 0..3 {
        let r = req.clone();
        let t = Instant::now();
        let rep = r.run().expect("solve");
        best = best.min(t.elapsed().as_secs_f64() * 1000.0);
        report = Some(rep);
    }
    let rep = report.unwrap();
    eprintln!("rows {n}  groups {}  residual {}  best {:.1} ms", rep.groups.len(), rep.residual.len(), best);
}
