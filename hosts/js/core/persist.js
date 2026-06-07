// Workspace persistence + result export — the JS mirror of persist.py.
//
// Durable operator state = pinned decisions (allocation-native, row-id keyed) +
// the tag overlay. Everything proposed is re-derived by `solve`. Unlike the
// Python host, the browser workspace also embeds a *dataset echo* (the browser
// has no other data source), so a saved file reloads with no re-upload.
import { primaryAssignments } from "./florecon.js";

export const WORKSPACE_KIND = "florecon.workspace";
export const WORKSPACE_VERSION = 1;

// --- durable-decision extraction -------------------------------------------
export function decisions(report) {
  const byG = new Map();
  for (const g of report.groups || [])
    if (g.status === "pinned")
      byG.set(g.group_id, { reason: g.reason ?? null, origin: g.origin || "manual", allocations: [] });
  for (const a of report.allocations || []) {
    const d = byG.get(a.group_id);
    if (d) d.allocations.push({ id: a.id, amount: a.amount });
  }
  return [...byG.values()].filter((d) => d.allocations.length);
}

// Re-assert saved pinned decisions onto a freshly solved workspace. Multi-leg
// groups via `merge` (exact amounts); lone accepted rows via `pinSingletons`.
// Merges first so they pull rows out of proposed groups.
export function applyDecisions(ws, saved) {
  const multi = saved.filter((d) => d.allocations.filter((a) => a.amount).length >= 2);
  const singles = [];
  for (const d of saved)
    if (d.allocations.filter((a) => a.amount).length < 2)
      singles.push(...d.allocations.map((a) => a.id));

  let groups = 0,
    failed = 0;
  const errors = [];
  for (const d of multi) {
    try {
      ws.merge(d.allocations.filter((a) => a.amount), d.origin || "manual", d.reason);
      groups++;
    } catch (e) {
      failed++;
      errors.push(String(e));
    }
  }
  let pinnedSingles = 0;
  if (singles.length) {
    try {
      ws.pinSingletons(singles);
      pinnedSingles = singles.length;
    } catch (e) {
      failed++;
      errors.push(String(e));
    }
  }
  return { groups, singles: pinnedSingles, failed, errors };
}

// --- serialize / restore ----------------------------------------------------
export function serialize(report, { tags, dataset, meta } = {}) {
  return {
    kind: WORKSPACE_KIND,
    version: WORKSPACE_VERSION,
    domain: (meta || {}).domain ?? null,
    decisions: decisions(report),
    tags: tags ? tags.dump() : { tags: {}, meta: {} },
    dataset: dataset || null, // browser-only echo (source rows + mapping)
    meta: meta || {},
  };
}

export function parse(textOrObj) {
  const o = typeof textOrObj === "string" ? JSON.parse(textOrObj) : textOrObj;
  if (!o || o.kind !== WORKSPACE_KIND) throw new Error("not a florecon workspace");
  if (o.version !== WORKSPACE_VERSION)
    throw new Error(`workspace version ${o.version} != supported ${WORKSPACE_VERSION}`);
  return o;
}

// --- result export ----------------------------------------------------------
function csv(rows) {
  const cell = (v) => {
    const s = v == null ? "" : String(v);
    return /[",\n]/.test(s) ? '"' + s.replace(/"/g, '""') + '"' : s;
  };
  return rows.map((r) => r.map(cell).join(",")).join("\n");
}

export function groupsCsv(report, { moneyScale = 0.01 } = {}) {
  const rows = [["group_id", "origin", "reason", "status", "size", "net"]];
  for (const g of report.groups || [])
    rows.push([g.group_id, g.origin || "", g.reason || "", g.status || "", g.size ?? "", (g.net * moneyScale).toFixed(2)]);
  return csv(rows);
}

export function resultsCsv(report, { policy = "largest_abs", moneyScale = 0.01 } = {}) {
  const gmeta = new Map((report.groups || []).map((g) => [g.group_id, g]));
  const rows = [["row_id", "group_id", "origin", "reason", "status", "group_net"]];
  for (const [rid, gid] of primaryAssignments(report, policy)) {
    const g = gmeta.get(gid) || {};
    rows.push([rid, gid, g.origin || "", g.reason || "", g.status || "", ((g.net || 0) * moneyScale).toFixed(2)]);
  }
  return csv(rows);
}

export function resultJson(report, { meta } = {}) {
  return JSON.stringify({ kind: "florecon.result", meta: meta || {}, report }, null, 2);
}

// --- tiny browser download helper ------------------------------------------
export function download(name, text, mime = "text/plain") {
  const blob = new Blob([text], { type: mime });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  a.click();
  URL.revokeObjectURL(url);
}
