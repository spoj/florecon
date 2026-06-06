//! florecon — incremental financial reconciliation as a conserving strategy algebra.
//!
//! The centerpiece is the [`strategy`] algebra: `Strategy: Bag -> (Groups,
//! residual)`, conserving by construction. Everything else is scaffolding around
//! it.
//!
//! - [`strategy`] — **the spine.** A combinator algebra over an unordered bag of
//!   items. Cheap deterministic primitives (`exact_1to1`, `agg_net`,
//!   `signal_group`, `running_zero`, `pivot`, `trim`, `snap`) cascade and shard
//!   via `seq`, `partition_by`, `branch`, and `windowed`. One leaf —
//!   [`strategy::flow`] — is the min-cost-flow arbiter: describe a domain via
//!   [`strategy::Model`] and it drives a [`strategy::Matcher`] over the
//!   [`engine`]. The engine is just that one node's implementation.
//! - [`engine`] — the network-simplex transportation solver behind the `flow`
//!   node: stable [`engine::NodeId`]/[`engine::ArcId`] handles, warm-started
//!   re-solving, [`engine::Snapshot`] persistence. Frozen because it is a
//!   hard-won correct algorithm, not because it is central.
//! - [`recon`] — the stateful facade [`recon::Recon`] around *any* strategy:
//!   `upsert` / `remove` / `solve` / `freeze` / `breakup` / `group`, returning an
//!   allocation-hypergraph [`Report`]. Conservation is enforced at the boundary.
//! - [`sdk`] — the plugin authoring surface: implement [`sdk::Plugin`] (project a
//!   columnar Arrow [`sdk::Table`] the host ships into typed items, name a stable
//!   key, pick a [`strategy`]), then `export_plugin!` emits a self-describing
//!   wasm module. The host stays a dumb columnar-table carrier.
//!
//! Enable the `serde` feature to serialize [`engine::Snapshot`] /
//! [`strategy::MatcherSnapshot`] to disk and warm-start next month off this
//! month's tree.
//!
//! ```
//! use florecon::strategy::{Model, Matcher};
//!
//! struct Tx { amount: i64, date: i64 }
//! struct M;
//! impl Model for M {
//!     type Tx = Tx;
//!     fn base_amount(&self, t: &Tx) -> i64 { t.amount }
//!     fn penalty(&self, _t: &Tx) -> f64 { 1e6 }
//!     fn block_key(&self, t: &Tx) -> i64 { t.date }
//!     fn window(&self) -> i64 { 3 }
//!     fn cost(&self, a: &Tx, b: &Tx) -> Option<f64> {
//!         Some(1.0 + (a.amount + b.amount).abs() as f64 * 0.1)
//!     }
//! }
//!
//! let mut r = Matcher::new(M);
//! r.upsert(1, Tx { amount: 100, date: 0 });
//! r.upsert(2, Tx { amount: -100, date: 0 });
//! r.solve();
//! assert!(r.groups()[0].clean);
//! ```

pub mod engine;
pub mod error;
pub mod token;

pub mod strategy;

pub mod recon;
pub mod report;

#[cfg(feature = "sdk")]
pub mod sdk;

pub use engine::{ArcId, Network, NodeId, SolveStatus};
pub use error::ApiError;
pub use strategy::{ExtId, Matcher, Model};

pub use recon::Recon;
pub use report::{AllocationOut, Component, GroupOut, ProjectionError, Report, Status};
