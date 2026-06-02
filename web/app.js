import { Florecon } from "./core/florecon.js";
import { TagStore } from "./core/tagstore.js";

const TOL = 100; // group is "clean" if |net| <= 1.00 value unit
const WASM = "./core/engine.wasm";
const DATA = "./data.json";

const state = {
  fe: null,
  data: null,
  fields: [],           // portable display descriptor from data.json
  slicers: [],          // [{key,label,valueOf}] system dims first, then data dims
  detailCols: [],       // [{label, kind, render}]
  valueKey: "usd",      // which amount field is the conserved "value"
  netKey: "native",     // display key holding the engine-conserved amount
  displayById: new Map(),
  tags: null,            // host-side TagStore (review/attention overlay)
  slicerByKey: new Map(),
  report: null,
  lines: [],            // joined display + group attrs
  groupsById: new Map(),
  filters: new Map(),   // dim -> Set(values)
  selectedGids: new Set(),
  selectedLines: new Set(), // line ids selected in the detail pane
  shownLineIds: [],     // ids currently shown in the detail pane (for select-all)
  hlIndex: new Map(),   // col -> (value -> [td]) for hover cross-highlight
  hlCur: null,
  search: "",
  sort: { key: "value", dir: -1 },
  detailSort: { key: "date", dir: 1 },
};

const $ = (id) => document.getElementById(id);
const SHORT = { live: "L", frozen: "F", unmatched: "U", exception: "E", manual: "M" };
const fmt = (cents) => (cents / 100).toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
const fmt0 = (cents) => (cents / 100).toLocaleString("en-US", { maximumFractionDigits: 0 });
const esc = (s) => (s ?? "").toString().replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

// Stable dataset fingerprint for the localStorage namespace: pair + the sorted
// row ids. Row ids are the stable identity tags are keyed by, so the same book
// rehydrates the same tag overlay across reloads (djb2-ish, base36).
function datasetHash(data) {
  const ids = (data.display || []).map((d) => d.id).sort((a, b) => a - b);
  let h = 5381 >>> 0;
  const mix = (s) => { for (let i = 0; i < s.length; i++) h = (((h << 5) + h) ^ s.charCodeAt(i)) >>> 0; };
  mix(String(data.pair || ""));
  mix("|" + ids.length + "|");
  mix(ids.join(","));
  return (data.pair || "ds").replace(/[^\w]+/g, "_") + "." + h.toString(36);
}

function setStatus(msg, err = false) {
  const el = $("status");
  el.textContent = msg;
  el.classList.toggle("err", err);
}

