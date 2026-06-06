//! Layer 4 — the consumption surface (the `plan` API).
//!
//! This is the surface external hosts (a Python wheel over wasmtime, a browser
//! module, an agent emitting config) drive. It turns the closure-based
//! combinators of [`crate::strategy`] into a **data-driven, serializable**
//! pipeline so nothing but plans and results ever cross a language boundary.
//!
//! Pieces:
//! - [`Plan`] — the strategy tree *as data*, pricing included via [`CostSpec`]
//!   (no host callbacks). Serializable, so an agent can author it and a native
//!   interpreter runs it.
//! - [`Recon`] — the one generic stateful facade (`upsert` / `remove` / `solve`
//!   / `freeze` / `breakup` / …); [`Workspace`] is its [`PhysicalRow`] + [`Plan`]
//!   specialization, the schema-aware surface the WASM `dispatch` drives.
//! - [`Report`] — the allocation hypergraph result (`groups` + `allocations`).
//!
//! Conservation is enforced at the boundary: a solve verifies that every input
//! id is represented by at least one allocation edge, so a malformed plan can
//! never silently lose rows. Amount conservation lives in the strategy/flow
//! algebra and is exposed directly in report allocations.

use crate::ExtId;
use crate::plan_compile::compile;

/// The wire-contract version: the shape of [`Plan`], [`Report`], and the WASM
/// command set. Hosts (the Python wheel, the browser module) read it back from
/// the engine and refuse to run against a mismatched binary. Bump it on any
/// breaking change to those shapes.
///
/// v10 converges the wire to a single concept: one `dispatch` entry point
/// driving a persistent workspace via the [`Cmd`](crate::wasm) protocol. There
/// is no separate stateless `solve` export or `SolveRequest` shape -- a batch
/// solve is just `init` + `solve` on a workspace the caller discards. Column
/// identity still rides in the Arrow batch schema (the schema *is* the map).
/// v11 generalizes plan selectors (`branch.pred`, `partition.by`,
/// `windowed.order`, `agg_net.key`, `pivot.amount`) from a bare column name to a
/// [`Sel`](crate::sel::Sel) integer expression. Backward compatible on the
/// wire: a bare JSON string still parses as a column reference, so every v10
/// plan is a valid v11 plan.
/// v14 adds [`PlanNode::Filter`]: gate an inner subtree's output by a [`Sel`]
/// predicate over *group metrics* (`size`, `min_side`, `abs_net`, …), dissolving
/// rejected groups back into the residual. Purely additive — every v13 plan is a
/// valid v14 plan.
/// v15 adds [`PlanNode::Coalesce`]: collapse an inner subtree's allocation
/// hyperedges into connected-component clusters (groups sharing a row merge into
/// one). Also additive — every v14 plan is a valid v15 plan.
/// v17 reworks the output-shaping algebra: `coalesce` becomes a pure
/// group→group merge (residual untouched); new [`PlanNode::Trim`] and
/// [`PlanNode::Snap`] move sub-`Tol` edges to/onto the residual/dominant edge;
/// and `flow` is generalized (`day` → `order_by`, `max_day` dropped,
/// `day_slope` → `slope`, `amount_bps` → `amount_tol: Tol`). Breaking.
/// v18 adds the [`Cmd::Replan`](crate::wasm) command: recompile a new [`Plan`]
/// against the live schema and swap it into an existing workspace, preserving
/// rows, frozen decisions, and the id allocator. Purely additive to the wire
/// (one new command); every v17 plan is a valid v18 plan.
pub const CONTRACT_VERSION: u32 = 18;
pub use crate::error::ApiError;
pub use crate::report::{AllocationOut, Component, GroupOut, ProjectionError, Report, Status};
pub use crate::row::{PhysicalRow, ColumnMap};
pub use crate::sel::Sel;
pub use crate::strategy::Tol;

use crate::strategy::{Item, Strategy};
use std::collections::{BTreeMap, BTreeSet};

/// Allocation request used by allocation-native manual workspace operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AllocationSpec {
    pub id: ExtId,
    pub amount: i64,
}

// ---------------------------------------------------------------------------
// The plan (strategy tree as data)
// ---------------------------------------------------------------------------

/// A reconciliation pipeline expressed as data. `primary` names the report /
/// conservation numeraire at the plan boundary; every primitive in `root`
/// operates on that current amount unless wrapped in a [`PlanNode::Pivot`].
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Plan {
    pub primary: String,
    pub root: PlanNode,
}

