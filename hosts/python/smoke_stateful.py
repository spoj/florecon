"""Stateful smoke: drive the interco *plugin* wasm through the generic host —
describe-driven init, incremental upserts, warm re-solve, remove, pin, and
dissolve. The host ships raw ledger columns; the plugin owns the domain.

Build the plugin first, then run:

    just build-wasm   # or: cargo build -p interco-plugin --target wasm32-unknown-unknown --release
    PYTHONPATH=hosts/python/src .venv/bin/python hosts/python/smoke_stateful.py

Exits non-zero with a clear message on any regression.
"""

import pathlib
import sys

from florecon import Workspace

ROOT = pathlib.Path(__file__).resolve().parents[2]
WASM = ROOT / "target/wasm32-unknown-unknown/release/interco_plugin.wasm"


def check(cond, msg):
    if not cond:
        print(f"FAIL: {msg}")
        sys.exit(1)


def gid_of(rep, rid):
    for a in rep["allocations"]:
        if a["id"] == rid:
            return a["group_id"]
    return None


def group(rep, gid):
    for g in rep["groups"]:
        if g["group_id"] == gid:
            return g
    return None


def matched_pair(rep, a, b):
    """True iff rows a and b sit in the same group and that group nets to zero."""
    ga, gb = gid_of(rep, a), gid_of(rep, b)
    g = group(rep, ga) if ga is not None else None
    return ga is not None and ga == gb and g is not None and g["net"] == 0 and g["size"] >= 2


def line(row_id, co, icp, objsub, usd, day, ref):
    """One ledger line as the raw columns the plugin declared."""
    return {
        "row_id": row_id,
        "company": co,
        "icp": icp,
        "objsub": objsub,
        "indicative_usd_amt": usd,
        "gl_date": day,
        "trx_currency": "USD",
        "trx_amt": abs(usd),
        "reference": ref,
    }


if not WASM.exists():
    print(f"FAIL: plugin wasm not found at {WASM}\n  build it with:\n"
          "  cargo build -p interco-plugin --target wasm32-unknown-unknown --release")
    sys.exit(1)

ws = Workspace(str(WASM))
check(ws.domain.get("id") == "florecon.intercompany", "host should discover the domain via describe()")

# --- first invoice pair: opposite books, shared reference, nets clean --------
ws.upsert(
    line(1, "A", "B", "61500", 100.0, 0, "INV0001"),
    line(2, "B", "A", "61500", -100.0, 1, "INV0001"),
)
rep = ws.solve()
check(matched_pair(rep, 1, 2), "first invoice pair (1,2) should net to a matched group")

# --- stream a second pair in and WARM re-solve -------------------------------
ws.upsert(
    line(3, "A", "B", "61600", 250.0, 2, "INV0009"),
    line(4, "B", "A", "61600", -250.0, 3, "INV0009"),
)
rep = ws.solve()
check(matched_pair(rep, 1, 2), "original pair (1,2) must stay matched after warm re-solve")
check(matched_pair(rep, 3, 4), "new pair (3,4) must match on warm re-solve")
check(gid_of(rep, 1) != gid_of(rep, 3), "distinct invoice buckets must form distinct groups")

# --- remove one leg: its partner falls back to a live singleton --------------
ws.remove(4)
rep = ws.solve()
check(matched_pair(rep, 1, 2), "pair (1,2) untouched by removing row 4")
g3 = group(rep, gid_of(rep, 3))
check(g3 is not None and g3["size"] == 1, "row 3 must become a proposed singleton once its partner is removed")

# --- pin the clean match: an operator decision survives recalc ---------------
ws.pin_clean(tol=0)
rep = ws.report()
g12 = group(rep, gid_of(rep, 1))
check(g12 is not None and g12["status"] == "pinned", "pin_clean must pin the clean (1,2) match")
rep = ws.solve()
g12 = group(rep, gid_of(rep, 1))
check(g12 is not None and g12["status"] == "pinned" and g12["size"] >= 2,
      "pinned (1,2) group must survive a subsequent solve")

# --- re-add the partner, re-match, then dissolve the proposed group ----------
ws.upsert(line(4, "B", "A", "61600", -250.0, 3, "INV0009"))
rep = ws.solve()
check(matched_pair(rep, 3, 4), "re-adding row 4 must re-form the (3,4) match on warm re-solve")
ws.dissolve(gid_of(rep, 3))
rep = ws.report()
check(gid_of(rep, 3) != gid_of(rep, 4), "dissolve must split rows 3 and 4 back into separate groups")
check(group(rep, gid_of(rep, 1))["status"] == "pinned", "dissolve must not disturb the pinned (1,2) group")

print("STATEFUL SMOKE OK")
