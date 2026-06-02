import { Florecon } from "./core/florecon.js";

const TOL = 100; // group is "clean" if |net| <= 1.00 native unit
const WASM = "./core/engine.wasm";
const DATA = "./data.json";

const DIMS = [
  { key: "ccy", label: "Currency" },
  { key: "policy", label: "Policy" },
  { key: "status", label: "Status" },
  { key: "origin", label: "Strategy" },
  { key: "month", label: "Month" },
  { key: "account", label: "Account" },
];

const state = {
  fe: null,
  data: null,
  displayById: new Map(),
  report: null,
  lines: [],            // joined display + group attrs
  groupsById: new Map(),
  filters: new Map(),   // dim -> Set(values)
  search: "",
  selected: null,       // gid
  sort: { key: "value", dir: -1 },
};

const $ = (id) => document.getElementById(id);
const fmt = (cents) => (cents / 100).toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
const fmt0 = (cents) => (cents / 100).toLocaleString("en-US", { maximumFractionDigits: 0 });
const esc = (s) => (s ?? "").toString().replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

function setStatus(msg, err = false) {
  const el = $("status");
  el.textContent = msg;
  el.classList.toggle("err", err);
}

// ---- boot ----------------------------------------------------------------
async function boot() {
  setStatus("loading wasm + data…");
  state.fe = await Florecon.load(WASM);
  state.data = await (await fetch(DATA)).json();
  for (const d of state.data.display) state.displayById.set(d.id, d);

  setStatus(`init: ${state.data.rows.length} rows…`);
  const init = state.fe.dispatch({
    op: "init", schema: state.data.schema, plan: state.data.plan, rows: state.data.rows,
  });
  if (!init.ok) return setStatus("init error: " + init.error, true);
  solve();
  wireUi();
}

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
      ...g, members: [], value: 0, clean: Math.abs(g.net) <= TOL,
    });
  }

  state.lines = state.data.display.map((d) => {
    const gid = gidOf.has(d.id) ? gidOf.get(d.id) : -1;
    const g = state.groupsById.get(gid);
    if (g) { g.members.push(d.id); g.value += Math.abs(d.usd); }
    return {
      ...d,
      gid,
      month: (d.date || "").slice(0, 7),
      origin: g ? g.origin : "residual",
      status: g ? (g.frozen ? "frozen" : "live") : "unmatched",
      clean: g ? g.clean : false,
    };
  });
}

// ---- filtering -----------------------------------------------------------
function lineMatches(l, exceptDim) {
  for (const [dim, set] of state.filters) {
    if (dim === exceptDim || set.size === 0) continue;
    if (!set.has(l[dim])) return false;
  }
  if (state.search) {
    const q = state.search.toLowerCase();
    if (!(`${l.id} ${l.account} ${l.ref} ${l.doc}`.toLowerCase().includes(q))) return false;
  }
  return true;
}
const filteredLines = () => state.lines.filter((l) => lineMatches(l, null));

// ---- render --------------------------------------------------------------
function render() {
  const fl = filteredLines();
  renderMetrics(fl);
  renderFacets();
  renderGroups(fl);
  renderDetail();
}

function renderMetrics(fl) {
  const total = state.lines.length;
  const shown = fl.length;
  const matched = fl.filter((l) => l.gid >= 0);
  const valTot = fl.reduce((a, l) => a + Math.abs(l.usd), 0);
  const valMatched = matched.reduce((a, l) => a + Math.abs(l.usd), 0);
  const frozen = [...state.groupsById.values()].filter((g) => g.frozen).length;
  const resid = state.lines.filter((l) => l.gid < 0).length;
  // The real airlock identity over the whole set, from the wasm report.
  const conserved = state.report.assignments.length + state.report.residual.length === total;

  const m = [
    ["lines", `${shown.toLocaleString()}${shown < total ? ` / ${total.toLocaleString()}` : ""}`, ""],
    ["matched", `${(100 * matched.length / Math.max(shown, 1)).toFixed(1)}%`, ""],
    ["value matched", `${(100 * valMatched / Math.max(valTot, 1)).toFixed(1)}%`, ""],
    ["value (usd)", fmt0(valTot), ""],
    ["residual", resid.toLocaleString(), resid ? "" : "good"],
    ["frozen", String(frozen), frozen ? "" : ""],
    ["conservation", conserved ? "✓" : "✗", conserved ? "good" : "bad"],
  ];
  $("metrics").innerHTML = m.map(([k, v, cls]) =>
    `<div class="metric"><span class="v ${cls}">${v}</span><span class="k">${k}</span></div>`).join("");
}

function renderFacets() {
  const host = $("facet-list");
  host.innerHTML = "";
  for (const { key, label } of DIMS) {
    // cross-filter: count this dim over lines filtered by every OTHER dim
    const base = state.lines.filter((l) => lineMatches(l, key));
    const agg = new Map();
    for (const l of base) {
      const v = l[key] || "—";
      const e = agg.get(v) || { n: 0, val: 0 };
      e.n++; e.val += Math.abs(l.usd); agg.set(v, e);
    }
    let vals = [...agg.entries()].sort((a, b) => b[1].n - a[1].n);
    const max = vals.length ? vals[0][1].n : 1;
    const active = state.filters.get(key) || new Set();
    const cap = 14;
    const more = vals.length - cap;
    vals = vals.slice(0, cap);

    const div = document.createElement("div");
    div.className = "facet";
    div.innerHTML = `<h4>${label}</h4>` + vals.map(([v, e]) => {
      const on = active.has(v) ? " active" : "";
      const w = (100 * e.n / max).toFixed(0);
      return `<div class="facet-val${on}" data-dim="${key}" data-val="${esc(v)}">
        <span class="bar" style="width:${w}%"></span>
        <span class="lbl" title="${esc(v)}">${esc(v)}</span>
        <span class="cnt">${e.n}</span></div>`;
    }).join("") + (more > 0 ? `<div class="facet-val"><span class="lbl" style="color:var(--dim)">+${more} more…</span></div>` : "");
    host.appendChild(div);
  }
  host.querySelectorAll(".facet-val[data-dim]").forEach((el) => {
    el.onclick = () => toggleFilter(el.dataset.dim, el.dataset.val);
  });
}