// ---- field-driven configuration -----------------------------------------
// Slicers and detail columns are derived from data.json `fields`, so a book
// of a different shape ports by shipping a different descriptor.
function configureFromFields() {
  const fields = state.data.fields || [];
  state.fields = fields;
  const valField = fields.find((f) => f.value) || { amt: "usd" };
  state.valueKey = valField.amt || "usd";
  // The engine conserves its plan `amount` column, which may differ from the
  // displayed value (e.g. native vs usd). Datasets name it explicitly; default
  // to the legacy `native` so the bundled demo keeps working.
  state.netKey = state.data.netKey || "native";

  // System slicers (engine-derived) lead; underlying-data dims follow.
  const sys = [
    { key: "status", label: "status", system: true },
    { key: "origin", label: "strat", system: true },
    { key: "month", label: "month", system: true },
  ];
  const dataDims = fields
    .filter((f) => f.slicer && f.kind === "dim")
    .map((f) => ({ key: f.key, label: f.label }));
  state.slicers = [...sys, ...dataDims].map((s) => ({
    ...s, valueOf: (l) => l[s.key],
  }));
  // Tag facet: a many-to-many overlay, so it exposes `valuesOf` (a list)
  // instead of `valueOf` (one value). A line can match several tag values.
  state.slicers.push({
    key: "tag", label: "tag", system: true, multi: true,
    valuesOf: (l) => [...state.tags.tagsOf(l.id)],
    labelOf: (tid) => state.tags.label(tid),
    colorOf: (tid) => state.tags.color(tid),
  });
  state.slicerByKey = new Map(state.slicers.map((s) => [s.key, s]));

  // Detail columns: business fields in declared order, then engine columns.
  const amt = (l, k) => Number(l[k] || 0);
  const cols = [];
  for (const f of fields) {
    if (!f.detail) continue;
    if (f.kind === "amount") {
      cols.push({
        key: f.key, label: f.label, num: true,
        sortVal: (l) => amt(l, f.amt), hl: (l) => String(amt(l, f.amt)),
        render: (l) => {
          const v = amt(l, f.amt);
          const ccy = f.ccy ? esc(l[f.ccy]) + " " : "";
          return `<span class="${v >= 0 ? "pos" : "neg"}">${ccy}${fmt(v)}</span>`;
        },
      });
    } else if (f.kind === "date") {
      cols.push({
        key: f.key, label: f.label,
        sortVal: (l) => l[f.key] || "", hl: (l) => l[f.key] || "",
        render: (l) => esc(l[f.key]),
      });
    } else {
      cols.push({
        key: f.key, label: f.label, wide: f.key === "ref" || f.key === "account",
        sortVal: (l) => l[f.key] || "", hl: (l) => l[f.key] || "",
        render: (l) => `<span title="${esc(l[f.key])}">${esc(l[f.key])}</span>`,
      });
    }
  }
  // Engine columns lead so matching is legible at a glance; compact status
  // legend (L/F/U) saves width, group # is clickable to focus that group.
  const sysCols = [
    { key: "status", label: "st", sortVal: (l) => l.status, hl: (l) => l.status,
      render: (l) =>
        `<span class="badge b-${l.status}" title="${l.status}">${SHORT[l.status] || "?"}</span>` },
    { key: "grp", label: "grp", sortVal: (l) => l.gid, hl: (l) => String(l.gid),
      render: (l) => l.gid >= 0
        ? `<span class="gref" data-gid="${l.gid}" title="focus group ${l.gid}">#${l.gid}</span>`
        : `<span class="dim">\u2014</span>` },
    { key: "tags", label: "tags", sortVal: (l) => state.tags.tagsOf(l.id).size, hl: () => "",
      render: (l) => {
        const t = [...state.tags.tagsOf(l.id)];
        return t.length
          ? t.map((tid) => `<span class="tag-chip" style="--c:${state.tags.color(tid)}">${esc(state.tags.label(tid))}</span>`).join("")
          : `<span class="dim">\u2014</span>`;
      } },
  ];
  state.detailCols = [...sysCols, ...cols];
}

// ---- boot ----------------------------------------------------------------
async function startApp(data) {
  setStatus("loading wasm + data…");
  if (!state.fe) state.fe = await Florecon.load(WASM);
  state.data = data;
  state.displayById = new Map();
  for (const d of state.data.display) state.displayById.set(d.id, d);
  // Tags persist under a key derived from a dataset hash (no dataset identity
  // exists in the wire today — derive one from the pair + the sorted row ids).
  state.tags = new TagStore(datasetHash(state.data));
  configureFromFields();

  setStatus(`init: ${state.data.rows.length} rows…`);
  const init = state.fe.dispatch({
    op: "init", schema: state.data.schema, plan: state.data.plan, rows: state.data.rows,
  });
  if (!init.ok) return setStatus("init error: " + init.error, true);
  solve();
  wireUi();
}

// Load the bundled interco demo dataset and start.
export async function startDemo() {
  const data = await (await fetch(DATA)).json();
  return startApp(data);
}

export { startApp };

function solve() {
  const t0 = performance.now();
  const r = state.fe.dispatch({ op: "solve" });
  if (!r.ok) return setStatus("solve error: " + r.error, true);
  state.report = r.report;
  rebuild();
  render();
  setStatus(`${state.data.pair} — solved in ${(performance.now() - t0).toFixed(0)} ms · wasm interactive workspace`);
}

function command(cmd) {
  const r = state.fe.dispatch(cmd);
  if (!r.ok) return setStatus(cmd.op + " error: " + r.error, true);
  state.report = r.report;
  rebuild();
  render();
}

