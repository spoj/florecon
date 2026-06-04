"""Bind real intercompany parquet to the florecon WASM engine via wasmtime."""
import pathlib
import sys
import time
import pyarrow as pa
import pyarrow.parquet as pq
import json

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "py" / "src"))
from florecon import Florecon, strict_assignments

WASM = "target/wasm32-unknown-unknown/release/florecon.wasm"
FIELDS = ["reference", "reference2", "description", "name_remark_explanation", "invoice_no"]
COLS = ["company", "icp", "objsub", "indicative_usd_amt", "gl_date", "base_currency",
        "trx_currency", "trx_amt", "fc_amt", "is_offset"] + FIELDS

def fnv1a(text):
    hash_val = 0xcbf29ce484222325
    for b in text.encode('utf-8'):
        hash_val ^= b
        hash_val = (hash_val * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
    return hash_val

def ingest(path, pair=None, maxrows=None):
    t = pq.read_table(path, columns=COLS)
    cols = {n: t.column(n).to_pylist() for n in COLS}
    n = t.num_rows

    ids, units, ccys, days, objsubs, natives, tokens_list = [], [], [], [], [], [], []
    usd_by_id = []

    for i in range(n):
        if cols["is_offset"][i]: continue
        co, icp = cols["company"][i] or "", cols["icp"][i] or ""
        if not co or not icp or co == icp: continue
        if pair and frozenset((co, icp)) != pair: continue

        usd = cols["indicative_usd_amt"][i] or 0.0
        trx = cols["trx_amt"][i] or 0.0
        if abs(trx) >= 0.005:
            ccy_s, amt = cols["trx_currency"][i] or "", trx
        else:
            ccy_s, amt = cols["base_currency"][i] or "", cols["fc_amt"][i] or 0.0

        usd_cents = round(usd * 100.0)
        sign = (usd_cents > 0) - (usd_cents < 0)
        snative = round(abs(amt) * 100.0) * sign
        gl = cols["gl_date"][i]
        gl_day = gl.toordinal() - 719163 if gl else 0

        rid = len(ids)
        usd_by_id.append(usd_cents)

        text = " ".join(s for s in (cols[f][i] for f in FIELDS) if s)

        ids.append(rid)
        units.append(fnv1a(f"{co}|{icp}"))
        ccys.append(fnv1a(ccy_s))
        days.append(gl_day)
        objsubs.append(fnv1a(cols["objsub"][i] or ""))
        natives.append(snative)
        tokens_list.append(text)

        if maxrows and len(ids) >= maxrows:
            break

    # Build Arrow RecordBatch. The reference text is sent raw utf8; the engine
    # tokenizes and hashes it itself (no pre-hashed token lists on the wire).
    batch = pa.RecordBatch.from_arrays(
        [
            pa.array(ids, type=pa.uint64()),
            pa.array(units, type=pa.int64()),
            pa.array(ccys, type=pa.int64()),
            pa.array(days, type=pa.int64()),
            pa.array(objsubs, type=pa.int64()),
            pa.array(natives, type=pa.int64()),
            pa.array(tokens_list, type=pa.string())
        ],
        names=["id", "unit", "ccy", "day", "objsub", "native", "tokens"]
    )

    sink = pa.BufferOutputStream()
    with pa.ipc.new_stream(sink, batch.schema) as writer:
        writer.write_batch(batch)
    arrow_bytes = sink.getvalue().to_pybytes()

    return arrow_bytes, usd_by_id, len(ids)

def plan():
    leg = {"op": "seq", "steps": [
        {"op": "agg_net", "key": "objsub", "tol": 100},
        {"op": "exact"},
        {"op": "signal", "signals": "tokens", "tol": 100, "cap": 256},
        {"op": "flow", "day": "day", "tokens": "tokens", "penalty": 1000.0, "window": -1},
    ]}
    return {"primary": "native", "root": {"op": "partition", "by": "unit", "inner": {"op": "partition", "by": "ccy", "inner": leg}}}

def main():
    args = sys.argv[1:]
    pair = None
    maxrows = None
    positional = []
    i = 0
    while i < len(args):
        if args[i] == "--pair":
            pair = frozenset((args[i + 1], args[i + 2]))
            i += 3
        elif args[i] == "--max":
            maxrows = int(args[i + 1])
            i += 2
        else:
            positional.append(args[i])
            i += 1
    path = positional[0] if positional else "data/ledger.parquet"

    if not pathlib.Path(path).exists():
        print(f"no parquet at {path}; skipping (pass a path to run against real data)")
        return

    t0 = time.time()
    arrow_bytes, usd_by_id, num_rows = ingest(path, pair, maxrows)
    print(f"ingested {num_rows} rows in {time.time()-t0:.2f}s")

    fe = Florecon(WASM)
    # Stateless batch solve = `init` (plan + rows) then `solve`, over a
    # workspace we discard. Column identity comes from the Arrow batch schema.
    t1 = time.time()
    env = fe.dispatch({"op": "init", "plan": plan()}, arrow_bytes)
    if not env["ok"]:
        print("ERROR:", env["error"]); return
    env = fe.dispatch({"op": "solve"})
    dt = time.time() - t1
    if not env["ok"]:
        print("ERROR:", env["error"]); return
    rep = env["report"]
    print("SOLVED", dt, "seconds")
    
if __name__ == "__main__":
    main()
