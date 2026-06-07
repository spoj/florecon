// florecon workbench — the interactive review UI. Data-driven from the plugin's
// describe(): no column is hard-coded, so the same page drives any florecon
// plugin. All client-side; the wasm + your data never leave the browser.
//
// This is the DOM layer; everything beneath it (host, ingest, persist, tags,
// projections) is verified head-less under node (see core/*.test). The flow:
//   load plugin .wasm -> describe() -> map CSV columns -> upsert + solve
//   -> review (facets / groups / detail) -> pin / merge / tag -> save / export.
import { Workspace } from "./core/florecon.js";
import { TagStore } from "./core/tagstore.js";
import { primaryAssignments } from "./core/projections.js";
import {
  serialize,
  parse,
  applyDecisions,
  groupsCsv,
  resultsCsv,
  resultJson,
  download,
} from "./core/persist.js";
import { parseCsv, buildBatch } from "./ingest.js";

const $ = (id) => document.getElementById(id);
const esc = (s) =>
  String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const money = (cents) => (Number(cents || 0) / 100).toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 });

const state = {
  ws: null,
  wasmBytes: null,
  fields: [],
  csv: null, // {header, rows}
  mapping: {},
  source: null, // ingest source echo (for save)
  report: { groups: [], allocations: [] },
  display: [], // row echo from ingest, each has __id
  byId: new Map(), // id -> display row
  idAmount: new Map(), // id -> engine-native original amount (sum of allocations)
  lines: [], // display joined to primary group
  tags: new TagStore(),
  selected: new Set(),
  filters: new Map(), // field -> Set(values)
  tagFilter: new Set(),
  sort: { key: "__id", dir: 1 },
};

// ── setup ──────────────────────────────────────────────────────────────────
$("wasm-file").addEventListener("change", async (e) => {
  const f = e.target.files[0];
  if (!f) return;
  try {
    state.wasmBytes = await f.arrayBuffer();
    await loadPlugin();
  } catch (err) {
    setupMsg(err.message, true);
  }
});

$("csv-file").addEventListener("change", async (e) => {
  const f = e.target.files[0];
  if (!f) return;
  state.csv = parseCsv(await f.text());
  setupMsg(`loaded ${state.csv.rows.length.toLocaleString()} rows × ${state.csv.header.length} columns`);
  renderMapping();
});

$("ws-file").addEventListener("change", async (e) => {
  const f = e.target.files[0];
  if (!f) return;
  await restoreWorkspace(parse(await f.text()));
});

$("solve-btn").addEventListener("click", () => runSolve());

async function loadPlugin() {
  const config = parseConfig();
  state.ws = await Workspace.load(state.wasmBytes.slice(0), config ? { config } : {});
  state.fields = state.ws.fields;
  $("domain").textContent = `${state.ws.domain.id} v${state.ws.domain.version} · ${state.fields.length} columns`;
  setupMsg("plugin loaded — now choose a CSV");
  renderMapping();
}

function parseConfig() {
  const t = $("config").value.trim();
  if (!t) return null;
  try {
    return JSON.parse(t);
  } catch {
    setupMsg("config is not valid JSON — ignoring", true);
    return null;
  }
}

function renderMapping() {
  if (!state.ws || !state.csv) return;
  const sel = (name) => {
    const opts = ['<option value="">— none —</option>']
      .concat(state.csv.header.map((h, i) => `<option value="${i}">${esc(h)}</option>`))
      .join("");
    return `<select data-field="${esc(name)}">${opts}</select>`;
  };
  $("map-grid").innerHTML = state.fields
    .map(
      (f) => `<div class="m"><span><code>${esc(f.name)}</code> <span class="ty">${f.type}</span>${
        f.amount ? ' <span class="amt-badge">▲ amount</span>' : ""
      }</span>${sel(f.name)}</div>`
    )
    .join("");
  // auto-map by exact (then case-insensitive) header match
  $("map-grid")
    .querySelectorAll("select")
    .forEach((s) => {
      const name = s.dataset.field;
      let ci = state.csv.header.indexOf(name);
      if (ci < 0) ci = state.csv.header.findIndex((h) => h.toLowerCase() === name.toLowerCase());
      if (ci >= 0) s.value = String(ci);
    });
  $("mapping").classList.remove("hidden");
}

function readMapping() {
  const m = {};
  $("map-grid")
    .querySelectorAll("select")
    .forEach((s) => {
      m[s.dataset.field] = s.value === "" ? null : Number(s.value);
    });
  return m;
}