/// A strategy tree expressed as data. Compiles to the closure-based
/// combinators of [`crate::strategy`]; selectors reference columns by name, but
/// primitives do not choose amount columns.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "op", rename_all = "snake_case"))]
pub enum PlanNode {
    /// Cascade: each step runs on the previous step's residual.
    Seq { steps: Vec<PlanNode> },
    /// Stamp an author `tag` onto every group `inner` produces (its report
    /// `reason`), naming a stage ("S3a exact", "intercompany netting") without
    /// changing what forms the group. Residual lots are never labeled.
    Label { tag: String, inner: Box<PlanNode> },
    /// Repeat `inner` on its own residual until it reaches a fixed point (a pass
    /// that groups nothing more) or `max` passes elapse. State inside `inner`
    /// (e.g. a warm flow `Matcher`) persists across passes; every node treats
    /// its incoming bag as the authoritative present-set, which is what makes
    /// the loop reentrant-safe.
    FixedPoint {
        inner: Box<PlanNode>,
        #[cfg_attr(feature = "serde", serde(default = "default_fixed_point_passes"))]
        max: usize,
    },
    /// Fork/join shard by a scalar [`Sel`] key, run `inner` per shard.
    Partition { by: Sel, inner: Box<PlanNode> },
    /// Route rows by a [`Sel`] predicate (non-zero = true), run different child
    /// subtrees on each side, then join. A structural split; both sides conserve.
    Branch {
        pred: Sel,
        and_then: Box<PlanNode>,
        or_else: Box<PlanNode>,
    },
    /// Run `inner` within a sliding window over an integer order [`Sel`].
    Windowed {
        order: Sel,
        width: i64,
        inner: Box<PlanNode>,
    },
    /// Temporarily match `inner` in another numeraire (a [`Sel`] expression),
    /// converting produced allocations and residuals back to the caller's active
    /// amount on exit. The conserving boundary makes the `amount` expression
    /// safe: it sets apportionment ratios, never the conserved total.
    Pivot {
        amount: Sel,
        inner: Box<PlanNode>,
    },
    /// Gate `inner`'s output: keep only the groups for which the `keep`
    /// selector evaluates non-zero, dissolving every rejected group back into
    /// the residual so a later stage (or the [`flow`](PlanNode::Flow) arbiter)
    /// can reconsider those lots. Conservation is preserved.
    ///
    /// Unlike every other selector, `keep` is a [`Sel`] over *group metrics*,
    /// not row columns — the named integer lanes are `size` (member count),
    /// `pos` / `neg` (per-sign counts), `min_side` / `max_side`
    /// (`min`/`max` of the two), `net`, `abs_net`, and `max_abs` / `min_abs`
    /// (largest/smallest member magnitude). So "reject graphs over 12 lots whose
    /// smaller side is <= 2" is
    /// `{"and":[{"le":["size",12]},{"gt":["min_side",2]}]}`.
    Filter { keep: Sel, inner: Box<PlanNode> },
    /// Collapse `inner`'s allocation-hyperedge groups into their connected
    /// components: groups that share any member id merge into one coarse group
    /// (each member id's allocations summed to a single clean edge), `origin`
    /// stamped on every merged cluster. The residual passes through. This turns
    /// the matcher's interlocking partial-allocation view into the "settlement
    /// cluster" view a human actions against. Conservation is preserved.
    ///
    /// Collapse the inner subtree's allocation hyperedges into
    /// connected-component clusters (groups sharing a row merge into one). Pure
    /// group→group transform: the residual is never touched. Compose with
    /// [`PlanNode::Trim`] / [`PlanNode::Snap`] to move material to/from residual.
    Coalesce {
        origin: String,
        inner: Box<PlanNode>,
    },
    /// Cut every group edge within `tol` (of its row's `original`) to the
    /// residual. One-directional: matched → residual. Post-condition: every
    /// surviving group edge is material.
    Trim {
        tol: Tol,
        inner: Box<PlanNode>,
    },
    /// Fold every sub-`tol` edge onto its row's dominant edge instead of the
    /// floor. The residual edge is eligible both ways, so this absorbs a small
    /// tail into its group or leaks a small match to residual, whichever side is
    /// the minority.
    Snap {
        tol: Tol,
        inner: Box<PlanNode>,
    },
    /// Accept an aggregation bucket (a [`Sel`] `key`) that nets to zero within
    /// `tol` (absolute, or relative to the bucket's smallest leg; see [`Tol`]).
    AggNet { key: Sel, tol: Tol },
    /// Pair opposite-sign rows with equal current amount magnitude.
    Exact {},
    /// Group rows that share an out-of-band token signal and net to zero
    /// within `tol` (absolute, or relative to the bucket's smallest leg).
    Signal {
        signals: String,
        tol: Tol,
        cap: usize,
    },
    /// Consume small residual allocations into variance/writeoff/unmatched
    /// classes. If `by` is present, all consumed residuals sharing that scalar
    /// key become one bucketed group; otherwise each consumed residual becomes
    /// its own singleton group.
    SoakSmall {
        #[cfg_attr(feature = "serde", serde(default))]
        max_bps: Option<i64>,
        #[cfg_attr(feature = "serde", serde(default))]
        max_abs: Option<i64>,
        origin: String,
        #[cfg_attr(feature = "serde", serde(default))]
        by: Option<String>,
    },
    /// Consume all remaining residual allocations into singleton or bucketed
    /// groups. This is normally a terminal classifier after more selective
    /// soakers.
    SoakAll {
        origin: String,
        #[cfg_attr(feature = "serde", serde(default))]
        by: Option<String>,
    },
    /// The min-cost-flow arbiter over the residual.
    Flow {
        /// 1-D ordering expression for proximity candidate generation. Flow is
        /// domain-agnostic: this is just "sort by X"; the `window` is a radius in
        /// these units.
        order_by: String,
        /// Token-signal column used for reference-bridge candidates and cost.
        tokens: String,
        penalty: f64,
        window: i64,
        /// The cost model as data. Omitted in serialized plans means the
        /// default reference-bridge > exact-amount cascade.
        #[cfg_attr(feature = "serde", serde(default))]
        cost: CostSpec,
    },
}

/// The default pass cap for [`PlanNode::FixedPoint`] when omitted on the wire.
/// Generous enough for any realistic cascade to converge, but bounded so a
/// non-convergent inner strategy can never spin unboundedly.
#[cfg(feature = "serde")]
fn default_fixed_point_passes() -> usize {
    16
}

// ---------------------------------------------------------------------------
// Flow cost model as data
// ---------------------------------------------------------------------------

/// A predicate on a candidate pair, evaluated by the flow [`CostSpec`].
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Cond {
    /// The two rows share at least one reference token.
    TokenShared,
    /// The two rows have equal, non-zero absolute flow amount.
    AmountEqual,
}

/// One confidence tier. A candidate pair takes the first tier whose `when`
/// conditions all hold; its cost is `base + slope * |Δorder|`, where `Δorder` is
/// the distance on the flow's `order_by` key. A pair matched by no tier is
/// forbidden. The candidate's order distance is already bounded by the flow
/// `window`, so a tier carries no distance cutoff of its own.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CostTier {
    pub when: Vec<Cond>,
    pub base: f64,
    /// Cost added per unit of `order_by` distance between the pair.
    #[cfg_attr(feature = "serde", serde(default))]
    pub slope: f64,
    /// Tolerance for this tier's [`Cond::AmountEqual`], measured against the
    /// smaller leg. `None` means strict equality; `Some(Tol::Rel { bps: 10, .. })`
    /// accepts amounts within 0.1% of each other (the relative-tolerance idiom).
    #[cfg_attr(feature = "serde", serde(default))]
    pub amount_tol: Option<Tol>,
}

/// The flow arbiter's cost model as ordered confidence tiers. This is the last
/// piece of strategy that used to be hardcoded; making it data closes the gap
/// between the serializable [`Plan`] and the closure-based strategy algebra.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CostSpec {
    pub tiers: Vec<CostTier>,
}

