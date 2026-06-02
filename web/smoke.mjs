// Node smoke test for the browser host ABI against real data.
//   node web/smoke.mjs
import { readFileSync } from "fs";
import { Florecon } from "./core/florecon.js";

const here = new URL(".", import.meta.url);
const rel = (p) => new URL(p, here);
const wasm = readFileSync(rel("core/engine.wasm"));
const { instance } = await WebAssembly.instantiate(wasm, {});
const fe = new Florecon(instance);
const data = JSON.parse(readFileSync(rel("data.json"), "utf8"));

const t0 = performance.now();
let r = fe.dispatch({ op: "init", schema: data.schema, plan: data.plan, rows: data.rows });
if (!r.ok) throw new Error("init: " + r.error);
const tColdStart = performance.now();
r = fe.dispatch({ op: "solve" });
if (!r.ok) throw new Error("solve: " + r.error);
const coldMs = performance.now() - tColdStart;
// Warm-start recalc (§2): the flow Matcher is kept alive per shard inside the
// stored strategy, so a no-op re-solve applies an empty delta and warm-solves
// from the cached basis. It must be dramatically faster than the cold solve.
const tWarmStart = performance.now();
const r1 = fe.dispatch({ op: "solve" });
if (!r1.ok) throw new Error("warm solve: " + r1.error);
const warmMs = performance.now() - tWarmStart;
// Use the warm re-solve's report from here on: a re-solve re-mints ephemeral
// live-singleton ids (§1), so the cold report's group ids are stale.
const rep = r1.report;
const warmConserve = rep.assignments.length === data.rows.length;
const speedup = warmMs > 0 ? coldMs / warmMs : Infinity;
// Expect at least an order of magnitude; the no-op warm path skips the simplex.
const warmFast = warmMs <= coldMs / 10;
console.log(
  `  warm     : cold ${coldMs.toFixed(1)} ms -> no-op re-solve ${warmMs.toFixed(2)} ms ` +
    `(${speedup.toFixed(0)}x faster, warm<<cold=${warmFast})`,
);
// No separate residual set: an unmatched row is a live singleton group
// (status==live && size==1). Helpers derive the old "residual" view from groups.
const residCount = (rp) => rp.groups.filter((g) => g.status !== "frozen" && g.size === 1).length;
const singletonIds = (rp) => {
  const live = new Set(rp.groups.filter((g) => g.status !== "frozen" && g.size === 1).map((g) => g.group_id));
  return rp.assignments.filter(([, gid]) => live.has(gid)).map(([id]) => id);
};
const total = data.rows.length;
const matched = rep.assignments.length;
// Conservation: every id lands in exactly one group (assignments cover all ids).
const conserve = matched === total;
const resid = residCount(rep);
console.log(`pair ${data.pair}: ${total} rows`);
console.log(`  groups   : ${rep.groups.length}`);
console.log(`  matched  : ${matched} (${(100 * matched / total).toFixed(1)}%)`);
console.log(`  residual : ${resid} (live singletons)`);
console.log(`  conserve : ${matched} == ${total} -> ${conserve}`);
console.log(`  solve    : ${(performance.now() - t0).toFixed(0)} ms`);

// exercise the interactive verbs the UI uses
const g = rep.groups.find((x) => x.size >= 2);
let n = fe.dispatch({ op: "freeze", group_id: g.group_id }).report.groups.find((x) => x.group_id === g.group_id);
console.log(`  freeze   : group ${g.group_id} frozen=${n.status === "frozen"}`);
const g2 = rep.groups.filter((x) => x.size >= 2)[1].group_id;
const after = fe.dispatch({ op: "breakup", group_id: g2 }).report;
const stillThere = after.groups.some((x) => x.group_id === g2);
console.log(`  breakup  : group ${g2} present=${stillThere} residual=${residCount(after)}`);
const re = fe.dispatch({ op: "solve" }).report;
const frozenKept = re.groups.some((x) => x.group_id === g.group_id && x.status === "frozen");
const reConserve = re.assignments.length === total;
console.log(`  re-solve : frozen kept=${frozenKept} conserve=${reConserve} groups=${re.groups.length}`);
// manual match (group) over two live singletons -> a frozen "manual" group
const pick = singletonIds(re).slice(0, 2);
let manualOk = false, frozenRefused = false, ungroupOk = false;
if (pick.length === 2) {
  const gm = fe.dispatch({ op: "group", ids: pick, net: 0, origin: "manual" });
  const mg = gm.ok && gm.report.assignments.filter(([id]) => pick.includes(id)).length === 2;
  const frozenManual = gm.ok && gm.report.groups.some((x) => x.origin === "manual" && x.status === "frozen");
  manualOk = mg && frozenManual;
  // a frozen group's members cannot be ungrouped (signed-off is protected)
  frozenRefused = fe.dispatch({ op: "ungroup", ids: pick }).ok === false;
  console.log(`  group    : manual frozen match=${manualOk} frozen-protected=${frozenRefused}`);
}
// freeze_singletons: an accepted unmatched exception survives re-solve, id stable
const exId = singletonIds(fe.dispatch({ op: "report" }).report)[0];
let exceptionOk = false;
if (exId != null) {
  const fr = fe.dispatch({ op: "freeze_singletons", ids: [exId] }).report;
  const exGid = fr.assignments.find(([id]) => id === exId)?.[1];
  const frozenSingleton = fr.groups.some((x) => x.group_id === exGid && x.status === "frozen" && x.size === 1);
  const sur = fe.dispatch({ op: "solve" }).report;
  const kept = sur.groups.some((x) => x.group_id === exGid && x.status === "frozen" && x.size === 1);
  exceptionOk = frozenSingleton && kept;
  console.log(`  freeze1  : exception frozen=${frozenSingleton} survives-solve=${kept}`);
}
// ungroup a LIVE group's members -> back to live singletons
const live = fe.dispatch({ op: "report" }).report.groups.find((x) => x.status !== "frozen" && x.size >= 2);
if (live) {
  const cur = fe.dispatch({ op: "report" }).report;
  const mem = cur.assignments.filter(([, gid]) => gid === live.group_id).map(([id]) => id);
  const ug = fe.dispatch({ op: "ungroup", ids: mem });
  const singles = new Set(singletonIds(ug.report));
  ungroupOk = ug.ok && mem.every((id) => singles.has(id));
  console.log(`  ungroup  : live ${mem.length} rows back to live singletons=${ungroupOk}`);
}
if (!conserve || !reConserve || !frozenKept || !manualOk || !frozenRefused || !exceptionOk || !ungroupOk || !warmConserve || !warmFast) { console.error("SMOKE FAILED"); process.exit(1); }
console.log("SMOKE OK");
