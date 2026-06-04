import { Utf8, Int64, Uint64, makeVector, vectorFromArray, tableFromArrays, tableToIPC } from "apache-arrow";
// Browser-side ingest: turn an uploaded CSV + a column mapping into the viewer
// `data` object the workbench consumes. Parse, map columns to engine roles,
// lower nothing (the wasm engine lowers strings by column kind), build a plan,
// and emit the display/fields descriptor the data-driven UI renders.
//
// Pure and DOM-free so it can be unit-tested under node (see ingest.smoke.mjs).

// ---- CSV ------------------------------------------------------------------
// RFC-4180-ish: quoted fields may contain commas, newlines, and "" escapes.
export function parseCsv(text) {
  const rows = [];
  let row = [], field = "", q = false, i = 0;
  // Normalize newlines; strip a UTF-8 BOM if present.
  text = text.replace(/^\uFEFF/, "");
  const push = () => { row.push(field); field = ""; };
  const eol = () => { push(); rows.push(row); row = []; };
  while (i < text.length) {
    const c = text[i];
    if (q) {
      if (c === '"') {
        if (text[i + 1] === '"') { field += '"'; i += 2; continue; }
        q = false; i++; continue;
      }
      field += c; i++; continue;
    }
    if (c === '"') { q = true; i++; continue; }
    if (c === ",") { push(); i++; continue; }
    if (c === "\r") { i++; continue; }
    if (c === "\n") { eol(); i++; continue; }
    field += c; i++;
  }
  // Flush a trailing field/row unless the file ended on a clean newline.
  if (field.length || row.length) eol();
  // Drop fully-empty trailing rows.
  while (rows.length && rows[rows.length - 1].every((s) => s === "")) rows.pop();
  if (!rows.length) return { header: [], rows: [] };
  const header = rows[0].map((h) => h.trim());
  return { header, rows: rows.slice(1) };
}

// ---- scalar coercion ------------------------------------------------------
// Money -> integer minor units (cents). Strips currency symbols, thousands
// separators, and parenthesised negatives. Conservation lives in this integer
// domain; the UI divides by 100 for display.
export function toCents(raw) {
  let s = String(raw ?? "").trim();
  if (!s) return 0;
  let neg = false;
  if (/^\(.*\)$/.test(s)) { neg = true; s = s.slice(1, -1); }
  s = s.replace(/[^0-9.\-]/g, "");
  const n = Number(s);
  if (!Number.isFinite(n)) return 0;
  return Math.round((neg ? -n : n) * 100);
}

// A free integer column (rare; e.g. a pre-scaled count).
export function toInt(raw) {
  const n = Number(String(raw ?? "").replace(/[^0-9.\-]/g, ""));
  return Number.isFinite(n) ? Math.round(n) : 0;
}

// A free-form numeric column for *display only* (extra columns the engine does
// not reconcile): strips symbols/separators/parenthesised negatives but keeps
// the natural magnitude (NOT scaled to cents). Blank/unparseable -> 0.
export function toNum(raw) {
  let s = String(raw ?? "").trim();
  if (!s) return 0;
  let neg = false;
  if (/^\(.*\)$/.test(s)) { neg = true; s = s.slice(1, -1); }
  s = s.replace(/[^0-9.\-]/g, "");
  const n = Number(s);
  if (!Number.isFinite(n)) return 0;
  return neg ? -n : n;
}

// Date string -> epoch day (days since 1970-01-01, UTC). Unparseable -> 0.
export function toEpochDay(raw) {
  const s = String(raw ?? "").trim();
  if (!s) return 0;
  const ms = Date.parse(s);
  if (Number.isNaN(ms)) return 0;
  return Math.floor(ms / 86400000);
}

// ---- dataset builder ------------------------------------------------------
// buildDataset({ header, rows, columns, plan, name, derive }):
//   columns: [{ ci, name, kind }]  - see the column-spec note below.
//   plan:    {primary, root}       - the strategy; defaults to planFromCols.
//   name:    <string>              - dataset label.
//   derive:  [{ name, value(rawRow, display) }] - extra Int64 lanes a Sel can
//            branch/partition on (materialized; opt-in memory ~ rows x lanes).
//
// Returns { pair, plan, fields, display, netKey, arrowBytes } — exactly the
// shape the workbench consumes. Column identity is NOT shipped separately: the
// engine derives it from the Arrow IPC batch schema (column 0 is the row id;
// Int64 columns are integer lanes, Utf8 columns are free text it tokenizes).
// A column spec is a list of { ci, name, kind } where kind is one of:
//   amount  - conserved money lane (Int64 cents). The plan's `primary` picks
//             which amount column is the conserved value.
//   number  - free integer lane (Int64, natural magnitude).
//   date    - epoch-day lane (Int64), enables time-windowed flow.
//   key     - hashed equality key (Int64 FNV); use for partition/agg_net keys.
//   text    - free-text lane (Utf8) mined for shared tokens by signal/flow.
//   display - JS-only column shown in the detail table; never enters the Arrow
//             batch, so it costs zero engine memory and cannot perturb a solve.
// Names are sanitised to engine identifiers and de-duplicated; the plan editor
// references columns by these names.
export function sanitizeName(s) {
  let n = String(s ?? "").trim().toLowerCase().replace(/[^a-z0-9_]+/g, "_").replace(/^_+|_+$/g, "");
  if (/^[0-9]/.test(n)) n = "_" + n;
  return n;
}

