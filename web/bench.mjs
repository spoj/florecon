import { readFileSync } from "fs";
import { Florecon, primaryAssignments } from "./core/florecon.js";
import { buildDataset } from "./ingest.js";

const here = new URL(".", import.meta.url);
const rel = (p) => new URL(p, here);
const wasmBytes = readFileSync(rel("core/engine.wasm"));

// Synthetic dataset: N pairs that net to zero on a shared ref, plus noise.
function makeRows(nPairs) {
  const header = ["Entity", "Account", "Date", "Amount", "Ref", "Memo"];
  const rows = [];
  for (let i = 0; i < nPairs; i++) {
    const amt = ((i * 37) % 9000) / 100 + 1;
    const ent = ["ACME", "GLOBEX", "INITECH"][i % 3];
    const d = `2024-01-${String((i % 27) + 1).padStart(2, "0")}`;
    const ref = `INV${String(i).padStart(5, "0")}`;
    rows.push([ent, "4000", d, amt.toFixed(2), ref, "charge"]);
    rows.push([ent, "4000", d, (-amt).toFixed(2), ref, "credit"]);
  }
  return { header, rows };
}

const N = Number(process.argv[2] || 5000);
const { header, rows } = makeRows(N);

const tBuild = performance.now();
const data = buildDataset({
  header, rows,
  columns: [
    { ci: 0, name: "entity", kind: "key" },
    { ci: 1, name: "account", kind: "key" },
    { ci: 2, name: "date", kind: "date" },
    { ci: 3, name: "amount", kind: "amount" },
    { ci: 4, name: "ref", kind: "text" },
    { ci: 5, name: "memo", kind: "display" },
  ],
});
const buildMs = performance.now() - tBuild;

const tInst = performance.now();
const { instance } = await WebAssembly.instantiate(wasmBytes, {});
const fe = new Florecon(instance);
const instMs = performance.now() - tInst;

const tInit = performance.now();
let r = fe.dispatch({ op: "init", plan: data.plan }, data.arrowBytes);
if (!r.ok) throw new Error("init: " + r.error);
const initMs = performance.now() - tInit;

const tCold = performance.now();
r = fe.dispatch({ op: "solve" });
if (!r.ok) throw new Error("solve: " + r.error);
const coldMs = performance.now() - tCold;

const tWarm = performance.now();
r = fe.dispatch({ op: "solve" });
const warmMs = performance.now() - tWarm;

// Isolate readback (JSON parse) cost vs. the report op.
const tReport = performance.now();
const rep = fe.dispatch({ op: "report" }).report;
const reportMs = performance.now() - tReport;

const tPa = performance.now();
const pa = primaryAssignments(rep);
const paMs = performance.now() - tPa;

const allocN = (rep.allocations || []).length;
const groupN = (rep.groups || []).length;
const jsonBytes = JSON.stringify(rep).length;

console.log(`rows=${rows.length}  allocations=${allocN}  groups=${groupN}  report JSON≈${(jsonBytes/1e6).toFixed(2)}MB`);
console.log(`buildDataset (CSV->Arrow) : ${buildMs.toFixed(1)} ms`);
console.log(`wasm instantiate          : ${instMs.toFixed(1)} ms`);
console.log(`init (Arrow ingest)       : ${initMs.toFixed(1)} ms`);
console.log(`cold solve + readback     : ${coldMs.toFixed(1)} ms`);
console.log(`warm solve + readback     : ${warmMs.toFixed(1)} ms`);
console.log(`report op (readback only) : ${reportMs.toFixed(1)} ms`);
console.log(`primaryAssignments (JS)   : ${paMs.toFixed(1)} ms  (${pa.length} rows)`);

// --- bulk remove ---------------------------------------------------------
// Drop the first half of all ids in one Remove command, then re-solve.
const victimIds = [];
for (let i = 0; i < rows.length; i += 2) victimIds.push(i);
const tRm = performance.now();
const rr = fe.dispatch({ op: "remove", ids: victimIds });
if (!rr.ok) throw new Error("remove: " + rr.error);
const rmMs = performance.now() - tRm;
console.log(`bulk remove (${victimIds.length} ids)   : ${rmMs.toFixed(1)} ms`);
const afterRm = fe.dispatch({ op: "solve" });
const survivors = rows.length - victimIds.length;
const assignedAfter = primaryAssignments(afterRm.report).length;
console.log(`conservation after remove : ${assignedAfter}/${survivors} ${assignedAfter === survivors ? "OK" : "MISMATCH"}`);