impl Default for CostSpec {
    /// The interco cascade: a shared reference token (cheapest, then cheaper
    /// still if the amount also matches) outranks an exact native amount. Order
    /// distance is bounded by the flow `window`, not per tier.
    fn default() -> Self {
        CostSpec {
            tiers: vec![
                CostTier {
                    when: vec![Cond::TokenShared, Cond::AmountEqual],
                    base: 1.5,
                    slope: 0.002,
                    amount_tol: None,
                },
                CostTier {
                    when: vec![Cond::TokenShared],
                    base: 2.0,
                    slope: 0.002,
                    amount_tol: None,
                },
                CostTier {
                    when: vec![Cond::AmountEqual],
                    base: 4.5,
                    slope: 0.02,
                    amount_tol: None,
                },
            ],
        }
    }
}

/// Amount-conservation guard for the allocation-native report. The report is a
/// lot hypergraph, so a row may be split across many groups — or, when its
/// amount is zero, appear in no allocation at all. Row *presence* is therefore
/// the wrong invariant; what must hold is that every input id's allocations sum
/// to its original amount. `originals` is the authoritative input set (id ->
/// original amount); `allocated` is the per-id sum over every group allocation.
fn conservation_airlock(
    originals: &BTreeMap<ExtId, i64>,
    allocated: &BTreeMap<ExtId, i64>,
) -> Result<(), ApiError> {
    for (&id, &original) in originals {
        let accounted = allocated.get(&id).copied().unwrap_or(0);
        if accounted != original {
            return Err(ApiError::ConservationViolated {
                id,
                original,
                accounted,
            });
        }
    }
    // No allocation may reference an id absent from the input set.
    for (&id, &accounted) in allocated {
        if !originals.contains_key(&id) {
            return Err(ApiError::ConservationViolated {
                id,
                original: 0,
                accounted,
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Workspace — the interactive, stateful surface
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct StoredAlloc {
    id: ExtId,
    amount: i64,
    original: i64,
}

struct GroupRec {
    id: u64,
    allocations: Vec<StoredAlloc>,
    origin: String,
    net: i64,
    status: Status,
    reason: Option<String>,
}

impl GroupRec {
    fn is_frozen(&self) -> bool {
        self.status == Status::Frozen
    }

    fn contains(&self, id: ExtId) -> bool {
        self.allocations.iter().any(|a| a.id == id)
    }

    fn size(&self) -> usize {
        self.allocations.len()
    }
}

/// The interactive allocation-hypergraph result: groups plus signed allocation
/// incidences, each group carrying its [`Status`].
pub type WorkspaceReport = Report;

/// A long-lived, editable reconciliation workspace over items of type `E`,
/// driven by any [`Strategy`]. This is the one stateful facade; [`Workspace`]
/// is its `Row` + [`Plan`] specialization and a typed Rust caller can drive
/// `Recon<MyTx>` directly with a strategy built from the combinators.
///
/// It supports the interactive loop a UI drives: [`solve`](Recon::solve)
/// recomputes the unfrozen allocation pool; [`freeze`](Recon::freeze) locks a
/// group an analyst trusts so re-solves leave its allocation edges alone;
/// [`breakup`](Recon::breakup) dissolves a group back to the pool. The report is
/// an allocation hypergraph; row-level grouping is an explicit projection.
pub struct Recon<E> {
    strategy: Box<dyn Strategy<E>>,
    primary: Box<dyn Fn(&E) -> i64>,
    items: BTreeMap<ExtId, E>,
    groups: Vec<GroupRec>,
    /// Monotonic group-id allocator. **Never reset, never reused** — this is what
    /// makes live-singleton id ephemerality *safe*: each solve dissolves the
    /// live pool and re-mints its groups with brand-new ids, so a stale id held
    /// by a host across a solve can never silently land on a *different* group.
    /// It either still names the same frozen group (frozen ids are stable) or
    /// fails loudly as [`ApiError::UnknownGroup`].
    next_id: u64,
}

impl<E: Clone> Recon<E> {
    /// Create an empty workspace driven by `strategy`.
    pub fn new(strategy: Box<dyn Strategy<E>>, primary: impl Fn(&E) -> i64 + 'static) -> Self {
        Recon {
            strategy,
            primary: Box::new(primary),
            items: BTreeMap::new(),
            groups: Vec::new(),
            next_id: 0,
        }
    }

    /// Swap the compiled strategy and primary-amount extractor in place, keeping
    /// the rows, the groups (frozen decisions included), and the monotonic id
    /// allocator. The next [`solve`](Self::solve) recomputes the live pool under
    /// the new strategy; frozen groups are preserved verbatim with stable ids.
    /// Backs [`Workspace::replan`], which lets a caller iterate on a plan without
    /// re-ingesting rows or re-applying frozen decisions. The freshly compiled
    /// strategy starts cold (no warm flow state) — correct, since a changed plan
    /// invalidates the old basis anyway.
    pub fn replace_strategy(
        &mut self,
        strategy: Box<dyn Strategy<E>>,
        primary: impl Fn(&E) -> i64 + 'static,
    ) {
        self.strategy = strategy;
        self.primary = Box::new(primary);
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Push a fresh live singleton group (origin `"unmatched"`) for `id`. Live
    /// singleton ids are ephemeral: each solve dissolves and re-mints them.
    fn push_live_singleton(&mut self, id: ExtId) {
        let Some(item) = self.items.get(&id) else {
            return;
        };
        let amount = (self.primary)(item);
        self.groups.push(GroupRec {
            id: self.next_id,
            allocations: vec![StoredAlloc {
                id,
                amount,
                original: amount,
            }],
            origin: "unmatched".to_string(),
            net: amount,
            status: Status::Live,
            reason: None,
        });
        self.next_id += 1;
    }

    fn singleton_from_item(&mut self, item: Item<E>) {
        self.push_stored_singleton(StoredAlloc {
            id: item.id,
            amount: item.amount,
            original: item.original,
        });
    }

    fn push_stored_singleton(&mut self, alloc: StoredAlloc) {
        self.groups.push(GroupRec {
            id: self.next_id,
            net: alloc.amount,
            allocations: vec![alloc],
            origin: "unmatched".to_string(),
            status: Status::Live,
            reason: None,
        });
        self.next_id += 1;
    }

    /// Insert or replace an item. A new id starts life as a live singleton
    /// group; the caller re-solves to fold it into matches.
    pub fn upsert(&mut self, id: ExtId, item: E) {
        // A new id (insert returned None) cannot already be in a group: every
        // grouped id is, by invariant, present in `items` (live singletons
        // early-return on `items.get`; frozen/match groups are built from items
        // and `remove` prunes them). So the old `&& !self.in_group(id)` guard
        // was always true here — and, since it scans every group, it made a
        // bulk init O(n^2) (each of n upserts scanning the growing singleton
        // pool). Dropping it keeps init O(n log n) with identical semantics.
        if self.items.insert(id, item).is_none() {
            self.push_live_singleton(id);
        }
    }

    /// Remove an item from the workspace and from its group. A match that loses
    /// a member dissolves; its survivor returns to a fresh live singleton.
    pub fn remove(&mut self, id: ExtId) {
        self.remove_many(&[id]);
    }

    /// Remove many items in a single pass over the groups. Removing ids one at a
    /// time is O(groups) per id (each `remove` scans every group), so a bulk
    /// delete of m ids over n groups is O(n*m) — quadratic when m ~ n. This does
    /// one `retain_mut` over the groups regardless of how many ids are dropped,
    /// with identical end-state semantics to looping `remove`.
    pub fn remove_many(&mut self, ids: &[ExtId]) {
        if ids.is_empty() {
            return;
        }
        let victims: BTreeSet<ExtId> = ids.iter().copied().collect();
        for id in &victims {
            self.items.remove(id);
        }
        let mut orphaned = Vec::new();
        self.groups.retain_mut(|g| {
            // Untouched groups pass through without rebuilding net.
            if !g.allocations.iter().any(|a| victims.contains(&a.id)) {
                return true;
            }
            g.allocations.retain(|a| !victims.contains(&a.id));
            g.net = g.allocations.iter().map(|a| a.amount).sum();
            if g.allocations.is_empty() {
                false
            } else if g.size() == 1 {
                // A match reduced to one allocation can no longer net; its
                // survivor returns to the live pool as a fresh singleton.
                orphaned.extend(g.allocations.iter().cloned());
                false
            } else {
                true
            }
        });
        for o in orphaned {
            self.push_stored_singleton(o);
        }
    }

    #[allow(dead_code)]
    fn in_group(&self, id: ExtId) -> bool {
        self.groups.iter().any(|g| g.contains(id))
    }

    /// Recompute the live pool: dissolve every live group (singletons included)
    /// into a flat pool, run the strategy, and install fresh live groups plus a
    /// live singleton for each leftover. Frozen groups are kept verbatim with
    /// stable ids.
    pub fn solve(&mut self) -> Result<(), ApiError> {
        let mut frozen: BTreeMap<ExtId, i64> = BTreeMap::new();
        for g in self.groups.iter().filter(|g| g.is_frozen()) {
            for a in &g.allocations {
                *frozen.entry(a.id).or_insert(0) += a.amount;
            }
        }
        let bag: Vec<Item<E>> = self
            .items
            .iter()
            .filter_map(|(id, item)| {
                let original = (self.primary)(item);
                let rem = original - frozen.get(id).copied().unwrap_or(0);
                (rem != 0).then(|| Item {
                    id: *id,
                    original,
                    amount: rem,
                    data: item.clone(),
                })
            })
            .collect();
        let meta: BTreeMap<ExtId, i64> = bag.iter().map(|i| (i.id, i.original)).collect();
        let res = self.strategy.run(bag);

        // Dissolve all live groups; keep frozen allocation groups verbatim.
        self.groups.retain(|g| g.is_frozen());
        let mut new_groups = res.groups;
        new_groups.sort_by_key(|g| g.members.iter().map(|a| a.id).min().unwrap_or(0));
        for g in new_groups {
            self.groups.push(GroupRec {
                id: self.next_id,
                allocations: g
                    .members
                    .into_iter()
                    .map(|a| StoredAlloc {
                        id: a.id,
                        amount: a.amount,
                        original: meta.get(&a.id).copied().unwrap_or(0),
                    })
                    .collect(),
                origin: g.origin,
                net: g.net,
                status: Status::Live,
                reason: g.reason,
            });
            self.next_id += 1;
        }
        // Every residual lot becomes its own live allocation group. Do not drop
        // a residual merely because the same row id was partly allocated above:
        // the report is a hypergraph, not a row partition.
        for item in res.residual {
            self.singleton_from_item(item);
        }
        let allocated: BTreeMap<ExtId, i64> = self
            .groups
            .iter()
            .flat_map(|g| g.allocations.iter())
            .fold(BTreeMap::new(), |mut m, a| {
                *m.entry(a.id).or_insert(0) += a.amount;
                m
            });
        let originals: BTreeMap<ExtId, i64> = self
            .items
            .iter()
            .map(|(id, item)| (*id, (self.primary)(item)))
            .collect();
        conservation_airlock(&originals, &allocated)?;
        Ok(())
    }

    /// Lock a group so future solves leave it intact. Valid on singletons too:
    /// freezing a live singleton records an accepted unmatched exception.
    pub fn freeze(&mut self, group_id: u64) -> Result<(), ApiError> {
        self.group_mut(group_id)?.status = Status::Frozen;
        Ok(())
    }

    /// Freeze every live *match* (size >= 2) whose net is within `tol` (a clean
    /// group). Returns how many were newly frozen. Live singletons (unmatched
    /// rows) are never "clean" and are left alone; use [`freeze`](Recon::freeze)
    /// or [`freeze_singletons`](Recon::freeze_singletons) to accept those.
    pub fn freeze_clean(&mut self, tol: i64) -> usize {
        let mut n = 0;
        for g in &mut self.groups {
            if !g.is_frozen() && g.size() >= 2 && g.net.abs() <= tol {
                g.status = Status::Frozen;
                n += 1;
            }
        }
        n
    }

    /// Freeze the live singleton groups holding any of `ids` (accepted unmatched
    /// exceptions) in one crossing — the FE "Freeze N unmatched" path. Ids that
    /// are not currently live singletons are ignored.
    pub fn freeze_singletons(&mut self, ids: &[ExtId]) {
        let want: BTreeSet<ExtId> = ids.iter().copied().collect();
        for g in &mut self.groups {
            if !g.is_frozen()
                && g.size() == 1
                && g.allocations
                    .first()
                    .map(|a| want.contains(&a.id))
                    .unwrap_or(false)
            {
                g.status = Status::Frozen;
            }
        }
    }

    /// Unlock a frozen group; the next solve may reshape it.
    pub fn unfreeze(&mut self, group_id: u64) -> Result<(), ApiError> {
        self.group_mut(group_id)?.status = Status::Live;
        Ok(())
    }

    /// Dissolve a group (live or frozen); each allocation edge returns to the
    /// pool as a fresh live singleton until the next explicit solve.
    pub fn breakup(&mut self, group_id: u64) -> Result<(), ApiError> {
        let pos = self
            .groups
            .iter()
            .position(|g| g.id == group_id)
            .ok_or(ApiError::UnknownGroup(group_id))?;
        let g = self.groups.remove(pos);
        for a in g.allocations {
            self.push_stored_singleton(a);
        }
        Ok(())
    }

    /// Manually assert a group over `ids` with a caller-supplied `net` and
    /// `origin`. Convenience wrapper: pulls all currently live allocation mass
    /// for those row ids into one frozen manual group. Allocation-native clients
    /// should prefer [`group_allocations`](Recon::group_allocations) when they
    /// want to target exact residual amounts.
    pub fn group(&mut self, ids: &[ExtId], net: i64, origin: &str, reason: Option<String>) -> Result<u64, ApiError> {
        let mut members: Vec<ExtId> = Vec::new();
        for &id in ids {
            if !members.contains(&id) {
                members.push(id);
            }
        }
        if members.len() < 2 {
            return Err(ApiError::DegenerateGroup);
        }
        for &id in &members {
            if !self.items.contains_key(&id) {
                return Err(ApiError::UnknownId(id));
            }
            if self.groups.iter().any(|g| g.is_frozen() && g.contains(id)) {
                return Err(ApiError::FrozenMember(id));
            }
        }
        // Pull the chosen ids out of any live group, preserving their current
        // allocation amounts. Manual groups are frozen allocation hyperedges;
        // the row-id API is a convenience wrapper over currently available live
        // allocations for those ids.
        let claim: BTreeSet<ExtId> = members.iter().copied().collect();
        let mut allocations = self.pull_from_live(&claim);
        let pulled: BTreeSet<ExtId> = allocations.iter().map(|a| a.id).collect();
        for id in members {
            if !pulled.contains(&id) {
                let original = (self.primary)(&self.items[&id]);
                allocations.push(StoredAlloc {
                    id,
                    amount: original,
                    original,
                });
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        let alloc_net: i64 = allocations.iter().map(|a| a.amount).sum();
        self.groups.push(GroupRec {
            id,
            allocations,
            origin: origin.to_string(),
            // Preserve the legacy caller-supplied net only when no allocation
            // amounts are known yet (e.g. manual grouping before first solve).
            net: if alloc_net == 0 { net } else { alloc_net },
            status: Status::Frozen,
            reason,
        });
        Ok(id)
    }

    /// Manually assert a frozen group over exact allocation amounts. This is
    /// the allocation-native override: requested amounts are taken from the live
    /// unfrozen pool, splitting existing allocations if needed. Frozen groups
    /// are never disturbed.
    pub fn group_allocations(
        &mut self,
        specs: &[AllocationSpec],
        origin: &str,
        reason: Option<String>,
    ) -> Result<u64, ApiError> {
        let mut want: BTreeMap<ExtId, i64> = BTreeMap::new();
        for s in specs {
            if s.amount != 0 {
                *want.entry(s.id).or_insert(0) += s.amount;
            }
        }
        if want.len() < 2 {
            return Err(ApiError::DegenerateGroup);
        }
        for (&id, &amount) in &want {
            if !self.items.contains_key(&id) {
                return Err(ApiError::UnknownId(id));
            }
            let available = self.live_available(id, amount.signum());
            if available.abs() < amount.abs() {
                return Err(ApiError::InsufficientLiveAmount {
                    id,
                    requested: amount,
                    available,
                });
            }
        }
        let mut allocations = Vec::new();
        for (id, amount) in want {
            allocations.extend(self.take_live_amount(id, amount)?);
        }
        let net: i64 = allocations.iter().map(|a| a.amount).sum();
        let id = self.next_id;
        self.next_id += 1;
        self.groups.push(GroupRec {
            id,
            allocations,
            origin: origin.to_string(),
            net,
            status: Status::Frozen,
            reason,
        });
        Ok(id)
    }

    /// Remove specific row allocations from one live group and return those
    /// allocation edges to live singleton groups. This is the precise
    /// allocation-aware counterpart to broad row-id `ungroup`.
    pub fn remove_allocations(&mut self, group_id: u64, ids: &[ExtId]) -> Result<(), ApiError> {
        let want: BTreeSet<ExtId> = ids.iter().copied().collect();
        let pos = self
            .groups
            .iter()
            .position(|g| g.id == group_id)
            .ok_or(ApiError::UnknownGroup(group_id))?;
        if self.groups[pos].is_frozen() {
            let id = want.iter().next().copied().unwrap_or(0);
            return Err(ApiError::FrozenMember(id));
        }
        for &id in &want {
            if !self.groups[pos].contains(id) {
                return Err(ApiError::UnknownAllocation { group_id, id });
            }
        }
        let mut g = self.groups.remove(pos);
        let mut removed = Vec::new();
        let mut keep = Vec::new();
        for a in g.allocations {
            if want.contains(&a.id) {
                removed.push(a);
            } else {
                keep.push(a);
            }
        }
        g.allocations = keep;
        g.net = g.allocations.iter().map(|a| a.amount).sum();
        if g.allocations.len() >= 2 {
            self.groups.push(g);
        } else if g.allocations.len() == 1 {
            self.push_stored_singleton(g.allocations.remove(0));
        }
        for a in removed {
            self.push_stored_singleton(a);
        }
        Ok(())
    }

    fn live_available(&self, id: ExtId, sign: i64) -> i64 {
        self.groups
            .iter()
            .filter(|g| !g.is_frozen())
            .flat_map(|g| &g.allocations)
            .filter(|a| a.id == id && a.amount.signum() == sign)
            .map(|a| a.amount)
            .sum()
    }

    fn take_live_amount(&mut self, id: ExtId, amount: i64) -> Result<Vec<StoredAlloc>, ApiError> {
        let sign = amount.signum();
        let mut remaining = amount.abs();
        let mut pulled = Vec::new();
        for g in &mut self.groups {
            if g.is_frozen() {
                continue;
            }
            let mut keep = Vec::new();
            for mut a in g.allocations.drain(..) {
                if a.id == id && a.amount.signum() == sign && remaining > 0 {
                    let take = remaining.min(a.amount.abs());
                    remaining -= take;
                    let pulled_amount = sign * take;
                    pulled.push(StoredAlloc {
                        id: a.id,
                        amount: pulled_amount,
                        original: a.original,
                    });
                    a.amount -= pulled_amount;
                    if a.amount != 0 {
                        keep.push(a);
                    }
                } else {
                    keep.push(a);
                }
            }
            g.allocations = keep;
            g.net = g.allocations.iter().map(|a| a.amount).sum();
            if remaining == 0 {
                break;
            }
        }
        if remaining != 0 {
            let requested = amount;
            let taken: i64 = pulled.iter().map(|a| a.amount).sum();
            return Err(ApiError::InsufficientLiveAmount {
                id,
                requested,
                available: taken,
            });
        }
        self.cleanup_live_groups();
        Ok(pulled)
    }

    fn cleanup_live_groups(&mut self) {
        let mut orphaned = Vec::new();
        self.groups.retain_mut(|g| {
            if g.is_frozen() {
                return true;
            }
            g.net = g.allocations.iter().map(|a| a.amount).sum();
            if g.allocations.is_empty() {
                false
            } else if g.size() == 1 && g.origin != "unmatched" {
                orphaned.extend(g.allocations.iter().cloned());
                false
            } else {
                true
            }
        });
        for o in orphaned {
            self.push_stored_singleton(o);
        }
    }

    /// Remove `claim` from every live group, dropping emptied groups and
    /// re-minting any survivor of a now-singleton live group. Frozen groups are
    /// untouched (callers guard against frozen members first). Returns the
    /// allocations that belonged to `claim` so callers can re-materialize them
    /// without losing lot amounts.
    fn pull_from_live(&mut self, claim: &BTreeSet<ExtId>) -> Vec<StoredAlloc> {
        let mut pulled = Vec::new();
        for g in &mut self.groups {
            if !g.is_frozen() {
                let mut keep = Vec::new();
                for a in g.allocations.drain(..) {
                    if claim.contains(&a.id) {
                        pulled.push(a);
                    } else {
                        keep.push(a);
                    }
                }
                g.allocations = keep;
            }
        }
        let mut orphaned = Vec::new();
        self.groups.retain_mut(|g| {
            if g.is_frozen() {
                return true;
            }
            g.net = g.allocations.iter().map(|a| a.amount).sum();
            if g.allocations.is_empty() {
                false
            } else if g.size() == 1 {
                orphaned.extend(g.allocations.iter().cloned());
                false
            } else {
                true
            }
        });
        for o in orphaned {
            self.push_stored_singleton(o);
        }
        pulled
    }

    /// Send `ids` back to live singletons, removing them from their live group.
    /// Rows in a frozen group are refused (unfreeze or break it up first). A
    /// live group that falls below two members dissolves. Idempotent for ids
    /// already standing as live singletons.
    pub fn ungroup(&mut self, ids: &[ExtId]) -> Result<(), ApiError> {
        for &id in ids {
            if !self.items.contains_key(&id) {
                return Err(ApiError::UnknownId(id));
            }
            if self.groups.iter().any(|g| g.is_frozen() && g.contains(id)) {
                return Err(ApiError::FrozenMember(id));
            }
        }
        let claim: BTreeSet<ExtId> = ids.iter().copied().collect();
        let pulled = self.pull_from_live(&claim);
        // Each claimed allocation stands alone as a fresh live singleton,
        // preserving split allocation amounts. If an id had no live allocation
        // yet, initialize one from the plan primary amount.
        let pulled_ids: BTreeSet<ExtId> = pulled.iter().map(|a| a.id).collect();
        for a in pulled {
            self.push_stored_singleton(a);
        }
        for id in claim.difference(&pulled_ids) {
            self.push_live_singleton(*id);
        }
        Ok(())
    }

    fn group_mut(&mut self, group_id: u64) -> Result<&mut GroupRec, ApiError> {
        self.groups
            .iter_mut()
            .find(|g| g.id == group_id)
            .ok_or(ApiError::UnknownGroup(group_id))
    }

    /// Snapshot the current allocation hypergraph.
    pub fn report(&self) -> WorkspaceReport {
        let mut allocations = Vec::new();
        let mut groups = Vec::with_capacity(self.groups.len());
        for g in &self.groups {
            for a in &g.allocations {
                allocations.push(AllocationOut {
                    id: a.id,
                    group_id: g.id,
                    amount: a.amount,
                });
            }
            groups.push(GroupOut {
                group_id: g.id,
                origin: g.origin.clone(),
                net: g.net,
                size: g.size(),
                status: g.status,
                reason: g.reason.clone(),
            });
        }
        allocations.sort_by_key(|a| (a.id, a.group_id));
        groups.sort_by_key(|g| g.group_id);
        WorkspaceReport {
            groups,
            allocations,
        }
    }
}

/// The interactive [`Plan`]-driven workspace over [`PhysicalRow`]s: a [`Recon<PhysicalRow>`]
/// plus its [`Schema`] (for arity validation). This is what the WASM `dispatch`
/// surface drives.
pub struct Workspace {
    map: ColumnMap,
    inner: Recon<PhysicalRow>,
}

impl Workspace {
    /// Compile `plan` against `schema` and create an empty workspace. Fails if
    /// the plan references an unknown column.
    pub fn new(map: ColumnMap, plan: Plan) -> Result<Self, ApiError> {
        // The interactive workspace persists across solves (Recon stores the
        // strategy once), so compile the warm, shard-keyed flow leaf.
        let compiled = compile(&plan, &map)?;
        let primary = compiled.primary;
        Ok(Workspace {
            map,
            inner: Recon::new(compiled.strategy, move |r: &PhysicalRow| r.int(primary)),
        })
    }

    pub fn map(&self) -> &ColumnMap {
        &self.map
    }

    /// Recompile `plan` against the existing schema and swap it in, preserving
    /// rows, frozen decisions, and the id allocator. Live groups are recomputed
    /// on the next [`solve`](Recon::solve). Fails — leaving the workspace
    /// unchanged — if the plan references an unknown column (the plan is compiled
    /// before anything is mutated). The schema is unchanged; typically the
    /// primary column is too, so existing frozen amounts stay meaningful.
    pub fn replan(&mut self, plan: Plan) -> Result<(), ApiError> {
        let compiled = compile(&plan, &self.map)?;
        let primary = compiled.primary;
        self.inner
            .replace_strategy(compiled.strategy, move |r: &PhysicalRow| r.int(primary));
        Ok(())
    }
}

impl std::ops::Deref for Workspace {
    type Target = Recon<PhysicalRow>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for Workspace {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn map() -> ColumnMap {
        let mut int_cols = HashMap::new();
        int_cols.insert("usd".into(), 0);
        int_cols.insert("day".into(), 1);
        int_cols.insert("class".into(), 2);
        int_cols.insert("objsub".into(), 3);
        int_cols.insert("native".into(), 4);
        let mut token_cols = HashMap::new();
        token_cols.insert("tokens".into(), 0);
        ColumnMap { int_cols, token_cols }
    }

    fn row(usd: i64, day: i64, objsub: i64, native: i64, tokens: &[u64]) -> PhysicalRow {
        PhysicalRow {
            ints: vec![usd, day, 0, objsub, native],
            tokens: vec![tokens.to_vec()],
        }
    }

    fn plan(root: PlanNode) -> Plan { Plan { primary: "usd".into(), root } }

    fn full_pipeline() -> Plan {
        plan(PlanNode::Seq { steps: vec![
            PlanNode::AggNet { key: "objsub".into(), tol: Tol::Abs(0) },
            PlanNode::Exact {},
            PlanNode::Signal { signals: "tokens".into(), tol: Tol::Abs(0), cap: 256 },
            PlanNode::Flow { order_by: "day".into(), tokens: "tokens".into(), penalty: 1000.0, window: 30, cost: CostSpec::default() },
        ]})
    }

    #[test]
    fn replan_preserves_rows_and_frozen_then_applies_new_plan() {
        // Start under `exact`: it pairs (1,2) cleanly but cannot touch the
        // three-leg objsub bucket (3,4,5), which nets to zero only in aggregate.
        let mut ws = Workspace::new(map(), plan(PlanNode::Exact {})).unwrap();
        ws.upsert(1, row(100, 1, 0, 0, &[]));
        ws.upsert(2, row(-100, 2, 0, 0, &[]));
        ws.upsert(3, row(50, 1, 700, 0, &[]));
        ws.upsert(4, row(30, 1, 700, 0, &[]));
        ws.upsert(5, row(-80, 1, 700, 0, &[]));
        ws.solve().unwrap();
        ws.freeze_clean(0); // sign off the clean (1,2) pair

        // Retune the plan in place: net by the objsub bucket instead. Rows and
        // the frozen decision must survive; the new rule must now apply.
        ws.replan(plan(PlanNode::AggNet { key: "objsub".into(), tol: Tol::Abs(0) }))
            .unwrap();
        ws.solve().unwrap();
        let rep = ws.report();

        // The frozen (1,2) pair is preserved verbatim.
        let frozen: Vec<_> = rep
            .groups
            .iter()
            .filter(|g| g.status == Status::Frozen)
            .collect();
        assert_eq!(frozen.len(), 1);
        assert_eq!(frozen[0].size, 2);
        let frozen_rows: BTreeSet<ExtId> = rep
            .allocations
            .iter()
            .filter(|a| a.group_id == frozen[0].group_id)
            .map(|a| a.id)
            .collect();
        assert_eq!(frozen_rows, BTreeSet::from([1, 2]));

        // The three-leg objsub bucket now nets into one live group under the new plan.
        let netted: Vec<_> = rep
            .groups
            .iter()
            .filter(|g| g.status == Status::Live && g.origin == "agg_net")
            .collect();
        assert_eq!(netted.len(), 1);
        assert_eq!(netted[0].size, 3);
        assert_eq!(netted[0].net, 0);

        // Conservation: every one of the five rows lands in exactly one group.
        let rows: BTreeSet<ExtId> = rep.allocations.iter().map(|a| a.id).collect();
        assert_eq!(rows, BTreeSet::from([1, 2, 3, 4, 5]));
    }

    /// A replan that references an unknown column is rejected, leaving the
    /// workspace unchanged and still solvable under the original plan.
    #[test]
    fn replan_with_unknown_column_fails_and_preserves_workspace() {
        let mut ws = Workspace::new(map(), plan(PlanNode::Exact {})).unwrap();
        ws.upsert(1, row(100, 1, 0, 0, &[]));
        ws.upsert(2, row(-100, 2, 0, 0, &[]));
        assert!(
            ws.replan(plan(PlanNode::AggNet { key: "nope".into(), tol: Tol::Abs(0) }))
                .is_err()
        );
        ws.solve().unwrap();
        let rep = ws.report();
        assert!(rep.groups.iter().any(|g| g.size == 2 && g.net == 0));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn filter_node_serde_and_group_metric_predicate() {
        // `keep` is a Sel over group metrics: keep only pairs whose smaller side
        // exceeds 1 lot AND whose size is <= 3. A clean 1:1 pair has min_side==1,
        // so it is rejected back to residual; a 2-vs-2 net survives.
        let node: PlanNode = serde_json::from_str(
            r#"{"op":"filter",
                "keep":{"and":[{"gt":["min_side",1]},{"le":["size",3]}]},
                "inner":{"op":"exact"}}"#,
        )
        .unwrap();
        match &node {
            PlanNode::Filter { keep, inner } => {
                assert!(matches!(**inner, PlanNode::Exact {}));
                // round-trips
                let back: PlanNode =
                    serde_json::from_str(&serde_json::to_string(&node).unwrap()).unwrap();
                assert_eq!(back, node);
                let _ = keep;
            }
            _ => panic!("expected filter"),
        }

        // exact_1to1 forms a single clean pair (min_side == 1) -> rejected.
        let mut ws = Workspace::new(map(), plan(node)).unwrap();
        ws.upsert(1, row(100, 1, 0, 999, &[]));
        ws.upsert(2, row(-100, 2, 0, -999, &[]));
        ws.solve().unwrap();
        let rep = ws.report();
        // Rejected back to residual: two unmatched singleton groups, no pair.
        assert!(rep.groups.iter().all(|g| g.origin == "unmatched"));
        assert_eq!(rep.groups.len(), 2);
        // Conservation still holds (the solve airlock would have errored otherwise).
        assert_eq!(rep.allocations.len(), 2);
    }

    #[test]
    fn filter_size_cap_dissolves_oversized_group_in_plan() {
        // agg_net nets a 5-lot bucket; a size cap of 4 rejects it back to the
        // residual, where soak_all then classifies the leftovers.
        let plan = plan(PlanNode::Seq {
            steps: vec![
                PlanNode::Filter {
                    keep: Sel::Le(Box::new("size".into()), Box::new(4i64.into())),
                    inner: Box::new(PlanNode::AggNet {
                        key: "objsub".into(),
                        tol: Tol::Abs(0),
                    }),
                },
                PlanNode::SoakAll {
                    origin: "leftover".into(),
                    by: None,
                },
            ],
        });
        let mut ws = Workspace::new(map(), plan).unwrap();
        // Five rows sharing objsub=7 that net to zero (40, -10, -10, -10, -10).
        ws.upsert(1, row(40, 1, 7, 0, &[]));
        ws.upsert(2, row(-10, 1, 7, 0, &[]));
        ws.upsert(3, row(-10, 1, 7, 0, &[]));
        ws.upsert(4, row(-10, 1, 7, 0, &[]));
        ws.upsert(5, row(-10, 1, 7, 0, &[]));
        ws.solve().unwrap();
        let rep = ws.report();
        // The oversized net is rejected; every row lands in a `leftover` group.
        assert!(rep.groups.iter().all(|g| g.origin == "leftover"));
        assert_eq!(rep.allocations.len(), 5);
    }

    #[test]
    fn coalesce_node_clusters_flow_allocations_in_plan() {
        // A 1-to-many settlement: row 1 (+100) clears rows 2,3,4,5 (-25 each) by
        // shared token. The flow leaf may emit these as interlocking partial
        // allocations; `coalesce` collapses them into one settlement cluster.
        let plan = plan(PlanNode::Coalesce {
            origin: "settlement".into(),
            inner: Box::new(PlanNode::Flow {
                order_by: "day".into(),
                tokens: "tokens".into(),
                penalty: 1000.0,
                window: -1,
                cost: CostSpec::default(),
            }),
        });
        let mut ws = Workspace::new(map(), plan).unwrap();
        ws.upsert(1, row(100, 1, 0, 0, &[42]));
        ws.upsert(2, row(-25, 1, 0, 0, &[42]));
        ws.upsert(3, row(-25, 1, 0, 0, &[42]));
        ws.upsert(4, row(-25, 1, 0, 0, &[42]));
        ws.upsert(5, row(-25, 1, 0, 0, &[42]));
        ws.solve().unwrap();
        let rep = ws.report();
        // Whatever the matcher's internal edge split, the matched rows end up in
        // a single coalesced cluster (plus any unmatched singletons).
        let clusters: Vec<&GroupOut> =
            rep.groups.iter().filter(|g| g.origin == "settlement").collect();
        assert_eq!(clusters.len(), 1, "one settlement cluster");
        assert_eq!(clusters[0].size, 5, "all five rows in the cluster");
        assert_eq!(clusters[0].net, 0);
        // Conservation airlock passed (solve would have errored otherwise).
        assert_eq!(rep.allocations.iter().filter(|a| a.amount != 0).count(), 5);
    }

    #[test]
    fn exact_pair_matches() {
        let mut ws = Workspace::new(map(), full_pipeline()).unwrap();
        ws.upsert(1, row(100, 1, 0, 999, &[]));
        ws.upsert(2, row(-100, 2, 0, -999, &[]));
        ws.solve().unwrap();
        let rep = ws.report();
        assert_eq!(rep.allocations.len(), 2);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn fixed_point_serde_shape_and_default_max() {
        // `max` omitted on the wire takes the default; the node round-trips.
        let node: PlanNode =
            serde_json::from_str(r#"{"op":"fixed_point","inner":{"op":"exact"}}"#).unwrap();
        match &node {
            PlanNode::FixedPoint { inner, max } => {
                assert_eq!(*max, default_fixed_point_passes());
                assert!(matches!(**inner, PlanNode::Exact {}));
            }
            _ => panic!("expected fixed_point"),
        }
        let explicit: PlanNode = serde_json::from_str(
            r#"{"op":"fixed_point","max":3,"inner":{"op":"exact"}}"#,
        )
        .unwrap();
        assert_eq!(
            serde_json::from_str::<PlanNode>(&serde_json::to_string(&explicit).unwrap()).unwrap(),
            explicit,
        );
    }

    #[test]
    fn zero_amount_row_conserves() {
        // A zero-amount row legitimately yields no allocation in the lot
        // hypergraph. Conservation is by amount, not row presence, so this must
        // NOT trip the airlock. (Regression: real files carry blank/$0 rows.)
        let mut ws = Workspace::new(map(), full_pipeline()).unwrap();
        ws.upsert(1, row(100, 1, 0, 999, &[]));
        ws.upsert(2, row(-100, 2, 0, -999, &[]));
        ws.upsert(3, row(0, 3, 0, 0, &[])); // blank amount -> 0
        ws.solve().unwrap();
        let rep = ws.report();
        // The pair nets; the zero row simply has no lot. Amounts conserve.
        assert!(rep.allocations.iter().all(|a| a.id != 3) || rep.allocations.iter().any(|a| a.id == 3 && a.amount == 0));
        assert!(rep.allocations.iter().any(|a| a.id == 1));
        assert!(rep.allocations.iter().any(|a| a.id == 2));
    }

    #[test]
    fn workspace_zero_amount_row_conserves() {
        // Same invariant through the interactive Workspace/Recon solve path.
        let mut ws = Workspace::new(map(), full_pipeline()).unwrap();
        ws.upsert(1, row(100, 1, 0, 999, &[]));
        ws.upsert(2, row(-100, 2, 0, -999, &[]));
        ws.upsert(3, row(0, 3, 0, 0, &[]));
        ws.solve().unwrap(); // must not return ConservationViolated
    }
}