export function normalizeColumns(columns, header = []) {
  const seen = new Set();
  const out = [];
  for (const c of columns || []) {
    if (!c || c.ci == null || c.kind === "ignore" || c.include === false) continue;
    let name = sanitizeName(c.name || header[c.ci] || `col${c.ci}`) || `col${c.ci}`;
    let base = name, k = 2;
    while (seen.has(name)) name = `${base}_${k++}`;
    seen.add(name);
    out.push({ ci: c.ci, name, kind: c.kind || "display", label: header[c.ci] || name });
  }
  return out;
}

// The conserved column name for a normalized spec + optional explicit primary:
// the explicit choice if valid, else the first amount column, else the first
// number column.
function pickPrimary(cols, primary) {
  if (primary && cols.some((c) => c.name === primary)) return primary;
  return (cols.find((c) => c.kind === "amount")
    || cols.find((c) => c.kind === "number") || { name: "amount" }).name;
}

// The auto-built strategy from a normalized column spec: cheap/strict leaves
// first (key net, exact, reference signal), the min-cost flow last and only
// when it has both a text signal and a date to use. No auto-partitioning — that
// is a plan-editor choice. Single source of the default, shared by buildDataset
// and the setup screen's "Default" template so the two can never drift.
function planFromCols(cols, { primary, tol = 0 } = {}) {
  const keys = cols.filter((c) => c.kind === "key");
  const texts = cols.filter((c) => c.kind === "text");
  const dates = cols.filter((c) => c.kind === "date");
  const steps = [];
  if (keys.length) steps.push({ op: "agg_net", key: keys[0].name, tol });
  steps.push({ op: "exact" });
  if (texts.length) steps.push({ op: "signal", signals: texts[0].name, tol, cap: 256 });
  if (texts.length && dates.length)
    steps.push({ op: "flow", order_by: dates[0].name, tokens: texts[0].name, penalty: 1000.0, window: -1 });
  return { primary: pickPrimary(cols, primary), root: { op: "seq", steps } };
}

/// The default `{primary, root}` plan for a column spec, without building the
/// Arrow batch — cheap enough for a live template preview on the setup screen.
export function defaultPlan(columns, { header = [], primary, tol = 0 } = {}) {
  return planFromCols(normalizeColumns(columns, header), { primary, tol });
}

/// The engine Int64 column names a plan `Sel` may reference for this spec.
/// `text` columns are excluded (Utf8 signal lanes, usable by signal/flow but not
/// as a `Sel` integer); `display` columns never reach the engine.
export function intColumns(columns, header = []) {
  return normalizeColumns(columns, header)
    .filter((c) => c.kind === "amount" || c.kind === "number" || c.kind === "date" || c.kind === "key")
    .map((c) => c.name);
}

