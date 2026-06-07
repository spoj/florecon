// Browser ingest — turn an uploaded CSV + a column mapping into the Arrow batch
// a florecon *plugin* consumes. v2 difference from master: there is no plan to
// build. The plugin owns the strategy and *declares* its raw input columns via
// `describe()`; ingest's only job is to map CSV columns onto those declared
// names+types and coerce. Pure and DOM-free so it can be unit-tested under node.
import { Int64, Float64, Utf8, makeVector, vectorFromArray, tableFromArrays } from "apache-arrow";

// ---- CSV (RFC-4180-ish: quoted fields may hold commas/newlines/"" escapes) --
export function parseCsv(text) {
  const rows = [];
  let row = [],
    field = "",
    q = false,
    i = 0;
  text = String(text).replace(/^\uFEFF/, "");
  const push = () => {
    row.push(field);
    field = "";
  };
  const eol = () => {
    push();
    rows.push(row);
    row = [];
  };
  while (i < text.length) {
    const c = text[i];
    if (q) {
      if (c === '"') {
        if (text[i + 1] === '"') {
          field += '"';
          i += 2;
          continue;
        }
        q = false;
        i++;
        continue;
      }
      field += c;
      i++;
      continue;
    }
    if (c === '"') {
      q = true;
      i++;
      continue;
    }
    if (c === ",") {
      push();
      i++;
      continue;
    }
    if (c === "\r") {
      i++;
      continue;
    }
    if (c === "\n") {
      eol();
      i++;
      continue;
    }
    field += c;
    i++;
  }
  if (field.length || row.length) eol();
  while (rows.length && rows[rows.length - 1].every((s) => s === "")) rows.pop();
  if (!rows.length) return { header: [], rows: [] };
  return { header: rows[0].map((h) => h.trim()), rows: rows.slice(1) };
}

// ---- scalar coercion (by declared wire type) -------------------------------
export function toInt(raw) {
  const n = Number(String(raw ?? "").replace(/[^0-9.\-]/g, ""));
  return Number.isFinite(n) ? Math.round(n) : 0;
}
export function toNum(raw) {
  let s = String(raw ?? "").trim();
  if (!s) return 0;
  let neg = false;
  if (/^\(.*\)$/.test(s)) {
    neg = true;
    s = s.slice(1, -1);
  }
  s = s.replace(/[^0-9.\-]/g, "");
  const n = Number(s);
  return Number.isFinite(n) ? (neg ? -n : n) : 0;
}

// The conventional id column: the explicit one if given, else the first i64
// field. (describe() does not flag the id; the plugin reads ext_id from its
// `#[record(id)]` column, which is the first integer lane by convention.)
export function idField(fields, explicit) {
  if (explicit && fields.some((f) => f.name === explicit)) return explicit;
  const first = fields.find((f) => f.type === "i64");
  return first ? first.name : null;
}

// buildBatch({ header, rows, fields, mapping, idName }):
//   fields:  describe().input  -> [{name, type, amount}]
//   mapping: { <fieldName>: <csv column index | null> }
//   idName:  which declared field is the row id (default: first i64)
// Returns { table, display, idName, source } where `display` is the JS-side row
// echo (keyed by the same id the engine will emit) the workbench detail renders.
export function buildBatch({ header, rows, fields, mapping, idName }) {
  const N = rows.length;
  const id = idField(fields, idName);
  const idMapped = id != null && mapping[id] != null;

  const cols = {};
  const display = [];

  // typed Arrow lanes, one per declared field
  const lanes = {};
  for (const f of fields)
    lanes[f.name] = f.type === "utf8" ? new Array(N).fill("") : new BigInt64Array(0); // text vs numeric below

  const numeric = {};
  for (const f of fields) if (f.type !== "utf8") numeric[f.name] = new (f.type === "i64" ? BigInt64Array : Float64Array)(N);

  rows.forEach((r, ix) => {
    const d = {};
    let rid = ix + 1;
    for (const f of fields) {
      const ci = mapping[f.name];
      const raw = ci == null ? "" : r[ci];
      if (f.type === "utf8") {
        const v = String(raw ?? "");
        lanes[f.name][ix] = v;
        d[f.name] = v;
      } else if (f.type === "i64") {
        const v = f.name === id && !idMapped ? ix + 1 : toInt(raw);
        numeric[f.name][ix] = BigInt(v);
        d[f.name] = v;
        if (f.name === id) rid = v;
      } else {
        const v = toNum(raw);
        numeric[f.name][ix] = v;
        d[f.name] = v;
      }
    }
    d.__id = rid;
    display.push(d);
  });

  for (const f of fields) {
    if (f.type === "utf8") cols[f.name] = vectorFromArray(lanes[f.name], new Utf8());
    else if (f.type === "i64") cols[f.name] = makeVector({ data: numeric[f.name], type: new Int64() });
    else cols[f.name] = makeVector({ data: numeric[f.name], type: new Float64() });
  }

  return {
    table: tableFromArrays(cols),
    display,
    idName: id,
    source: { header, rows, mapping, idName: id },
  };
}
