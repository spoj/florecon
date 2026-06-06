//! Arrow IPC is the single source of column identity at the host boundary.
//!
//! A batch's **schema** *is* the [`ColumnMap`]: column 0 is the row id; every
//! other column is an integer lane (`Int64`) or a free-text reference lane
//! (`Utf8`/`LargeUtf8`/dictionary-of-utf8, tokenized here into reference-signal
//! ids). Hosts therefore never ship a separate column map — they ship typed
//! Arrow and the engine derives the map from it.
//!
//! Two readers serve the two host modes:
//! - [`rows_from_ipc`] derives a fresh map from the batch (stateless `solve`,
//!   and the first `init` of a stateful workspace). It tolerates a *schema-only*
//!   batch (zero rows), so a workspace can be opened on a schema and then have
//!   rows streamed in later.
//! - [`rows_from_ipc_mapped`] lowers a batch against an *existing* map by column
//!   **name**, so incremental `upsert` batches are order-independent and a typo
//!   or kind mismatch fails loudly instead of silently shifting a lane.

use arrow::array::{Array, AsArray};
use arrow::datatypes::DataType;
use arrow::ipc::reader::StreamReader;

use crate::error::ApiError;
use crate::ExtId;
use crate::row::{ColumnMap, PhysicalRow};
use crate::token::fnv1a;

/// A parsed Arrow batch in column-major form: row ids plus integer and token
/// lanes, each tagged with its schema name and kept in schema order.
struct Parsed {
    ids: Vec<ExtId>,
    int_names: Vec<String>,
    token_names: Vec<String>,
    int_vals: Vec<Vec<i64>>,
    token_vals: Vec<Vec<Vec<u64>>>,
    n: usize,
}

/// Lower one free-text cell to a sorted, deduped set of reference-signal ids:
/// split on whitespace, keep alphanumerics, drop fragments under six chars
/// (too common to discriminate), uppercase, and hash. This token policy lives
/// in the engine so every host tokenizes identically.
fn tokenize(text: &str) -> Vec<u64> {
    let mut t = Vec::new();
    for word in text.split_whitespace() {
        let clean: String = word.chars().filter(|c| c.is_alphanumeric()).collect();
        if clean.len() >= 6 {
            t.push(fnv1a(clean.to_uppercase().as_bytes()));
        }
    }
    t.sort_unstable();
    t.dedup();
    t
}

/// Decode the first record batch of an IPC stream into column-major lanes. The
/// schema is read even when the stream carries no batch (or a zero-row batch),
/// so a schema-only payload still yields the full column map with empty lanes.
fn parse(bytes: &[u8]) -> Result<Parsed, ApiError> {
    let empty = Parsed {
        ids: Vec::new(),
        int_names: Vec::new(),
        token_names: Vec::new(),
        int_vals: Vec::new(),
        token_vals: Vec::new(),
        n: 0,
    };
    if bytes.is_empty() {
        return Ok(empty);
    }

    let cursor = std::io::Cursor::new(bytes);
    let mut reader = StreamReader::try_new(cursor, None)
        .map_err(|e| ApiError::BadExpr(format!("Arrow IPC error: {e}")))?;
    let schema = reader.schema();

    // The id column is mandatory; everything else is data.
    if schema.fields().is_empty() {
        return Err(ApiError::SchemaArity {
            expected: 1,
            got: 0,
        });
    }

    let batch = match reader.next() {
        Some(Ok(b)) => Some(b),
        Some(Err(e)) => return Err(ApiError::BadExpr(format!("Arrow IPC batch error: {e}"))),
        None => None,
    };
    let n = batch.as_ref().map_or(0, |b| b.num_rows());

    let mut ids = Vec::with_capacity(n);
    if let Some(batch) = &batch {
        let id_col = batch.column(0);
        match id_col.data_type() {
            DataType::UInt64 => {
                let arr = id_col.as_primitive::<arrow::datatypes::UInt64Type>();
                ids.extend((0..n).map(|i| arr.value(i)));
            }
            DataType::Int64 => {
                let arr = id_col.as_primitive::<arrow::datatypes::Int64Type>();
                ids.extend((0..n).map(|i| arr.value(i) as u64));
            }
            _ => return Err(ApiError::BadCell { col: 0, want: "uint64 id" }),
        }
    }

    let mut int_names = Vec::new();
    let mut token_names = Vec::new();
    let mut int_vals: Vec<Vec<i64>> = Vec::new();
    let mut token_vals: Vec<Vec<Vec<u64>>> = Vec::new();

    for (col_idx, field) in schema.fields().iter().enumerate().skip(1) {
        let name = field.name().clone();
        match field.data_type() {
            DataType::Int64 => {
                let mut vals = vec![0i64; n];
                if let Some(batch) = &batch {
                    let arr = batch.column(col_idx).as_primitive::<arrow::datatypes::Int64Type>();
                    for (row, slot) in vals.iter_mut().enumerate() {
                        if !arr.is_null(row) {
                            *slot = arr.value(row);
                        }
                    }
                }
                int_names.push(name);
                int_vals.push(vals);
            }
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Dictionary(_, _) => {
                let mut vals = vec![Vec::new(); n];
                if let Some(batch) = &batch {
                    let arr = arrow::compute::cast(batch.column(col_idx), &DataType::Utf8)
                        .map_err(|e| ApiError::BadExpr(format!("utf8 cast error: {e}")))?;
                    let arr = arr.as_string::<i32>();
                    for (row, slot) in vals.iter_mut().enumerate() {
                        if !arr.is_null(row) {
                            *slot = tokenize(arr.value(row));
                        }
                    }
                }
                token_names.push(name);
                token_vals.push(vals);
            }
            _ => return Err(ApiError::BadCell { col: col_idx, want: "int64 or utf8" }),
        }
    }

    Ok(Parsed {
        ids,
        int_names,
        token_names,
        int_vals,
        token_vals,
        n,
    })
}

