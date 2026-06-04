// Centralized workspace persistence + result export. One place that knows how
// to (a) serialize/restore the *operator's* working state to a portable file —
// NOT browser storage, so reloads are predictable — and (b) export the
// reconciliation result as CSV/JSON.
//
// Design note — what is "state"?
//   Everything *live* (the machine's current groups) is a deterministic
//   function of (dataset, plan) via solve, so it never needs saving. The only
//   durable operator state is:
//     • the FROZEN groups (the committed decisions), expressed allocation-native
//       in stable row-id terms (group ids are ephemeral across solves), and
//     • the tag overlay (review buckets), already keyed by stable row id.
//   So a saved workspace = dataset echo + frozen groups + tags. On load we
//   rebuild + solve to recover the live proposals, then re-assert the frozen
//   decisions on top. This is robust and small.

export const WORKSPACE_VERSION = 1;
export const WORKSPACE_KIND = "florecon.workspace";

// ---- frozen-decision extraction -------------------------------------------
// Collapse a report into the allocation-native frozen groups, keyed by row id.
export function frozenGroups(report) {
  const byG = new Map();
  for (const g of report.groups || [])
    if (g.status === "frozen")
      byG.set(g.group_id, { origin: g.origin || "manual", reason: g.reason ?? null, net: g.net | 0, allocations: [] });
  for (const a of report.allocations || []) {
    const f = byG.get(a.group_id);
    if (f) f.allocations.push({ id: a.id, amount: a.amount });
  }
  return [...byG.values()].filter((f) => f.allocations.length);
}

// ---- serialize / parse -----------------------------------------------------
export function serializeWorkspace({ data, report, tags }) {
  if (!data || !data.source || !data.source.header)
    throw new Error("dataset is not serializable (missing source echo)");
  return {
    kind: WORKSPACE_KIND,
    version: WORKSPACE_VERSION,
    savedAt: new Date().toISOString(),
    dataset: data.source, // { name, header, rows, columns, plan }
    frozen: frozenGroups(report),
    tags: tags && tags.dump ? tags.dump() : { tags: {}, meta: {} },
  };
}

export function parseWorkspace(text) {
  let o;
  try { o = JSON.parse(text); }
  catch (e) { throw new Error("not valid JSON: " + e.message); }
  if (!o || o.kind !== WORKSPACE_KIND) throw new Error("not a florecon workspace file");
  if (o.version !== WORKSPACE_VERSION)
    throw new Error(`workspace version ${o.version} != supported ${WORKSPACE_VERSION}`);
  if (!o.dataset || !Array.isArray(o.dataset.header)) throw new Error("workspace has no dataset");
  return o;
}

// Re-assert saved frozen decisions onto a freshly solved engine. Singletons go
// through freeze_singletons (group_allocations needs >=2 legs); multi-leg groups
// through group_allocations so exact amounts (and splits) are preserved. Returns
// a short report of what applied. `dispatch` is the engine's dispatch fn.
export function applyFrozen(dispatch, frozen) {
  let groups = 0, singles = 0, failed = 0;
  const errors = [];
  for (const f of frozen || []) {
    const allocs = (f.allocations || []).filter((a) => a && a.amount !== 0);
    if (allocs.length === 0) { // pure singleton(s): freeze the ids in place
      const ids = (f.allocations || []).map((a) => a.id);
      if (!ids.length) continue;
      const r = dispatch({ op: "freeze_singletons", ids });
      if (r.ok) singles += ids.length; else { failed++; errors.push(r.error); }
      continue;
    }
    if (allocs.length === 1) {
      const r = dispatch({ op: "freeze_singletons", ids: [allocs[0].id] });
      if (r.ok) singles++; else { failed++; errors.push(r.error); }
      continue;
    }
    const r = dispatch({
      op: "group_allocations", allocations: allocs,
      origin: f.origin || "manual", reason: f.reason ?? undefined,
    });
    if (r.ok) groups++; else { failed++; errors.push(r.error); }
  }
  return { groups, singles, failed, errors };
}

// ---- result export ---------------------------------------------------------
const csvCell = (v) => {
  const s = v === null || v === undefined ? "" : String(v);
  return /[",\n]/.test(s) ? '"' + s.replace(/"/g, '""') + '"' : s;
};
const csvRow = (a) => a.map(csvCell).join(",");

// Format one display field value for export (money in units, not cents).
function fieldVal(f, d) {
  const v = d[f.key];
  if (f.kind === "amount") return (Number(v || 0) / 100).toFixed(2);
  return v ?? "";
}

// Row-level result: every business row with the group it landed in. `assignments`
// is [[id, group_id], …] (e.g. primaryAssignments(report)).
export function resultsCsv({ data, report, assignments }) {
  const gmeta = new Map((report.groups || []).map((g) => [g.group_id, g]));
  const aMap = new Map(assignments || []);
  const fields = (data.fields || []).filter((f) => f.detail);
  const head = ["row_id", ...fields.map((f) => f.label || f.key),
    "group_id", "origin", "reason", "status", "group_net"];
  const lines = [csvRow(head)];
  for (const d of data.display) {
    const gid = aMap.get(d.id);
    const g = gid != null ? gmeta.get(gid) : null;
    lines.push(csvRow([
      d.id, ...fields.map((f) => fieldVal(f, d)),
      g ? g.group_id : "", g ? g.origin : "", g ? (g.reason ?? "") : "",
      g ? g.status : "unassigned", g ? (Number(g.net) / 100).toFixed(2) : "",
    ]));
  }
  return lines.join("\n");
}

// Group-level summary: one line per group.
export function groupsCsv({ report }) {
  const head = ["group_id", "origin", "reason", "status", "size", "net"];
  const lines = [csvRow(head)];
  for (const g of report.groups || [])
    lines.push(csvRow([g.group_id, g.origin, g.reason ?? "", g.status, g.size, (Number(g.net) / 100).toFixed(2)]));
  return lines.join("\n");
}

// Whole result as JSON (the raw allocation-native report + a little context).
export function resultJson({ data, report }) {
  return JSON.stringify({
    kind: "florecon.result", dataset: data.source ? data.source.name : data.pair,
    primary: data.netKey, savedAt: new Date().toISOString(), report,
  }, null, 2);
}

// ---- browser file helpers --------------------------------------------------
export function download(filename, text, mime = "application/json") {
  const blob = new Blob([text], { type: mime });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url; a.download = filename;
  document.body.appendChild(a); a.click(); a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export function pickTextFile(accept = ".json,application/json") {
  return new Promise((resolve, reject) => {
    const input = document.createElement("input");
    input.type = "file"; input.accept = accept;
    input.onchange = () => {
      const f = input.files && input.files[0];
      if (!f) return resolve(null);
      const r = new FileReader();
      r.onload = () => resolve(String(r.result));
      r.onerror = () => reject(r.error || new Error("read failed"));
      r.readAsText(f);
    };
    input.click();
  });
}

// A filesystem-safe slug for download names.
export function slug(s) {
  return String(s || "workspace").replace(/[^\w.-]+/g, "_").replace(/^_+|_+$/g, "") || "workspace";
}
