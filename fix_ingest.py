from pathlib import Path
import re

t = Path('web/ingest.js').read_text()
t = 'import { tableFromArrays, tableToIPC } from "apache-arrow";\n' + t

t = re.sub(r'const outRows = \[\], display = \[\];\n\s*rows.forEach\(\(r, id\) => \{',
'''const outRows = [], display = [];
  const arrowCols = { id: new BigInt64Array(rows.length) };
  for (const c of cols) {
    if (c.text) arrowCols[c.name] = new Array(rows.length);
    else arrowCols[c.name] = new BigInt64Array(rows.length);
  }

  rows.forEach((r, id) => {
    arrowCols.id[id] = BigInt(id);
''', t)

t = re.sub(r'outRows\.push\(\[id, cells\]\);',
'''for (const c of cols) {
      if (c.amount) arrowCols[c.name][id] = BigInt(toCents(r[c.ci]));
      else if (c.date) arrowCols[c.name][id] = BigInt(toEpochDay(r[c.ci]));
      else if (c.text) arrowCols[c.name][id] = c.cis.map((i) => String(r[i] ?? "")).filter(Boolean).join(" ");
      else if (c.kind === "number") arrowCols[c.name][id] = BigInt(toInt(r[c.ci]));
      else arrowCols[c.name][id] = BigInt(fnv1a(String(r[c.ci] ?? ""))); // Wait, JS doesn't have fnv1a readily available here.
    }''', t)

Path('web/ingest.js').write_text(t)
