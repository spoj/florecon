"""Bind real intercompany parquet to the florecon WASM engine via wasmtime.

Replicates examples/interco.rs ingestion in Python, ships one JSON SolveRequest
into WASM, and reports the same metrics from the returned Report. State and all
computation live in WASM; Python only generates the plan + rows and reads back
the partition.

  python python/run_interco.py [parquet] [--pair COMPANY ICP] [--max N]
"""

import sys
import time
import pyarrow.parquet as pq
from florecon import Florecon

WASM = "target/wasm32-unknown-unknown/release/florecon.wasm"
FIELDS = ["reference", "reference2", "description", "name_remark_explanation", "invoice_no"]
COLS = ["company", "icp", "objsub", "indicative_usd_amt", "gl_date", "base_currency",
        "trx_currency", "trx_amt", "fc_amt", "is_offset"] + FIELDS


def fnv1a(s: str) -> int:
    h = 0xCBF29CE484222325
    for b in s.encode("utf-8"):
        h ^= b
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


def tokens(parts) -> list:
    out = []
    for field in parts:
        if not field:
            continue
        for raw in field.split():
            t = "".join(c for c in raw if c.isalnum()).upper()
            if len(t) < 6 or len(t) > 40 or t == "OFFSETENTRY" or t.isalpha():
                continue
            h = fnv1a(t)
            if h not in out:
                out.append(h)
    return out


def ingest(path, pair=None, maxrows=None):
    t = pq.read_table(path, columns=COLS)
    cols = {n: t.column(n).to_pylist() for n in COLS}
    n = t.num_rows
    schema = ["unit", "ccy", "day", "objsub", "native", "tokens"]
    rows, usd_by_id = [], []
    for i in range(n):
        if cols["is_offset"][i]:
            continue
        co, icp = cols["company"][i] or "", cols["icp"][i] or ""
        if not co or not icp or co == icp:
            continue
        if pair and frozenset((co, icp)) != pair:
            continue
        lo, hi = sorted((co, icp))
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
        gl_day = gl.toordinal() - 719163 if gl else 0  # days since 1970-01-01
        rid = len(rows)
        usd_by_id.append(usd_cents)
        rows.append([rid, {"values": [
            {"Int": fnv1a(f"{lo}|{hi}") & 0x7FFFFFFFFFFFFFFF},
            {"Int": fnv1a(ccy_s) & 0x7FFFFFFFFFFFFFFF},
            {"Int": gl_day},
            {"Int": fnv1a(cols["objsub"][i] or "") & 0x7FFFFFFFFFFFFFFF},
            {"Int": snative},
            {"Tokens": tokens([cols[f][i] for f in FIELDS])},
        ]}])
        if maxrows and len(rows) >= maxrows:
            break
    return schema, rows, usd_by_id


def plan():
    leg = {"op": "seq", "steps": [
        {"op": "agg_net", "key": "objsub", "amount": "native", "tol": 100},
        {"op": "exact", "amount": "native"},
        {"op": "signal", "signals": "tokens", "amount": "native", "tol": 100, "cap": 256},
        {"op": "flow", "amount": "native", "day": "day", "native": "native",
         "tokens": "tokens", "penalty": 1000.0, "window": -1},
    ]}
    return {"op": "partition", "by": "unit",
            "inner": {"op": "partition", "by": "ccy", "inner": leg}}


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

    t0 = time.time()
    schema, rows, usd_by_id = ingest(path, pair, maxrows)
    print(f"ingested {len(rows)} rows in {time.time()-t0:.2f}s")

    fe = Florecon(WASM)
    req = {"schema": {"cols": schema}, "rows": rows, "plan": plan()}
    t1 = time.time()
    env = fe.solve(req)
    dt = time.time() - t1
    if not env["ok"]:
        print("ERROR:", env["error"]); return
    rep = env["report"]

    total = len(rows)
    total_value = sum(abs(v) for v in usd_by_id)
    matched_ids = {a[0] for a in rep["assignments"]}
    matched_value = sum(abs(usd_by_id[i]) for i in matched_ids)
    clean = sum(1 for g in rep["groups"] if abs(g["net"]) <= 100)
    by_origin = {}
    for g in rep["groups"]:
        c, r = by_origin.get(g["origin"], (0, 0))
        by_origin[g["origin"]] = (c + 1, r + g["size"])

    print("\n=== florecon via WASM (wasmtime) ===")
    print(f"  rows            : {total}")
    print(f"  matched rows    : {len(matched_ids)} ({100*len(matched_ids)/max(total,1):.1f}% by count)")
    print(f"  matched value   : {matched_value/100:.0f} of {total_value/100:.0f} usd "
          f"({100*matched_value/max(total_value,1):.1f}% by value)")
    print(f"  groups          : {len(rep['groups'])} ({clean} clean)")
    for origin in sorted(by_origin):
        c, r = by_origin[origin]
        print(f"    {origin:<13} {c:>7} groups  {r:>8} rows")
    print(f"  residual rows   : {len(rep['residual'])}")
    print(f"  wasm solve time : {dt:.2f}s")
    # Conservation is enforced inside solve(); echo the partition identity.
    print(f"  conservation    : {len(matched_ids)} + {len(rep['residual'])} == {total} "
          f"-> {len(matched_ids)+len(rep['residual'])==total}")


if __name__ == "__main__":
    main()
