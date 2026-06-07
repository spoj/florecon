//! The conformance kit: mechanically prove a plugin got identity right.
//!
//! These properties only break when `id`/`project`/`primary` are wrong
//! (non-unique, unstable, order-dependent, or non-conserving). Because the SDK
//! drives the harness, a plugin author gets them for free — no hand-written
//! invariant tests.

use std::collections::{BTreeMap, BTreeSet};

use crate::ExtId;
use crate::Report;
use crate::recon::Recon;
use crate::report::Status;
use crate::sdk::plugin::Plugin;
use crate::sdk::record::Record;
use crate::sdk::table::Table;

/// Canonical, label-free form of a report: each group as its sorted membership
/// plus metadata, with the whole set sorted. This ignores the monotonic
/// group-id integers, which legitimately differ between a one-solve cold load
/// and a multi-solve warm load (live ids are ephemeral by design).
type Canon = Vec<(String, i64, Status, Option<String>, Vec<(ExtId, i64)>)>;

fn canon(report: &Report) -> Canon {
    let mut members: BTreeMap<u64, Vec<(ExtId, i64)>> = BTreeMap::new();
    for a in &report.allocations {
        members
            .entry(a.group_id)
            .or_default()
            .push((a.id, a.amount));
    }
    let mut out: Canon = report
        .groups
        .iter()
        .map(|g| {
            let mut m = members.remove(&g.group_id).unwrap_or_default();
            m.sort();
            (g.origin.clone(), g.net, g.status, g.reason.clone(), m)
        })
        .collect();
    out.sort();
    out
}

fn project_items<P: Plugin>(plugin: &P, table: &Table) -> Vec<(ExtId, P::Row)> {
    (0..table.len())
        .map(|i| {
            let input = P::Input::from_view(&table.row(i));
            (input.ext_id(), plugin.project(&input))
        })
        .collect()
}

/// Cold load: upsert everything, then one solve.
fn run_cold<P: Plugin + 'static>(items: &[(ExtId, P::Row)]) -> Report {
    let plugin = P::new(P::Config::default());
    let mut recon = Recon::new(plugin.strategy(), P::primary);
    for (id, row) in items {
        recon.upsert(*id, row.clone());
    }
    recon.solve().expect("cold solve");
    recon.report()
}

/// Warm load: ingest and solve in two interleaved halves.
fn run_warm<P: Plugin + 'static>(items: &[(ExtId, P::Row)]) -> Report {
    let plugin = P::new(P::Config::default());
    let mut recon = Recon::new(plugin.strategy(), P::primary);
    let mid = items.len() / 2;
    for (id, row) in &items[..mid] {
        recon.upsert(*id, row.clone());
    }
    recon.solve().expect("warm solve 1");
    for (id, row) in &items[mid..] {
        recon.upsert(*id, row.clone());
    }
    recon.solve().expect("warm solve 2");
    recon.report()
}

/// Assert that a plugin satisfies the identity/derive contract on `arrow` (an
/// Arrow IPC batch the host would ship). Panics with a precise message on the
/// first violated property.
pub fn assert_conformance<P: Plugin + 'static>(arrow: &[u8]) {
    let doc = P::describe();
    assert!(
        doc.input.iter().filter(|f| f.amount).count() <= 1,
        "describe() declares more than one amount() column"
    );
    let table = Table::from_ipc(arrow, &doc).expect("decode arrow batch");
    let plugin = P::new(P::Config::default());
    let items = project_items::<P>(&plugin, &table);

    // Unique identity within the batch.
    let mut seen: BTreeSet<ExtId> = BTreeSet::new();
    for (id, _) in &items {
        assert!(
            seen.insert(*id),
            "non-unique id: two rows hash to id {id} (fix Plugin::id)"
        );
    }

    let cold = run_cold::<P>(&items);
    let cold_c = canon(&cold);

    // Idempotent re-upsert: same ids overwrite, report is unchanged.
    let mut twice = items.clone();
    twice.extend(items.clone());
    assert_eq!(
        canon(&run_cold::<P>(&twice)),
        cold_c,
        "re-upserting the same batch changed the report (unstable id)"
    );

    // Order independence: no positional identity.
    let mut reversed = items.clone();
    reversed.reverse();
    assert_eq!(
        canon(&run_cold::<P>(&reversed)),
        cold_c,
        "row order changed the report (positional identity)"
    );

    // Warm == cold: incremental ingestion matches a cold load (up to ephemeral
    // group-id relabeling).
    assert_eq!(
        canon(&run_warm::<P>(&items)),
        cold_c,
        "warm incremental result differs from cold (warm-start integrity)"
    );
}