function runSolve() {
  try {
    state.mapping = readMapping();
    const built = buildBatch({ header: state.csv.header, rows: state.csv.rows, fields: state.fields, mapping: state.mapping });
    state.display = built.display;
    state.source = built.source;
    state.ws.upsert(built.table);
    state.ws.solve();
    enterWorkbench();
  } catch (err) {
    setupMsg(err.message, true);
  }
}

async function restoreWorkspace(obj) {
  // Need a live plugin + data. Either re-upsert from the embedded echo, or the
  // operator already loaded the same CSV + plugin above.
  if (!state.ws) return setupMsg("load the plugin .wasm first, then restore", true);
  const ds = obj.dataset || state.source;
  if (!ds) return setupMsg("no dataset to restore against — load the CSV first", true);
  state.mapping = ds.mapping;
  state.csv = state.csv || { header: ds.header, rows: ds.rows };
  const built = buildBatch({ header: state.csv.header, rows: state.csv.rows, fields: state.fields, mapping: state.mapping });
  state.display = built.display;
  state.source = built.source;
  state.ws.upsert(built.table);
  state.ws.solve();
  state.tags.restore(obj.tags);
  const summary = applyDecisions(state.ws, obj.decisions || []);
  enterWorkbench();
  workMsg(`restored: ${summary.groups} groups, ${summary.singles} singletons${summary.failed ? `, ${summary.failed} failed` : ""}`);
}

function enterWorkbench() {
  $("setup").classList.add("hidden");
  $("work").classList.remove("hidden");
  refresh();
}

// ── join + filter ────────────────────────────────────────────────────────
function refresh() {
  state.report = state.ws.report();
  state.byId = new Map(state.display.map((d) => [d.__id, d]));
  state.idAmount = new Map();
  for (const a of state.report.allocations) state.idAmount.set(a.id, (state.idAmount.get(a.id) || 0) + a.amount);
  const gmeta = new Map(state.report.groups.map((g) => [g.group_id, g]));
  const prim = new Map(primaryAssignments(state.report));
  state.lines = state.display.map((d) => {
    const gid = prim.get(d.__id);
    const g = gmeta.get(gid) || {};
    return { ...d, id: d.__id, gid, origin: g.origin, status: g.status, gnet: g.net || 0, gsize: g.size || 1 };
  });
  render();
}

function passes(l, exceptField) {
  for (const [field, set] of state.filters)
    if (field !== exceptField && set.size && !set.has(String(l[field]))) return false;
  if (state.tagFilter.size) {
    const ts = state.tags.tagsOf(l.id);
    if (![...state.tagFilter].some((t) => ts.has(t))) return false;
  }
  return true;
}
const filtered = () => state.lines.filter((l) => passes(l, null));

// ── render ─────────────────────────────────────────────────────────────────
function render() {
  const fl = filtered();
  renderMetrics(fl);
  renderFacets();
  renderGroups(fl);
  renderDetail(fl);
}

function renderMetrics(fl) {
  const groups = new Set(fl.filter((l) => l.gsize > 1).map((l) => l.gid)).size;
  const matched = fl.filter((l) => l.gsize > 1).length;
  const residual = fl.filter((l) => l.gsize <= 1).length;
  const pinned = new Set(fl.filter((l) => l.status === "pinned").map((l) => l.gid)).size;
  const imbalance = state.report.groups.filter((g) => g.status !== "pinned").reduce((s, g) => s + Math.abs(g.net), 0);
  const m = (b, s) => `<div class="m"><b>${b}</b><span>${s}</span></div>`;
  $("metrics").innerHTML =
    m(fl.length.toLocaleString(), "rows") +
    m(groups.toLocaleString(), "matched groups") +
    m(`${((100 * matched) / Math.max(fl.length, 1)).toFixed(1)}%`, "matched rows") +
    m(residual.toLocaleString(), "residual rows") +
    m(pinned.toLocaleString(), "pinned") +
    m(money(imbalance), "open imbalance");
}