// ---- join report -> lines ------------------------------------------------
function rebuild() {
  const rep = state.report;
  const gidOf = new Map();
  for (const [id, gid] of rep.assignments) gidOf.set(id, gid);

  state.groupsById = new Map();
  for (const g of rep.groups) {
    state.groupsById.set(g.group_id, {
      // The wire carries only `status`; derive the local `frozen` convenience
      // boolean here so the rest of the UI can stay terse.
      ...g, frozen: g.status === "frozen", members: [], value: 0, clean: Math.abs(g.net) <= TOL,
    });
  }
  // drop selections that no longer exist after a recalc/breakup
  for (const gid of [...state.selectedGids])
    if (!state.groupsById.has(gid)) state.selectedGids.delete(gid);

  const vk = state.valueKey;
  state.lines = state.data.display.map((d) => {
    // Every id now lands in exactly one group (no separate residual set); an
    // unmatched row is a live singleton group (origin "unmatched").
    const gid = gidOf.has(d.id) ? gidOf.get(d.id) : -1;
    const g = state.groupsById.get(gid);
    if (g) { g.members.push(d.id); g.value += Math.abs(Number(d[vk] || 0)); }
    const matched = g ? g.size >= 2 : false;
    return {
      ...d,
      gid,
      month: (d.date || "").slice(0, 7),
      origin: g ? g.origin : "unmatched",
      // Render-time status from (status × arity): live match / frozen match /
      // live singleton (unmatched) / frozen singleton (accepted exception).
      status: g
        ? (g.frozen ? (matched ? "frozen" : "exception") : (matched ? "live" : "unmatched"))
        : "unmatched",
      clean: g ? g.clean : false,
    };
  });
}

// ---- filtering -----------------------------------------------------------
function lineMatches(l, exceptDim) {
  for (const [dim, set] of state.filters) {
    if (dim === exceptDim || set.size === 0) continue;
    const sl = state.slicerByKey.get(dim);
    if (sl && sl.valuesOf) {
      // many-to-many (tags): match if the line carries ANY of the active values
      const vals = sl.valuesOf(l);
      if (!vals.some((v) => set.has(v))) return false;
    } else if (!set.has(l[dim])) return false;
  }
  if (state.search) {
    const q = state.search.toLowerCase();
    if (!(`${l.id} ${l.account} ${l.ref} ${l.doc}`.toLowerCase().includes(q))) return false;
  }
  return true;
}
const filteredLines = () => state.lines.filter((l) => lineMatches(l, null));

// small ▲/▼ indicator for the active sort column
const sortArrow = (key, s) =>
  s.key === key ? `<span class="arrow">${s.dir > 0 ? "\u25B2" : "\u25BC"}</span>` : "";

// ---- render --------------------------------------------------------------
function render() {
  const fl = filteredLines();
  renderMetrics(fl);
  renderFacets();
  renderGroups(fl);
  renderDetail(fl);
}

function renderMetrics(fl) {
  const total = state.lines.length;
  const shown = fl.length;
  const vk = state.valueKey;
  const matched = fl.filter((l) => l.status === "live" || l.status === "frozen");
  const valTot = fl.reduce((a, l) => a + Math.abs(Number(l[vk] || 0)), 0);
  const valMatched = matched.reduce((a, l) => a + Math.abs(Number(l[vk] || 0)), 0);
  const frozen = [...state.groupsById.values()].filter((g) => g.frozen).length;
  // residual = live singleton groups (status==live && size==1).
  const resid = state.lines.filter((l) => l.status === "unmatched").length;

  const m = [
    ["lines", `${shown.toLocaleString()}${shown < total ? ` / ${total.toLocaleString()}` : ""}`, ""],
    ["matched", `${(100 * matched.length / Math.max(shown, 1)).toFixed(1)}%`, ""],
    ["value matched", `${(100 * valMatched / Math.max(valTot, 1)).toFixed(1)}%`, ""],
    [`value (${vk})`, fmt0(valTot), ""],
    ["residual", resid.toLocaleString(), resid ? "" : "good"],
    ["frozen", String(frozen), ""],
  ];
  $("metrics").innerHTML = m.map(([k, v, cls]) =>
    `<div class="metric"><span class="v ${cls}">${v}</span><span class="k">${k}</span></div>`).join("");
}

