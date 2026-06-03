from pathlib import Path
t = Path('web/ingest.js').read_text()
t = t.replace('import { Utf8, Int64, Uint64, makeVector, tableFromArrays, tableToIPC } from "apache-arrow";',
              'import { Utf8, Int64, Uint64, makeVector, vectorFromArray, tableFromArrays, tableToIPC } from "apache-arrow";')
t = t.replace('makeVector({data: arrowCols[c.name], type: new Utf8()});', 'vectorFromArray(arrowCols[c.name], new Utf8());')
Path('web/ingest.js').write_text(t)
