//! The plugin authoring SDK (private, `sdk` feature).
//!
//! A domain author implements [`Plugin`] — project the host's columnar Arrow
//! [`Table`] into typed items, name a stable [`Plugin::id`], pick a
//! [`Strategy`](crate::strategy::Strategy) — and `export_plugin!` emits a
//! self-describing wasm module. Everything stateful is inherited from
//! [`Recon`](crate::Recon); the only invariant-bearing author code is
//! `key`/`primary`, which the [`conformance`] kit checks mechanically.

/// The host↔plugin ABI version. Bumped only on a breaking change to the export
/// signatures, the `Cmd` set, or the Report envelope.
pub const ABI_VERSION: u32 = 1;

pub mod abi;
pub mod conformance;
mod describe;
mod plugin;
mod record;
mod table;

pub use abi::Session;
pub use describe::{DescribeDoc, Domain, Field, FieldType};
pub use plugin::{Plugin, StableHasher, hash_key};
pub use record::Record;
pub use table::{RowView, Table};

/// `#[derive(Record)]` — one struct as schema + projection + identity.
pub use florecon_derive::Record;

/// FNV-1a of a category string to a signed id lane (re-exported for plugins).
pub use crate::token::cat;
