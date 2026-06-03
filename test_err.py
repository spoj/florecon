from pathlib import Path
t = Path('src/arrow.rs').read_text()
t = t.replace('Ok((ids, rows, map))', 'return Err(crate::error::ApiError::BadExpr(format!("{:?}", map.int_cols.keys())));\n    Ok((ids, rows, map))')
Path('src/arrow.rs').write_text(t)
