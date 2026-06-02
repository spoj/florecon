// Node smoke test for the CSV upload -> ingest -> solve path.
//   node web/ingest.smoke.mjs
import { readFileSync } from "fs";
import { Florecon } from "./core/florecon.js";
import { parseCsv, buildDataset, toCents, toEpochDay } from "./ingest.js";

const here = new URL(".", import.meta.url);
const wasm = readFileSync(new URL("core/engine.wasm", here));
const { instance } = await WebAssembly.instantiate(wasm, {});
const fe = new Florecon(instance);

const ok = (cond, msg) => { if (!cond) throw new Error("FAIL: " + msg); };

// --- CSV parser: quotes, embedded commas/newlines, "" escapes -------------
const p = parseCsv('a,b,c\n1,"x,y",3\n4,"line\n2",6\n7,"he said ""hi""",9\n');
ok(p.header.join(",") === "a,b,c", "header");
ok(p.rows.length === 3, "row count " + p.rows.length);
ok(p.rows[0][1] === "x,y", "embedded comma");
ok(p.rows[1][1] === "line\n2", "embedded newline");
ok(p.rows[2][1] === 'he said "hi"', "escaped quote");

// --- scalar coercion ------------------------------------------------------
ok(toCents("1,234.56") === 123456, "thousands+decimals");
ok(toCents("($12.00)") === -1200, "parenthesised negative");
ok(toCents("") === 0, "empty -> 0");
ok(toEpochDay("2024-01-01") === 19723, "epoch day " + toEpochDay("2024-01-01"));

// --- end-to-end: a tiny book that should net to zero ----------------------
// Two entities (ACME/GLOBEX), each with an offsetting +/- pair on the same
// account + currency; one stray unmatched line.
const csv = [
  "Entity,Currency,Account,Date,Amount,Ref,Memo",
  "ACME,USD,4000,2024-01-02,100.00,INV0001,widgets",
  "ACME,USD,4000,2024-01-03,-100.00,INV0001,credit",
  "GLOBEX,EUR,5000,2024-02-01,250.50,INV0009,services",
  "GLOBEX,EUR,5000,2024-02-04,-250.50,INV0009,reversal",
  "ACME,USD,4000,2024-01-05,42.00,INV0002,stray",
].join("\n");
const parsed = parseCsv(csv);
parsed.name = "smoke";
const H = parsed.header;
const col = (n) => H.indexOf(n);
const mapping = {
  amount: col("Amount"), gkey: col("Account"), date: col("Date"),
  tokens: [col("Ref"), col("Memo")], partitions: [col("Entity"), col("Currency")],
  tol: 0, name: "smoke",
};
const data = buildDataset({ header: H, rows: parsed.rows, mapping });

ok(data.rows.length === 5, "5 rows built");
ok(data.netKey === "amount", "netKey");
ok(data.schema.cols.map((c) => c.name).join(",") === "p0,p1,gkey,date,amount,tokens", "schema order");
ok(data.plan.op === "partition" && data.plan.by === "p0", "outer partition");
// cells positional: [p0,p1,gkey,date,amount,tokens]
ok(data.rows[0][1][4] === 10000, "amount cents in cell");
ok(typeof data.rows[0][1][0] === "string", "key cell is string");
ok(data.rows[0][1][5] === "INV0001 widgets", "multi-column reference concatenated: " + data.rows[0][1][5]);
ok(data.display[0].amount === 10000, "display amount cents");

let r = fe.dispatch({ op: "init", schema: data.schema, plan: data.plan, rows: data.rows });
ok(r.ok, "init: " + r.error);
r = fe.dispatch({ op: "solve" });
ok(r.ok, "solve: " + r.error);
const rep = r.report;

// conservation: every row lands in exactly one group
ok(rep.assignments.length === data.rows.length, "conserve " + rep.assignments.length + "/" + data.rows.length);
// the two offsetting pairs should each form a net-zero 2-member group
const multi = rep.groups.filter((g) => g.size >= 2);
ok(multi.length === 2, "two matched groups, got " + multi.length);
ok(multi.every((g) => g.net === 0), "matched groups net to zero");
// the stray line is an unmatched live singleton
const singles = rep.groups.filter((g) => g.size === 1);
ok(singles.length === 1, "one singleton, got " + singles.length);

console.log(`  ingest   : ${data.rows.length} rows -> ${rep.groups.length} groups (${multi.length} matched, ${singles.length} residual)`);
console.log("INGEST SMOKE OK");
