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
//!   specialization and [`Session`] is the stateless one-shot form.
//! - [`Report`] — the allocation hypergraph result (`groups` + `allocations`).
//!
//! Conservation is enforced at the boundary: a solve verifies that every input
//! id is represented by at least one allocation edge, so a malformed plan can
//! never silently lose rows. Amount conservation lives in the strategy/flow
//! algebra and is exposed directly in report allocations.

use crate::flow::ExtId;
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
pub const CONTRACT_VERSION: u32 = 10;
pub use crate::error::ApiError;
pub use crate::report::{AllocationOut, Component, GroupOut, ProjectionError, Report, Status};
pub use crate::row::{PhysicalRow, ColumnMap};

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
    /// Fork/join shard by a scalar column/expression, run `inner` per shard.
    Partition { by: String, inner: Box<PlanNode> },
    /// Route rows by a boolean column/expression, run different child subtrees
    /// on each side, then join. This is a structural split; both sides conserve.
    Branch {
        pred: String,
        and_then: Box<PlanNode>,
        or_else: Box<PlanNode>,
    },
    /// Run `inner` within a sliding window over an integer order expression.
    Windowed {
        order: String,
        width: i64,
        inner: Box<PlanNode>,
    },
    /// Temporarily match `inner` in another numeraire, converting produced
    /// allocations and residuals back to the caller's active amount on exit.
    Pivot {
        amount: String,
        inner: Box<PlanNode>,
    },
    /// Accept an aggregation bucket (`key`) that nets to zero in `tol`.
    AggNet { key: String, tol: i64 },
    /// Pair opposite-sign rows with equal current amount magnitude.
    Exact {},
    /// Group rows that share an out-of-band token signal and net to zero.
    Signal {
        signals: String,
        tol: i64,
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
        /// Date/order expression (days) for proximity candidate generation.
        day: String,
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
/// conditions all hold and whose `|Δday|` is within `max_day`; its cost is
/// `base + day_slope * |Δday|`. A pair matched by no tier is forbidden.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CostTier {
    pub when: Vec<Cond>,
    pub base: f64,
    #[cfg_attr(feature = "serde", serde(default))]
    pub day_slope: f64,
    #[cfg_attr(feature = "serde", serde(default))]
    pub max_day: Option<i64>,
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
    /// still if the amount also matches) outranks an exact native amount, which
    /// is only trusted within a 92-day window.
    fn default() -> Self {
        CostSpec {
            tiers: vec![
                CostTier {
                    when: vec![Cond::TokenShared, Cond::AmountEqual],
                    base: 1.5,
                    day_slope: 0.002,
                    max_day: None,
                },
                CostTier {
                    when: vec![Cond::TokenShared],
                    base: 2.0,
                    day_slope: 0.002,
                    max_day: None,
                },
                CostTier {
                    when: vec![Cond::AmountEqual],
                    base: 4.5,
                    day_slope: 0.02,
                    max_day: Some(92),
                },
            ],
        }
    }
}

