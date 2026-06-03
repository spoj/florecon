from pathlib import Path
t = Path('src/arrow.rs').read_text()
t = t.replace('map.int_cols.insert(name.clone(), int_arrays.len()); println!("int_col {}", name);', 'map.int_cols.insert(name, int_arrays.len());')
t = t.replace('map.token_cols.insert(name.clone(), token_arrays.len()); println!("token_col {}", name);', 'map.token_cols.insert(name, token_arrays.len());')
t = t.replace('return Err(crate::error::ApiError::BadExpr(format!("{:?}", map.int_cols.keys())));\n    Ok((ids, rows, map))', 'Ok((ids, rows, map))')
Path('src/arrow.rs').write_text(t)
