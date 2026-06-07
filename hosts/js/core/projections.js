// Explicit projections over the allocation-native Report hypergraph — the JS
// mirror of projections.py. No Arrow/DOM deps, so it is unit-testable under node.

export function strictAssignments(report) {
  const byId = new Map();
  for (const a of report.allocations || []) {
    if (!byId.has(a.id)) byId.set(a.id, new Set());
    byId.get(a.id).add(a.group_id);
  }
  const out = [];
  for (const [id, gs] of byId) {
    if (gs.size !== 1) throw new Error(`row ${id} is split across groups ${[...gs]}`);
    out.push([id, [...gs][0]]);
  }
  return out.sort((a, b) => a[0] - b[0]);
}

export function primaryAssignments(report, policy = "largest_abs") {
  const byId = new Map();
  for (const a of report.allocations || []) {
    if (!byId.has(a.id)) byId.set(a.id, []);
    byId.get(a.id).push(a);
  }
  const groups = new Map((report.groups || []).map((g) => [g.group_id, g]));
  const score = (a) => {
    const g = groups.get(a.group_id) || {};
    const clean = Math.abs(g.net || 0) === 0 ? 1 : 0;
    if (policy === "first_group") return [-a.group_id];
    if (policy === "prefer_clean") return [clean, Math.abs(a.amount || 0), -a.group_id];
    return [Math.abs(a.amount || 0), clean, -a.group_id];
  };
  const better = (a, b) => {
    const sa = score(a),
      sb = score(b);
    for (let i = 0; i < Math.max(sa.length, sb.length); i++) {
      const da = sa[i] ?? 0,
        db = sb[i] ?? 0;
      if (da !== db) return da > db;
    }
    return false;
  };
  const out = [];
  for (const [id, allocs] of byId) {
    let best = allocs[0];
    for (const a of allocs) if (better(a, best)) best = a;
    out.push([id, best.group_id]);
  }
  return out.sort((a, b) => a[0] - b[0]);
}

export function connectedComponents(report) {
  const r2g = new Map(),
    g2r = new Map();
  const push = (m, k, v) => (m.has(k) ? m.get(k) : m.set(k, []).get(k)).push(v);
  for (const a of report.allocations || []) {
    push(r2g, a.id, a.group_id);
    push(g2r, a.group_id, a.id);
  }
  const seenR = new Set(),
    seenG = new Set(),
    out = [];
  for (const start of r2g.keys()) {
    if (seenR.has(start)) continue;
    const rows = new Set(),
      groups = new Set(),
      rs = [start],
      gs = [];
    while (rs.length || gs.length) {
      while (rs.length) {
        const r = rs.pop();
        if (seenR.has(r)) continue;
        seenR.add(r);
        rows.add(r);
        gs.push(...(r2g.get(r) || []));
      }
      while (gs.length) {
        const g = gs.pop();
        if (seenG.has(g)) continue;
        seenG.add(g);
        groups.add(g);
        rs.push(...(g2r.get(g) || []));
      }
    }
    out.push({ rows: [...rows].sort((a, b) => a - b), groups: [...groups].sort((a, b) => a - b) });
  }
  return out;
}
