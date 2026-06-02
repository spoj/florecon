"""Export a real bilateral book to web/data.json for the browser front-end.

Carries two views, joined by id:
  - rows    : the recon schema columns (numeric) shipped into the WASM engine
  - display : human-readable fields the UI renders (never crosses into wasm)

  python python/export_web.py [parquet] [--pair COMPANY ICP] [--max N]
"""

import sys
import json
import pyarrow.parquet as pq

OUT = "web/data.json"
FIELDS = ["reference", "reference2", "description", "name_remark_explanation", "invoice_no"]
COLS = ["company", "icp", "source_policy", "account_description", "objsub",
        "indicative_usd_amt", "base_currency", "trx_currency", "trx_amt", "fc_amt",
        "gl_date", "is_offset", "doc_no"] + FIELDS


def fnv1a(s: str) -> int:
    h = 0xCBF29CE484222325
    for b in s.encode("utf-8"):
        h ^= b
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


def tokens(parts):
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
    pair, maxrows, positional, i = None, None, [], 0
    while i < len(args):
        if args[i] == "--pair":
            pair = frozenset((args[i + 1], args[i + 2])); i += 3
        elif args[i] == "--max":
            maxrows = int(args[i + 1]); i += 2
        else:
            positional.append(args[i]); i += 1
    path = positional[0] if positional else "data/ledger.parquet"

    t = pq.read_table(path, columns=COLS)
    cols = {n: t.column(n).to_pylist() for n in COLS}
    if pair is None:
        # Default to the busiest bilateral pair in the data (no hardcoded ids).
        from collections import Counter

        cnt: Counter = Counter()
        for co, icp in zip(cols["company"], cols["icp"]):
            if co and icp and co != icp:
                cnt[frozenset((co, icp))] += 1
        pair = cnt.most_common(1)[0][0]
    schema = ["unit", "ccy", "day", "objsub", "native", "tokens"]
    rows, display = [], []
    for k in range(t.num_rows):
        if cols["is_offset"][k]:
            continue
        co, icp = cols["company"][k] or "", cols["icp"][k] or ""
        if not co or not icp or co == icp or frozenset((co, icp)) != pair:
            continue
        lo, hi = sorted((co, icp))
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
        rows.append([rid, {"values": [
            {"Int": fnv1a(f"{lo}|{hi}") & 0x7FFFFFFFFFFFFFFF},
            {"Int": fnv1a(ccy_s) & 0x7FFFFFFFFFFFFFFF},
            {"Int": gl_day},
            {"Int": fnv1a(cols["objsub"][k] or "") & 0x7FFFFFFFFFFFFFFF},
            {"Int": native_cents},
            {"Tokens": tokens([cols[f][k] for f in FIELDS])},
        ]}])
        ref = " ".join(s for s in (cols["reference"][k], cols["reference2"][k],
                                   cols["description"][k]) if s).strip()
        display.append({
            "id": rid,
            "co": co, "icp": icp,
            "policy": cols["source_policy"][k] or "?",
            "ccy": ccy_s,
            "native": native_cents,
            "usd": usd_cents,
            "date": str(gl) if gl else "",
            "account": (cols["account_description"][k] or "")[:40],
            "ref": ref[:80],
            "doc": cols["doc_no"][k] or "",
        })
        if maxrows and len(rows) >= maxrows:
            break

    out = {"pair": " / ".join(sorted(pair)), "schema": {"cols": schema},
           "plan": plan(), "rows": rows, "display": display}
    import os
    os.makedirs("web", exist_ok=True)
    with open(OUT, "w") as f:
        json.dump(out, f)
    print(f"wrote {OUT}: {len(rows)} rows, pair {out['pair']}")


if __name__ == "__main__":
    main()
