//! florecon — incremental financial reconciliation via min-cost transportation.
//!
//! Four layers, each a thin lowering of the one above:
//!
//! - [`engine`] — a domain-agnostic network-simplex engine with stable
//!   [`engine::NodeId`]/[`engine::ArcId`] handles, single-dummy transportation
//!   model, warm-started re-solving with incremental potential updates, and
//!   [`engine::Snapshot`] persistence for caching the basis across runs.
//! - [`strategy`] — a combinator algebra over an unordered bag of items:
//!   `Strategy: Bag -> (Groups, residual)`, conserving by construction. Cheap
//!   deterministic primitives (`exact_1to1`, `agg_net`, `signal_group`,
//!   `running_zero`) cascade ahead of the `flow` arbiter via `seq`,
//!   `partition_by`, `branch`, and `windowed`. The `flow` leaf is itself the
//!   incremental min-cost-flow matcher: describe your domain once via
//!   [`strategy::Model`], then it drives a [`strategy::Matcher`]
//!   (`upsert` / `remove` / `solve`) and reads back netted
//!   [`strategy::Group`]s on top of the [`engine`].
//! - [`plan`] — the consumption surface: a serializable [`plan::Plan`] (the
//!   strategy tree as data, pricing included via [`plan::CostSpec`]), one
//!   generic stateful facade [`plan::Recon`] (with [`plan::Workspace`] its
//!   `Row` specialization), and a relational [`plan::Report`]. Conservation is
//!   enforced at the boundary, so a malformed plan degrades to a bad proposal,
//!   never a broken ledger. With the `wasm` feature, [`wasm`] exports this as a
//!   single C-ABI module any runtime (wasmtime, browser) can drive.
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
pub mod arrow;
pub mod sel;

pub mod plan;
mod plan_compile;
pub mod report;
pub mod row;
pub mod token;

pub mod strategy;

#[cfg(feature = "wasm")]
pub mod wasm;

pub use engine::{ArcId, Network, NodeId, SolveStatus};
pub use error::ApiError;
pub use strategy::flow::Group;
pub use strategy::{ExtId, Matcher, Model};

pub use report::{AllocationOut, Component, GroupOut, ProjectionError, Report, Status};
pub use row::{PhysicalRow, ColumnMap};

