from pathlib import Path
t = Path('src/arrow.rs').read_text()

import re
t = re.sub(r'DataType::Utf8 \| DataType::LargeUtf8 => \{',
'''DataType::Utf8 | DataType::LargeUtf8 => {
                map.token_cols.insert(name, token_arrays.len());
                token_arrays.push(col.as_string::<i32>());
            }
            DataType::Dictionary(_, _) => {''', t)

Path('src/arrow.rs').write_text(t)
