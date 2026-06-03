from pathlib import Path
t = Path('src/wasm.rs').read_text()
t = t.replace('Ok((ids, rows, _map)) => ids.into_iter().zip(rows.into_iter()).collect::<Vec<_>>(),',
'''Ok((ids, rows, map)) => {
                    req.map = map;
                    ids.into_iter().zip(rows.into_iter()).collect::<Vec<_>>()
                }''')
Path('src/wasm.rs').write_text(t)
