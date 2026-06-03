from pathlib import Path
t = Path('web/app.js').read_text()
t = t.replace('setStatus(`init: ${state.data.rows.length} rows…`);', 'setStatus(`init: ${state.data.display.length} rows…`);')
t = t.replace('''  const init = state.fe.dispatch({
    op: "init", schema: state.data.schema, plan: state.data.plan, rows: state.data.rows,
  });''', '''  const init = state.fe.dispatch({
    op: "init", map: state.data.map, plan: state.data.plan
  }, state.data.arrowBytes);''')

t = t.replace('''  const init = state.fe.dispatch({
    op: "init", schema: state.data.schema, plan: state.data.plan, rows: state.data.rows,
  });''', '''  const init = state.fe.dispatch({
    op: "init", map: state.data.map, plan: state.data.plan
  }, state.data.arrowBytes);''')

Path('web/app.js').write_text(t)
