// Setup screen + entry point: upload a CSV, pick which columns to bring in and
// how to encode each (amount/number/date/key/text/display), optionally author a
// plan against those column names, build the viewer `data` object, and hand it
// to the workbench. There is no role "mapper" — columns are just typed lanes and
// everything else (keys, partitions, signals) is expressed in the plan.

import { startApp } from "./app.js";
import {
  parseCsv, buildDataset, defaultPlan, intColumns, normalizeColumns, sanitizeName,
} from "./ingest.js";
import {
  plan as mkPlan, seq, label, partition, branch, aggNet, exact, signal, flow, pivot, windowed,
  relTol, col, abs, ge,
} from "./core/plan.js";

const $ = (id) => document.getElementById(id);
const el = (tag, attrs = {}, kids = []) => {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") n.className = v;
    else if (k === "html") n.innerHTML = v;
    else if (k === "text") n.textContent = v;
    else if (k === "checked") { if (v) n.checked = true; }
    else n.setAttribute(k, v);
  }
  for (const c of [].concat(kids)) n.append(c);
  return n;
};

// The column kinds the picker offers. `amount`/`number`/`date`/`key` are typed
// Int64 lanes; `text` is a Utf8 signal lane; `display` is JS-only (no engine
// cost). `ignore` drops the column entirely.
const KINDS = [
  ["amount", "Amount (money, conserved)"],
  ["number", "Number"],
  ["date", "Date"],
  ["key", "Key (group / partition)"],
  ["text", "Reference text"],
  ["display", "Display only"],
  ["ignore", "Ignore"],
];

let parsed = null; // { header, rows, name }

function show(view) {
  $("setup").hidden = view !== "setup";
  $("topbar").hidden = view !== "app";
  $("main").hidden = view !== "app";
  $("status").hidden = view !== "app";
}

// ---- per-column kind guess ------------------------------------------------
function colLooksNumeric(ci) {
  return parsed.rows.slice(0, 20).some((r) =>
    /^[\s$(-]*[\d,]+\.?\d*\)?$/.test(String(r[ci] ?? "").trim()) && /\d/.test(r[ci] ?? ""));
}
function guessKind(name, ci) {
  if (/date|day|posted|gl_?date|period|time/i.test(name)) return "date";
  if (colLooksNumeric(ci) && /amount|amt|value|total|debit|credit|sum|balance|usd|eur|net/i.test(name)) return "amount";
  if (/account|entity|company|icp|currency|ccy|category|class|objsub|gl\b|cost.?cent|dept|book/i.test(name)) return "key";
  if (/desc|ref|memo|narrativ|detail|invoice|comment|note|trace|doc|particular/i.test(name)) return "text";
  return "display";
}

// ---- column picker --------------------------------------------------------
function renderColumns() {
  const body = $("col-body");
  body.innerHTML = "";
  let firstAmount = -1;
  parsed.header.forEach((h, ci) => {
    const kind = guessKind(h, ci);
    if (kind === "amount" && firstAmount < 0) firstAmount = ci;
    const include = el("input", { type: "checkbox", id: `inc-${ci}`, checked: true });
    const name = el("input", { type: "text", id: `nm-${ci}`, class: "col-name", value: sanitizeName(h) || `col${ci}` });
    const sel = el("select", { id: `kind-${ci}` },
      KINDS.map(([v, lbl]) => el("option", { value: v }, lbl)));
    sel.value = kind;
    const prim = el("input", { type: "radio", name: "primary", id: `prim-${ci}`, value: String(ci) });
    sel.onchange = () => syncPrimaryEnabled();
    include.onchange = () => syncPrimaryEnabled();
    body.append(el("div", { class: "col-row" }, [
      include,
      name,
      sel,
      el("label", { class: "col-prim", title: "the conserved column (plan primary)" },
        [prim, document.createTextNode(" conserved")]),
      el("div", { class: "hint mono", text: h }),
    ]));
  });
  // default conserved = first amount column
  if (firstAmount >= 0) { const r = $(`prim-${firstAmount}`); if (r) r.checked = true; }
  syncPrimaryEnabled();
  renderPreview();
  renderPlanEditor();
  $("map-panel").hidden = false;
}

// Only included amount/number columns can be the conserved primary.
function syncPrimaryEnabled() {
  let firstEligible = -1;
  let anyChecked = false;
  parsed.header.forEach((_, ci) => {
    const r = $(`prim-${ci}`);
    if (!r) return;
    const k = $(`kind-${ci}`).value;
    const eligible = (k === "amount" || k === "number") && $(`inc-${ci}`).checked;
    r.disabled = !eligible;
    if (eligible && firstEligible < 0) firstEligible = ci;
    if (r.checked && !eligible) r.checked = false;
    if (r.checked && eligible) anyChecked = true;
  });
  if (!anyChecked && firstEligible >= 0) $(`prim-${firstEligible}`).checked = true;
}

