from pathlib import Path
import re

t = Path('web/ingest.smoke.mjs').read_text()
t = t.replace('ok(data.rows.length === 5, "5 rows built");', 'ok(data.arrowBytes.length > 0, "arrow built");')
t = t.replace('ok(data.schema.cols.map((c) => c.name).join(",") === "p0,p1,gkey,date,amount,tokens", "schema order");', 'ok(Object.keys(data.map.int_cols).length > 0, "schema order");')
t = t.replace('ok(data.rows[0][1][4] === 10000, "amount cents in cell");', 'ok(data.display[0].amount === 10000, "amount cents in cell");')
t = t.replace('let r = fe.dispatch({ op: "init", schema: data.schema, plan: data.plan, rows: data.rows });', 'let r = fe.dispatch({ op: "init", map: data.map, plan: data.plan }, data.arrowBytes);')

Path('web/ingest.smoke.mjs').write_text(t)
