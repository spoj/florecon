import { Florecon, primaryAssignments } from "./core/florecon.js";
import { TagStore } from "./core/tagstore.js";
import { buildDataset } from "./ingest.js";
import {
  serializeWorkspace, parseWorkspace, applyFrozen,
  resultsCsv, groupsCsv, resultJson, download, pickTextFile, slug,
} from "./core/persist.js";

const TOL = 100; // group is "clean" if |net| <= 1.00 value unit
const WASM = "./core/engine.wasm";

const state = {
  fe: null,
  data: null,
  fields: [],           // portable display descriptor carried alongside the data
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
  // Frontend-only, session-local group numbers. The engine deliberately
  // re-mints live group_id values on each solve; these maps keep the visible
  // # stable when the same allocation set reappears after Recalc.
  groupLabelBySig: new Map(),
  groupLabelByGid: new Map(),
  nextGroupLabel: 1,
  filters: new Map(),   // dim -> Set(values)
  selectedGids: new Set(),
  selectedLines: new Set(), // line ids selected in the detail pane
  shownLineIds: [],     // ids currently shown in the detail pane (for select-all)
  hlIndex: new Map(),   // col -> (value -> [td]) for hover cross-highlight
  hlCur: null,
  search: "",
  sort: { key: "value", dir: -1 },
  detailSort: { key: "date", dir: 1 },
  tagging: false,        // inline tag-entry box open in the detail action bar
};

// Cancellation token for the async/cancellable slicer (filter) recompute. Each
// filter change bumps it; a superseded recompute sees a stale token and bails,
// so only the final selection finishes (see renderFiltered).
let filterToken = 0;

const $ = (id) => document.getElementById(id);
const SHORT = { live: "L", frozen: "F", unmatched: "U", manual: "M" };
const fmt = (cents) => (cents / 100).toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
const fmt0 = (cents) => (cents / 100).toLocaleString("en-US", { maximumFractionDigits: 0 });
const esc = (s) => (s ?? "").toString().replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

// Stable dataset fingerprint, retained only for diagnostics/labels. Tags are
// in-memory for the page session only (see TagStore) — nothing is persisted to
// the browser, so a reload always starts from a clean, predictable slate.
function datasetHash(data) {
  const ids = (data.display || []).map((d) => d.id).sort((a, b) => a - b);
  let h = 5381 >>> 0;
  const mix = (s) => { for (let i = 0; i < s.length; i++) h = (((h << 5) + h) ^ s.charCodeAt(i)) >>> 0; };
  mix(String(data.pair || ""));
  mix("|" + ids.length + "|");
  mix(ids.join(","));
  return (data.pair || "ds").replace(/[^\w]+/g, "_") + "." + h.toString(36);
}

function resetGroupDisplayLabels() {
  state.groupLabelBySig = new Map();
  state.groupLabelByGid = new Map();
  state.nextGroupLabel = 1;
}

// Stable identity of a report group for display purposes: the sorted allocation
// incidences that make up the group. Include amount because split lots can put
// the same row id in more than one allocation group.
function groupSignatures(rep) {
  const byGid = new Map();
  for (const a of rep.allocations || []) {
    if (!byGid.has(a.group_id)) byGid.set(a.group_id, []);
    byGid.get(a.group_id).push({ id: a.id, amount: a.amount });
  }
  const out = new Map();
  for (const [gid, allocs] of byGid) {
    allocs.sort((a, b) => a.id - b.id || a.amount - b.amount);
    out.set(gid, allocs.map((a) => `${a.id}:${a.amount}`).join("|"));
  }
  return out;
}

function assignGroupLabel(gid, sig) {
  let label = state.groupLabelByGid.get(gid);
  if (label == null && sig) label = state.groupLabelBySig.get(sig);
  if (label == null) label = state.nextGroupLabel++;
  state.groupLabelByGid.set(gid, label);
  if (sig) state.groupLabelBySig.set(sig, label);
  return label;
}

const groupNo = (g) => g ? (g.display_id ?? g.group_id) : "?";