/// Materialize physical rows packed against the parsed batch's own schema order.
fn pack_in_order(p: &Parsed) -> Vec<PhysicalRow> {
    (0..p.n)
        .map(|r| PhysicalRow {
            ints: p.int_vals.iter().map(|c| c[r]).collect(),
            tokens: p.token_vals.iter().map(|c| c[r].clone()).collect(),
        })
        .collect()
}

/// Derive a [`ColumnMap`] and rows directly from an IPC batch's schema. Used by
/// the stateless `solve` and by the first `init` of a stateful workspace, where
/// the batch defines column identity for the session.
pub fn rows_from_ipc(bytes: &[u8]) -> Result<(Vec<ExtId>, Vec<PhysicalRow>, ColumnMap), ApiError> {
    let p = parse(bytes)?;
    let mut map = ColumnMap::default();
    for (i, name) in p.int_names.iter().enumerate() {
        map.int_cols.insert(name.clone(), i);
    }
    for (i, name) in p.token_names.iter().enumerate() {
        map.token_cols.insert(name.clone(), i);
    }
    let rows = pack_in_order(&p);
    Ok((p.ids, rows, map))
}

/// Lower an IPC batch against an existing [`ColumnMap`], placing each column by
/// **name** into that map's lane layout. This is what makes incremental
/// `upsert` batches order-independent: a batch may list its columns in any
/// order (or omit some, which default to zero/empty), but an unknown column or
/// a column whose kind disagrees with the workspace fails loudly rather than
/// silently shifting a lane.
pub fn rows_from_ipc_mapped(
    bytes: &[u8],
    map: &ColumnMap,
) -> Result<(Vec<ExtId>, Vec<PhysicalRow>), ApiError> {
    let p = parse(bytes)?;

    let resolve = |name: &str, here: &std::collections::HashMap<String, usize>,
                   other: &std::collections::HashMap<String, usize>,
                   this_kind: &'static str|
     -> Result<usize, ApiError> {
        if let Some(&idx) = here.get(name) {
            Ok(idx)
        } else if other.contains_key(name) {
            Err(ApiError::BadExpr(format!(
                "column '{name}' arrived as {this_kind} but the workspace knows it as the other kind"
            )))
        } else {
            Err(ApiError::UnknownColumn(name.to_string()))
        }
    };

    let int_targets = p
        .int_names
        .iter()
        .map(|name| resolve(name, &map.int_cols, &map.token_cols, "an integer"))
        .collect::<Result<Vec<_>, _>>()?;
    let token_targets = p
        .token_names
        .iter()
        .map(|name| resolve(name, &map.token_cols, &map.int_cols, "text"))
        .collect::<Result<Vec<_>, _>>()?;

    let n_int = map.int_cols.len();
    let n_tok = map.token_cols.len();
    let rows = (0..p.n)
        .map(|r| {
            let mut ints = vec![0i64; n_int];
            for (ci, &t) in int_targets.iter().enumerate() {
                ints[t] = p.int_vals[ci][r];
            }
            let mut tokens = vec![Vec::new(); n_tok];
            for (ci, &t) in token_targets.iter().enumerate() {
                tokens[t] = p.token_vals[ci][r].clone();
            }
            PhysicalRow { ints, tokens }
        })
        .collect();
    Ok((p.ids, rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Int64Array, StringArray, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    /// Serialize a batch (id, amount:i64, ref:utf8) to an IPC stream. `cols`
    /// may be reordered/renamed by callers to exercise by-name mapping.
    fn ipc(fields: Vec<Field>, cols: Vec<Arc<dyn Array>>) -> Vec<u8> {
        let schema = Arc::new(Schema::new(fields));
        let batch = RecordBatch::try_new(schema.clone(), cols).unwrap();
        let mut buf = Vec::new();
        {
            let mut w = arrow::ipc::writer::StreamWriter::try_new(&mut buf, &schema).unwrap();
            w.write(&batch).unwrap();
            w.finish().unwrap();
        }
        buf
    }

    fn schema_only(fields: Vec<Field>) -> Vec<u8> {
        let schema = Arc::new(Schema::new(fields));
        let mut buf = Vec::new();
        {
            let mut w = arrow::ipc::writer::StreamWriter::try_new(&mut buf, &schema).unwrap();
            w.finish().unwrap();
        }
        buf
    }

    #[test]
    fn derives_map_and_tokenizes() {
        let bytes = ipc(
            vec![
                Field::new("id", DataType::UInt64, false),
                Field::new("amount", DataType::Int64, true),
                Field::new("ref", DataType::Utf8, true),
            ],
            vec![
                Arc::new(UInt64Array::from(vec![7u64, 9])),
                Arc::new(Int64Array::from(vec![100i64, -100])),
                Arc::new(StringArray::from(vec!["INV0001 widgets", "INV0001 credit"])),
            ],
        );
        let (ids, rows, map) = rows_from_ipc(&bytes).unwrap();
        assert_eq!(ids, vec![7, 9]);
        assert_eq!(map.int_cols["amount"], 0);
        assert_eq!(map.token_cols["ref"], 0);
        assert_eq!(rows[0].int(0), 100);
        // "INV0001" survives (>=6 alnum); "credit" is 6 chars so it survives too;
        // both rows share the INV token, which is what bridges them.
        assert_eq!(tokenize("INV0001 widgets"), rows[0].tokens(0));
        assert!(!rows[0].tokens(0).is_empty());
    }

    #[test]
    fn schema_only_init_yields_full_map_no_rows() {
        let bytes = schema_only(vec![
            Field::new("id", DataType::UInt64, false),
            Field::new("amount", DataType::Int64, true),
            Field::new("ref", DataType::Utf8, true),
        ]);
        let (ids, rows, map) = rows_from_ipc(&bytes).unwrap();
        assert!(ids.is_empty() && rows.is_empty());
        assert_eq!(map.int_cols["amount"], 0);
        assert_eq!(map.token_cols["ref"], 0);
    }

    #[test]
    fn mapped_upsert_is_order_independent() {
        let (_, _, map) = rows_from_ipc(&schema_only(vec![
            Field::new("id", DataType::UInt64, false),
            Field::new("amount", DataType::Int64, true),
            Field::new("day", DataType::Int64, true),
            Field::new("ref", DataType::Utf8, true),
        ]))
        .unwrap();
        // Upsert batch lists columns in a different order than init.
        let bytes = ipc(
            vec![
                Field::new("id", DataType::UInt64, false),
                Field::new("ref", DataType::Utf8, true),
                Field::new("day", DataType::Int64, true),
                Field::new("amount", DataType::Int64, true),
            ],
            vec![
                Arc::new(UInt64Array::from(vec![1u64])),
                Arc::new(StringArray::from(vec!["hello world"])),
                Arc::new(Int64Array::from(vec![42i64])),
                Arc::new(Int64Array::from(vec![500i64])),
            ],
        );
        let (ids, rows) = rows_from_ipc_mapped(&bytes, &map).unwrap();
        assert_eq!(ids, vec![1]);
        // Lanes land where the *map* says, not where the batch ordered them.
        assert_eq!(rows[0].int(map.int_cols["amount"]), 500);
        assert_eq!(rows[0].int(map.int_cols["day"]), 42);
        assert_eq!(rows[0].tokens(map.token_cols["ref"]), tokenize("hello world"));
    }

    #[test]
    fn mapped_upsert_rejects_unknown_column() {
        let (_, _, map) = rows_from_ipc(&schema_only(vec![
            Field::new("id", DataType::UInt64, false),
            Field::new("amount", DataType::Int64, true),
        ]))
        .unwrap();
        let bytes = ipc(
            vec![
                Field::new("id", DataType::UInt64, false),
                Field::new("bogus", DataType::Int64, true),
            ],
            vec![
                Arc::new(UInt64Array::from(vec![1u64])),
                Arc::new(Int64Array::from(vec![1i64])),
            ],
        );
        assert!(matches!(
            rows_from_ipc_mapped(&bytes, &map),
            Err(ApiError::UnknownColumn(c)) if c == "bogus"
        ));
    }
}