function renderFacets() {
  const host = $("facet-list");
  host.innerHTML = "";
  const vk = state.valueKey;
  for (const sl of state.slicers) {
    // cross-filter: count this dim over lines filtered by every OTHER dim
    const base = state.lines.filter((l) => lineMatches(l, sl.key));
    const agg = new Map();
    for (const l of base) {
      // single-value slicers contribute one value; the tag facet contributes a
      // list (a line in N buckets bumps N facet rows). Untagged lines add none.
      const vals0 = sl.valuesOf ? sl.valuesOf(l) : [sl.valueOf(l) || "—"];
      for (const v of vals0) {
        const e = agg.get(v) || { n: 0, val: 0 };
        e.n++; e.val += Math.abs(Number(l[vk] || 0)); agg.set(v, e);
      }
    }
    // hide an empty many-to-many facet (no tags yet) so it is not a bare header
    if (sl.multi && agg.size === 0) continue;
    let vals = [...agg.entries()].sort((a, b) => b[1].n - a[1].n);
    const max = vals.length ? vals[0][1].n : 1;
    const active = state.filters.get(sl.key) || new Set();
    const cap = 12;
    const more = vals.length - cap;
    vals = vals.slice(0, cap);

    const div = document.createElement("div");
    div.className = "facet" + (sl.system ? " sys" : "");
    div.innerHTML = `<h4>${esc(sl.label)}</h4>` + vals.map(([v, e]) => {
      const on = active.has(v) ? " active" : "";
      const w = (100 * e.n / max).toFixed(0);
      // tag facet renders the human label + a colour swatch; data-val stays the
      // stable TagId so filtering/cross-filter keep working.
      const lbl = sl.labelOf ? sl.labelOf(v) : v;
      const dot = sl.colorOf ? `<span class="tag-dot" style="background:${esc(sl.colorOf(v))}"></span>` : "";
      return `<div class="facet-val${on}" data-dim="${esc(sl.key)}" data-val="${esc(v)}">
        <span class="bar" style="width:${w}%"></span>
        <span class="lbl" title="${esc(lbl)}">${dot}${esc(lbl)}</span>
        <span class="cnt">${e.n}</span></div>`;
    }).join("") + (more > 0 ? `<div class="facet-val muted"><span class="lbl dim">+${more} more…</span></div>` : "");
    host.appendChild(div);
  }
  host.querySelectorAll(".facet-val[data-dim]").forEach((el) => {
    el.onclick = () => toggleFilter(el.dataset.dim, el.dataset.val);
  });
}

function renderGroups(fl) {
  // groups touched by at least one filtered line (cross-filter). Live
  // singletons (unmatched rows) are excluded from the matches table; they are
  // surfaced via the status slicer and detail pane instead.
  const gids = new Set(fl.map((l) => l.gid).filter((g) => g >= 0));
  let rows = [...gids]
    .map((gid) => state.groupsById.get(gid))
    .filter((g) => g && (g.size >= 2 || g.frozen));

  const { key, dir } = state.sort;
  rows.sort((a, b) => {
    const av = key === "status" ? (a.frozen ? 1 : 0) : a[key];
    const bv = key === "status" ? (b.frozen ? 1 : 0) : b[key];
    return av > bv ? dir : av < bv ? -dir : a.group_id - b.group_id;
  });

  const CAP = 600;
  const body = $("groups-body");
  body.innerHTML = rows.slice(0, CAP).map((g) => {
    const sel = state.selectedGids.has(g.group_id) ? " sel" : "";
    const st = g.frozen ? "frozen" : "live";
    const netCls = g.clean ? "" : "dirty";
    return `<tr class="${sel}" data-gid="${g.group_id}">
      <td><span class="badge b-${st}" title="${st}">${SHORT[st] || "?"}</span></td>
      <td>${g.group_id}</td>
      <td><span class="badge o-${g.origin}">${g.origin}</span></td>
      <td class="num">${g.size}</td>
      <td class="num">${fmt0(g.value)}</td>
      <td class="num ${netCls}">${g.clean ? "0" : fmt(g.net)}</td>
    </tr>`;
  }).join("");
  body.querySelectorAll("tr").forEach((tr) => {
    tr.onclick = (e) => toggleGroup(+tr.dataset.gid, e);
  });
  // sort indicator on the active column header
  document.querySelectorAll("#groups-table th[data-sort]").forEach((th) => {
    if (th.dataset.base == null) th.dataset.base = th.textContent;
    th.innerHTML = esc(th.dataset.base) + sortArrow(th.dataset.sort, state.sort);
  });

  const dirty = rows.filter((g) => !g.clean).length;
  const nsel = state.selectedGids.size;
  $("groups-foot").textContent =
    `${rows.length.toLocaleString()} groups in view${rows.length > CAP ? ` (showing ${CAP})` : ""} · ${dirty} not clean`
    + (nsel ? ` · ${nsel} selected` : "");
  $("clear-groupsel").hidden = nsel === 0;
}