fn conservation_airlock(input: usize, accounted: usize) -> Result<(), ApiError> {
    if accounted != input {
        return Err(ApiError::ConservationViolated { input, accounted });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// A long-lived reconciliation handle. Owns the rows natively; hosts cross the
/// boundary only with coarse deltas and plan submissions.
#[derive(Default)]
pub struct Session {
    map: ColumnMap,
    rows: BTreeMap<ExtId, PhysicalRow>,
}

impl Session {
    pub fn new(map: ColumnMap) -> Self {
        Session {
            map,
            rows: BTreeMap::new(),
        }
    }

    /// Build a session from a schema and a batch of business rows (the batch
    /// boundary mode: the whole shard crosses once, e.g. from a WASM host).
    /// Rows are lowered against the schema.
    pub fn from_rows<I>(map: ColumnMap, rows: I) -> Result<Self, ApiError>
    where
        I: IntoIterator<Item = (ExtId, PhysicalRow)>,
    {
        let mut s = Session::new(map);
        for (id, row) in rows {
            s.upsert(id, row)?;
        }
        Ok(s)
    }

    pub fn map(&self) -> &ColumnMap {
        &self.map
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Insert or replace a row. Takes a business [`Row`] (bare cells) and lowers
    /// it against the schema's per-column [`Kind`]s; one boundary crossing per
    /// edit. Lowering arity-checks against the schema.
    pub fn upsert(&mut self, id: ExtId, row: PhysicalRow) -> Result<(), ApiError> {
        self.rows.insert(id, row);
        Ok(())
    }

    /// Remove a row if present.
    pub fn remove(&mut self, id: ExtId) {
        self.rows.remove(&id);
    }

    fn run_strategy(
        &self,
        plan: &Plan,
    ) -> Result<(usize, crate::strategy::Resolution<PhysicalRow>), ApiError> {
        // Session is a stateless one-shot: compile the cold flow leaf.
        let compiled = compile(plan, &self.map)?;
        let mut strategy = compiled.strategy;
        // Materialize in id order for deterministic candidate generation, with
        // the plan's primary amount already stamped on every item.
        let bag: Vec<Item<PhysicalRow>> = self
            .rows
            .iter()
            .map(|(id, row)| Item::new(*id, row.int(compiled.primary), row.clone()))
            .collect();
        let input = bag.len();
        Ok((input, strategy.run(bag)))
    }

    /// Run a plan over the current rows and return the allocation hypergraph.
    /// `allocations` is the single source of truth; row/group views are client
    /// projections over the report.
    pub fn solve(&self, plan: &Plan) -> Result<Report, ApiError> {
        let (input, res) = self.run_strategy(plan)?;
        report_from_resolution(input, res)
    }
}

fn report_from_resolution(
    input: usize,
    res: crate::strategy::Resolution<PhysicalRow>,
) -> Result<Report, ApiError> {
    let mut groups = res.groups;
    groups.sort_by_key(|g| g.members.iter().map(|a| a.id).min().unwrap_or(0));

    let mut allocations = Vec::new();
    let mut group_out = Vec::with_capacity(groups.len() + res.residual.len());
    let mut next_gid = 0u64;
    for g in groups {
        let gid = next_gid;
        next_gid += 1;
        for m in &g.members {
            allocations.push(AllocationOut {
                id: m.id,
                group_id: gid,
                amount: m.amount,
            });
        }
        group_out.push(GroupOut {
            group_id: gid,
            origin: g.origin,
            net: g.net,
            size: g.members.len(),
            status: Status::Live,
        });
    }

    let mut residual = res.residual;
    residual.sort_by_key(|i| i.id);
    for i in residual {
        let gid = next_gid;
        next_gid += 1;
        allocations.push(AllocationOut {
            id: i.id,
            group_id: gid,
            amount: i.amount,
        });
        group_out.push(GroupOut {
            group_id: gid,
            origin: "unmatched".to_string(),
            net: i.amount,
            size: 1,
            status: Status::Live,
        });
    }
    allocations.sort_by_key(|a| (a.id, a.group_id));

    let accounted: BTreeSet<ExtId> = allocations.iter().map(|a| a.id).collect();
    conservation_airlock(input, accounted.len())?;

    Ok(Report {
        groups: group_out,
        allocations,
    })
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
        });
        self.next_id += 1;
    }

    /// Insert or replace an item. A new id starts life as a live singleton
    /// group; the caller re-solves to fold it into matches.
    pub fn upsert(&mut self, id: ExtId, item: E) {
        if self.items.insert(id, item).is_none() && !self.in_group(id) {
            self.push_live_singleton(id);
        }
    }

    /// Remove an item from the workspace and from its group. A match that loses
    /// a member dissolves; its survivor returns to a fresh live singleton.
    pub fn remove(&mut self, id: ExtId) {
        self.items.remove(&id);
        let mut orphaned = Vec::new();
        self.groups.retain_mut(|g| {
            if !g.contains(id) {
                return true;
            }
            g.allocations.retain(|a| a.id != id);
            g.net = g.allocations.iter().map(|a| a.amount).sum();
            if g.allocations.is_empty() {
                false
            } else if g.size() == 1 {
                // A match reduced to one allocation can no longer net; its survivor
                // returns to the live pool as a fresh singleton.
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

    fn in_group(&self, id: ExtId) -> bool {
        self.groups.iter().any(|g| g.contains(id))
    }

    /// Recompute the live pool: dissolve every live group (singletons included)
    /// into a flat pool, run the strategy, and install fresh live groups plus a
    /// live singleton for each leftover. Frozen groups are kept verbatim with
    /// stable ids.
    pub fn solve(&mut self) -> Result<(), ApiError> {
        let total = self.items.len();
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
            });
            self.next_id += 1;
        }
        // Every residual lot becomes its own live allocation group. Do not drop
        // a residual merely because the same row id was partly allocated above:
        // the report is a hypergraph, not a row partition.
        for item in res.residual {
            self.singleton_from_item(item);
        }
        let accounted: BTreeSet<ExtId> = self
            .groups
            .iter()
            .flat_map(|g| g.allocations.iter().map(|a| a.id))
            .collect();
        conservation_airlock(total, accounted.len())?;
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
    pub fn group(&mut self, ids: &[ExtId], net: i64, origin: &str) -> Result<u64, ApiError> {
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
        let primary = compiled.primary.clone();
        Ok(Workspace {
            map,
            inner: Recon::new(compiled.strategy, move |r: &PhysicalRow| r.int(primary)),
        })
    }

    pub fn map(&self) -> &ColumnMap {
        &self.map
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
            PlanNode::AggNet { key: "objsub".into(), tol: 0 },
            PlanNode::Exact {},
            PlanNode::Signal { signals: "tokens".into(), tol: 0, cap: 256 },
            PlanNode::Flow { day: "day".into(), tokens: "tokens".into(), penalty: 1000.0, window: 30, cost: CostSpec::default() },
        ]})
    }

    #[test]
    fn exact_pair_matches() {
        let mut s = Session::new(map());
        s.upsert(1, row(100, 1, 0, 999, &[])).unwrap();
        s.upsert(2, row(-100, 2, 0, -999, &[])).unwrap();
        let rep = s.solve(&full_pipeline()).unwrap();
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
}
