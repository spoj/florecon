"""Export a real bilateral book to web/data.json for the browser front-end.

Carries two views, joined by id:
  - rows    : the recon schema columns (numeric) shipped into the WASM engine
  - display : human-readable fields the UI renders (never crosses into wasm)

  python python/export_web.py [parquet] [--pair COMPANY ICP] [--max N]
"""

import pathlib
import sys
import json
import pyarrow.parquet as pq

# Use the one canonical host — the wheel package under py/src — rather than a
# second copy. Pinned ahead of any installed build so dev edits are live.
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "py" / "src"))
from florecon import KEY, NUMBER, TOKENS, col, key

OUT = "web/data.json"
FIELDS = ["reference", "reference2", "description", "name_remark_explanation", "invoice_no"]
# Business-legible source columns the UI renders. Optional ones (doc_company,
# doc_type) are tolerated if the book does not carry them.
CORE = ["company", "icp", "source_policy", "account_description", "objsub",
        "indicative_usd_amt", "base_currency", "trx_currency", "trx_amt", "fc_amt",
        "gl_date", "is_offset", "doc_no"]
OPTIONAL = ["doc_company", "doc_type"]
COLS = CORE + OPTIONAL + FIELDS


def plan():
    leg = {"op": "seq", "steps": [
        {"op": "agg_net", "key": "objsub", "tol": 100},
        {"op": "exact"},
        {"op": "signal", "signals": "tokens", "tol": 100, "cap": 256},
        {"op": "flow", "day": "day",
         "tokens": "tokens", "penalty": 1000.0, "window": -1},
    ]}
    return {"op": "partition", "by": "unit",
            "inner": {"op": "partition", "by": "ccy", "inner": leg}}


def fields_spec():
    # Portable display descriptor: the UI renders slicers and detail columns
    # from this, so a different book only has to ship a different list.
    return [
        {"key": "date", "label": "gl date", "kind": "date", "slicer": False, "detail": True},
        {"key": "gl_co", "label": "gl co", "kind": "dim", "slicer": True, "detail": True},
        {"key": "doc_co", "label": "doc co", "kind": "dim", "slicer": True, "detail": True},
        {"key": "policy", "label": "policy", "kind": "dim", "slicer": True, "detail": True},
        {"key": "doc_type", "label": "doc type", "kind": "dim", "slicer": True, "detail": True},
        {"key": "doc", "label": "doc no", "kind": "text", "slicer": False, "detail": True},
        {"key": "account", "label": "account", "kind": "dim", "slicer": True, "detail": True},
        {"key": "ccy", "label": "ccy", "kind": "dim", "slicer": True, "detail": False},
        {"key": "trx", "label": "trx", "kind": "amount", "ccy": "trx_ccy", "amt": "trx_amt", "detail": True},
        {"key": "base", "label": "base", "kind": "amount", "ccy": "base_ccy", "amt": "base_amt", "detail": True},
        {"key": "usd", "label": "usd", "kind": "amount", "amt": "usd", "ccy": None, "detail": True, "value": True},
        {"key": "ref", "label": "reference", "kind": "text", "slicer": False, "detail": True},
    ]


def main():
    args = sys.argv[1:]
    pair, maxrows, positional, i = None, None, [], 0
    while i < len(args):
        if args[i] == "--pair":
            pair = frozenset((args[i + 1], args[i + 2])); i += 3
        elif args[i] == "--max":
            maxrows = int(args[i + 1]); i += 2
        else:
            positional.append(args[i]); i += 1
    path = positional[0] if positional else "data/ledger.parquet"

    have = set(pq.ParquetFile(path).schema.names)
    read_cols = [c for c in COLS if c in have]
    t = pq.read_table(path, columns=read_cols)
    cols = {n: t.column(n).to_pylist() for n in read_cols}
    for missing in COLS:
        cols.setdefault(missing, [None] * t.num_rows)
    if pair is None:
        # Default to the busiest bilateral pair in the data (no hardcoded ids).
        from collections import Counter

        cnt: Counter = Counter()
        for co, icp in zip(cols["company"], cols["icp"]):
            if co and icp and co != icp:
                cnt[frozenset((co, icp))] += 1
        pair = cnt.most_common(1)[0][0]
    schema = [
        col("unit", KEY), col("ccy", KEY), col("day", NUMBER),
        col("objsub", KEY), col("native", NUMBER), col("tokens", TOKENS),
    ]
    rows, display = [], []
    for k in range(t.num_rows):
        if cols["is_offset"][k]:
            continue
        co, icp = cols["company"][k] or "", cols["icp"][k] or ""
        if not co or not icp or co == icp or frozenset((co, icp)) != pair:
            continue
        usd = cols["indicative_usd_amt"][k] or 0.0
        trx = cols["trx_amt"][k] or 0.0
        if abs(trx) >= 0.005:
            ccy_s, amt = cols["trx_currency"][k] or "", trx
        else:
            ccy_s, amt = cols["base_currency"][k] or "", cols["fc_amt"][k] or 0.0
        usd_cents = round(usd * 100.0)
        sign = (usd_cents > 0) - (usd_cents < 0)
        native_cents = round(abs(amt) * 100.0) * sign
        gl = cols["gl_date"][k]
        gl_day = gl.toordinal() - 719163 if gl else 0
        rid = len(rows)
        # Bare cells, positional against `schema`. The engine lowers strings
        # (FNV-1a) by column kind; we ship business values, not hashes.
        text = " ".join(s for s in (cols[f][k] for f in FIELDS) if s)
        rows.append([rid, [
            key(co, icp),                # unit: composite bilateral key
            ccy_s,                       # ccy: categorical key
            gl_day,                      # day: genuine integer
            cols["objsub"][k] or "",     # objsub: categorical key
            native_cents,                # native: money, minor units
            text,                        # tokens: free text
        ]])
        ref = " ".join(s for s in (cols["reference"][k], cols["reference2"][k],
                                   cols["description"][k]) if s).strip()
        base_ccy = cols["base_currency"][k] or ""
        base_amt = cols["fc_amt"][k] or 0.0
        trx_ccy = cols["trx_currency"][k] or base_ccy
        trx_amt = cols["trx_amt"][k] or 0.0
        if abs(trx_amt) < 0.005:
            trx_ccy, trx_amt = base_ccy, base_amt
        display.append({
            "id": rid,
            "co": co, "icp": icp,
            "gl_co": co,
            "doc_co": cols["doc_company"][k] or co,
            "doc_type": cols["doc_type"][k] or "",
            "policy": cols["source_policy"][k] or "?",
            "ccy": ccy_s,
            "native": native_cents,
            "usd": usd_cents,
            "trx_ccy": trx_ccy,
            "trx_amt": round(trx_amt * 100.0),
            "base_ccy": base_ccy,
            "base_amt": round(base_amt * 100.0),
            "date": str(gl) if gl else "",
            "account": (cols["account_description"][k] or "")[:40],
            "ref": ref[:80],
            "doc": cols["doc_no"][k] or "",
        })
        if maxrows and len(rows) >= maxrows:
            break

    out = {"pair": " / ".join(sorted(pair)), "schema": {"cols": schema, "token_drop": ["OFFSETENTRY"]},
           "plan": {"primary": "native", "root": plan()}, "fields": fields_spec(), "rows": rows, "display": display}
    import os
    os.makedirs("web", exist_ok=True)
    with open(OUT, "w") as f:
        json.dump(out, f)
    print(f"wrote {OUT}: {len(rows)} rows, pair {out['pair']}")


if __name__ == "__main__":
    main()
