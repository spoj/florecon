from pathlib import Path

t = Path('web/ingest.js').read_text()

# Insert arrow imports
t = 'import { Utf8, Int64, Uint64, makeVector, tableFromArrays, tableToIPC } from "apache-arrow";\n' + t

# Plan generation adjustments
t = t.replace('if (mapping.gkey != null) steps.push({ op: "agg_net", key: "gkey", amount: "amount", tol });', 'if (mapping.gkey != null) steps.push({ op: "agg_net", key: "gkey", tol });')
t = t.replace('steps.push({ op: "exact", amount: "amount" });', 'steps.push({ op: "exact" });')
t = t.replace('steps.push({ op: "signal", signals: "tokens", amount: "amount", tol, cap: 256 });', 'steps.push({ op: "signal", signals: "tokens", tol, cap: 256 });')
t = t.replace('op: "flow", amount: "amount", day: "date",', 'op: "flow", day: "date",')

# Arrow builder replacements
t = t.replace('const outRows = [], display = [];', '''const display = [];
  const arrowCols = { id: makeVector({data: new BigInt64Array(rows.length), type: new Uint64()}) };
  for (const c of cols) {
    if (c.text) arrowCols[c.name] = new Array(rows.length);
    else arrowCols[c.name] = makeVector({data: new BigInt64Array(rows.length), type: new Int64()});
  }''')

# Row mapping logic
row_mapping = '''
    arrowCols.id.data[0].values[id] = BigInt(id);
    for (const c of cols) {
      if (c.amount) arrowCols[c.name].data[0].values[id] = BigInt(toCents(r[c.ci]));
      else if (c.date) arrowCols[c.name].data[0].values[id] = BigInt(toEpochDay(r[c.ci]));
      else if (c.text) arrowCols[c.name][id] = c.cis.map((i) => String(r[i] ?? "")).filter(Boolean).join(" ");
      else if (c.kind === "number") arrowCols[c.name].data[0].values[id] = BigInt(toInt(r[c.ci]));
      else arrowCols[c.name].data[0].values[id] = BigInt(fnv1a(String(r[c.ci] ?? "")));
    }
'''
t = t.replace('''    const cells = cols.map((c) => {
      if (c.amount) return toCents(r[c.ci]);
      if (c.date) return toEpochDay(r[c.ci]);
      if (c.text) return c.cis.map((i) => String(r[i] ?? "")).filter(Boolean).join(" ");
      if (c.kind === "number") return toInt(r[c.ci]);
      return String(r[c.ci] ?? "");
    });
    outRows.push([id, cells]);''', row_mapping)

return_mapping = '''  for (const c of cols) {
    if (c.text) {
      arrowCols[c.name] = makeVector({data: arrowCols[c.name], type: new Utf8()});
    }
  }

  return {
    pair: mapping.name || "uploaded",
    map: {
      int_cols: Object.fromEntries(cols.filter(c => !c.text).map((c, i) => [c.name, i])),
      token_cols: Object.fromEntries(cols.filter(c => c.text).map((c, i) => [c.name, i]))
    },
    plan: {primary: "amount", root: plan}, fields, display, netKey: "amount",
    arrowBytes: tableToIPC(tableFromArrays(arrowCols), "stream"),
  };'''

t = t.replace('''  return {
    pair: mapping.name || "uploaded",
    schema, plan, fields, rows: outRows, display, netKey: "amount",
  };''', return_mapping)

# Add FNV1A
t += '''\n
// FNV-1a for keys
function fnv1a(str) {
  let hash = 0xcbf29ce484222325n;
  for (let i = 0; i < str.length; i++) {
    hash ^= BigInt(str.charCodeAt(i));
    hash = (hash * 0x100000001b3n) & 0xffffffffffffffffn;
  }
  return hash > 0x7fffffffffffffffn ? hash - 0x10000000000000000n : hash;
}
'''

Path('web/ingest.js').write_text(t)
