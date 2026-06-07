// Head-less smoke for the JS host stack: wasm load -> describe -> ingest CSV ->
// upsert -> solve -> pin -> serialize/restore -> export. Mirrors the Python
// host smoke and asserts the two hosts agree. Run: `node core/host.test.mjs`
// (needs `npm install` for apache-arrow, and the interco plugin wasm built).
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import assert from "node:assert/strict";
import { Workspace } from "./florecon.js";
import { TagStore } from "./tagstore.js";
import { decisions, serialize, parse, applyDecisions, groupsCsv, resultsCsv } from "./persist.js";
import { parseCsv, buildBatch } from "../ingest.js";

const here = dirname(fileURLToPath(import.meta.url));
const WASM = resolve(here, "../../../target/wasm32-unknown-unknown/release/interco_plugin.wasm");
const buf = readFileSync(WASM);
const ab = buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength);

const csv = `row_id,company,icp,objsub,indicative_usd_amt,gl_date,base_currency,trx_currency,trx_amt,fc_amt,reference,reference2,description,name_remark_explanation,invoice_no,is_offset
1,A,B,61500,100.0,0,USD,USD,100,0,INV1,,,,,0
2,B,A,61500,-100.0,1,USD,USD,100,0,INV1,,,,,0
3,A,B,61600,250.0,2,USD,USD,250,0,INV9,,,,,0
4,B,A,61600,-250.0,3,USD,USD,250,0,INV9,,,,,0`;
const { header, rows } = parseCsv(csv);

async function fresh() {
  const ws = await Workspace.load(ab.slice(0));
  const mapping = {};
  ws.fields.forEach((f) => (mapping[f.name] = header.indexOf(f.name)));
  ws.upsert(buildBatch({ header, rows, fields: ws.fields, mapping }).table);
  ws.solve();
  return ws;
}

// session 1: solve + pin + tag + serialize
const ws = await fresh();
ws.pinClean(0);
const rep = ws.report();
assert.equal(rep.groups.filter((g) => g.status === "pinned").length, 2, "two clean pairs pinned");
assert.deepEqual(decisions(rep).map((d) => d.allocations.map((a) => a.id)), [[1, 2], [3, 4]]);

const tags = new TagStore();
const t = tags.ensureTag("needs-review");
tags.add(3, t);
tags.add(4, t);
const saved = serialize(rep, { tags, meta: { domain: ws.domain } });

// session 2: fresh engine, restore
const ws2 = await fresh();
const obj = parse(JSON.stringify(saved));
const tags2 = new TagStore().restore(obj.tags);
const summary = applyDecisions(ws2, obj.decisions);
const rep2 = ws2.report();
assert.equal(summary.failed, 0, "restore had no failures");
assert.equal(rep2.groups.filter((g) => g.status === "pinned").length, 2, "pins survive restore");
assert.ok(tags2.tagsOf(3).has(t) && tags2.tagsOf(4).has(t), "tags round-trip");

// export shape
assert.match(groupsCsv(rep2), /group_id,origin,reason,status,size,net/);
assert.equal(resultsCsv(rep2).trim().split("\n").length, 5, "4 rows + header");

console.log("JS host smoke OK — two clean pairs, pins+tags round-trip, exports shape OK");
