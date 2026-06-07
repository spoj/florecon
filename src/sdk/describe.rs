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
    /// True for the single column the host shows as the headline figure. This
    /// is a *display* hint, distinct from the conserved numeraire
    /// ([`Plugin::primary`](crate::sdk::Plugin::primary)), which a plugin may
    /// derive from several columns.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub amount: bool,
}

impl Field {
    pub fn int(name: &str) -> Self {
        Field {
            name: name.into(),
            ty: FieldType::I64,
            amount: false,
        }
    }
    pub fn float(name: &str) -> Self {
        Field {
            name: name.into(),
            ty: FieldType::F64,
            amount: false,
        }
    }
    pub fn text(name: &str) -> Self {
        Field {
            name: name.into(),
            ty: FieldType::Utf8,
            amount: false,
        }
    }
    /// Mark this column as the host's headline display amount.
    pub fn amount(mut self) -> Self {
        self.amount = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Domain {
    pub id: String,
    pub version: String,
}

impl Domain {
    /// A domain identity `id` at semantic `version` (e.g. `"florecon.intercompany"`, `"1.0.0"`).
    pub fn new(id: &str, version: &str) -> Self {
        Domain {
            id: id.into(),
            version: version.into(),
        }
    }
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
            domain: Domain {
                id: id.into(),
                version: version.into(),
            },
            input: Vec::new(),
            report_schema: 1,
        }
    }

    /// Declare the raw input columns the host must supply (exactly one `primary`).
    pub fn input(mut self, fields: Vec<Field>) -> Self {
        self.input = fields;
        self
    }

    /// The name of the declared headline-amount column, if any.
    pub fn amount_field(&self) -> Option<&str> {
        self.input
            .iter()
            .find(|f| f.amount)
            .map(|f| f.name.as_str())
    }
}