function renderFacets() {
  const host = $("facets");
  const dims = state.fields.filter((f) => f.type === "utf8");
  let html = "";
  for (const f of dims) {
    const counts = new Map();
    for (const l of state.lines.filter((x) => passes(x, f.name))) {
      const v = String(l[f.name] ?? "");
      if (v) counts.set(v, (counts.get(v) || 0) + 1);
    }
    if (counts.size === 0 || counts.size > 60) continue; // skip free-text / id-like
    const sel = state.filters.get(f.name) || new Set();
    const vals = [...counts.entries()].sort((a, b) => b[1] - a[1]).slice(0, 30);
    html += `<div class="facet"><h4>${esc(f.name)}</h4>${vals
      .map(
        ([v, c]) =>
          `<div class="facet-val ${sel.has(v) ? "on" : ""}" data-dim="${esc(f.name)}" data-val="${esc(v)}"><span>${esc(v)}</span><span class="c">${c}</span></div>`
      )
      .join("")}</div>`;
  }
  // tags facet
  if (state.tags.meta.size) {
    html += `<div class="facet"><h4>tags</h4>${[...state.tags.meta.keys()]
      .map((t) => {
        const c = state.tags.tagged(t).length;
        return `<div class="facet-val ${state.tagFilter.has(t) ? "on" : ""}" data-tag="${esc(t)}"><span><span class="swatch" style="background:${state.tags.color(t)}"></span>${esc(state.tags.label(t))}</span><span class="c">${c}</span></div>`;
      })
      .join("")}</div>`;
  }
  host.innerHTML = html || '<div class="dim">no slicers</div>';
  host.querySelectorAll(".facet-val[data-dim]").forEach((el) => {
    el.onclick = () => {
      const f = el.dataset.dim,
        v = el.dataset.val;
      const set = state.filters.get(f) || new Set();
      set.has(v) ? set.delete(v) : set.add(v);
      set.size ? state.filters.set(f, set) : state.filters.delete(f);
      render();
    };
  });
  host.querySelectorAll(".facet-val[data-tag]").forEach((el) => {
    el.onclick = () => {
      const t = el.dataset.tag;
      state.tagFilter.has(t) ? state.tagFilter.delete(t) : state.tagFilter.add(t);
      render();
    };
  });
}

function renderGroups(fl) {
  const ids = new Set(fl.map((l) => l.gid));
  const groups = state.report.groups.filter((g) => ids.has(g.group_id) && g.size > 1);
  groups.sort((a, b) => Math.abs(b.net) - Math.abs(a.net) || b.size - a.size);
  const thead = $("groups").querySelector("thead");
  const tbody = $("groups").querySelector("tbody");
  thead.innerHTML = `<tr><th>group</th><th>origin</th><th>status</th><th class="num">size</th><th class="num">net</th><th>actions</th></tr>`;
  tbody.innerHTML = groups
    .slice(0, 400)
    .map(
      (g) => `<tr class="${g.status === "pinned" ? "pinned" : ""}" data-g="${g.group_id}">
        <td>${g.group_id}</td><td>${esc(g.origin)}${g.reason ? ` <span class="dim">${esc(g.reason)}</span>` : ""}</td>
        <td><span class="pill ${g.status}">${g.status}</span></td>
        <td class="num">${g.size}</td><td class="num">${money(g.net)}</td>
        <td>${
          g.status === "pinned"
            ? `<button data-vact="unpin" data-g="${g.group_id}">unpin</button>`
            : `<button data-vact="pin" data-g="${g.group_id}">pin</button>`
        } <button data-vact="dissolve" data-g="${g.group_id}">dissolve</button></td>
      </tr>`
    )
    .join("");
  tbody.querySelectorAll("tr").forEach((tr) => {
    tr.onclick = (e) => {
      if (e.target.dataset.vact) return; // button handled below
      const gid = Number(tr.dataset.g);
      const rows = state.report.allocations.filter((a) => a.group_id === gid).map((a) => a.id);
      state.selected = new Set(rows);
      render();
    };
  });
  tbody.querySelectorAll("button[data-vact]").forEach((b) => {
    b.onclick = (e) => {
      e.stopPropagation();
      verb(b.dataset.vact, Number(b.dataset.g));
    };
  });
}

