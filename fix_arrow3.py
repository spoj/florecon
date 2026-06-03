from pathlib import Path
t = Path('src/arrow.rs').read_text()

t = t.replace('''            DataType::Utf8 | DataType::LargeUtf8 => {
                map.token_cols.insert(name, token_arrays.len());
                token_arrays.push(col.as_string::<i32>());
            }
            DataType::Dictionary(_, _) => {
                map.token_cols.insert(name, token_arrays.len());
                token_arrays.push(col.as_string::<i32>()); // Assuming Utf8 for simplicity
            }''', '''            DataType::Utf8 | DataType::LargeUtf8 => {
                map.token_cols.insert(name, token_arrays.len());
                token_arrays.push(arrow::compute::cast(col, &DataType::Utf8).unwrap());
            }
            DataType::Dictionary(_, _) => {
                map.token_cols.insert(name, token_arrays.len());
                token_arrays.push(arrow::compute::cast(col, &DataType::Utf8).unwrap());
            }''')

t = t.replace('for (i, arr) in token_arrays.iter().enumerate() {', '''for (i, arr_dyn) in token_arrays.iter().enumerate() {
        let arr = arr_dyn.as_string::<i32>();''')

Path('src/arrow.rs').write_text(t)
