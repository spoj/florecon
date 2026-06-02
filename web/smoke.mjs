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
r = fe.dispatch({ op: "solve" });
if (!r.ok) throw new Error("solve: " + r.error);
const rep = r.report;
const total = data.rows.length;
const matched = rep.assignments.length;
const conserve = matched + rep.residual.length === total;
console.log(`pair ${data.pair}: ${total} rows`);
console.log(`  groups   : ${rep.groups.length}`);
console.log(`  matched  : ${matched} (${(100 * matched / total).toFixed(1)}%)`);
console.log(`  residual : ${rep.residual.length}`);
console.log(`  conserve : ${matched} + ${rep.residual.length} == ${total} -> ${conserve}`);
console.log(`  solve    : ${(performance.now() - t0).toFixed(0)} ms`);

// exercise the interactive verbs the UI uses
const g = rep.groups[0];
let n = fe.dispatch({ op: "freeze", group_id: g.group_id }).report.groups.find((x) => x.group_id === g.group_id);
console.log(`  freeze   : group ${g.group_id} frozen=${n.frozen}`);
const g2 = rep.groups[1].group_id;
const after = fe.dispatch({ op: "breakup", group_id: g2 }).report;
const stillThere = after.groups.some((x) => x.group_id === g2);
console.log(`  breakup  : group ${g2} present=${stillThere} residual=${after.residual.length}`);
const re = fe.dispatch({ op: "solve" }).report;
const frozenKept = re.groups.some((x) => x.group_id === g.group_id && x.frozen);
const reConserve = re.assignments.length + re.residual.length === total;
console.log(`  re-solve : frozen kept=${frozenKept} conserve=${reConserve} groups=${re.groups.length}`);
if (!conserve || !reConserve || !frozenKept) { console.error("SMOKE FAILED"); process.exit(1); }
console.log("SMOKE OK");