function renderDetail(fl) {
  const cols = state.fields.filter((f) => f.amount || f.type === "utf8").slice(0, 6);
  const thead = $("detail").querySelector("thead");
  const tbody = $("detail").querySelector("tbody");
  thead.innerHTML =
    `<tr><th></th><th class="num" data-sort="__id">id</th>` +
    cols.map((c) => `<th class="${c.amount ? "num" : ""}" data-sort="${esc(c.name)}">${esc(c.name)}</th>`).join("") +
    `<th class="num" data-sort="gid">group</th><th data-sort="status">status</th><th>tags</th></tr>`;
  thead.querySelectorAll("th[data-sort]").forEach((th) => {
    th.onclick = () => {
      const k = th.dataset.sort;
      state.sort = { key: k, dir: state.sort.key === k ? -state.sort.dir : 1 };
      render();
    };
  });
  const dir = state.sort.dir,
    key = state.sort.key;
  const lines = fl.slice().sort((a, b) => {
    const x = a[key],
      y = b[key];
    return (x < y ? -1 : x > y ? 1 : 0) * dir;
  });
  tbody.innerHTML = lines
    .slice(0, 1000)
    .map((l) => {
      const tagHtml = [...state.tags.tagsOf(l.id)]
        .map((t) => `<span class="tag" style="background:${state.tags.color(t)}">${esc(state.tags.label(t))}</span>`)
        .join("");
      return `<tr class="${state.selected.has(l.id) ? "sel" : ""} ${l.status === "pinned" ? "pinned" : ""}" data-id="${l.id}">
        <td><input type="checkbox" ${state.selected.has(l.id) ? "checked" : ""} data-id="${l.id}"></td>
        <td class="num">${l.id}</td>
        ${cols.map((c) => `<td class="${c.amount ? "num" : ""}">${esc(l[c.name])}</td>`).join("")}
        <td class="num">${l.gid ?? ""}</td>
        <td><span class="pill ${l.status || "proposed"}">${l.status || "—"}</span></td>
        <td>${tagHtml}</td>
      </tr>`;
    })
    .join("");
  tbody.querySelectorAll('input[type="checkbox"]').forEach((cb) => {
    cb.onclick = (e) => {
      e.stopPropagation();
      const id = Number(cb.dataset.id);
      cb.checked ? state.selected.add(id) : state.selected.delete(id);
    };
  });
  tbody.querySelectorAll("tr").forEach((tr) => {
    tr.onclick = (e) => {
      if (e.target.tagName === "INPUT") return;
      const id = Number(tr.dataset.id);
      state.selected.has(id) ? state.selected.delete(id) : state.selected.add(id);
      render();
    };
  });
}

// ── verbs / toolbar ─────────────────────────────────────────────────────────
function verb(act, gid) {
  try {
    if (act === "pin") state.ws.pin(gid);
    else if (act === "unpin") state.ws.unpin(gid);
    else if (act === "dissolve") state.ws.dissolve(gid);
    state.ws.solve();
    refresh();
  } catch (err) {
    workMsg(err.message, true);
  }
}

document.querySelector(".toolbar").addEventListener("click", (e) => {
  const act = e.target.dataset.act;
  if (!act) return;
  try {
    if (act === "solve") {
      state.ws.solve();
      refresh();
    } else if (act === "pin-clean") {
      state.ws.pinClean(0);
      state.ws.solve();
      refresh();
      workMsg("pinned all clean (net=0) groups");
    } else if (act === "merge") {
      if (state.selected.size < 2) return workMsg("select ≥ 2 rows to merge", true);
      const reason = prompt("reason for this manual group?", "manual match") || undefined;
      const allocs = [...state.selected].map((id) => ({ id, amount: state.idAmount.get(id) || 0 }));
      state.ws.merge(allocs, "manual", reason);
      state.ws.solve();
      state.selected.clear();
      refresh();
    } else if (act === "tag") {
      if (!state.selected.size) return workMsg("select rows to tag", true);
      const label = prompt("tag label?", "reviewing");
      if (!label) return;
      const t = state.tags.ensureTag(label);
      for (const id of state.selected) state.tags.add(id, t);
      render();
    } else if (act === "save") {
      const obj = serialize(state.report, { tags: state.tags, dataset: state.source, meta: { domain: state.ws.domain } });
      download(`${state.ws.domain.id}.workspace.json`, JSON.stringify(obj, null, 2), "application/json");
    } else if (act === "export-groups") download("groups.csv", groupsCsv(state.report), "text/csv");
    else if (act === "export-results") download("results.csv", resultsCsv(state.report), "text/csv");
    else if (act === "export-json") download("result.json", resultJson(state.report, { meta: { domain: state.ws.domain } }), "application/json");
  } catch (err) {
    workMsg(err.message, true);
  }
});

function setupMsg(m, err) {
  const el = $("setup-msg");
  el.textContent = m;
  el.className = "msg" + (err ? " err" : "");
}
function workMsg(m, err) {
  const el = $("work-msg");
  el.textContent = m;
  el.className = "msg" + (err ? " err" : "");
}