export function buildDataset({ header, rows, columns, plan, name, derive }) {
  const N = rows.length;
  const cols = normalizeColumns(columns, header);
  const engineCols = cols.filter((c) => c.kind !== "display"); // typed Arrow lanes
  const displayCols = cols.filter((c) => c.kind === "display"); // JS-only columns
  const prim = pickPrimary(cols, plan && plan.primary);
  const dateCol = cols.find((c) => c.kind === "date"); // alias for the month slicer

  // Derived Int64 lanes: full-JS columns computed at ingest and shipped in the
  // batch so a plan `Sel` can branch/partition on them by name. Materialized
  // (memory ~ rows x lanes), so opt-in. `value(rawRow, display)` returns a number.
  const derived = (derive || []).filter((d) => d && d.name && typeof d.value === "function");

  // Display-only column kind detection (numeric / low-card dim / free text).
  const numRe = /^[\s$()+\-]*[\d,]+\.?\d*%?$/;
  const extras = displayCols.map((c) => {
    const sample = rows.slice(0, 200).map((r) => String(r[c.ci] ?? "").trim()).filter(Boolean);
    const numeric = sample.length > 0 && sample.every((s) => numRe.test(s) && /\d/.test(s));
    let kind = "text", slicer = false;
    if (numeric) kind = "num";
    else {
      const distinct = new Set(rows.map((r) => String(r[c.ci] ?? ""))).size;
      if (distinct <= Math.min(200, Math.max(20, N * 0.05))) { kind = "dim"; slicer = true; }
    }
    return { ci: c.ci, key: c.name, label: c.label, kind, slicer, numeric };
  });

  // Arrow lanes: one typed column per engine col (Int64, or Utf8 for text) +
  // any derived Int64 lanes. Column 0 is the row id.
  const arrowCols = { id: makeVector({ data: new BigInt64Array(N), type: new Uint64() }) };
  for (const c of engineCols)
    arrowCols[c.name] = c.kind === "text"
      ? new Array(N).fill("")
      : makeVector({ data: new BigInt64Array(N), type: new Int64() });
  for (const dv of derived)
    arrowCols[dv.name] = makeVector({ data: new BigInt64Array(N), type: new Int64() });

  const display = [];
  rows.forEach((r, id) => {
    arrowCols.id.data[0].values[id] = BigInt(id);
    for (const c of engineCols) {
      const v = r[c.ci];
      if (c.kind === "amount") arrowCols[c.name].data[0].values[id] = BigInt(toCents(v));
      else if (c.kind === "number") arrowCols[c.name].data[0].values[id] = BigInt(toInt(v));
      else if (c.kind === "date") arrowCols[c.name].data[0].values[id] = BigInt(toEpochDay(v));
      else if (c.kind === "text") arrowCols[c.name][id] = String(v ?? "");
      else arrowCols[c.name].data[0].values[id] = BigInt(fnv1a(String(v ?? ""))); // key
    }

    const d = { id };
    for (const c of engineCols) {
      if (c.kind === "amount") d[c.name] = toCents(r[c.ci]);
      else if (c.kind === "number") d[c.name] = toInt(r[c.ci]);
      else d[c.name] = String(r[c.ci] ?? "");
    }
    for (const e of extras) d[e.key] = e.numeric ? toNum(r[e.ci]) : String(r[e.ci] ?? "");
    for (const dv of derived)
      arrowCols[dv.name].data[0].values[id] = BigInt(Math.round(Number(dv.value(r, d)) || 0));
    // Canonical aliases the data-driven workbench relies on: `native` is the
    // conserved value (cents) for manual-group net math; `date` feeds the month
    // slicer regardless of the date column's chosen name.
    d.native = Number(d[prim] || 0);
    d.date = dateCol ? String(r[dateCol.ci] ?? "") : "";
    display.push(d);
  });

  // Field descriptor: amounts are money (cents) columns; the primary is the
  // value column; numbers are natural-magnitude; keys become slicers.
  const fields = [];
  for (const c of engineCols) {
    if (c.kind === "amount")
      fields.push(c.name === prim
        ? { key: c.name, label: c.label, kind: "amount", amt: c.name, ccy: null, detail: true, value: true }
        : { key: c.name, label: c.label, kind: "amount", amt: c.name, ccy: null, detail: true });
    else if (c.kind === "number")
      fields.push({ key: c.name, label: c.label, kind: "num", amt: c.name, detail: true, value: c.name === prim });
    else if (c.kind === "date")
      fields.push({ key: c.name, label: c.label, kind: "date", slicer: false, detail: true });
    else if (c.kind === "key")
      fields.push({ key: c.name, label: c.label, kind: "dim", slicer: true, detail: true });
    else if (c.kind === "text")
      fields.push({ key: c.name, label: c.label, kind: "text", slicer: false, detail: true, wide: true });
  }
  for (const e of extras) {
    if (e.kind === "num") fields.push({ key: e.key, label: e.label, kind: "num", amt: e.key, detail: true });
    else if (e.kind === "dim") fields.push({ key: e.key, label: e.label, kind: "dim", slicer: true, detail: true });
    else fields.push({ key: e.key, label: e.label, kind: "text", slicer: false, detail: true, wide: true });
  }

  for (const c of engineCols)
    if (c.kind === "text") arrowCols[c.name] = vectorFromArray(arrowCols[c.name], new Utf8());

  // The plan is just data: the editor's plan if given (its primary defaulted in
  // when omitted), else the one auto-built from the column kinds.
  const finalPlan = plan && plan.root
    ? (plan.primary ? plan : { ...plan, primary: prim })
    : planFromCols(cols, { primary: prim, tol: 0 });

  return {
    pair: name || "uploaded",
    plan: finalPlan, fields, display, netKey: finalPlan.primary || prim,
    arrowBytes: tableToIPC(tableFromArrays(arrowCols), "stream"),
    // Self-describing echo of the inputs, so the workspace can be serialized and
    // rebuilt later (predictable reload) without re-uploading the CSV. `derive`
    // is intentionally omitted (functions don't serialize); reload uses plan + cols.
    source: { name: name || "uploaded", header, rows, columns, plan: finalPlan },
  };
}


// FNV-1a for keys
function fnv1a(str) {
  let hash = 0xcbf29ce484222325n;
  for (let i = 0; i < str.length; i++) {
    hash ^= BigInt(str.charCodeAt(i));
    hash = (hash * 0x100000001b3n) & 0xffffffffffffffffn;
  }
  return hash > 0x7fffffffffffffffn ? hash - 0x10000000000000000n : hash;
}