function renderDetailHeader() {
  const ind = (k) => sortArrow(k, state.detailSort);
  $("detail-head-row").innerHTML = "<tr>" + state.detailCols.map((c) =>
    `<th class="${c.num ? "num" : ""}" data-sort="${esc(c.key)}">${esc(c.label)}${ind(c.key)}</th>`).join("") + "</tr>";
  $("detail-head-row").querySelectorAll("th[data-sort]").forEach((th) => {
    th.onclick = () => {
      const k = th.dataset.sort;
      state.detailSort = { key: k, dir: state.detailSort.key === k ? -state.detailSort.dir : 1 };
      render();
    };
  });
}

function renderDetail(fl) {
  const head = $("detail-head");
  const acts = $("detail-actions");
  const body = $("detail-body");
  const foot = $("detail-foot");
  const vk = state.valueKey;
  const sel = [...state.selectedGids].map((g) => state.groupsById.get(g)).filter(Boolean);

  let lines, scope;
  if (sel.length) {
    // Group focus: show the FULL union of selected groups, unfiltered, so a
    // selected group is never partially "matched away" by other slicers.
    const ids = new Set();
    for (const g of sel) for (const id of g.members) ids.add(id);
    const byId = new Map(state.lines.map((l) => [l.id, l]));
    lines = [...ids].map((id) => byId.get(id)).filter(Boolean);
    head.innerHTML = sel.length === 1
      ? `Group ${sel[0].group_id} · <span class="badge o-${sel[0].origin}">${sel[0].origin}</span>`
      : `${sel.length} groups · union`;
    scope = "selected groups (full)";
  } else {
    // Faithful timeline: every line the slicers select, nothing hidden by
    // volatile grouping. Group/status columns show the matching inline.
    lines = fl;
    head.textContent = "Detail";
    scope = "filtered";
  }

  // action bar: line-selection tools always on, plus group-focus verbs
  state.shownLineIds = lines.map((l) => l.id);
  const nsl = state.selectedLines.size;
  const focusActs = [];
  if (sel.length) {
    const net = sel.reduce((a, g) => a + g.net, 0);
    const clean = Math.abs(net) <= TOL;
    const frozenAll = sel.every((g) => g.frozen);
    const val = fmt0(lines.reduce((a, l) => a + Math.abs(Number(l[vk] || 0)), 0));
    focusActs.push(
      `<button id="act-freeze">${frozenAll ? "Unfreeze" : "Freeze"}${sel.length > 1 ? " all" : ""}</button>`,
      `<button id="act-breakup">Break up${sel.length > 1 ? " all" : ""}</button>`,
      `<span class="gmeta">${lines.length} lines · ${val} ${vk} · net ${clean ? "0 ✓" : fmt(net)}</span>`);
  }
  const selTool =
    `<span class="seltool">select
      <button class="link" id="sel-all">all</button>
      <button class="link" id="sel-none">none</button>
      <button class="link" id="sel-invert">invert</button></span>`;
  const lineActs = nsl ? [
    `<button id="act-match" class="primary">Match ${nsl}</button>`,
    `<button id="act-freezesel">Freeze ${nsl}</button>`,
    `<button id="act-unmatch">Unmatch ${nsl}</button>`,
    `<button id="act-tag">Tag\u2026</button>`,
    `<button id="act-untag">Untag</button>`,
  ] : [];
  // Commit verbs: a tagged selection is a pre-decision "review bucket".
  // Each verb reuses an existing engine op, then drops the tags (no new state).
  const taggedSel = [...state.selectedLines].filter((id) => state.tags.tagsOf(id).size > 0);
  const commitActs = taggedSel.length ? [
    `<button id="act-promote-match" class="primary">\u2192 Match ${taggedSel.length}</button>`,
    `<button id="act-promote-exc">\u2192 Exceptions</button>`,
    `<button id="act-release">Release</button>`,
  ] : [];
  acts.innerHTML = selTool + lineActs.join("") + commitActs.join("") + focusActs.join("");
  $("sel-all").onclick = () => { for (const id of state.shownLineIds) state.selectedLines.add(id); render(); };
  $("sel-none").onclick = () => { state.selectedLines.clear(); render(); };
  $("sel-invert").onclick = () => {
    for (const id of state.shownLineIds)
      if (state.selectedLines.has(id)) state.selectedLines.delete(id); else state.selectedLines.add(id);
    render();
  };
  if (nsl) {
    $("act-match").onclick = matchSelected;
    $("act-freezesel").onclick = freezeSelected;
    $("act-unmatch").onclick = unmatchSelected;
    $("act-tag").onclick = tagSelected;
    $("act-untag").onclick = untagSelected;
  }
  if (taggedSel.length) {
    $("act-promote-match").onclick = promoteMatch;
    $("act-promote-exc").onclick = promoteExceptions;
    $("act-release").onclick = releaseTagged;
  }
  if (sel.length) {
    $("act-freeze").onclick = () => groupCmds(sel.map((g) =>
      ({ op: sel.every((x) => x.frozen) ? "unfreeze" : "freeze", group_id: g.group_id })));
    $("act-breakup").onclick = () => {
      const cmds = sel.map((g) => ({ op: "breakup", group_id: g.group_id }));
      state.selectedGids.clear(); groupCmds(cmds);
    };
  }

  // sort the lines by the active detail column
  const sc = state.detailCols.find((c) => c.key === state.detailSort.key);
  const dir = state.detailSort.dir;
  lines = lines.slice().sort((a, b) => {
    const av = sc ? sc.sortVal(a) : a.date, bv = sc ? sc.sortVal(b) : b.date;
    return av < bv ? -dir : av > bv ? dir : a.id - b.id;
  });

  const CAP = 2000;
  const cols = state.detailCols;
  body.innerHTML = lines.slice(0, CAP).map((l) => {
    const c = (state.selectedLines.has(l.id) ? " linesel" : "")
      + (state.selectedGids.has(l.gid) ? " insel" : "");
    return `<tr class="${c.trim()}" data-id="${l.id}">` +
      cols.map((col) => {
        const hv = col.hl ? col.hl(l) : "";
        return `<td class="${col.num ? "num" : ""}${col.wide ? " wide" : ""}"` +
          ` data-col="${esc(col.key)}" data-hv="${esc(String(hv))}">${col.render(l)}</td>`;
      }).join("") + `</tr>`;
  }).join("");

  // row select (toggle); the group # cell stops propagation and focuses instead
  body.querySelectorAll("tr[data-id]").forEach((tr) => {
    tr.onclick = () => toggleLine(+tr.dataset.id);
  });
  body.querySelectorAll(".gref[data-gid]").forEach((el) => {
    el.onclick = (e) => { e.stopPropagation(); toggleGroup(+el.dataset.gid, e); };
  });

  // index the rendered (visible) cells for hover cross-highlight
  state.hlIndex = new Map();
  body.querySelectorAll("td[data-col]").forEach((td) => {
    const k = td.dataset.col, v = td.dataset.hv;
    if (v === "" || v === "\u2014") return;
    let m = state.hlIndex.get(k); if (!m) { m = new Map(); state.hlIndex.set(k, m); }
    let a = m.get(v); if (!a) { a = []; m.set(v, a); } a.push(td);
  });
  state.hlCur = null;
  body.onmouseover = (e) => {
    const td = e.target.closest("td[data-col]");
    applyHighlight(td ? td.dataset.col : null, td ? td.dataset.hv : null);
  };
  body.onmouseleave = () => applyHighlight(null, null);

  renderDetailHeader();
  foot.textContent = `${lines.length.toLocaleString()} lines · ${scope}`
    + (lines.length > CAP ? ` (showing ${CAP})` : "")
    + (nsl ? ` · ${nsl} selected` : "");
}

