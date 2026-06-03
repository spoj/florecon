use arrow::array::{Array, AsArray};
use arrow::datatypes::DataType;
use arrow::ipc::reader::StreamReader;

use crate::error::ApiError;
use crate::flow::ExtId;
use crate::row::{PhysicalRow, ColumnMap};
use crate::token::fnv1a;

pub fn rows_from_ipc(bytes: &[u8]) -> Result<(Vec<ExtId>, Vec<PhysicalRow>, ColumnMap), ApiError> {
    if bytes.is_empty() {
        return Ok((Vec::new(), Vec::new(), ColumnMap::default()));
    }

    let cursor = std::io::Cursor::new(bytes);
    let mut reader = match StreamReader::try_new(cursor, None) {
        Ok(r) => r,
        Err(e) => return Err(ApiError::BadExpr(format!("Arrow IPC error: {}", e))),
    };

    let batch = match reader.next() {
        Some(Ok(b)) => b,
        Some(Err(e)) => return Err(ApiError::BadExpr(format!("Arrow IPC batch error: {}", e))),
        None => return Ok((Vec::new(), Vec::new(), ColumnMap::default())),
    };

    let n = batch.num_rows();
    if n == 0 {
        return Ok((Vec::new(), Vec::new(), ColumnMap::default()));
    }

    if batch.num_columns() == 0 {
        return Err(ApiError::SchemaArity { expected: 1, got: 0 });
    }

    let mut ids = Vec::with_capacity(n);
    let id_col = batch.column(0);
    match id_col.data_type() {
        DataType::UInt64 => {
            let arr = id_col.as_primitive::<arrow::datatypes::UInt64Type>();
            for i in 0..n {
                ids.push(arr.value(i) as u64);
            }
        }
        DataType::Int64 => {
            let arr = id_col.as_primitive::<arrow::datatypes::Int64Type>();
            for i in 0..n {
                ids.push(arr.value(i) as u64);
            }
        }
        _ => return Err(ApiError::BadCell { col: 0, want: "uint64 id" }),
    }

    let mut map = ColumnMap::default();
    let mut int_arrays = Vec::new();
    let mut token_arrays = Vec::new(); // these are string arrays that need hashing

    let schema = batch.schema();
    for (col_idx, field) in schema.fields().iter().enumerate().skip(1) {
        let name = field.name().clone();
        let col = batch.column(col_idx);
        match col.data_type() {
            DataType::Int64 => {
                map.int_cols.insert(name, int_arrays.len());
                int_arrays.push(col.as_primitive::<arrow::datatypes::Int64Type>());
            }
            DataType::Utf8 | DataType::LargeUtf8 => {
                map.token_cols.insert(name, token_arrays.len());
                token_arrays.push(arrow::compute::cast(col, &DataType::Utf8).unwrap());
            }
            DataType::Dictionary(_, _) => {
                map.token_cols.insert(name, token_arrays.len());
                token_arrays.push(arrow::compute::cast(col, &DataType::Utf8).unwrap());
            }
            _ => return Err(ApiError::BadCell { col: col_idx, want: "int64 or utf8" }),
        }
    }

    let mut rows = vec![PhysicalRow { ints: vec![0; int_arrays.len()], tokens: vec![Vec::new(); token_arrays.len()] }; n];

    for (i, arr) in int_arrays.iter().enumerate() {
        for row in 0..n {
            if !arr.is_null(row) {
                rows[row].ints[i] = arr.value(row);
            }
        }
    }

    for (i, arr_dyn) in token_arrays.iter().enumerate() {
        let arr = arrow::compute::cast(arr_dyn, &DataType::Utf8).unwrap();
        let arr = arr.as_string::<i32>();
        for row in 0..n {
            if arr.is_null(row) {
                continue;
            }
            let text = arr.value(row);
            let mut t = Vec::new();
            for word in text.split_whitespace() {
                let clean: String = word.chars().filter(|c| c.is_alphanumeric()).collect();
                if clean.len() >= 6 {
                    t.push(fnv1a(clean.to_uppercase().as_bytes()));
                }
            }
            t.sort_unstable();
            t.dedup();
            rows[row].tokens[i] = t;
        }
    }

    Ok((ids, rows, map))
}
