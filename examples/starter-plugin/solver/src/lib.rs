//! solver — a florecon plugin starter.
//!
//! The matching strategy is Rust compiled to WebAssembly; a generic host (the
//! Python `florecon-host`, or the native `harness` next door) ships raw rows to
//! it and reports the proposed groups. The plugin never modifies accounting
//! values — conservation of the numeraire is guaranteed by construction, so a
//! bad strategy yields a bad *proposal*, never a broken ledger.
//!
//! Fill in the four numbered spots, then iterate with `just author` (native,
//! fast, on `data/sample.csv`). Ship with `just ship` (the production wasm).

use florecon::export_plugin;
use florecon::sdk::{Domain, Plugin, Record};
use florecon::strategy::{Item, Strategy, agg_net, exact_1to1, partition_by, seq};
use florecon::token::fnv1a;
use serde::Deserialize;

/// 1. The raw row the host ships. This one struct is the input schema
///    (`describe()`), the typed projection, and the row identity. Mark the
///    stable id with `#[record(id)]` and the display amount with
///    `#[record(amount)]`.
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

/// 2. Runtime tunables, delivered at `init` as JSON (tune without rebuilding).
///    Use `type Config = ()` if the plugin has none.
#[derive(Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    tol: i64,
}

pub struct Solver {
    config: Config,
}

impl Plugin for Solver {
    type Input = Line;
    type Row = Row;
    type Config = Config;

    fn domain() -> Domain {
        Domain::new("example.starter", "0.1.0")
    }

    fn new(config: Config) -> Self {
        Solver { config }
    }

    /// 3. Derive the typed match row from the raw input (row-local, no other
    ///    rows). Hash categorical keys with `fnv1a`; convert money to signed
    ///    minor units (cents) so netting is exact.
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
    ///    (clean pairs), `signal_group` (token buckets), `flow` (N:M), wrapped
    ///    in `partition_by` / `when` / `seq` / `fixed_point`.
    fn strategy(&self) -> Box<dyn Strategy<Row>> {
        let tol = self.config.tol;
        partition_by(
            |r: &Item<Row>| r.data.key,
            move |_| {
                seq(vec![
                    agg_net(|_r: &Item<Row>| 0u64, move |g| g.net().abs() <= tol),
                    exact_1to1(|_r: &Item<Row>| Some(0)),
                ])
            },
        )
    }
}

export_plugin!(Solver);
