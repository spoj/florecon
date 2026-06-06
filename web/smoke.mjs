// Node smoke: drive the interco *plugin* wasm through the generic browser host
// (the new ABI: describe-driven, planless). Proves the JS host stack works
// against a plugin end to end. Mirrors py/smoke_stateful.py.
//
//   cargo build -p interco-plugin --target wasm32-unknown-unknown --release
//   node web/smoke.mjs

import { readFileSync } from "fs";
import { Florecon } from "./core/florecon.js";
import { Session } from "./host.js";

const wasmUrl = new URL(
  "../target/wasm32-unknown-unknown/release/interco_plugin.wasm",
  import.meta.url,
);

function check(cond, msg) {
  if (!cond) {
    console.error("FAIL: " + msg);
    process.exit(1);
  }
}

const gidOf = (rep, rid) => (rep.allocations.find((a) => a.id === rid) || {}).group_id;
const group = (rep, gid) => rep.groups.find((g) => g.group_id === gid);
function matchedPair(rep, a, b) {
  const ga = gidOf(rep, a), gb = gidOf(rep, b);
  const g = ga != null ? group(rep, ga) : null;
  return ga != null && ga === gb && g && g.net === 0 && g.size >= 2;
}

const line = (row_id, co, icp, objsub, usd, day, ref) => ({
  row_id, company: co, icp, objsub,
  indicative_usd_amt: usd, gl_date: day,
  trx_currency: "USD", trx_amt: Math.abs(usd), reference: ref,
});

const { instance } = await WebAssembly.instantiate(readFileSync(wasmUrl), {});
const fe = new Florecon(instance);
const s = new Session(fe);
check(s.domain.id === "florecon.intercompany", "host should discover the domain via describe()");

// first invoice pair: opposite books, shared reference, nets clean
s.upsert(line(1, "A", "B", "61500", 100.0, 0, "INV0001"), line(2, "B", "A", "61500", -100.0, 1, "INV0001"));
let rep = s.solve();
check(matchedPair(rep, 1, 2), "first invoice pair (1,2) should net to a matched group");

// stream a second pair and WARM re-solve
s.upsert(line(3, "A", "B", "61600", 250.0, 2, "INV0009"), line(4, "B", "A", "61600", -250.0, 3, "INV0009"));
rep = s.solve();
check(matchedPair(rep, 1, 2), "original pair (1,2) stays matched after warm re-solve");
check(matchedPair(rep, 3, 4), "new pair (3,4) matches on warm re-solve");
check(gidOf(rep, 1) !== gidOf(rep, 3), "distinct invoice buckets form distinct groups");

// remove one leg: partner falls back to a live singleton
s.remove(4);
rep = s.solve();
check(matchedPair(rep, 1, 2), "pair (1,2) untouched by removing row 4");
const g3 = group(rep, gidOf(rep, 3));
check(g3 && g3.size === 1, "row 3 becomes a live singleton once its partner is removed");

// freeze the clean match; it survives recalc
s.freezeClean(0);
rep = s.report();
check(group(rep, gidOf(rep, 1)).status === "frozen", "freeze_clean freezes the clean (1,2) match");
rep = s.solve();
check(group(rep, gidOf(rep, 1)).status === "frozen", "frozen (1,2) survives a subsequent solve");

// re-add the partner, re-match, then break the live group apart
s.upsert(line(4, "B", "A", "61600", -250.0, 3, "INV0009"));
rep = s.solve();
check(matchedPair(rep, 3, 4), "re-adding row 4 re-forms the (3,4) match");
s.breakup(gidOf(rep, 3));
rep = s.report();
check(gidOf(rep, 3) !== gidOf(rep, 4), "breakup splits rows 3 and 4 apart");
check(group(rep, gidOf(rep, 1)).status === "frozen", "breakup leaves the frozen (1,2) group alone");

console.log("WEB PLUGIN SMOKE OK");