// highlight every visible cell sharing the hovered column + value
function applyHighlight(col, val) {
  if (state.hlCur) for (const td of state.hlCur) td.classList.remove("hl");
  state.hlCur = null;
  if (col == null || val == null || val === "") return;
  const m = state.hlIndex.get(col);
  const arr = m && m.get(val);
  if (!arr) return;
  for (const td of arr) td.classList.add("hl");
  state.hlCur = arr;
}

// ---- interactions --------------------------------------------------------
function toggleFilter(dim, val) {
  let set = state.filters.get(dim);
  if (!set) { set = new Set(); state.filters.set(dim, set); }
  if (set.has(val)) set.delete(val); else set.add(val);
  if (set.size === 0) state.filters.delete(dim);
  render();
}

function toggleGroup(gid, e) {
  const multi = e && (e.metaKey || e.ctrlKey || e.shiftKey);
  if (multi) {
    if (state.selectedGids.has(gid)) state.selectedGids.delete(gid);
    else state.selectedGids.add(gid);
  } else if (state.selectedGids.size === 1 && state.selectedGids.has(gid)) {
    state.selectedGids.clear();
  } else {
    state.selectedGids.clear();
    state.selectedGids.add(gid);
  }
  render();
}

function toggleLine(id) {
  if (state.selectedLines.has(id)) state.selectedLines.delete(id);
  else state.selectedLines.add(id);
  render();
}

