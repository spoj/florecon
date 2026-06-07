//! The wire record: one struct that is simultaneously a plugin's input schema,
//! its typed projection, and its identity — produced by `#[derive(Record)]`.
//!
//! An author derives it on a plain struct whose fields are the raw columns the
//! host ships; the derive generates [`Record::fields`] (which feeds
//! [`describe()`](crate::sdk::Plugin::describe)), [`Record::from_view`] (the
//! typed projection of a [`RowView`]), and [`Record::ext_id`]. The author's
//! [`project`](crate::sdk::Plugin::project) then maps this typed record into the
//! match row — never a stringly column name, never a raw `RowView`.

use crate::ExtId;
use crate::sdk::describe::Field;
use crate::sdk::table::RowView;

/// A typed image of one host row, with a declared columnar schema and identity.
pub trait Record: Sized {
    /// The declared input columns, in wire order (drives `describe()`).
    fn fields() -> Vec<Field>;
    /// Decode one [`RowView`] into the typed record (the schema-checked read).
    fn from_view(row: &RowView<'_>) -> Self;
    /// The stable external id of this row (the `#[record(id)]` column).
    fn ext_id(&self) -> ExtId;
}
