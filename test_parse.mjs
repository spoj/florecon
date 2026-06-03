import { parseCsv } from "./web/ingest.js";
const p = parseCsv('a,b,c\n1,"x,y",3\n4,"line\n2",6\n7,"he said ""hi""",9\n');
console.log(p);