function renderGroups(fl) {
  // groups that have at least one filtered line
  const gids = new Set(fl.map((l) => l.gid).filter((g) => g >= 0));
  let rows = [...gids].map((gid) => state.groupsById.get(gid)).filter(Boolean);

  const { key, dir } = state.sort;
  rows.sort((a, b) => {
    const av = key === "status" ? (a.frozen ? 1 : 0) : a[key];
    const bv = key === "status" ? (b.frozen ? 1 : 0) : b[key];
    return av > bv ? dir : av < bv ? -dir : a.group_id - b.group_id;
  });

  const CAP = 400;
  const body = $("groups-body");
  body.innerHTML = rows.slice(0, CAP).map((g) => {
    const sel = g.group_id === state.selected ? " sel" : "";
    const st = g.frozen ? "frozen" : "live";
    const netCls = g.clean ? "" : "dirty";
    return `<tr class="${sel}" data-gid="${g.group_id}">
      <td>${g.group_id}</td>
      <td><span class="badge o-${g.origin}">${g.origin}</span></td>
      <td><span class="badge b-${st}">${st}</span></td>
      <td class="num">${g.size}</td>
      <td class="num">${fmt0(g.value)}</td>
      <td class="num ${netCls}">${g.clean ? "0" : fmt(g.net)}</td>
    </tr>`;
  }).join("");
  body.querySelectorAll("tr").forEach((tr) => {
    tr.onclick = () => { state.selected = +tr.dataset.gid; render(); };
  });

  const dirty = rows.filter((g) => !g.clean).length;
  $("groups-foot").textContent =
    `${rows.length.toLocaleString()} groups in view${rows.length > CAP ? ` (showing ${CAP})` : ""} · ${dirty} not clean`;
  $("groups-title").textContent = `Groups`;
}

function renderDetail() {
  const head = $("detail-head");
  const acts = $("detail-actions");
  const body = $("detail-body");
  const g = state.selected != null ? state.groupsById.get(state.selected) : null;
  if (!g) {
    head.textContent = "Detail — select a group";
    acts.innerHTML = "";
    body.innerHTML = "";
    return;
  }
  head.innerHTML = `Group ${g.group_id} · <span class="badge o-${g.origin}">${g.origin}</span>`;
  acts.innerHTML =
    `<button id="act-freeze">${g.frozen ? "Unfreeze" : "Freeze"}</button>
     <button id="act-breakup">Break up</button>
     <span class="gmeta">${g.size} lines · ${fmt0(g.value)} usd · net ${g.clean ? "0 ✓" : fmt(g.net)}</span>`;
  $("act-freeze").onclick = () => {
    command({ op: g.frozen ? "unfreeze" : "freeze", group_id: g.group_id });
  };
  $("act-breakup").onclick = () => {
    const id = g.group_id; state.selected = null; command({ op: "breakup", group_id: id });
  };

  const lines = g.members.map((id) => state.displayById.get(id))
    .sort((a, b) => (a.date < b.date ? -1 : a.date > b.date ? 1 : 0));
  body.innerHTML = lines.map((d) => {
    const nc = d.native >= 0 ? "pos" : "neg";
    const uc = d.usd >= 0 ? "pos" : "neg";
    return `<tr>
      <td>${esc(d.date)}</td>
      <td>${esc(d.policy)}</td>
      <td>${esc(d.co)}→${esc(d.icp)}</td>
      <td class="num ${nc}">${esc(d.ccy)} ${fmt(d.native)}</td>
      <td class="num ${uc}">${fmt(d.usd)}</td>
      <td title="${esc(d.account)}">${esc(d.account)}</td>
      <td title="${esc(d.ref)}">${esc(d.ref)}</td>
    </tr>`;
  }).join("");
}

// ---- interactions --------------------------------------------------------
function toggleFilter(dim, val) {
  let set = state.filters.get(dim);
  if (!set) { set = new Set(); state.filters.set(dim, set); }
  if (set.has(val)) set.delete(val); else set.add(val);
  if (set.size === 0) state.filters.delete(dim);
  render();
}

function wireUi() {
  $("recalc").onclick = () => solve();
  $("reset").onclick = () => { state.filters.clear(); state.selected = null; state.search = ""; $("search").value = ""; boot2(); };
  $("clear-filters").onclick = () => { state.filters.clear(); render(); };
  $("freeze-clean").onclick = () => {
    const r = state.fe.dispatch({ op: "freeze_clean", tol: TOL });
    if (!r.ok) return setStatus("freeze_clean error: " + r.error, true);
    const before = [...state.groupsById.values()].filter((g) => g.frozen).length;
    state.report = r.report; rebuild(); render();
    const after = [...state.groupsById.values()].filter((g) => g.frozen).length;
    setStatus(`froze ${after - before} clean groups (${after} frozen total)`);
  };
  $("search").oninput = (e) => { state.search = e.target.value.trim(); render(); };
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

boot().catch((e) => setStatus("fatal: " + e.message, true));
