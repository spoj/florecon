//! florecon — incremental financial reconciliation via min-cost transportation.
//!
//! Two layers:
//!
//! - [`net`] — a domain-agnostic network-simplex engine with stable
//!   [`net::NodeId`]/[`net::ArcId`] handles, single-dummy transportation model,
//!   and warm-started re-solving across mutations.
//! - [`recon`] — an ergonomic facade: describe your domain once via
//!   [`recon::Model`], then drive it with `upsert` / `remove` / `solve` and read
//!   back netted [`recon::Group`]s.
//!
//! ```
//! use florecon::recon::{Model, Reconciler};
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
//! let mut r = Reconciler::new(M);
//! r.upsert(1, Tx { amount: 100, date: 0 });
//! r.upsert(2, Tx { amount: -100, date: 0 });
//! r.solve();
//! assert!(r.groups()[0].clean);
//! ```

pub mod net;
pub mod recon;

pub use net::{ArcId, Network, NodeId, SolveStatus};
pub use recon::{ExtId, Group, Model, Reconciler};
