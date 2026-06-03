import { vectorFromArray, Utf8, tableFromArrays, tableToIPC } from "apache-arrow";
const strArray = ["A", "B", "", ""];
const vec = vectorFromArray(strArray, new Utf8());
const table = tableFromArrays({ texts: vec });
const bytes = tableToIPC(table, "stream");
console.log("Len:", bytes.length);