function renderPreview() {
  const head = el("tr", {}, parsed.header.map((h) => el("th", { text: h })));
  const rows = parsed.rows.slice(0, 6).map((r) =>
    el("tr", {}, parsed.header.map((_, i) => el("td", { text: String(r[i] ?? "") }))));
  const tbl = $("preview");
  tbl.innerHTML = "";
  tbl.append(el("thead", {}, head), el("tbody", {}, rows));
  $("preview-foot").textContent = `${parsed.rows.length} rows, ${parsed.header.length} columns`;
}

// Read the picker into a raw column spec + normalized cols + conserved primary.
function readColumns() {
  const raw = [];
  parsed.header.forEach((h, ci) => {
    if (!$(`inc-${ci}`).checked) return;
    const kind = $(`kind-${ci}`).value;
    if (kind === "ignore") return;
    raw.push({ ci, name: $(`nm-${ci}`).value || h || `col${ci}`, kind });
  });
  const cols = normalizeColumns(raw, parsed.header);
  const primRadio = document.querySelector('input[name="primary"]:checked');
  let primary = null;
  if (primRadio) {
    const ci = Number(primRadio.value);
    const match = cols.find((c) => c.ci === ci);
    if (match) primary = match.name;
  }
  return { columns: raw, cols, primary, name: parsed.name || "uploaded" };
}

// ---- plan editor + templates ----------------------------------------------
function keyCol(cols) {
  const k = cols.find((c) => c.kind === "key");
  if (k) return k.name;
  const a = cols.find((c) => c.kind === "amount" || c.kind === "number");
  return a ? a.name : "amount";
}
function coreCascade(cols, { relative }) {
  const keys = cols.filter((c) => c.kind === "key");
  const texts = cols.filter((c) => c.kind === "text");
  const dates = cols.filter((c) => c.kind === "date");
  const steps = [];
  if (keys.length) steps.push(label("aggregate net", aggNet(col(keys[0].name), relative ? relTol(10, 1) : 0)));
  steps.push(label("exact 1:1", exact()));
  if (texts.length) steps.push(label("reference bridge", signal(texts[0].name, { tol: 0, cap: 256 })));
  if (texts.length && dates.length)
    steps.push(label("flow arbiter", flow(dates[0].name, texts[0].name, { penalty: 1000, window: -1 })));
  return seq(...steps);
}

const PLAN_TEMPLATES = [
  { name: "Default", build: ({ cols, primary }) => defaultPlan(cols, { primary }) },
  { name: "Labelled stages", build: ({ cols, primary }) => mkPlan(primary, coreCascade(cols, { relative: false })) },
  { name: "Relative tolerance", build: ({ cols, primary }) => mkPlan(primary, coreCascade(cols, { relative: true })) },
  {
    name: "Route large amounts",
    build: ({ cols, primary }) => mkPlan(primary, branch(
      ge(abs(col(primary)), 100000),
      label("large", coreCascade(cols, { relative: false })),
      label("small", seq(label("exact 1:1", exact()))),
    )),
  },
];

const PLAN_SNIPPETS = [
  { name: "agg_net", build: ({ cols }) => aggNet(col(keyCol(cols)), relTol(10, 1)) },
  { name: "exact", build: () => exact() },
  { name: "signal", build: ({ cols }) => signal((cols.find((c) => c.kind === "text") || { name: "tokens" }).name, { tol: 0, cap: 256 }) },
  { name: "flow", build: ({ cols }) => {
      const t = cols.find((c) => c.kind === "text"), d = cols.find((c) => c.kind === "date");
      return flow(d ? d.name : "date", t ? t.name : "tokens", { penalty: 1000, window: -1 });
  } },
  { name: "branch", build: ({ primary }) => branch(ge(abs(col(primary || "amount")), 100000), exact(), exact()) },
  { name: "partition", build: ({ cols }) => partition(col(keyCol(cols)), exact()) },
  { name: "windowed", build: ({ cols }) => {
      const d = cols.find((c) => c.kind === "date");
      return windowed(col(d ? d.name : keyCol(cols)), 7, exact());
  } },
  { name: "pivot", build: ({ primary }) => pivot(col(primary || "amount"), exact()) },
  { name: "label", build: () => label("stage", exact()) },
  { name: "seq", build: () => seq(exact()) },
];

function insertSnippet(text) {
  const ta = $("plan-json");
  if (!ta) return;
  const s = ta.selectionStart ?? ta.value.length;
  const e = ta.selectionEnd ?? ta.value.length;
  ta.value = ta.value.slice(0, s) + text + ta.value.slice(e);
  const pos = s + text.length;
  ta.focus();
  ta.setSelectionRange(pos, pos);
}