function setStatus(msg, err = false) {
  const el = $("status");
  el.textContent = msg;
  el.classList.toggle("err", err);
}

// ---- field-driven configuration -----------------------------------------
// Slicers and detail columns are derived from the viewer `data` object's
// `fields` descriptor, so a book of a different shape ports by building a
// different descriptor.
function configureFromFields() {
  const fields = state.data.fields || [];
  state.fields = fields;
  const valField = fields.find((f) => f.value) || { amt: "usd" };
  state.valueKey = valField.amt || "usd";
  // The engine conserves its plan `amount` column, which may differ from the
  // displayed value (e.g. native vs usd). Datasets name it explicitly; default
  // to `native` when a dataset omits it.
  state.netKey = (typeof state.data.plan?.primary === "string" ? state.data.plan.primary : null) || state.data.netKey || "native";

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
        total: (l) => amt(l, f.amt), fmtTotal: (v) => fmt(v),
        render: (l) => {
          const v = amt(l, f.amt);
          const ccy = f.ccy ? esc(l[f.ccy]) + " " : "";
          return `<span class="${v >= 0 ? "pos" : "neg"}">${ccy}${fmt(v)}</span>`;
        },
      });
    } else if (f.kind === "num") {
      // Display-only numeric column (natural magnitude, not cents). Summed in
      // the detail totals row like the conserved amount, but never reconciled.
      const numFmt = (v) => v.toLocaleString("en-US", { maximumFractionDigits: 2 });
      cols.push({
        key: f.key, label: f.label, num: true,
        sortVal: (l) => Number(l[f.amt] || 0), hl: (l) => String(Number(l[f.amt] || 0)),
        total: (l) => Number(l[f.amt] || 0), fmtTotal: numFmt,
        render: (l) => {
          const v = Number(l[f.amt] || 0);
          return `<span class="${v >= 0 ? "pos" : "neg"}">${numFmt(v)}</span>`;
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
        key: f.key, label: f.label, wide: f.wide || f.key === "ref" || f.key === "account",
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
    { key: "grp", label: "grp", sortVal: (l) => l.display_gid ?? l.gid, hl: (l) => String(l.display_gid ?? l.gid),
      render: (l) => l.gid >= 0
        ? `<span class="gref" data-gid="${l.gid}" title="focus group #${l.display_gid ?? l.gid}">#${l.display_gid ?? l.gid}</span>`
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
  resetGroupDisplayLabels();
  configureFromFields();

  setStatus(`init: ${state.data.display.length} rows…`);
  const init = state.fe.dispatch({
    op: "init", plan: state.data.plan
  }, state.data.arrowBytes);
  if (!init.ok) return setStatus("init error: " + init.error, true);
  solve();
  wireUi();
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
  for (const [id, gid] of primaryAssignments(rep)) gidOf.set(id, gid);

  // Remember selected group signatures so a no-op Recalc keeps focus even
  // though live engine group_id values were intentionally re-minted.
  const selectedSigs = new Set([...state.selectedGids]
    .map((gid) => state.groupsById.get(gid)?.signature)
    .filter(Boolean));

  const sigByGid = groupSignatures(rep);
  state.groupsById = new Map();
  const gidsBySig = new Map();
  for (const g of rep.groups) {
    const signature = sigByGid.get(g.group_id) || `gid:${g.group_id}`;
    const display_id = assignGroupLabel(g.group_id, signature);
    const view = {
      // The wire carries only `status`; derive the local `frozen` convenience
      // boolean here so the rest of the UI can stay terse.
      ...g,
      engine_id: g.group_id,
      display_id,
      signature,
      frozen: g.status === "frozen", members: [], value: 0, netProj: 0, clean: Math.abs(g.net) <= TOL,
    };
    state.groupsById.set(g.group_id, view);
    gidsBySig.set(signature, g.group_id);
  }
  // Drop selections that no longer exist after a recalc/breakup, but preserve
  // them by signature when Recalc only changed the engine's ephemeral ids.
  const nextSel = new Set();
  for (const gid of state.selectedGids) {
    if (state.groupsById.has(gid)) nextSel.add(gid);
  }
  for (const sig of selectedSigs) {
    const gid = gidsBySig.get(sig);
    if (gid != null) nextSel.add(gid);
  }
  state.selectedGids = nextSel;

  const vk = state.valueKey;
  state.lines = state.data.display.map((d) => {
    // The workbench chooses a primary-group projection over the allocation
    // hypergraph for its row table. Raw allocations remain available on report.
    const gid = gidOf.has(d.id) ? gidOf.get(d.id) : -1;
    const g = state.groupsById.get(gid);
    if (g) { g.members.push(d.id); g.value += Math.abs(Number(d[vk] || 0)); g.netProj += Number(d.native || 0); }
    const matched = g ? g.size >= 2 : false;
    return {
      ...d,
      gid,
      display_gid: g ? g.display_id : -1,
      month: (d.date || "").slice(0, 7),
      origin: g ? g.origin : "unmatched",
      // Render-time status from (status × arity). With the exception concept
      // removed there are three states: live match, frozen (accepted, any
      // arity), and live singleton (unmatched/residual).
      status: g
        ? (g.frozen ? "frozen" : (matched ? "live" : "unmatched"))
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
    // Stable universe + order from ALL lines (independent of the current
    // filters), so toggling one slicer never reorders or drops another's chips
    // — a value whose cross-filtered count falls to 0 stays in place, dimmed,
    // and returns to the same screen position when you deselect.
    const total = new Map();
    for (const l of state.lines) {
      const vals0 = sl.valuesOf ? sl.valuesOf(l) : [sl.valueOf(l) || "—"];
      for (const v of vals0) total.set(v, (total.get(v) || 0) + 1);
    }
    // hide an empty many-to-many facet (no tags yet) so it is not a bare header
    if (sl.multi && total.size === 0) continue;

    // current cross-filtered counts (this dim over lines filtered by every
    // OTHER dim). Missing => 0; the chip still renders from the stable universe.
    const base = state.lines.filter((l) => lineMatches(l, sl.key));
    const cur = new Map();
    for (const l of base) {
      const vals0 = sl.valuesOf ? sl.valuesOf(l) : [sl.valueOf(l) || "—"];
      for (const v of vals0) {
        const e = cur.get(v) || { n: 0, val: 0 };
        e.n++; e.val += Math.abs(Number(l[vk] || 0)); cur.set(v, e);
      }
    }

    const active = state.filters.get(sl.key) || new Set();
    // fixed order by total count desc, value as tiebreak (deterministic).
    const order = [...total.keys()].sort((a, b) =>
      total.get(b) - total.get(a) || (a < b ? -1 : a > b ? 1 : 0));
    const max = order.length ? total.get(order[0]) : 1;
    const cap = 12;
    const head = order.slice(0, cap);
    for (const v of active) if (total.has(v) && !head.includes(v)) head.push(v);
    const more = order.length - head.length;

    const div = document.createElement("div");
    div.className = "facet" + (sl.system ? " sys" : "");
    div.innerHTML = `<h4>${esc(sl.label)}</h4>` + head.map((v) => {
      const e = cur.get(v) || { n: 0, val: 0 };
      const on = active.has(v) ? " active" : "";
      const zero = e.n === 0 ? " zero" : "";
      const w = (100 * e.n / max).toFixed(0);
      // tag facet renders the human label + a colour swatch; data-val stays the
      // stable TagId so filtering/cross-filter keep working.
      const lbl = sl.labelOf ? sl.labelOf(v) : v;
      const dot = sl.colorOf ? `<span class="tag-dot" style="background:${esc(sl.colorOf(v))}"></span>` : "";
      return `<div class="facet-val${on}${zero}" data-dim="${esc(sl.key)}" data-val="${esc(v)}" title="click to filter · ⌘/Ctrl-click to add">
        <span class="bar" style="width:${w}%"></span>
        <span class="lbl" title="${esc(lbl)}">${dot}${esc(lbl)}</span>
        <span class="cnt">${e.n}</span></div>`;
    }).join("") + (more > 0 ? `<div class="facet-val muted"><span class="lbl dim">+${more} more…</span></div>` : "");
    host.appendChild(div);
  }
  host.querySelectorAll(".facet-val[data-dim]").forEach((el) => {
    // Plain click = single-select (replace this dim's selection, or toggle off
    // if it was the only active value). ⌘/Ctrl/Shift-click = additive toggle.
    // Optimistic chip feedback; the cancellable recompute reconciles the rest.
    el.onclick = (e) => {
      const additive = e.ctrlKey || e.metaKey || e.shiftKey;
      if (!additive)
        el.closest(".facet").querySelectorAll(".facet-val.active")
          .forEach((x) => { if (x !== el) x.classList.remove("active"); });
      el.classList.toggle("active");
      selectFilter(el.dataset.dim, el.dataset.val, additive);
    };
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
    const pick = (g) =>
      key === "status" ? (g.frozen ? 1 : 0)
      : key === "group_id" ? g.display_id
      : key === "reason" ? (g.reason || g.origin || "")
      : key === "net" ? g.netProj
      : g[key];
    const av = pick(a), bv = pick(b);
    return av > bv ? dir : av < bv ? -dir : a.display_id - b.display_id;
  });

  const CAP = 600;
  const body = $("groups-body");
  body.innerHTML = rows.slice(0, CAP).map((g) => {
    const sel = state.selectedGids.has(g.group_id) ? " sel" : "";
    const st = g.frozen ? "frozen" : "live";
    const netCls = Math.abs(g.netProj) <= TOL ? "" : "dirty";
    return `<tr class="${sel}" data-gid="${g.group_id}" title="engine group_id ${g.group_id}">
      <td><span class="badge b-${st}" title="${st}">${SHORT[st] || "?"}</span></td>
      <td>${g.display_id}</td>
      <td><span class="badge o-${g.origin}">${g.origin}</span></td>
      <td class="why" title="${esc(g.reason || "")}">${esc(g.reason || "")}</td>
      <td class="num">${g.size}</td>
      <td class="num">${fmt0(g.value)}</td>
      <td class="num ${netCls}">${fmt(g.netProj)}</td>
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
      ? `Group #${groupNo(sel[0])} · <span class="badge o-${sel[0].origin}">${sel[0].origin}</span>`
        + (sel[0].reason ? ` · <span class="why-head">${esc(sel[0].reason)}</span>` : "")
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
  const tgt = nsl || lines.length;        // act on the selection, else all visible
  const onAll = !nsl && lines.length > 0; // operating on the whole visible set
  const focusActs = [];
  if (sel.length) {
    const net = sel.reduce((a, g) => a + g.net, 0);
    const clean = Math.abs(net) <= TOL;
    const frozenAll = sel.every((g) => g.frozen);
    const val = fmt0(lines.reduce((a, l) => a + Math.abs(Number(l[vk] || 0)), 0));
    focusActs.push(
      `<button id="act-freeze" class="mini">${frozenAll ? "Unfreeze" : "Freeze"}${sel.length > 1 ? " all" : ""}</button>`,
      `<button id="act-breakup" class="mini">Break up${sel.length > 1 ? " all" : ""}</button>`,
      `<span class="gmeta">${lines.length} lines · ${val} ${vk} · net ${fmt(net)}</span>`);
  }
  const selTool =
    `<span class="seltool">select
      <button class="link" id="sel-all">all</button>
      <button class="link" id="sel-none">none</button>
      <button class="link" id="sel-invert">invert</button></span>`;
  // Inline tag entry (no modal prompt): Tag\u2026 opens a text box right in the
  // action bar; Enter / “tag” commits, Esc / ✕ cancels.
  const tagCtl = state.tagging
    ? `<span class="tagbox"><input id="tag-input" type="text" placeholder="tag name\u2026" />`
      + `<button id="tag-ok" class="primary mini">tag ${tgt}</button>`
      + `<button id="tag-cancel" class="mini" title="cancel">✕</button></span>`
    : `<button id="act-tag" class="mini">Tag\u2026</button>`;
  const lineActs = tgt ? [
    `<button id="act-match" class="primary mini">Match ${tgt}${onAll ? " (all)" : ""}</button>`,
    `<button id="act-freezesel" class="mini">Freeze ${tgt}${onAll ? " (all)" : ""}</button>`,
    `<button id="act-unmatch" class="mini">Unmatch ${tgt}${onAll ? " (all)" : ""}</button>`,
    tagCtl,
    `<button id="act-untag" class="mini">Untag</button>`,
  ] : [];
  // Commit verbs for a tagged "review bucket": promote to a frozen manual match,
  // or release (just untag). Reuses existing engine ops; no exception state.
  const taggedSel = [...state.selectedLines].filter((id) => state.tags.tagsOf(id).size > 0);
  const commitActs = taggedSel.length ? [
    `<button id="act-promote-match" class="primary mini">\u2192 Match ${taggedSel.length}</button>`,
    `<button id="act-release" class="mini">Release</button>`,
  ] : [];
  acts.innerHTML = selTool + lineActs.join("") + commitActs.join("") + focusActs.join("");
  $("sel-all").onclick = () => { for (const id of state.shownLineIds) state.selectedLines.add(id); render(); };
  $("sel-none").onclick = () => { state.selectedLines.clear(); render(); };
  $("sel-invert").onclick = () => {
    for (const id of state.shownLineIds)
      if (state.selectedLines.has(id)) state.selectedLines.delete(id); else state.selectedLines.add(id);
    render();
  };
  if (tgt) {
    $("act-match").onclick = matchSelected;
    $("act-freezesel").onclick = freezeSelected;
    $("act-unmatch").onclick = unmatchSelected;
    $("act-untag").onclick = untagSelected;
    if (state.tagging) {
      const inp = $("tag-input");
      inp.value = state.tagText || "";
      inp.focus(); inp.select();
      inp.oninput = () => { state.tagText = inp.value; };
      inp.onkeydown = (e) => {
        if (e.key === "Enter") { e.preventDefault(); commitTag(inp.value); }
        else if (e.key === "Escape") { e.preventDefault(); closeTagBox(); }
      };
      $("tag-ok").onclick = () => commitTag(inp.value);
      $("tag-cancel").onclick = closeTagBox;
    } else {
      $("act-tag").onclick = () => { state.tagging = true; state.tagText = "reviewing"; render(); };
    }
  }
  if (taggedSel.length) {
    $("act-promote-match").onclick = promoteMatch;
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
  renderDetailTotals(lines, cols, nsl);
  foot.textContent = `${lines.length.toLocaleString()} lines · ${scope}`
    + (lines.length > CAP ? ` (showing ${CAP})` : "")
    + (nsl ? ` · ${nsl} selected` : "");
}

// Sticky totals row: sum every numeric detail column over the rows in scope.
// Scope is the current line selection when there is one (sum a subset like a
// spreadsheet), otherwise every shown line. Non-numeric columns stay blank.
function renderDetailTotals(lines, cols, nsl) {
  const foot = $("detail-foot-row");
  if (!cols.some((c) => c.total)) { foot.innerHTML = ""; return; }
  const scope = nsl ? lines.filter((l) => state.selectedLines.has(l.id)) : lines;
  let labelled = false;
  const cells = cols.map((c) => {
    if (c.total) {
      const sum = scope.reduce((a, l) => a + c.total(l), 0);
      return `<td class="num"><span class="${sum >= 0 ? "pos" : "neg"}">${c.fmtTotal(sum)}</span></td>`;
    }
    if (!labelled) { labelled = true; return `<td class="tot-lbl">\u03a3 ${scope.length.toLocaleString()}${nsl ? " sel" : ""}</td>`; }
    return `<td></td>`;
  });
  foot.innerHTML = `<tr>${cells.join("")}</tr>`;
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
  renderFiltered();
}

// Slicer click model: single-select by default (replace the dim's selection, or
// toggle it off when clicking the sole active value); additive toggle when a
// modifier is held. `toggleFilter` above stays the additive primitive.
function selectFilter(dim, val, additive) {
  let set = state.filters.get(dim);
  if (additive) {
    if (!set) { set = new Set(); state.filters.set(dim, set); }
    if (set.has(val)) set.delete(val); else set.add(val);
  } else {
    set = set && set.size === 1 && set.has(val) ? new Set() : new Set([val]);
    state.filters.set(dim, set);
  }
  if (set.size === 0) state.filters.delete(dim);
  renderFiltered();
}

// Background, cancellable slicer recompute. Filtering a large book is the one
// O(rows) pass that can jank the UI, so it runs off the click handler in
// chunks; each filter change bumps `filterToken`, and a superseded run sees a
// stale token and bails. Result: rapid toggles coalesce and only the final
// selection is computed and painted. Engine/selection ops keep using the
// synchronous render() (the DOM smoke reads selection state synchronously).
function setSlicerBusy(on) {
  const head = document.querySelector("#facets .panel-head");
  if (head) head.classList.toggle("busy", !!on);
}

async function computeFilteredCancellable(token) {
  const lines = state.lines, out = [];
  const CHUNK = 8000;
  for (let i = 0; i < lines.length; i += CHUNK) {
    const end = Math.min(i + CHUNK, lines.length);
    for (let j = i; j < end; j++) if (lineMatches(lines[j], null)) out.push(lines[j]);
    if (end < lines.length) {
      await new Promise((r) => setTimeout(r));
      if (token !== filterToken) return null; // superseded by a newer toggle
    }
  }
  return out;
}

async function renderFiltered() {
  const my = ++filterToken;
  setSlicerBusy(true);
  await Promise.resolve(); // let a burst of toggles collapse to the last one
  if (my !== filterToken) return;
  const fl = await computeFilteredCancellable(my);
  if (fl === null || my !== filterToken) return;
  renderMetrics(fl); renderFacets(); renderGroups(fl); renderDetail(fl);
  setSlicerBusy(false);
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
// The lines an action targets: the explicit selection when there is one,
// otherwise every line currently visible in the detail pane ("act on all").
function actionLineIds() {
  return state.selectedLines.size ? [...state.selectedLines] : [...state.shownLineIds];
}

function matchSelected() {
  const ids = actionLineIds();
  if (ids.length < 2) return setStatus("select at least two lines to match", true);
  const net = ids.reduce((a, id) => a + Number(state.displayById.get(id)?.[state.netKey] || 0), 0);
  const r = state.fe.dispatch({ op: "group", ids, net, origin: "manual" });
  if (!r.ok) return setStatus("match error: " + r.error, true);
  state.selectedLines.clear();
  state.report = r.report; rebuild(); render();
  setStatus(`matched ${ids.length} lines into a manual group`);
}

// Freeze from the selection: live singletons (unmatched rows) are accepted via
// `freeze_singletons`; live matches get their group frozen. Either way the
// result is simply "frozen" — there is no separate exception state.
function freezeSelected() {
  const ids = actionLineIds();
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
  const ids = actionLineIds();
  if (!ids.length) return;
  const r = state.fe.dispatch({ op: "ungroup", ids });
  if (!r.ok) return setStatus("unmatch error: " + r.error, true);
  state.selectedLines.clear();
  state.report = r.report; rebuild(); render();
  setStatus(`unmatched ${ids.length} lines back to residual`);
}

// ---- tag overlay: host-side, no engine dispatch ----------------------------
// Commit the inline tag box: tag the selected lines into a review bucket and
// close the box. Tags are keyed by row id, so they survive recalc.
function commitTag(name) {
  const ids = actionLineIds();
  state.tagging = false; state.tagText = "";
  if (!ids.length) return render();
  const label = (name || "reviewing").trim() || "reviewing";
  const tid = state.tags.ensureTag(label, "bucket");
  if (!tid) return render();
  for (const id of ids) state.tags.add(id, tid);
  render();
  setStatus(`tagged ${ids.length} lines as “${state.tags.label(tid)}”`);
}

// Cancel the inline tag box without tagging.
function closeTagBox() {
  state.tagging = false; state.tagText = "";
  render();
}

// Clear every tag on the selected lines.
function untagSelected() {
  const ids = actionLineIds();
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

// Commit verb: release — just untag; rows stay live and flow back into recalc.
function releaseTagged() {
  const ids = taggedSelection();
  for (const id of ids) state.tags.clear(id);
  render();
  setStatus(`released ${ids.length} tagged lines back to recalc`);
}

// ---- workspace save / load + result export (centralized in core/persist) ---
function saveWorkspace() {
  try {
    const ws = serializeWorkspace({ data: state.data, report: state.report, tags: state.tags });
    download(`${slug(state.data.pair)}.florecon.json`, JSON.stringify(ws, null, 2));
    setStatus(`saved workspace: ${ws.frozen.length} frozen group(s), ${Object.keys(ws.tags.tags).length} tagged row(s)`);
  } catch (e) {
    setStatus("save failed: " + (e && e.message || e), true);
  }
}

async function loadWorkspace() {
  let text;
  try { text = await pickTextFile(".json,application/json"); }
  catch (e) { return setStatus("load failed: " + (e && e.message || e), true); }
  if (!text) return;
  let ws;
  try { ws = parseWorkspace(text); }
  catch (e) { return setStatus("not a valid workspace file: " + (e && e.message || e), true); }
  try {
    const s = ws.dataset;
    const data = buildDataset({ header: s.header, rows: s.rows, columns: s.columns, plan: s.plan, name: s.name });
    await startApp(data);                 // rebuild + init + solve (live proposals) + wireUi
    const res = applyFrozen((cmd) => state.fe.dispatch(cmd), ws.frozen);
    const rep = state.fe.dispatch({ op: "report" });
    if (rep.ok) { state.report = rep.report; rebuild(); }
    state.tags.restore(ws.tags);
    render();
    const warn = res.failed ? ` (⚠ ${res.failed} decision(s) could not be re-applied)` : "";
    setStatus(`loaded workspace: ${res.groups} group(s) + ${res.singles} frozen row(s) restored${warn}`);
  } catch (e) {
    setStatus("load failed: " + (e && e.message || e), true);
  }
}

function exportResult(fmt) {
  if (!fmt) return;
  try {
    const base = slug(state.data.pair);
    const assignments = primaryAssignments(state.report);
    if (fmt === "rows")
      download(`${base}.results.csv`, resultsCsv({ data: state.data, report: state.report, assignments }), "text/csv");
    else if (fmt === "groups")
      download(`${base}.groups.csv`, groupsCsv({ report: state.report }), "text/csv");
    else if (fmt === "json")
      download(`${base}.result.json`, resultJson({ data: state.data, report: state.report }), "application/json");
    setStatus(`exported ${fmt}`);
  } catch (e) {
    setStatus("export failed: " + (e && e.message || e), true);
  }
}

function wireUi() {
  $("recalc").onclick = () => solve();
  $("save-ws").onclick = saveWorkspace;
  $("load-ws").onclick = loadWorkspace;
  $("export-fmt").onchange = (e) => { const v = e.target.value; e.target.value = ""; exportResult(v); };
  $("reset").onclick = () => { state.filters.clear(); state.selectedGids.clear(); state.selectedLines.clear(); state.search = ""; boot2(); };
  $("clear-filters").onclick = () => { state.filters.clear(); renderFiltered(); };
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
  resetGroupDisplayLabels();
  const init = state.fe.dispatch({
    op: "init", plan: state.data.plan
  }, state.data.arrowBytes);
  if (!init.ok) return setStatus("init error: " + init.error, true);
  solve();
}

// entry point is web/setup.js, which builds a dataset (demo or uploaded CSV)
// and calls startApp().
