//! __CRATE__ — a florecon plugin. Fill in the four numbered spots, then
//! `just run`. The host ships raw rows as a dataframe; this plugin owns the
//! domain (projection + matching) and conservation is guaranteed by construction.

use florecon::export_plugin;
use florecon::sdk::{Domain, Plugin, Record};
use florecon::strategy::{Strategy, Tol, agg_net, exact_1to1, partition_by, seq};
use florecon::token::fnv1a;
use serde::Deserialize;

/// 1. The raw row the host ships. This one struct is the input schema
///    (`describe()`), the typed projection, and the row identity.
#[derive(Record)]
pub struct Line {
    #[record(id)]
    id: i64,
    group: String,
    #[record(amount)]
    amount: f64,
}

/// The typed match row your strategy sees (derive whatever lanes you match on).
#[derive(Clone)]
pub struct Row {
    key: u64,
    cents: i64,
}

/// 2. Runtime tunables, delivered at `init` (tune without rebuilding). Use
///    `type Config = ()` if the plugin has none.
#[derive(Clone, Copy, Deserialize)]
#[serde(default)]
pub struct Config {
    tol: i64,
}

impl Default for Config {
    fn default() -> Self {
        Config { tol: 0 }
    }
}

pub struct Solver {
    config: Config,
}

impl Plugin for Solver {
    type Input = Line;
    type Row = Row;
    type Config = Config;

    fn domain() -> Domain {
        Domain::new("__DOMAIN__", "0.1.0")
    }

    fn new(config: Config) -> Self {
        Solver { config }
    }

    /// 3. Derive the typed match row from the raw input (row-local, no other rows).
    fn project(&self, l: &Line) -> Row {
        Row {
            key: fnv1a(l.group.as_bytes()),
            cents: (l.amount * 100.0).round() as i64,
        }
    }

    /// The conserved numeraire (single, signed, minor units).
    fn primary(row: &Row) -> i64 {
        row.cents
    }

    /// 4. The matching cascade. Reach for: `agg_net` (net by key), `exact_1to1`
    ///    (clean pairs), `signal_group` (token buckets), `flow` (N:M), wrapped in
    ///    `partition_by` / `when` / `seq` / `fixed_point`.
    fn strategy(&self) -> Box<dyn Strategy<Row>> {
        let tol = self.config.tol;
        partition_by(|r: &Row| r.key, move || {
            seq(vec![
                agg_net(|_r: &Row| 0u64, Tol::Abs(tol)),
                exact_1to1(|_r: &Row| Some(0)),
            ])
        })
    }
}

export_plugin!(Solver);
