// Browser-side ingest: turn an uploaded CSV + a column mapping into the same
// `data` object the viewer loads from data.json. This is the JS port of the
// generic core of python/export_web.py — parse, map columns to engine roles,
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

// Date string -> epoch day (days since 1970-01-01, UTC). Unparseable -> 0.
export function toEpochDay(raw) {
  const s = String(raw ?? "").trim();
  if (!s) return 0;
  const ms = Date.parse(s);
  if (Number.isNaN(ms)) return 0;
  return Math.floor(ms / 86400000);
}

// ---- dataset builder ------------------------------------------------------
// mapping: {
//   amount:     <colIndex>,              // required, the conserved value
//   gkey:       <colIndex> | null,       // net-to-zero aggregation key
//   date:       <colIndex> | null,       // for the time-windowed flow
//   tokens:     <colIndex>[] | <colIndex> | null,  // free-text columns, concatenated
//   partitions: [<colIndex>, ...],       // independent sub-books (0..n)
//   tol:        <integer minor units>,   // net tolerance for "clean"
//   name:       <string>,                // dataset label
// }
//
// Returns { pair, schema, plan, fields, rows, display, netKey } — exactly the
// shape data.json carries and app.js consumes.
export function buildDataset({ header, rows, mapping }) {
  const tol = Number.isFinite(mapping.tol) ? mapping.tol : 0;
  const parts = mapping.partitions || [];

  // The engine schema columns, in the order cells are emitted. Each carries the
  // source CSV column index and the role flags the descriptor/coercion need.
  const cols = [];
  parts.forEach((ci, i) =>
    cols.push({ name: `p${i}`, kind: "key", ci, label: header[ci], dim: true }));
  if (mapping.gkey != null)
    cols.push({ name: "gkey", kind: "key", ci: mapping.gkey, label: header[mapping.gkey], dim: true });
  if (mapping.date != null)
    cols.push({ name: "date", kind: "number", ci: mapping.date, label: header[mapping.date], date: true });
  cols.push({ name: "amount", kind: "number", ci: mapping.amount, label: header[mapping.amount], amount: true });
  // Reference text may span several CSV columns; they are concatenated into one
  // free-text tokens column (invoice nos, refs, memos mined for shared tokens).
  const tok = mapping.tokens;
  const tokCis = (Array.isArray(tok) ? tok : tok != null ? [tok] : []).filter((i) => i != null);
  if (tokCis.length)
    cols.push({ name: "tokens", kind: "tokens", cis: tokCis, label: tokCis.map((i) => header[i]).join(" + "), text: true });

  const schema = { cols: cols.map((c) => ({ name: c.name, kind: c.kind })), token_drop: [] };

  // Plan: only the steps whose columns exist. Cheap/strict leaves first, the
  // expensive min-cost flow last (and only when it has signals + time to use).
  const steps = [];
  if (mapping.gkey != null) steps.push({ op: "agg_net", key: "gkey", amount: "amount", tol });
  steps.push({ op: "exact", amount: "amount" });
  if (tokCis.length)
    steps.push({ op: "signal", signals: "tokens", amount: "amount", tol, cap: 256 });
  if (tokCis.length && mapping.date != null)
    steps.push({
      op: "flow", amount: "amount", day: "date", native: "amount",
      tokens: "tokens", penalty: 1000.0, window: -1,
    });
  let plan = { op: "seq", steps };
  for (let i = parts.length - 1; i >= 0; i--) plan = { op: "partition", by: `p${i}`, inner: plan };

  // Rows (bare cells, positional against schema) + display (human view, joined
  // by id). `native` mirrors the engine amount so manual-group conservation can
  // read it back; `value` is the same column for a generic single-amount book.
  const outRows = [], display = [];
  rows.forEach((r, id) => {
    const cells = cols.map((c) => {
      if (c.amount) return toCents(r[c.ci]);
      if (c.date) return toEpochDay(r[c.ci]);
      if (c.text) return c.cis.map((i) => String(r[i] ?? "")).filter(Boolean).join(" ");
      if (c.kind === "number") return toInt(r[c.ci]);
      return String(r[c.ci] ?? "");
    });
    outRows.push([id, cells]);
    const d = { id };
    for (const c of cols) {
      if (c.amount) d.amount = toCents(r[c.ci]);
      else if (c.date) d.date = String(r[c.ci] ?? "");
      else if (c.text) d[c.name] = c.cis.map((i) => String(r[i] ?? "")).filter(Boolean).join(" ");
      else d[c.name] = String(r[c.ci] ?? "");
    }
    d.native = d.amount; // engine-conserved column, for manual-group net
    display.push(d);
  });

  // Field descriptor: dims become slicers, the amount is the value column.
  const fields = [];
  for (const c of cols) {
    if (c.dim) fields.push({ key: c.name, label: c.label, kind: "dim", slicer: true, detail: true });
    else if (c.date) fields.push({ key: "date", label: c.label, kind: "date", slicer: false, detail: true });
    else if (c.amount) fields.push({ key: "amount", label: c.label, kind: "amount", amt: "amount", ccy: null, detail: true, value: true });
    else if (c.text) fields.push({ key: c.name, label: c.label, kind: "text", slicer: false, detail: true });
  }

  return {
    pair: mapping.name || "uploaded",
    schema, plan, fields, rows: outRows, display, netKey: "amount",
  };
}
