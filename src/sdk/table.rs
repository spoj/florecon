//! The columnar table the host ships and the per-row view a plugin projects.
//!
//! The host holds a simple columnar table and sends it as an Arrow IPC stream.
//! The SDK decodes it once into a [`Table`] **against the plugin's declared
//! schema** ([`DescribeDoc`]): every declared column must be shipped with a
//! compatible type, or [`Table::from_ipc`] fails loudly. The plugin's
//! `project`/`id` then see a zero-copy [`RowView`] and never touch bytes, IPC
//! framing, or null handling.
//!
//! Because the schema is enforced, [`RowView`] accessors **panic on a contract
//! breach** — reading an undeclared column, or one of the wrong type — turning
//! the old silent zero-fill (a conserved-amount bug that slipped past the
//! conformance kit) into an immediate, located failure.

use std::collections::HashMap;

use arrow::array::Array;
use arrow::datatypes::DataType;
use arrow::ipc::reader::StreamReader;

use crate::sdk::describe::{DescribeDoc, FieldType};

/// A decoded column, materialized to a dense Rust vector (nulls become the
/// type's zero / empty value — declare columns the plugin actually needs).
enum Column {
    I64(Vec<i64>),
    F64(Vec<f64>),
    Str(Vec<String>),
}

/// A columnar table decoded from the host's Arrow IPC stream, holding exactly
/// the columns the plugin declared.
pub struct Table {
    cols: HashMap<String, Column>,
    rows: usize,
}

impl Table {
    /// Number of rows.
    pub fn len(&self) -> usize {
        self.rows
    }
    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// True if `col` was declared and decoded.
    pub fn has(&self, col: &str) -> bool {
        self.cols.contains_key(col)
    }

    /// Decoded column names.
    pub fn columns(&self) -> impl Iterator<Item = &str> {
        self.cols.keys().map(|s| s.as_str())
    }

    /// A view over row `idx`.
    pub fn row(&self, idx: usize) -> RowView<'_> {
        RowView { table: self, idx }
    }

    /// Decode an Arrow IPC stream against `schema`, keeping only the declared
    /// columns. Every declared column must be present with a compatible type
    /// (`i64 ⇐ {Int64, Int32, Date32, Date64}`, `f64 ⇐ {Float64, Float32,
    /// Int64, Int32}`, `utf8 ⇐ {Utf8, LargeUtf8}`); a missing or mistyped column
    /// is an error rather than a silent zero. An empty stream decodes to an
    /// empty table.
    pub fn from_ipc(bytes: &[u8], schema: &DescribeDoc) -> Result<Self, String> {
        if bytes.is_empty() {
            return Ok(Table { cols: HashMap::new(), rows: 0 });
        }
        let reader = StreamReader::try_new(bytes, None).map_err(|e| e.to_string())?;
        let mut cols: HashMap<String, Column> = HashMap::new();
        let mut rows = 0usize;
        for batch in reader {
            let batch = batch.map_err(|e| e.to_string())?;
            let aschema = batch.schema();
            rows += batch.num_rows();
            for field in &schema.input {
                let idx = aschema
                    .index_of(&field.name)
                    .map_err(|_| format!("missing declared column {:?}", field.name))?;
                decode_column(&mut cols, &field.name, field.ty, batch.column(idx))?;
            }
        }
        Ok(Table { cols, rows })
    }
}

