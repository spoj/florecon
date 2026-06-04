// Node smoke test for the browser host ABI against real data.
//   node web/smoke.mjs
import { readFileSync } from "fs";
import { Florecon, primaryAssignments } from "./core/florecon.js";
import { parseCsv, buildDataset } from "./ingest.js";

const here = new URL(".", import.meta.url);
const rel = (p) => new URL(p, here);
const wasm = readFileSync(rel("core/engine.wasm"));
const { instance } = await WebAssembly.instantiate(wasm, {});
const fe = new Florecon(instance);

const csv = `Entity,Currency,Account,Date,Amount,Ref,Memo
ACME,USD,4000,2024-01-02,100.00,INV0001,widgets
ACME,USD,4000,2024-01-03,-100.00,INV0001,credit
GLOBEX,EUR,5000,2024-02-01,250.50,INV0009,services
GLOBEX,EUR,5000,2024-02-04,-250.50,INV0009,reversal
ACME,USD,4000,2024-01-05,42.00,INV0002,stray`;
const parsed = parseCsv(csv);
const data = buildDataset({
  header: parsed.header, rows: parsed.rows,
  mapping: { amount: 4, gkey: 2, date: 3, tokens: [5, 6], partitions: [0, 1], tol: 0 },
});

const t0 = performance.now();
let r = fe.dispatch({ op: "init", plan: data.plan }, data.arrowBytes);
if (!r.ok) throw new Error("init: " + r.error);
const tColdStart = performance.now();
r = fe.dispatch({ op: "solve" });
if (!r.ok) throw new Error("solve: " + r.error);
const coldMs = performance.now() - tColdStart;
const tWarmStart = performance.now();
const r1 = fe.dispatch({ op: "solve" });
if (!r1.ok) throw new Error("warm solve: " + r1.error);
const warmMs = performance.now() - tWarmStart;
const rep = r1.report;
const assignments = (rp) => primaryAssignments(rp);
const warmConserve = assignments(rep).length === data.display.length;
const residCount = (rp) => rp.groups.filter((g) => g.status !== "frozen" && g.size === 1).length;
const singletonIds = (rp) => {
  const live = new Set(rp.groups.filter((g) => g.status !== "frozen" && g.size === 1).map((g) => g.group_id));
  return assignments(rp).filter(([, gid]) => live.has(gid)).map(([id]) => id);
};
const total = data.display.length;
const matched = assignments(rep).length;
const conserve = matched === total;
const resid = residCount(rep);

const g = rep.groups.find((x) => x.size >= 2);
let n = fe.dispatch({ op: "freeze", group_id: g.group_id }).report.groups.find((x) => x.group_id === g.group_id);
const g2 = rep.groups.filter((x) => x.size >= 2)[1].group_id;
const after = fe.dispatch({ op: "breakup", group_id: g2 }).report;
const stillThere = after.groups.some((x) => x.group_id === g2);
const re = fe.dispatch({ op: "solve" }).report;
const frozenKept = re.groups.some((x) => x.group_id === g.group_id && x.status === "frozen");
const reConserve = assignments(re).length === total;

const pick = singletonIds(re).slice(0, 2);
let manualOk = false, frozenRefused = false, ungroupOk = false;
if (pick.length === 2) {
  const gm = fe.dispatch({ op: "group", ids: pick, net: 0, origin: "manual" });
  const mg = gm.ok && assignments(gm.report).filter(([id]) => pick.includes(id)).length === 2;
  const frozenManual = gm.ok && gm.report.groups.some((x) => x.origin === "manual" && x.status === "frozen");
  manualOk = mg && frozenManual;
  frozenRefused = fe.dispatch({ op: "ungroup", ids: pick }).ok === false;
}

const exId = singletonIds(fe.dispatch({ op: "report" }).report)[0];
let exceptionOk = false;
if (exId != null) {
  const fr = fe.dispatch({ op: "freeze_singletons", ids: [exId] }).report;
  const exGid = assignments(fr).find(([id]) => id === exId)?.[1];
  const frozenSingleton = fr.groups.some((x) => x.group_id === exGid && x.status === "frozen" && x.size === 1);
  const sur = fe.dispatch({ op: "solve" }).report;
  const kept = sur.groups.some((x) => x.group_id === exGid && x.status === "frozen" && x.size === 1);
  exceptionOk = frozenSingleton && kept;
}

const live = fe.dispatch({ op: "report" }).report.groups.find((x) => x.status !== "frozen" && x.size >= 2);
if (live) {
  const cur = fe.dispatch({ op: "report" }).report;
  const mem = assignments(cur).filter(([, gid]) => gid === live.group_id).map(([id]) => id);
  const ug = fe.dispatch({ op: "ungroup", ids: mem });
  const singles = new Set(singletonIds(ug.report));
  ungroupOk = ug.ok && mem.every((id) => singles.has(id));
}

console.log("SMOKE OK");
