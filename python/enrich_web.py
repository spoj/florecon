"""One-off: enrich web/data.json in place.

The committed data.json predates the richer display contract (doc company,
doc type, split trx/base amounts) and the portable `fields` descriptor the UI
now renders from. The source parquet is not in the tree, so the genuinely
source-only columns (doc_co, doc_type) are reconstructed with a deterministic
policy heuristic purely so the demo book exercises every column; a real export
via export_web.py carries the true values.
"""
import json

PATH = "web/data.json"

# kind: date | text | amount | dim ; amount fields pair with a ccy field.
FIELDS = [
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

DOC_TYPE = {"AP": "PV", "AR": "RI", "GA": "JE"}


def main():
    d = json.load(open(PATH))
    for r in d["display"]:
        co, icp = r.get("co", ""), r.get("icp", "")
        pol = r.get("policy", "")
        r["gl_co"] = co
        r["doc_co"] = icp if pol == "AP" else co
        r["doc_type"] = DOC_TYPE.get(pol, "JE")
        r["trx_ccy"] = r.get("ccy", "")
        r["trx_amt"] = r.get("native", 0)
        r["base_ccy"] = "USD"
        r["base_amt"] = r.get("usd", 0)
    d["fields"] = FIELDS
    json.dump(d, open(PATH, "w"))
    print(f"enriched {PATH}: {len(d['display'])} rows, {len(FIELDS)} fields")


if __name__ == "__main__":
    main()
