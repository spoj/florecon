from pathlib import Path
t = Path('web/ingest.js').read_text()

t = t.replace('''
    for (const c of cols) {
        if (c.text) {
            arrowCols[c.name] = makeVector({data: arrowCols[c.name], type: new Utf8()});
        }
    }
    arrowBytes: tableToIPC(tableFromArrays(arrowCols), "stream"),''', 'arrowBytes: tableToIPC(tableFromArrays(arrowCols), "stream"),')

t = t.replace('return {', '''
  for (const c of cols) {
    if (c.text) {
      arrowCols[c.name] = makeVector({data: arrowCols[c.name], type: new Utf8()});
    }
  }
  return {''')

Path('web/ingest.js').write_text(t)