/// Decode one Arrow column into the plugin's declared type, coercing within the
/// numeric/text families and erroring on an incompatible shipped type.
fn decode_column(
    cols: &mut HashMap<String, Column>,
    name: &str,
    ty: FieldType,
    arr: &dyn Array,
) -> Result<(), String> {
    use arrow::array::AsArray;
    use arrow::datatypes::{
        Date32Type, Date64Type, Float32Type, Float64Type, Int32Type, Int64Type,
    };
    let mismatch = |dt: &DataType| format!("column {name:?}: declared {ty:?}, shipped {dt:?}");
    match ty {
        FieldType::I64 => {
            let v: Vec<i64> = match arr.data_type() {
                DataType::Int64 => {
                    let a = arr.as_primitive::<Int64Type>();
                    (0..a.len()).map(|i| if a.is_null(i) { 0 } else { a.value(i) }).collect()
                }
                DataType::Int32 => {
                    let a = arr.as_primitive::<Int32Type>();
                    (0..a.len()).map(|i| if a.is_null(i) { 0 } else { a.value(i) as i64 }).collect()
                }
                DataType::Date32 => {
                    let a = arr.as_primitive::<Date32Type>();
                    (0..a.len()).map(|i| if a.is_null(i) { 0 } else { a.value(i) as i64 }).collect()
                }
                DataType::Date64 => {
                    let a = arr.as_primitive::<Date64Type>();
                    (0..a.len()).map(|i| if a.is_null(i) { 0 } else { a.value(i) }).collect()
                }
                other => return Err(mismatch(other)),
            };
            cols.entry(name.to_string()).or_insert_with(|| Column::I64(Vec::new()));
            if let Some(Column::I64(dst)) = cols.get_mut(name) {
                dst.extend(v);
            }
        }
        FieldType::F64 => {
            let v: Vec<f64> = match arr.data_type() {
                DataType::Float64 => {
                    let a = arr.as_primitive::<Float64Type>();
                    (0..a.len()).map(|i| if a.is_null(i) { 0.0 } else { a.value(i) }).collect()
                }
                DataType::Float32 => {
                    let a = arr.as_primitive::<Float32Type>();
                    (0..a.len()).map(|i| if a.is_null(i) { 0.0 } else { a.value(i) as f64 }).collect()
                }
                DataType::Int64 => {
                    let a = arr.as_primitive::<Int64Type>();
                    (0..a.len()).map(|i| if a.is_null(i) { 0.0 } else { a.value(i) as f64 }).collect()
                }
                DataType::Int32 => {
                    let a = arr.as_primitive::<Int32Type>();
                    (0..a.len()).map(|i| if a.is_null(i) { 0.0 } else { a.value(i) as f64 }).collect()
                }
                other => return Err(mismatch(other)),
            };
            cols.entry(name.to_string()).or_insert_with(|| Column::F64(Vec::new()));
            if let Some(Column::F64(dst)) = cols.get_mut(name) {
                dst.extend(v);
            }
        }
        FieldType::Utf8 => {
            let v: Vec<String> = match arr.data_type() {
                DataType::Utf8 => {
                    let a = arr.as_string::<i32>();
                    (0..a.len()).map(|i| if a.is_null(i) { String::new() } else { a.value(i).to_string() }).collect()
                }
                DataType::LargeUtf8 => {
                    let a = arr.as_string::<i64>();
                    (0..a.len()).map(|i| if a.is_null(i) { String::new() } else { a.value(i).to_string() }).collect()
                }
                other => return Err(mismatch(other)),
            };
            cols.entry(name.to_string()).or_insert_with(|| Column::Str(Vec::new()));
            if let Some(Column::Str(dst)) = cols.get_mut(name) {
                dst.extend(v);
            }
        }
    }
    Ok(())
}

/// A zero-copy view over one row of a [`Table`]. Accessors return the column's
/// zero value (`0` / `0.0` / `""`) for a declared-but-null cell, and **panic**
/// on a schema breach: reading an undeclared column or one of the wrong type is
/// a plugin bug, surfaced immediately rather than masked as a zero.
pub struct RowView<'a> {
    table: &'a Table,
    idx: usize,
}

impl<'a> RowView<'a> {
    /// Read an integer lane (money in minor units, an epoch day, …).
    pub fn i64(&self, col: &str) -> i64 {
        match self.table.cols.get(col) {
            Some(Column::I64(v)) => v.get(self.idx).copied().unwrap_or(0),
            Some(Column::F64(v)) => v.get(self.idx).copied().unwrap_or(0.0) as i64,
            Some(Column::Str(_)) => panic!("column {col:?} is text; read it with str()"),
            None => panic!("undeclared column {col:?} (add it to describe())"),
        }
    }
    /// Read a float lane.
    pub fn f64(&self, col: &str) -> f64 {
        match self.table.cols.get(col) {
            Some(Column::F64(v)) => v.get(self.idx).copied().unwrap_or(0.0),
            Some(Column::I64(v)) => v.get(self.idx).copied().unwrap_or(0) as f64,
            Some(Column::Str(_)) => panic!("column {col:?} is text; read it with str()"),
            None => panic!("undeclared column {col:?} (add it to describe())"),
        }
    }
    /// Read a text lane.
    pub fn str(&self, col: &str) -> &str {
        match self.table.cols.get(col) {
            Some(Column::Str(v)) => v.get(self.idx).map(|s| s.as_str()).unwrap_or(""),
            Some(Column::I64(_)) | Some(Column::F64(_)) => {
                panic!("column {col:?} is numeric; read it with i64()/f64()")
            }
            None => panic!("undeclared column {col:?} (add it to describe())"),
        }
    }
}
