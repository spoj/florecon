"""Explicit projections from the allocation-native Report hypergraph."""


def strict_assignments(report: dict) -> list[tuple[int, int]]:
    """Return one (row id, group id) per row only if the report is not split.

    Raises ValueError if a row id participates in multiple groups.
    """
    by_id = {}
    for a in report.get("allocations", []):
        by_id.setdefault(int(a["id"]), set()).add(int(a["group_id"]))
    out = []
    for id_, groups in sorted(by_id.items()):
        if len(groups) != 1:
            raise ValueError(f"row {id_} is split across groups {sorted(groups)}")
        out.append((id_, next(iter(groups))))
    return out


def connected_components(report: dict) -> list[dict]:
    """Connected components of the bipartite graph row id <-> group id."""
    row_to_groups = {}
    group_to_rows = {}
    for a in report.get("allocations", []):
        r = int(a["id"])
        g = int(a["group_id"])
        row_to_groups.setdefault(r, []).append(g)
        group_to_rows.setdefault(g, []).append(r)

    seen_rows = set()
    seen_groups = set()
    out = []
    for start in list(row_to_groups):
        if start in seen_rows:
            continue
        rows = set()
        groups = set()
        row_stack = [start]
        group_stack = []
        while row_stack or group_stack:
            while row_stack:
                r = row_stack.pop()
                if r in seen_rows:
                    continue
                seen_rows.add(r)
                rows.add(r)
                group_stack.extend(row_to_groups.get(r, []))
            while group_stack:
                g = group_stack.pop()
                if g in seen_groups:
                    continue
                seen_groups.add(g)
                groups.add(g)
                row_stack.extend(group_to_rows.get(g, []))
        out.append({"rows": sorted(rows), "groups": sorted(groups)})
    out.sort(key=lambda c: (c["rows"][0] if c["rows"] else 0, c["groups"][0] if c["groups"] else 0))
    return out