// run a batch of group ops, keeping the last good report, then refresh
function groupCmds(cmds) {
  let rep = state.report;
  for (const c of cmds) {
    const r = state.fe.dispatch(c);
    if (!r.ok) return setStatus(c.op + " error: " + r.error, true);
    rep = r.report;
  }
  state.report = rep; rebuild(); render();
}

// Manually match the selected lines into one frozen group. The net is the sum
// of the conserved amount (native) the engine reconciles on.
function matchSelected() {
  const ids = [...state.selectedLines];
  if (ids.length < 2) return setStatus("select at least two lines to match", true);
  const net = ids.reduce((a, id) => a + Number(state.displayById.get(id)?.[state.netKey] || 0), 0);
  const r = state.fe.dispatch({ op: "group", ids, net, origin: "manual" });
  if (!r.ok) return setStatus("match error: " + r.error, true);
  state.selectedLines.clear();
  state.report = r.report; rebuild(); render();
  setStatus(`matched ${ids.length} lines into a manual group`);
}

// Freeze from the selection: live singletons (unmatched rows) become accepted
// exceptions via `freeze_singletons`; live matches get their group frozen.
function freezeSelected() {
  const ids = [...state.selectedLines];
  const singles = [];
  const gids = new Set();
  for (const id of ids) {
    const l = state.lines.find((x) => x.id === id);
    if (!l || l.gid < 0) continue;
    const g = state.groupsById.get(l.gid);
    if (!g || g.frozen) continue;
    if (g.size === 1) singles.push(id); else gids.add(l.gid);
  }
  if (!singles.length && !gids.size)
    return setStatus("nothing to freeze: select live unmatched lines or live matches", true);
  const cmds = [...gids].map((g) => ({ op: "freeze", group_id: g }));
  if (singles.length) cmds.push({ op: "freeze_singletons", ids: singles });
  groupCmds(cmds);
  setStatus(`froze ${singles.length} unmatched + ${gids.size} group(s) from the selection`);
}

// Send the selected lines back to the residual (live groups only).
function unmatchSelected() {
  const ids = [...state.selectedLines];
  if (!ids.length) return;
  const r = state.fe.dispatch({ op: "ungroup", ids });
  if (!r.ok) return setStatus("unmatch error: " + r.error, true);
  state.selectedLines.clear();
  state.report = r.report; rebuild(); render();
  setStatus(`unmatched ${ids.length} lines back to residual`);
}