function showCols(ctx) {
  const ints = intColumns(ctx.cols, parsed.header);
  const texts = ctx.cols.filter((c) => c.kind === "text").map((c) => c.name);
  $("plan-cols").textContent =
    `primary: ${ctx.primary || "(pick a conserved column)"}  ·  Sel columns: ${ints.join(", ") || "(none)"}`
    + (texts.length ? `  ·  signal/flow text: ${texts.join(", ")}` : "");
}

function renderPlanEditor() {
  const host = $("plan-templates");
  if (!host) return;
  host.innerHTML = "";
  const fail = (e) => { $("plan-err").textContent = (e && e.message) || String(e); };

  host.append(el("span", { class: "adv-tlabel" }, "Sample plans:"));
  for (const t of PLAN_TEMPLATES) {
    const b = el("button", { class: "mini", type: "button" }, t.name);
    b.onclick = () => {
      $("plan-err").textContent = "";
      try { const ctx = readColumns(); $("plan-json").value = JSON.stringify(t.build(ctx), null, 2); showCols(ctx); }
      catch (e) { fail(e); }
    };
    host.append(b);
  }
  const clear = el("button", { class: "mini link", type: "button" }, "clear");
  clear.onclick = () => { $("plan-json").value = ""; $("plan-err").textContent = ""; };
  host.append(clear);

  host.append(el("span", { class: "adv-tlabel" }, "Insert node:"));
  for (const t of PLAN_SNIPPETS) {
    const b = el("button", { class: "mini snip", type: "button" }, t.name);
    b.onclick = () => {
      $("plan-err").textContent = "";
      try { const ctx = readColumns(); insertSnippet(JSON.stringify(t.build(ctx), null, 2)); showCols(ctx); }
      catch (e) { fail(e); }
    };
    host.append(b);
  }
  try { showCols(readColumns()); } catch { /* before columns ready */ }
}

// Resolve the plan to use: the editor's JSON if present (accepting a bare root
// node, wrapped with the chosen primary), else the auto plan from the columns.
function readPlan(ctx) {
  const text = $("plan-json") ? $("plan-json").value.trim() : "";
  if (!text) return defaultPlan(ctx.cols, { primary: ctx.primary });
  let p;
  try { p = JSON.parse(text); }
  catch (e) { throw new Error("Plan JSON is invalid: " + e.message); }
  if (!p || typeof p !== "object") throw new Error("Plan must be a JSON object.");
  if (p.root || p.primary) return p.primary ? p : { ...p, primary: ctx.primary };
  return { primary: ctx.primary, root: p }; // bare root node
}

async function runUpload() {
  $("setup-err").textContent = "";
  try {
    if (!parsed) throw new Error("Upload a CSV first.");
    const ctx = readColumns();
    if (!ctx.cols.length) throw new Error("Include at least one column.");
    if (!ctx.primary) throw new Error("Pick a conserved (Amount) column — it is the value reconciled to zero.");
    const plan = readPlan(ctx);
    const data = buildDataset({ header: parsed.header, rows: parsed.rows, columns: ctx.columns, plan, name: ctx.name });
    if (!data.display.length) throw new Error("No data rows after parsing.");
    await startApp(data);
    show("app");
  } catch (e) {
    console.error("run recon failed:", e);
    show("setup");
    $("setup-err").textContent = "Could not run: " + (e && e.message ? e.message : e);
  }
}

function onFile(file) {
  const reader = new FileReader();
  reader.onload = () => {
    parsed = parseCsv(String(reader.result));
    parsed.name = file.name.replace(/\.[^.]+$/, "");
    if (!parsed.header.length) { $("setup-err").textContent = "Could not read any columns from that file."; return; }
    $("setup-err").textContent = "";
    $("file-name").textContent = `${file.name} — ${parsed.rows.length} rows`;
    renderColumns();
  };
  reader.readAsText(file);
}

function wire() {
  const drop = $("dropzone");
  const input = $("file-input");
  drop.onclick = (e) => { if (e.target !== input) input.click(); };
  input.onchange = () => { if (input.files[0]) { onFile(input.files[0]); input.value = ""; } };
  drop.ondragover = (e) => { e.preventDefault(); drop.classList.add("over"); };
  drop.ondragleave = () => drop.classList.remove("over");
  drop.ondrop = (e) => {
    e.preventDefault(); drop.classList.remove("over");
    if (e.dataTransfer.files[0]) onFile(e.dataTransfer.files[0]);
  };
  $("run-recon").onclick = runUpload;
  // Back from the workbench to the column/plan editor. The picker DOM and parsed
  // CSV persist, so the user can retype a column, edit the plan, and re-run.
  const edit = $("edit-plan");
  if (edit) edit.onclick = () => { if (parsed) show("setup"); };
}

show("setup");
wire();
