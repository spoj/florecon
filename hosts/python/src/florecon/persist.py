"""Workspace persistence and result export.

One module that knows how to (a) serialize/restore the *operator's* durable
decisions to a portable file, and (b) export a reconciliation result as
dataframes / CSV / JSON.

Design note — what is durable state?
    Everything *proposed* is a deterministic function of (rows, plugin) via
    :meth:`Workspace.solve`, so it never needs saving. The only durable operator
    state is:
      * the **pinned** groups (committed decisions), expressed allocation-native
        in stable row-id terms (group ids are ephemeral across solves), and
      * the **tag** overlay (review buckets), already keyed by stable row id.
    So a saved workspace = pinned decisions + tags (+ optional metadata). On load
    we re-solve to recover the proposals, then re-assert the pinned decisions on
    top. Robust and small — it survives plugin tweaks that only move proposals.
"""

from __future__ import annotations

import json
from pathlib import Path

from .projections import primary_assignments

WORKSPACE_KIND = "florecon.workspace"
WORKSPACE_VERSION = 1


# --- durable-decision extraction -------------------------------------------

def decisions(report: dict) -> list[dict]:
    """Collapse a report into the allocation-native *pinned* decisions, keyed by
    row id. Each is ``{reason, allocations: [{id, amount}, ...]}``."""
    by_g: dict[int, dict] = {}
    for g in report.get("groups", []):
        if g.get("status") == "pinned":
            by_g[int(g["group_id"])] = {
                "reason": g.get("reason"),
                "origin": g.get("origin", "manual"),
                "allocations": [],
            }
    for a in report.get("allocations", []):
        d = by_g.get(int(a["group_id"]))
        if d is not None:
            d["allocations"].append({"id": int(a["id"]), "amount": int(a["amount"])})
    return [d for d in by_g.values() if d["allocations"]]


def apply_decisions(ws, saved: list[dict]) -> dict:
    """Re-assert saved pinned decisions onto a freshly solved workspace.

    Multi-leg groups go through :meth:`Workspace.merge` (exact amounts, so splits
    survive); lone accepted rows go through :meth:`Workspace.pin_singletons`.
    Merges are applied first so they pull their rows out of any proposed group,
    leaving accepted singletons free to pin. Returns a short apply summary.
    """
    multi = [d for d in saved if len([a for a in d["allocations"] if a["amount"]]) >= 2]
    singles: list[int] = []
    for d in saved:
        nz = [a for a in d["allocations"] if a["amount"]]
        if len(nz) < 2:
            singles.extend(a["id"] for a in d["allocations"])

    groups = failed = 0
    errors = []
    for d in multi:
        allocs = [a for a in d["allocations"] if a["amount"]]
        try:
            ws.merge(allocs, label=d.get("origin", "manual"), reason=d.get("reason"))
            groups += 1
        except Exception as e:  # noqa: BLE001 - collected, not swallowed
            failed += 1
            errors.append(str(e))
    pinned_singles = 0
    if singles:
        try:
            ws.pin_singletons(singles)
            pinned_singles = len(singles)
        except Exception as e:  # noqa: BLE001
            failed += 1
            errors.append(str(e))
    return {"groups": groups, "singles": pinned_singles, "failed": failed, "errors": errors}


# --- serialize / restore ----------------------------------------------------

def serialize(report: dict, *, tags=None, meta: dict | None = None) -> dict:
    """A portable workspace: pinned decisions + the tag overlay + metadata. The
    raw rows are *not* embedded (re-supply them with ``upsert`` before load)."""
    return {
        "kind": WORKSPACE_KIND,
        "version": WORKSPACE_VERSION,
        "domain": (meta or {}).get("domain"),
        "decisions": decisions(report),
        "tags": tags.dump() if tags is not None else {"tags": {}, "meta": {}},
        "meta": meta or {},
    }


def parse(obj_or_text) -> dict:
    o = json.loads(obj_or_text) if isinstance(obj_or_text, (str, bytes)) else obj_or_text
    if not isinstance(o, dict) or o.get("kind") != WORKSPACE_KIND:
        raise ValueError("not a florecon workspace")
    if o.get("version") != WORKSPACE_VERSION:
        raise ValueError(f"workspace version {o.get('version')} != supported {WORKSPACE_VERSION}")
    return o


def save_workspace(path, report: dict, *, tags=None, meta: dict | None = None) -> dict:
    """Write a workspace JSON file; returns the serialized object."""
    obj = serialize(report, tags=tags, meta=meta)
    Path(path).write_text(json.dumps(obj, indent=2))
    return obj


def load_workspace(path_or_obj, ws, *, tags=None) -> dict:
    """Restore decisions (and optionally tags) onto a workspace that has already
    been re-``upsert``ed and ``solve``d. Returns the apply summary."""
    obj = parse(Path(path_or_obj).read_text() if isinstance(path_or_obj, (str, Path)) and Path(path_or_obj).exists() else path_or_obj)
    if tags is not None:
        tags.restore(obj.get("tags"))
    return apply_decisions(ws, obj.get("decisions", []))


# --- result export ----------------------------------------------------------

def report_frames(report: dict):
    """The report as two pyarrow Tables ``(groups, allocations)`` — the natural
    bridge for writing results back to Spark/Delta in a notebook."""
    import pyarrow as pa

    groups = pa.Table.from_pylist(report.get("groups", []))
    allocations = pa.Table.from_pylist(report.get("allocations", []))
    return groups, allocations


def _csv(rows: list[list]) -> str:
    def cell(v):
        s = "" if v is None else str(v)
        return '"' + s.replace('"', '""') + '"' if any(c in s for c in ',"\n') else s
    return "\n".join(",".join(cell(c) for c in r) for r in rows)


def groups_csv(report: dict, *, money_scale: float = 0.01) -> str:
    """One line per group. ``money_scale`` converts the numeraire minor units to
    display (default cents -> currency units)."""
    head = ["group_id", "origin", "reason", "status", "size", "net"]
    rows = [head]
    for g in report.get("groups", []):
        rows.append([
            g["group_id"], g.get("origin", ""), g.get("reason", "") or "",
            g.get("status", ""), g.get("size", ""),
            f"{int(g.get('net', 0)) * money_scale:.2f}",
        ])
    return _csv(rows)


def results_csv(report: dict, *, policy: str = "largest_abs", money_scale: float = 0.01) -> str:
    """Row-level result: every row with the group it primarily landed in."""
    gmeta = {int(g["group_id"]): g for g in report.get("groups", [])}
    head = ["row_id", "group_id", "origin", "reason", "status", "group_net"]
    rows = [head]
    for rid, gid in primary_assignments(report, policy=policy):
        g = gmeta.get(int(gid), {})
        rows.append([
            rid, gid, g.get("origin", ""), g.get("reason", "") or "",
            g.get("status", ""), f"{int(g.get('net', 0)) * money_scale:.2f}",
        ])
    return _csv(rows)


def result_json(report: dict, *, meta: dict | None = None) -> str:
    """The whole allocation-native result plus a little context."""
    return json.dumps(
        {"kind": "florecon.result", "meta": meta or {}, "report": report}, indent=2
    )
