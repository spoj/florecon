//! florecon — two dual conservation laws over accounting data.
//!
//! The same ledger is read two ways, and florecon is the algebra for each:
//!
//! - **[`mod@strategy`] — reconcile.** Parse a bag of [`strategy::Entry`]s (a
//!   stable id plus an opaque payload) into a partition of [`strategy::Group`]s.
//!   A [`strategy::Strategy`] is a pure function `bag -> (groups, residual)`;
//!   combinators ([`strategy::seq`], [`strategy::partition_by`],
//!   [`strategy::when`]) arrange leaves ([`strategy::exact_1to1`],
//!   [`strategy::agg_net`], [`strategy::flow`], …). It conserves **identity**:
//!   each input id lands in exactly one group or in residual — nothing lost,
//!   nothing invented.
//!
//! - **[`mod@alloc`] — allocate.** Re-express a coarse [`alloc::Measure`] (a
//!   sparse quantity over named axes) on a finer basis: reshape
//!   ([`alloc::Measure::rekey`]), couple ([`alloc::Measure::combine`]),
//!   down-project ([`alloc::Measure::allocate`]). It conserves **value**: a cost
//!   split across a driver is exact to the penny, and every residual parks
//!   in-cube on [`alloc::ANY`] rather than vanishing.
//!
//! The two are duals — partition by identity vs. couple by value — and share one
//! stance: **no privileged numeraire**. Every number is a closure over the
//! caller's own payload, so multi-currency is just different closures, and there
//! is no `pivot`, no `original`, no magic field.
//!
//! ```
//! // reconcile: an opposite-and-equal pair collapses to one group.
//! use florecon::strategy::*;
//!
//! #[derive(Clone)]
//! struct Tx { amount: i64 }
//!
//! let s = exact_1to1(|_: &Entry<Tx>| Some(0), |e| e.amount);
//! let r = s.run(vec![Entry::new(1, Tx { amount: 100 }), Entry::new(2, Tx { amount: -100 })]);
//! assert_eq!(r.groups.len(), 1);
//! assert_eq!(r.groups[0].size(), 2);
//! ```
//!
//! ```
//! // allocate: rent pushed onto products by revenue, penny-exact.
//! use florecon::alloc::*;
//!
//! let rent = Measure::build(&["geog"], &[(&[("geog", 1)], 1000)]);
//! let rev = Measure::build(&["geog", "product"], &[
//!     (&[("geog", 1), ("product", 10)], 30),
//!     (&[("geog", 1), ("product", 11)], 70),
//! ]);
//! let a = rent.allocate(&rev);
//! assert_eq!(a.total(), rent.total()); // value conserved
//! assert_eq!(a.get(&Key::of(&[("geog", 1), ("product", 10)])), 300);
//! ```

pub mod alloc;
pub mod strategy;
pub mod token;

pub use strategy::{Entry, FlowSpec, Group, Id, Resolution, Strategy};
