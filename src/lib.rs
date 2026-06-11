//! florecon — reconciliation as partitioning.
//!
//! The problem: given a bag of [`strategy::Entry`]s (a stable id plus an opaque
//! payload), parse it into a list of [`strategy::Group`]s. That is the whole
//! contract. A [`strategy::Strategy`] is a pure function `bag -> (groups,
//! residual)`; combinators ([`strategy::seq`], [`strategy::partition_by`],
//! [`strategy::when`]) arrange leaves ([`strategy::exact_1to1`],
//! [`strategy::agg_net`], [`strategy::flow`], …).
//!
//! There is no privileged numeraire: every number a leaf needs is a closure over
//! the payload, so multi-currency is just different closures, and the framework
//! conserves *identity* — each input id lands in exactly one group or in
//! residual.
//!
//! ```
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

pub mod strategy;
pub mod token;

pub use strategy::{Entry, FlowSpec, Group, Id, Resolution, Strategy};
