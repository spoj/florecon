//! Self-description: what a plugin advertises to a generic host.
//!
//! The same document powers the host UI (which raw columns to ship, which is the
//! numeraire) and discovery. It is returned at runtime by the `describe()` wasm
//! export.

use serde::Serialize;

use crate::sdk::ABI_VERSION;

/// The wire type of a raw input column the host must supply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    I64,
    F64,
    Utf8,
}

/// One raw input column the plugin consumes from the host's columnar table.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Field {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: FieldType,
    /// True for the single column that carries the conserved numeraire.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub primary: bool,
}

impl Field {
    pub fn int(name: &str) -> Self {
        Field { name: name.into(), ty: FieldType::I64, primary: false }
    }
    pub fn float(name: &str) -> Self {
        Field { name: name.into(), ty: FieldType::F64, primary: false }
    }
    pub fn text(name: &str) -> Self {
        Field { name: name.into(), ty: FieldType::Utf8, primary: false }
    }
    /// Mark this column as the conserved numeraire.
    pub fn primary(mut self) -> Self {
        self.primary = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Domain {
    pub id: String,
    pub version: String,
}

/// The full self-description a plugin returns from `describe()`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DescribeDoc {
    pub abi_version: u32,
    pub domain: Domain,
    pub input: Vec<Field>,
    pub report_schema: u32,
}

impl DescribeDoc {
    /// Start a description for a domain `id` at semantic `version`.
    pub fn new(id: &str, version: &str) -> Self {
        DescribeDoc {
            abi_version: ABI_VERSION,
            domain: Domain { id: id.into(), version: version.into() },
            input: Vec::new(),
            report_schema: 1,
        }
    }

    /// Declare the raw input columns the host must supply (exactly one `primary`).
    pub fn input(mut self, fields: Vec<Field>) -> Self {
        self.input = fields;
        self
    }

    /// The name of the declared primary column, if any.
    pub fn primary_field(&self) -> Option<&str> {
        self.input.iter().find(|f| f.primary).map(|f| f.name.as_str())
    }
}
