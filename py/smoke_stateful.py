"""Stateful smoke: prove the Python host can drive the engine's *interactive*
workspace over the v9 Arrow contract — schema-only init, incremental upserts,
warm re-solve, remove, freeze, and breakup. This is the pre-Arrow-IPC stateful
capability, restored. Run it with the engine's own wasm + pyarrow:

    PYTHONPATH=py/src .venv/bin/python py/smoke_stateful.py

Exits non-zero with a clear message on any regression.
"""

import sys

from florecon import (
    KEY,
    NUMBER,
    TOKENS,
    Workspace,
    col,
    key,
    plan as P,
    schema,
)


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


# A single bilateral unit, single currency. Each invoice pair shares an objsub
# bucket and an opposite-sign equal-magnitude amount, so it nets cleanly.
UNIT = key("00492", "00288")
sch = schema([
    col("unit", KEY),
    col("day", NUMBER),
    col("objsub", KEY),
    col("native", NUMBER),
    col("tokens", TOKENS),
])
root = P.partition("unit", P.seq(
    P.agg_net("objsub", tol=0),
    P.exact(),
    P.signal("tokens", tol=0, cap=256),
    P.flow("day", "tokens", window=-1),
))

# --- open on a schema only, then stream the first invoice pair in ------------
ws = Workspace(sch, root, primary="native")
ws.upsert(1, [UNIT, 0, "61500", 100, "INV0001 widgets"])
ws.upsert(2, [UNIT, 1, "61500", -100, "INV0001 credit"])
rep = ws.solve()
check(matched_pair(rep, 1, 2), "first invoice pair (1,2) should net to a matched group after init+upsert+solve")

# --- stream a second pair in and WARM re-solve -------------------------------
ws.upsert_many([
    (3, [UNIT, 2, "61600", 250, "INV0009 services"]),
    (4, [UNIT, 3, "61600", -250, "INV0009 reversal"]),
])
rep = ws.solve()
check(matched_pair(rep, 1, 2), "original pair (1,2) must stay matched after warm re-solve")
check(matched_pair(rep, 3, 4), "new pair (3,4) must match on warm re-solve")
check(gid_of(rep, 1) != gid_of(rep, 3), "distinct invoice buckets must form distinct groups")

# --- remove one leg: its partner falls back to a live singleton --------------
ws.remove(4)
rep = ws.solve()
check(matched_pair(rep, 1, 2), "pair (1,2) untouched by removing row 4")
g3 = group(rep, gid_of(rep, 3))
check(g3 is not None and g3["size"] == 1, "row 3 must become a live singleton once its partner is removed")

# --- freeze the clean match: an operator decision survives recalc ------------
ws.freeze_clean(tol=0)
g12 = group(ws.report(), gid_of(ws.report(), 1))
check(g12 is not None and g12["status"] == "frozen", "freeze_clean must freeze the clean (1,2) match")
rep = ws.solve()
g12 = group(rep, gid_of(rep, 1))
check(g12 is not None and g12["status"] == "frozen" and g12["size"] >= 2,
      "frozen (1,2) group must survive a subsequent solve")

# --- re-add the partner, re-match, then break the live group apart -----------
ws.upsert(4, [UNIT, 3, "61600", -250, "INV0009 reversal"])
rep = ws.solve()
check(matched_pair(rep, 3, 4), "re-adding row 4 must re-form the (3,4) match on warm re-solve")
ws.breakup(gid_of(rep, 3))
rep = ws.report()
check(gid_of(rep, 3) != gid_of(rep, 4), "breakup must split rows 3 and 4 back into separate groups")
check(group(rep, gid_of(rep, 1))["status"] == "frozen", "breakup must not disturb the frozen (1,2) group")

print("STATEFUL SMOKE OK")