// ---- tag overlay: host-side, no engine dispatch ----------------------------
// Tag the selected lines into a review bucket. Tags are keyed by row id, so
// they survive recalc automatically.
function tagSelected() {
  const ids = [...state.selectedLines];
  if (!ids.length) return;
  const label = (typeof prompt === "function" ? prompt("Tag selected lines — review bucket name:", "reviewing") : "reviewing");
  const tid = state.tags.ensureTag(label, "bucket");
  if (!tid) return;
  for (const id of ids) state.tags.add(id, tid);
  render();
  setStatus(`tagged ${ids.length} lines as “${state.tags.label(tid)}”`);
}

// Clear every tag on the selected lines.
function untagSelected() {
  const ids = [...state.selectedLines];
  if (!ids.length) return;
  let n = 0;
  for (const id of ids) if (state.tags.tagsOf(id).size) { state.tags.clear(id); n++; }
  render();
  setStatus(`cleared tags on ${n} lines`);
}

const taggedSelection = () => [...state.selectedLines].filter((id) => state.tags.tagsOf(id).size > 0);

// Commit verb: promote a tagged bucket to a frozen manual match (op:"group"),
// then drop the tags. Reuses the same engine op as Match.
function promoteMatch() {
  const ids = taggedSelection();
  if (ids.length < 2) return setStatus("tag at least two lines to promote to a match", true);
  const net = ids.reduce((a, id) => a + Number(state.displayById.get(id)?.[state.netKey] || 0), 0);
  const r = state.fe.dispatch({ op: "group", ids, net, origin: "manual" });
  if (!r.ok) return setStatus("promote error: " + r.error, true);
  for (const id of ids) state.tags.clear(id);
  state.selectedLines.clear();
  state.report = r.report; rebuild(); render();
  setStatus(`promoted ${ids.length} tagged lines to a manual match`);
}

// Commit verb: promote a tagged bucket to accepted exceptions
// (op:"freeze_singletons"), then drop the tags.
function promoteExceptions() {
  const ids = taggedSelection();
  if (!ids.length) return;
  const r = state.fe.dispatch({ op: "freeze_singletons", ids });
  if (!r.ok) return setStatus("promote error: " + r.error, true);
  for (const id of ids) state.tags.clear(id);
  state.selectedLines.clear();
  state.report = r.report; rebuild(); render();
  setStatus(`promoted ${ids.length} tagged lines to accepted exceptions`);
}

// Commit verb: release — just untag; rows stay live and flow back into recalc.
function releaseTagged() {
  const ids = taggedSelection();
  for (const id of ids) state.tags.clear(id);
  render();
  setStatus(`released ${ids.length} tagged lines back to recalc`);
}

function wireUi() {
  $("recalc").onclick = () => solve();
  $("reset").onclick = () => { state.filters.clear(); state.selectedGids.clear(); state.selectedLines.clear(); state.search = ""; boot2(); };
  $("clear-filters").onclick = () => { state.filters.clear(); render(); };
  $("clear-groupsel").onclick = () => { state.selectedGids.clear(); render(); };
  $("freeze-clean").onclick = () => {
    const r = state.fe.dispatch({ op: "freeze_clean", tol: TOL });
    if (!r.ok) return setStatus("freeze_clean error: " + r.error, true);
    const before = [...state.groupsById.values()].filter((g) => g.frozen).length;
    state.report = r.report; rebuild(); render();
    const after = [...state.groupsById.values()].filter((g) => g.frozen).length;
    setStatus(`froze ${after - before} clean groups (${after} frozen total)`);
  };
  document.querySelectorAll("#groups-table th[data-sort]").forEach((th) => {
    th.onclick = () => {
      const k = th.dataset.sort;
      state.sort = { key: k, dir: state.sort.key === k ? -state.sort.dir : -1 };
      render();
    };
  });
}

// re-init the workspace from scratch (Reset)
function boot2() {
  const init = state.fe.dispatch({
    op: "init", schema: state.data.schema, plan: state.data.plan, rows: state.data.rows,
  });
  if (!init.ok) return setStatus("init error: " + init.error, true);
  solve();
}

// entry point is web/setup.js, which builds a dataset (demo or uploaded CSV)
// and calls startApp().
