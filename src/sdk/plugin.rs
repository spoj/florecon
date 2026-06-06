//! The plugin authoring trait: the entire surface a domain author implements.
//!
//! Everything stateful — warm-start, group-id minting, freeze stability,
//! conservation, the Report, the wasm ABI — is owned by [`Recon`](crate::Recon)
//! and `export_plugin!`. The author supplies only the domain: how to project the
//! host's columnar table into typed items, the stable identity of a row, the
//! conserved numeraire, and which [`Strategy`] to run.

use std::hash::{Hash, Hasher};

use crate::ExtId;
use crate::sdk::describe::DescribeDoc;
use crate::sdk::table::RowView;
use crate::strategy::Strategy;

/// A self-contained reconciliation domain, compiled to one wasm module.
pub trait Plugin: Sized {
    /// The typed row the strategy matches on (the author's lanes).
    type Row: Clone + 'static;

    /// Construct the plugin (load any baked-in reference data here).
    fn new() -> Self;

    /// Advertise the raw columns, numeraire, and identity to the host.
    fn describe() -> DescribeDoc;

    /// The stable external id of a row. Return the host's own stable id column
    /// when the data carries one, or derive one deterministically with
    /// [`hash_key`] over the natural key. MUST be unique per logical row —
    /// warm-start and frozen decisions key off it.
    fn id(&self, row: &RowView<'_>) -> ExtId;

    /// Row-local: derive the typed match lanes. Deterministic, no other rows.
    fn project(&self, row: &RowView<'_>) -> Self::Row;

    /// The conserved numeraire (single, signed, minor units). This is what
    /// [`Recon`](crate::Recon) conserves and may be *derived* from several
    /// columns — it is distinct from the host's display
    /// [`amount`](crate::sdk::Field::amount) column, which is only a UI hint.
    fn primary(row: &Self::Row) -> i64;

    /// The matching cascade.
    fn strategy(&self) -> Box<dyn Strategy<Self::Row>>;
}

/// A stable FNV-1a hasher, so `ext_id` is reproducible across builds and hosts
/// (unlike `std`'s `DefaultHasher`, whose seed is unspecified).
pub struct StableHasher(u64);

impl Default for StableHasher {
    fn default() -> Self {
        StableHasher(0xcbf29ce484222325)
    }
}

impl Hasher for StableHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }
}

/// Hash a composite natural key to a stable external id, deterministically
/// across builds and hosts (unlike `std`'s `DefaultHasher`).
pub fn hash_key<K: Hash>(key: &K) -> ExtId {
    let mut h = StableHasher::default();
    key.hash(&mut h);
    h.finish()
}
