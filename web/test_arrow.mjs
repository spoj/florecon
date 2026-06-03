import { tableFromArrays, RecordBatchStreamWriter, tableToIPC } from "apache-arrow";

const ids = new BigInt64Array([1n, 2n]);
const amount = new BigInt64Array([100n, -100n]);
const tokens = ["A", "B"];

const table = tableFromArrays({
    id: ids,
    amount: amount,
    tokens: tokens
});

const bytes = tableToIPC(table, "stream");
console.log("Bytes length:", bytes.length);
