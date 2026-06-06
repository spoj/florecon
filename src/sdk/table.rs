//! The columnar table the host ships and the per-row view a plugin projects.
//!
//! The host holds a simple columnar table and sends it as an Arrow IPC stream.
//! The SDK decodes it once into a [`Table`]; the plugin's `project`/`key` see a
//! zero-copy [`RowView`] and never touch bytes, IPC framing, or null handling.

use std::collections::HashMap;

use arrow::array::{Array, AsArray};
use arrow::datatypes::DataType;
use arrow::ipc::reader::StreamReader;

/// A decoded column, materialized to a dense Rust vector (nulls become the
/// type's zero / empty value — declare columns the plugin actually needs).
enum Column {
    I64(Vec<i64>),
    F64(Vec<f64>),
    Str(Vec<String>),
}

/// A columnar table decoded from the host's Arrow IPC stream.
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

    /// True if `col` is present in the shipped schema.
    pub fn has(&self, col: &str) -> bool {
        self.cols.contains_key(col)
    }

    /// Column names present in the table.
    pub fn columns(&self) -> impl Iterator<Item = &str> {
        self.cols.keys().map(|s| s.as_str())
    }

    /// A view over row `idx`.
    pub fn row(&self, idx: usize) -> RowView<'_> {
        RowView { table: self, idx }
    }

    /// Decode an Arrow IPC stream (one or more record batches) into a table.
    /// Supports `Int64`, `Float64`, `Utf8`/`LargeUtf8`, and `Date32`/`Date64`
    /// (lowered to `i64` days/millis as the engine's ordering lane).
    pub fn from_ipc(bytes: &[u8]) -> Result<Self, String> {
        if bytes.is_empty() {
            return Ok(Table { cols: HashMap::new(), rows: 0 });
        }
        let reader = StreamReader::try_new(bytes, None).map_err(|e| e.to_string())?;
        let mut cols: HashMap<String, Column> = HashMap::new();
        let mut rows = 0usize;
        for batch in reader {
            let batch = batch.map_err(|e| e.to_string())?;
            let schema = batch.schema();
            rows += batch.num_rows();
            for (ci, field) in schema.fields().iter().enumerate() {
                let arr = batch.column(ci);
                let name = field.name();
                match field.data_type() {
                    DataType::Int64 => {
                        let a = arr.as_primitive::<arrow::datatypes::Int64Type>();
                        push_i64(&mut cols, name, (0..a.len()).map(|i| if a.is_null(i) { 0 } else { a.value(i) }));
                    }
                    DataType::Int32 => {
                        let a = arr.as_primitive::<arrow::datatypes::Int32Type>();
                        push_i64(&mut cols, name, (0..a.len()).map(|i| if a.is_null(i) { 0 } else { a.value(i) as i64 }));
                    }
                    DataType::Date32 => {
                        let a = arr.as_primitive::<arrow::datatypes::Date32Type>();
                        push_i64(&mut cols, name, (0..a.len()).map(|i| if a.is_null(i) { 0 } else { a.value(i) as i64 }));
                    }
                    DataType::Date64 => {
                        let a = arr.as_primitive::<arrow::datatypes::Date64Type>();
                        push_i64(&mut cols, name, (0..a.len()).map(|i| if a.is_null(i) { 0 } else { a.value(i) }));
                    }
                    DataType::Float64 => {
                        let a = arr.as_primitive::<arrow::datatypes::Float64Type>();
                        push_f64(&mut cols, name, (0..a.len()).map(|i| if a.is_null(i) { 0.0 } else { a.value(i) }));
                    }
                    DataType::Float32 => {
                        let a = arr.as_primitive::<arrow::datatypes::Float32Type>();
                        push_f64(&mut cols, name, (0..a.len()).map(|i| if a.is_null(i) { 0.0 } else { a.value(i) as f64 }));
                    }
                    DataType::Utf8 => {
                        let a = arr.as_string::<i32>();
                        push_str(&mut cols, name, (0..a.len()).map(|i| if a.is_null(i) { String::new() } else { a.value(i).to_string() }));
                    }
                    DataType::LargeUtf8 => {
                        let a = arr.as_string::<i64>();
                        push_str(&mut cols, name, (0..a.len()).map(|i| if a.is_null(i) { String::new() } else { a.value(i).to_string() }));
                    }
                    other => return Err(format!("column {name:?}: unsupported arrow type {other:?}")),
                }
            }
        }
        Ok(Table { cols, rows })
    }
}

fn push_i64(cols: &mut HashMap<String, Column>, name: &str, it: impl Iterator<Item = i64>) {
    if let Column::I64(v) = cols.entry(name.to_string()).or_insert_with(|| Column::I64(Vec::new())) {
        v.extend(it);
    }
}
fn push_f64(cols: &mut HashMap<String, Column>, name: &str, it: impl Iterator<Item = f64>) {
    if let Column::F64(v) = cols.entry(name.to_string()).or_insert_with(|| Column::F64(Vec::new())) {
        v.extend(it);
    }
}
fn push_str(cols: &mut HashMap<String, Column>, name: &str, it: impl Iterator<Item = String>) {
    if let Column::Str(v) = cols.entry(name.to_string()).or_insert_with(|| Column::Str(Vec::new())) {
        v.extend(it);
    }
}

/// A zero-copy view over one row of a [`Table`]. Accessors return the column's
/// zero value (`0` / `0.0` / `""`) when the column is absent or null, so a
/// plugin reads declared columns directly.
pub struct RowView<'a> {
    table: &'a Table,
    idx: usize,
}

impl<'a> RowView<'a> {
    /// Read an integer lane (money in minor units, an epoch day, an int32, …).
    pub fn i64(&self, col: &str) -> i64 {
        match self.table.cols.get(col) {
            Some(Column::I64(v)) => v.get(self.idx).copied().unwrap_or(0),
            Some(Column::F64(v)) => v.get(self.idx).copied().unwrap_or(0.0) as i64,
            _ => 0,
        }
    }
    /// Read a float lane.
    pub fn f64(&self, col: &str) -> f64 {
        match self.table.cols.get(col) {
            Some(Column::F64(v)) => v.get(self.idx).copied().unwrap_or(0.0),
            Some(Column::I64(v)) => v.get(self.idx).copied().unwrap_or(0) as f64,
            _ => 0.0,
        }
    }
    /// Read a text lane.
    pub fn str(&self, col: &str) -> &str {
        match self.table.cols.get(col) {
            Some(Column::Str(v)) => v.get(self.idx).map(|s| s.as_str()).unwrap_or(""),
            _ => "",
        }
    }
}
