// Setup screen + entry point: upload a CSV, map its columns to engine roles,
// build the viewer `data` object, and hand it to the workbench. The workbench
// (app.js) is otherwise unchanged and data-driven.

import { startApp } from "./app.js";
import { parseCsv, buildDataset, toCents } from "./ingest.js";

const $ = (id) => document.getElementById(id);
const el = (tag, attrs = {}, kids = []) => {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") n.className = v;
    else if (k === "html") n.innerHTML = v;
    else if (k === "text") n.textContent = v;
    else n.setAttribute(k, v);
  }
  for (const c of [].concat(kids)) n.append(c);
  return n;
};

// Engine roles the mapping UI exposes. `req` roles must be assigned; `multi`
// roles accept several columns (partitions nest; reference columns concatenate).
const ROLES = [
  { key: "amount", label: "Amount", req: true, hint: "the conserved value (money). Net-to-zero is computed on this." },
  { key: "gkey", label: "Group key", hint: "rows sharing this key net against each other first (e.g. account)." },
  { key: "partitions", label: "Partition by", multi: true, hint: "split into independent sub-books solved separately (e.g. entity, currency)." },
  { key: "date", label: "Date", hint: "enables time-windowed flow matching." },
  { key: "tokens", label: "Reference text", multi: true, hint: "one or more free-text columns mined for shared tokens (invoice nos, refs). Ctrl/⌘-click to pick several." },
];

let parsed = null; // { header, rows }

function show(view) {
  $("setup").hidden = view !== "setup";
  $("topbar").hidden = view !== "app";
  $("main").hidden = view !== "app";
  $("status").hidden = view !== "app";
}

function colOptions(includeNone) {
  const opts = [];
  if (includeNone) opts.push(el("option", { value: "" }, "— none —"));
  parsed.header.forEach((h, i) =>
    opts.push(el("option", { value: String(i) }, `${h || "(col " + (i + 1) + ")"}`)));
  return opts;
}

// Heuristic default: first numeric-looking column for amount, first date-ish
// column for date — just a starting guess the user can override.
function guess(role) {
  const looksNum = (i) => parsed.rows.slice(0, 20).some((r) => /^[\s$(-]*[\d,]+\.?\d*\)?$/.test(String(r[i] ?? "").trim()) && /\d/.test(r[i] ?? ""));
  const nameHas = (re) => parsed.header.findIndex((h) => re.test(h));
  if (role === "amount") { const n = nameHas(/amount|amt|value|total|debit|credit|sum/i); return n >= 0 ? n : parsed.header.findIndex((_, i) => looksNum(i)); }
  if (role === "date") return nameHas(/date|day|posted|gl_?date/i);
  if (role === "gkey") return nameHas(/account|gl|objsub|category|class/i);
  if (role === "tokens") return nameHas(/desc|ref|memo|narrative|detail|invoice|comment/i);
  return -1;
}

function renderMapping() {
  const body = $("map-body");
  body.innerHTML = "";
  for (const role of ROLES) {
    const g = guess(role.key);
    let control;
    if (role.multi) {
      // A multi-select listing every column; pick none, one, or several.
      const sel = el("select", { id: "map-" + role.key, multiple: "", size: "4" }, colOptions(false));
      if (g >= 0) sel.options[g].selected = true;
      control = sel;
    } else {
      const s = el("select", { id: "map-" + role.key }, colOptions(!role.req));
      if (g >= 0) s.value = String(g);
      control = s;
    }
    body.append(el("div", { class: "map-row" }, [
      el("label", { class: role.req ? "req" : "" }, role.label),
      control,
      el("div", { class: "hint", text: role.hint }),
    ]));
  }
  const tolWrap = el("div", { class: "map-row" }, [
    el("label", {}, "Net tolerance"),
    el("input", { id: "map-tol", type: "number", value: "0", min: "0", step: "0.01" }),
    el("div", { class: "hint", text: "a group is 'clean' if |net| ≤ this, in the amount's own unit (e.g. dollars)." }),
  ]);
  body.append(tolWrap);
  renderPreview();
  $("map-panel").hidden = false;
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

function readMapping() {
  const one = (id) => { const e = $(id); return e && e.value !== "" ? Number(e.value) : null; };
  const many = (id) => { const e = $(id); return e ? [...e.selectedOptions].map((o) => Number(o.value)) : []; };
  // Tolerance is entered in the amount's own unit (e.g. dollars); the engine
  // works in integer minor units, so scale it the same way amounts are scaled.
  const tol = toCents($("map-tol") ? $("map-tol").value : 0);
  return {
    amount: one("map-amount"),
    gkey: one("map-gkey"),
    date: one("map-date"),
    tokens: many("map-tokens"),
    partitions: many("map-partitions"),
    tol: Math.max(0, tol),
    name: parsed.name || "uploaded",
  };
}

async function runUpload() {
  $("setup-err").textContent = "";
  try {
    if (!parsed) throw new Error("Upload a CSV first.");
    const m = readMapping();
    if (m.amount == null) throw new Error("Pick an Amount column — it is the conserved value.");
    const data = buildDataset({ header: parsed.header, rows: parsed.rows, mapping: m });
    if (!data.display.length) throw new Error("No data rows after parsing.");
    // Build + solve while still on the setup screen; only reveal the workbench
    // once it succeeds, so a failure stays visible here instead of a blank app.
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
    renderMapping();
  };
  reader.readAsText(file);
}

function wire() {
  const drop = $("dropzone");
  const input = $("file-input");
  drop.onclick = (e) => {
    if (e.target !== input) {
      input.click();
    }
  };
  input.onchange = () => { 
    if (input.files[0]) {
      onFile(input.files[0]); 
      input.value = "";
    }
  };
  drop.ondragover = (e) => { e.preventDefault(); drop.classList.add("over"); };
  drop.ondragleave = () => drop.classList.remove("over");
  drop.ondrop = (e) => {
    e.preventDefault(); drop.classList.remove("over");
    if (e.dataTransfer.files[0]) onFile(e.dataTransfer.files[0]);
  };
  $("run-recon").onclick = runUpload;
}

show("setup");
wire();
