//! The plugin authoring trait: the entire surface a domain author implements.
//!
//! Everything stateful — warm-start, group-id minting, freeze stability,
//! conservation, the Report, the wasm ABI — is owned by [`Recon`](crate::Recon)
//! and `export_plugin!`. The author supplies only the domain: how to project the
//! host's columnar table into typed items, the stable identity of a row, the
//! conserved numeraire, and which [`Strategy`] to run.

use std::hash::{Hash, Hasher};

use serde::de::DeserializeOwned;

use crate::ExtId;
use crate::sdk::describe::{DescribeDoc, Domain};
use crate::sdk::record::Record;
use crate::strategy::Strategy;

/// A self-contained reconciliation domain, compiled to one wasm module.
pub trait Plugin: Sized {
    /// The raw host row, as a `#[derive(Record)]` struct: this single type is
    /// the input schema (`describe()`), the typed projection, and the identity.
    type Input: Record;

    /// The typed row the strategy matches on (the author's lanes).
    type Row: Clone + 'static;

    /// Runtime configuration, deserialized from the `init` command's `config`
    /// (tolerances, windows, toggles — *data*, tuned without recompiling). Use
    /// `type Config = ()` when the plugin has none.
    type Config: DeserializeOwned + Default;

    /// The domain identity and semantic version (drives `describe()`).
    fn domain() -> Domain;

    /// Construct the plugin from its runtime config (load baked-in reference
    /// data, precompute tolerances from `config`, …).
    fn new(config: Self::Config) -> Self;

    /// Row-local: derive the typed match lanes from the typed input record.
    /// Deterministic, no other rows.
    fn project(&self, input: &Self::Input) -> Self::Row;

    /// The conserved numeraire (single, signed, minor units). This is what
    /// [`Recon`](crate::Recon) conserves and may be *derived* from several
    /// columns — it is distinct from the host's display
    /// [`amount`](crate::sdk::Field::amount) column, which is only a UI hint.
    fn primary(row: &Self::Row) -> i64;

    /// The matching cascade (may read `self`, hence the runtime config).
    fn strategy(&self) -> Box<dyn Strategy<Self::Row>>;

    /// The self-description shipped to the host: assembled from [`domain`] and
    /// the [`Input`](Self::Input) schema. Provided — authors never write it.
    fn describe() -> DescribeDoc {
        let d = Self::domain();
        DescribeDoc::new(&d.id, &d.version).input(Self::Input::fields())
    }
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
