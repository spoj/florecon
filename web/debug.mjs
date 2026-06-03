import { Florecon } from "./core/florecon.js";
import { parseCsv, buildDataset } from "./ingest.js";
import fs from "fs";

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
const wasmBytes = fs.readFileSync("web/core/engine.wasm");
const { instance } = await WebAssembly.instantiate(wasmBytes, {});
const fe = new Florecon(instance);

const r = fe.dispatch({ op: "init", plan: data.plan }, data.arrowBytes);
console.log(r);
