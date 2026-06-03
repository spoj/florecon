from pathlib import Path
t = Path('web/ingest.js').read_text()

t = 'import { Utf8, Int64, Uint64, makeVector } from "apache-arrow";\n' + t

replace_arrowCols = '''const outRows = [], display = [];
  const arrowCols = { id: makeVector({data: new BigInt64Array(rows.length), type: new Uint64()}) };
  for (const c of cols) {
    if (c.text) arrowCols[c.name] = new Array(rows.length);
    else arrowCols[c.name] = makeVector({data: new BigInt64Array(rows.length), type: new Int64()});
  }'''

t = t.replace('''const outRows = [], display = [];
  const arrowCols = { id: new BigInt64Array(rows.length) };
  for (const c of cols) {
    if (c.text) arrowCols[c.name] = new Array(rows.length);
    else arrowCols[c.name] = new BigInt64Array(rows.length);
  }''', replace_arrowCols)

replace_push = '''for (const c of cols) {
      if (c.amount) arrowCols[c.name].data[0].values[id] = BigInt(toCents(r[c.ci]));
      else if (c.date) arrowCols[c.name].data[0].values[id] = BigInt(toEpochDay(r[c.ci]));
      else if (c.text) arrowCols[c.name][id] = c.cis.map((i) => String(r[i] ?? "")).filter(Boolean).join(" ");
      else if (c.kind === "number") arrowCols[c.name].data[0].values[id] = BigInt(toInt(r[c.ci]));
      else arrowCols[c.name].data[0].values[id] = BigInt(fnv1a(String(r[c.ci] ?? ""))); // Wait, JS doesn't have fnv1a readily available here.
    }'''

t = t.replace('''for (const c of cols) {
      if (c.amount) arrowCols[c.name][id] = BigInt(toCents(r[c.ci]));
      else if (c.date) arrowCols[c.name][id] = BigInt(toEpochDay(r[c.ci]));
      else if (c.text) arrowCols[c.name][id] = c.cis.map((i) => String(r[i] ?? "")).filter(Boolean).join(" ");
      else if (c.kind === "number") arrowCols[c.name][id] = BigInt(toInt(r[c.ci]));
      else arrowCols[c.name][id] = BigInt(fnv1a(String(r[c.ci] ?? ""))); // Wait, JS doesn't have fnv1a readily available here.
    }''', replace_push)

t = t.replace('arrowCols.id[id] = BigInt(id);', 'arrowCols.id.data[0].values[id] = BigInt(id);')

replace_finish = '''
    for (const c of cols) {
        if (c.text) {
            arrowCols[c.name] = makeVector({data: arrowCols[c.name], type: new Utf8()});
        }
    }
    arrowBytes: tableToIPC(tableFromArrays(arrowCols), "stream"),'''

t = t.replace('arrowBytes: tableToIPC(tableFromArrays(arrowCols), "stream"),', replace_finish)

Path('web/ingest.js').write_text(t)
