from pathlib import Path
import re
t = Path('src/arrow.rs').read_text()
t = t.replace('map.int_cols.insert(name, int_arrays.len());', 'map.int_cols.insert(name.clone(), int_arrays.len()); println!("int_col {}", name);')
t = t.replace('map.token_cols.insert(name, token_arrays.len());', 'map.token_cols.insert(name.clone(), token_arrays.len()); println!("token_col {}", name);')
Path('src/arrow.rs').write_text(t)
