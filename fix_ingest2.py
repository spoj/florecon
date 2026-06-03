from pathlib import Path
import re

t = Path('web/ingest.js').read_text()

t = re.sub(r'schema, plan: \{primary: "amount", root: plan\}, fields, rows: outRows, display, netKey: "amount",',
'''map: {
      int_cols: Object.fromEntries(cols.filter(c => !c.text).map((c, i) => [c.name, i])),
      token_cols: Object.fromEntries(cols.filter(c => c.text).map((c, i) => [c.name, i]))
    },
    plan: {primary: "amount", root: plan}, fields, display, netKey: "amount",
    arrowBytes: tableToIPC(tableFromArrays(arrowCols), "stream"),
''', t)

Path('web/ingest.js').write_text(t)
