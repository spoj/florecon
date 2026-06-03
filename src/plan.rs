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
//!   / `freeze` / `breakup` / …); [`Workspace`] is its [`LoweredRow`] + [`Plan`]
//!   specialization and [`Session`] is the stateless one-shot form.
//! - [`Report`] — the relational partition result (`assignments` + `groups`).
//!
//! Conservation is enforced at the boundary: a solve verifies that every input
//! id lands in exactly one group, so a malformed plan can never silently lose or
//! double-count mass.

use crate::flow::ExtId;
use crate::lower::Row;
use crate::plan_compile::compile;

/// The wire-contract version: the shape of [`Plan`], [`Report`], and the WASM
/// command set. Hosts (the Python wheel, the browser module) read it back from
/// the engine and refuse to run against a mismatched binary. Bump it on any
/// breaking change to those shapes.
///
/// v5 uses a unified partition report and makes flow amount terminology generic: a row is bare
/// [`Cell`](crate::lower::Cell)s (a number or a string) and the schema's
/// per-column [`Kind`] decides how each lowers. The engine's lowered
/// [`LoweredCell`] (`Int`/`Tokens`) form is an internal implementation detail —
/// it is never on the wire.
pub const CONTRACT_VERSION: u32 = 5;
pub use crate::error::ApiError;
pub use crate::expr::{BoolExpr, BoolRef, ScalarExpr, ScalarRef};
pub use crate::report::{GroupOut, Report, Status};
pub use crate::row::{LoweredCell, LoweredRow};
pub use crate::schema::{Column, Schema};

use crate::strategy::{Item, Strategy};
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// The plan (strategy tree as data)
// ---------------------------------------------------------------------------

/// A reconciliation pipeline expressed as data. Compiles to the closure-based
/// combinators of [`crate::strategy`]; every leaf references columns by name.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "op", rename_all = "snake_case"))]
pub enum Plan {
    /// Cascade: each step runs on the previous step's residual.
    Seq { steps: Vec<Plan> },
    /// Fork/join shard by a scalar column/expression, run `inner` per shard.
    Partition { by: ScalarRef, inner: Box<Plan> },
    /// Route rows by a boolean column/expression, run different child subtrees
    /// on each side, then join. This is a structural split; both sides conserve.
    Branch {
        pred: BoolRef,
        and_then: Box<Plan>,
        or_else: Box<Plan>,
    },
    /// Run `inner` within a sliding window over an integer order expression.
    Windowed {
        order: ScalarRef,
        width: i64,
        inner: Box<Plan>,
    },
    /// Accept an aggregation bucket (`key`) that nets to zero in `tol`.
    AggNet {
        key: ScalarRef,
        amount: ScalarRef,
        tol: i64,
    },
    /// Pair opposite-sign rows with equal magnitude on `amount`.
    Exact { amount: ScalarRef },
    /// Group rows that share an out-of-band token signal and net to zero.
    Signal {
        signals: String,
        amount: ScalarRef,
        tol: i64,
        cap: usize,
    },
    /// The min-cost-flow arbiter over the residual.
    Flow {
        /// Numeraire expression conserved by the network and used for
        /// exact-amount candidates/cost.
        amount: ScalarRef,
        /// Date/order expression (days) for proximity candidate generation.
        day: ScalarRef,
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
    schema: Schema,
    rows: BTreeMap<ExtId, LoweredRow>,
}

impl Session {
    pub fn new(schema: Schema) -> Self {
        Session {
            schema,
            rows: BTreeMap::new(),
        }
    }

    /// Build a session from a schema and a batch of business rows (the batch
    /// boundary mode: the whole shard crosses once, e.g. from a WASM host).
    /// Rows are lowered against the schema.
    pub fn from_rows<I>(schema: Schema, rows: I) -> Result<Self, ApiError>
    where
        I: IntoIterator<Item = (ExtId, Row)>,
    {
        let mut s = Session::new(schema);
        for (id, row) in rows {
            s.upsert(id, row)?;
        }
        Ok(s)
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
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
    pub fn upsert(&mut self, id: ExtId, row: Row) -> Result<(), ApiError> {
        let lowered = row.lower(&self.schema.kinds(), &self.schema.token_cfg())?;
        self.rows.insert(id, lowered);
        Ok(())
    }

    /// Remove a row if present.
    pub fn remove(&mut self, id: ExtId) {
        self.rows.remove(&id);
    }

    /// Run a plan over the current rows and return the partitioned result.
    /// Verifies conservation before returning.
    pub fn solve(&self, plan: &Plan) -> Result<Report, ApiError> {
        // Session is a stateless one-shot: compile the cold flow leaf.
        let mut strategy = compile(plan, &self.schema)?;
        // Materialize in id order for deterministic candidate generation.
        let bag: Vec<Item<LoweredRow>> = self
            .rows
            .iter()
            .map(|(id, row)| Item {
                id: *id,
                data: row.clone(),
            })
            .collect();
        let input = bag.len();
        let res = strategy.run(bag);

        // Assign stable group ids: order groups by their smallest member.
        let mut groups = res.groups;
        groups.sort_by_key(|g| g.members.iter().copied().min().unwrap_or(0));

        let mut assignments = Vec::new();
        let mut group_out = Vec::with_capacity(groups.len() + res.residual.len());
        let mut next_gid = 0u64;
        for g in groups {
            let gid = next_gid;
            next_gid += 1;
            for &m in &g.members {
                assignments.push((m, gid));
            }
            group_out.push(GroupOut {
                group_id: gid,
                origin: g.origin.to_string(),
                net: g.net,
                size: g.members.len(),
                status: Status::Live,
            });
        }

        let mut residual: Vec<ExtId> = res.residual.into_iter().map(|i| i.id).collect();
        residual.sort_unstable();
        for id in residual {
            let gid = next_gid;
            next_gid += 1;
            assignments.push((id, gid));
            group_out.push(GroupOut {
                group_id: gid,
                origin: "unmatched".to_string(),
                net: 0,
                size: 1,
                status: Status::Live,
            });
        }
        assignments.sort();

        // Conservation airlock: every input id has exactly one assignment.
        conservation_airlock(input, assignments.len())?;

        Ok(Report {
            assignments,
            groups: group_out,
        })
    }
}

// ---------------------------------------------------------------------------
// Batch request (the portable wire shape for a whole-shard solve)
// ---------------------------------------------------------------------------

/// A self-contained batch solve: schema + rows + plan. This is the JSON a WASM
/// or other batch host ships across the boundary in one coarse crossing.
#[cfg(feature = "serde")]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SolveRequest {
    pub schema: Schema,
    pub rows: Vec<(ExtId, Row)>,
    pub plan: Plan,
}

#[cfg(feature = "serde")]
impl SolveRequest {
    /// Lower the rows against the schema, build the session, and run the plan
    /// (partition check included).
    pub fn run(self) -> Result<Report, ApiError> {
        let session = Session::from_rows(self.schema, self.rows)?;
        session.solve(&self.plan)
    }
}

// ---------------------------------------------------------------------------
// Workspace — the interactive, stateful surface
// ---------------------------------------------------------------------------

struct GroupRec {
    id: u64,
    members: Vec<ExtId>,
    origin: String,
    net: i64,
    status: Status,
}

impl GroupRec {
    fn is_frozen(&self) -> bool {
        self.status == Status::Frozen
    }
}

/// The interactive result: a single partition of every input id into groups,
/// each carrying its [`Status`]. There is no separate residual set — an
/// unmatched row is a live singleton group (origin `"unmatched"`).
pub type WorkspaceReport = Report;

/// A long-lived, editable reconciliation workspace over items of type `E`,
/// driven by any [`Strategy`]. This is the one stateful facade; [`Workspace`]
/// is its `Row` + [`Plan`] specialization and a typed Rust caller can drive
/// `Recon<MyTx>` directly with a strategy built from the combinators.
///
/// It supports the interactive loop a UI drives: [`solve`](Recon::solve)
/// recomputes the unfrozen pool; [`freeze`](Recon::freeze) locks a group an
/// analyst trusts so re-solves leave it alone; [`breakup`](Recon::breakup)
/// dissolves a group back to the pool. The conservation invariant — every item
/// id is in exactly one group — holds after every operation. An unmatched row
/// is simply a live singleton group (origin `"unmatched"`); there is no separate
/// residual set.
pub struct Recon<E> {
    strategy: Box<dyn Strategy<E>>,
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
    pub fn new(strategy: Box<dyn Strategy<E>>) -> Self {
        Recon {
            strategy,
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
        self.groups.push(GroupRec {
            id: self.next_id,
            members: vec![id],
            origin: "unmatched".to_string(),
            net: 0,
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
            if !g.members.contains(&id) {
                return true;
            }
            g.members.retain(|&m| m != id);
            if g.members.is_empty() {
                false
            } else if g.members.len() == 1 {
                // A match reduced to one member can no longer net; its survivor
                // returns to the live pool as a fresh singleton.
                orphaned.extend(g.members.iter().copied());
                false
            } else {
                true
            }
        });
        for o in orphaned {
            self.push_live_singleton(o);
        }
    }

    fn in_group(&self, id: ExtId) -> bool {
        self.groups.iter().any(|g| g.members.contains(&id))
    }

    /// Recompute the live pool: dissolve every live group (singletons included)
    /// into a flat pool, run the strategy, and install fresh live groups plus a
    /// live singleton for each leftover. Frozen groups are kept verbatim with
    /// stable ids.
    pub fn solve(&mut self) -> Result<(), ApiError> {
        let total = self.items.len();
        let frozen_members: BTreeSet<ExtId> = self
            .groups
            .iter()
            .filter(|g| g.is_frozen())
            .flat_map(|g| g.members.iter().copied())
            .collect();
        let bag: Vec<Item<E>> = self
            .items
            .iter()
            .filter(|(id, _)| !frozen_members.contains(id))
            .map(|(id, item)| Item {
                id: *id,
                data: item.clone(),
            })
            .collect();
        let res = self.strategy.run(bag);

        // Dissolve all live groups; keep frozen ones verbatim.
        self.groups.retain(|g| g.is_frozen());
        let mut new_groups = res.groups;
        new_groups.sort_by_key(|g| g.members.iter().copied().min().unwrap_or(0));
        for g in new_groups {
            self.groups.push(GroupRec {
                id: self.next_id,
                members: g.members,
                origin: g.origin.to_string(),
                net: g.net,
                status: Status::Live,
            });
            self.next_id += 1;
        }
        // Leftovers become live singleton groups (origin "unmatched").
        let mut leftover: Vec<ExtId> = res.residual.into_iter().map(|i| i.id).collect();
        leftover.sort_unstable();
        for id in leftover {
            self.push_live_singleton(id);
        }
        // Conservation airlock: the members across all groups equal the input
        // id set — every input id is in exactly one group (total-input relative,
        // frozen members included).
        let accounted: usize = self.groups.iter().map(|g| g.members.len()).sum();
        conservation_airlock(total, accounted)?;
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
            if !g.is_frozen() && g.members.len() >= 2 && g.net.abs() <= tol {
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
            if !g.is_frozen() && g.members.len() == 1 && want.contains(&g.members[0]) {
                g.status = Status::Frozen;
            }
        }
    }

    /// Unlock a frozen group; the next solve may reshape it.
    pub fn unfreeze(&mut self, group_id: u64) -> Result<(), ApiError> {
        self.group_mut(group_id)?.status = Status::Live;
        Ok(())
    }

    /// Dissolve a group (live or frozen); each member returns to the pool as a
    /// fresh live singleton until the next explicit solve.
    pub fn breakup(&mut self, group_id: u64) -> Result<(), ApiError> {
        let pos = self
            .groups
            .iter()
            .position(|g| g.id == group_id)
            .ok_or(ApiError::UnknownGroup(group_id))?;
        let g = self.groups.remove(pos);
        for m in g.members {
            self.push_live_singleton(m);
        }
        Ok(())
    }

    /// Manually assert a group over `ids` with a caller-supplied `net` and
    /// `origin`. This is the analyst override: rows are pulled out of any
    /// *live* group they currently sit in (a live group that falls below two
    /// members dissolves, its survivor returning to a live singleton — nothing
    /// is lost). Pulling a row out of a *frozen* group is refused, so a
    /// signed-off reconciliation is never silently disturbed. The new group is
    /// frozen, because a manual match is itself a signoff that a re-solve must
    /// not reshape. Returns the new stable group id.
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
            if self
                .groups
                .iter()
                .any(|g| g.is_frozen() && g.members.contains(&id))
            {
                return Err(ApiError::FrozenMember(id));
            }
        }
        // Pull the chosen ids out of any live group.
        let claim: BTreeSet<ExtId> = members.iter().copied().collect();
        self.pull_from_live(&claim);
        let id = self.next_id;
        self.next_id += 1;
        self.groups.push(GroupRec {
            id,
            members,
            origin: origin.to_string(),
            net,
            status: Status::Frozen,
        });
        Ok(id)
    }

    /// Remove `claim` from every live group, dropping emptied groups and
    /// re-minting any survivor of a now-singleton live group. Frozen groups are
    /// untouched (callers guard against frozen members first).
    fn pull_from_live(&mut self, claim: &BTreeSet<ExtId>) {
        for g in &mut self.groups {
            if !g.is_frozen() {
                g.members.retain(|m| !claim.contains(m));
            }
        }
        let mut orphaned = Vec::new();
        self.groups.retain(|g| {
            if g.is_frozen() {
                return true;
            }
            if g.members.is_empty() {
                false
            } else if g.members.len() == 1 {
                orphaned.extend(g.members.iter().copied());
                false
            } else {
                true
            }
        });
        for o in orphaned {
            self.push_live_singleton(o);
        }
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
            if self
                .groups
                .iter()
                .any(|g| g.is_frozen() && g.members.contains(&id))
            {
                return Err(ApiError::FrozenMember(id));
            }
        }
        let claim: BTreeSet<ExtId> = ids.iter().copied().collect();
        self.pull_from_live(&claim);
        // Each claimed id stands alone as a fresh live singleton.
        for id in claim {
            self.push_live_singleton(id);
        }
        Ok(())
    }

    fn group_mut(&mut self, group_id: u64) -> Result<&mut GroupRec, ApiError> {
        self.groups
            .iter_mut()
            .find(|g| g.id == group_id)
            .ok_or(ApiError::UnknownGroup(group_id))
    }

    /// Snapshot the current state as a relational report — a single partition of
    /// every id into groups, each carrying its status.
    pub fn report(&self) -> WorkspaceReport {
        let mut assignments = Vec::new();
        let mut groups = Vec::with_capacity(self.groups.len());
        for g in &self.groups {
            for &m in &g.members {
                assignments.push((m, g.id));
            }
            groups.push(GroupOut {
                group_id: g.id,
                origin: g.origin.clone(),
                net: g.net,
                size: g.members.len(),
                status: g.status,
            });
        }
        assignments.sort();
        groups.sort_by_key(|g| g.group_id);
        WorkspaceReport {
            assignments,
            groups,
        }
    }
}

/// The interactive [`Plan`]-driven workspace over [`LoweredRow`]s: a [`Recon<LoweredRow>`]
/// plus its [`Schema`] (for arity validation). This is what the WASM `dispatch`
/// surface drives.
pub struct Workspace {
    schema: Schema,
    inner: Recon<LoweredRow>,
}

impl Workspace {
    /// Compile `plan` against `schema` and create an empty workspace. Fails if
    /// the plan references an unknown column.
    pub fn new(schema: Schema, plan: Plan) -> Result<Self, ApiError> {
        // The interactive workspace persists across solves (Recon stores the
        // strategy once), so compile the warm, shard-keyed flow leaf.
        let strategy = compile(&plan, &schema)?;
        Ok(Workspace {
            schema,
            inner: Recon::new(strategy),
        })
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Insert or replace a row. Takes a business [`Row`] (bare cells) and lowers
    /// it against the schema's per-column [`Kind`]s before storing. Lowering
    /// arity-checks against the schema.
    pub fn upsert(&mut self, id: ExtId, row: Row) -> Result<(), ApiError> {
        let lowered = row.lower(&self.schema.kinds(), &self.schema.token_cfg())?;
        self.inner.upsert(id, lowered);
        Ok(())
    }

    /// Remove a row from the workspace and from wherever it currently sits.
    pub fn remove(&mut self, id: ExtId) {
        self.inner.remove(id);
    }

    /// Recompute the unfrozen pool, preserving frozen groups.
    pub fn solve(&mut self) -> Result<(), ApiError> {
        self.inner.solve()
    }

    /// Lock a group so future solves leave it intact.
    pub fn freeze(&mut self, group_id: u64) -> Result<(), ApiError> {
        self.inner.freeze(group_id)
    }

    /// Freeze every clean (size >= 2, |net| <= tol) live group. Returns count.
    pub fn freeze_clean(&mut self, tol: i64) -> usize {
        self.inner.freeze_clean(tol)
    }

    /// Freeze the live singleton groups holding any of `ids` (accepted
    /// unmatched exceptions) in one crossing.
    pub fn freeze_singletons(&mut self, ids: &[ExtId]) {
        self.inner.freeze_singletons(ids)
    }

    /// Unlock a frozen group; the next solve may reshape it.
    pub fn unfreeze(&mut self, group_id: u64) -> Result<(), ApiError> {
        self.inner.unfreeze(group_id)
    }

    /// Dissolve a group; each member returns to a live singleton.
    pub fn breakup(&mut self, group_id: u64) -> Result<(), ApiError> {
        self.inner.breakup(group_id)
    }

    /// Manually assert a frozen group over `ids` with a caller-supplied `net`.
    pub fn group(&mut self, ids: &[ExtId], net: i64, origin: &str) -> Result<u64, ApiError> {
        self.inner.group(ids, net, origin)
    }

    /// Send `ids` back to live singletons, removing them from their live group.
    pub fn ungroup(&mut self, ids: &[ExtId]) -> Result<(), ApiError> {
        self.inner.ungroup(ids)
    }

    /// Snapshot the current state as a relational report.
    pub fn report(&self) -> WorkspaceReport {
        self.inner.report()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lower::{Cell, Kind};

    fn schema() -> Schema {
        Schema::typed([
            ("usd", Kind::Number),
            ("day", Kind::Number),
            ("objsub", Kind::Number),
            ("native", Kind::Number),
            ("tokens", Kind::Tokens),
        ])
    }

    // Build a business row. usd/day/objsub/native are genuine ints (Number
    // columns, pass through); each token id becomes a distinct digit-bearing
    // word so the Tokens column lowers to a set with the same overlap (the
    // engine only cares about token equality, not the specific hash).
    fn row(usd: i64, day: i64, objsub: i64, native: i64, tokens: &[u64]) -> Row {
        let text = tokens
            .iter()
            .map(|n| format!("T{n:09}"))
            .collect::<Vec<_>>()
            .join(" ");
        Row::new(vec![
            Cell::Num(usd),
            Cell::Num(day),
            Cell::Num(objsub),
            Cell::Num(native),
            Cell::Str(text),
        ])
    }

    fn full_pipeline() -> Plan {
        Plan::Seq {
            steps: vec![
                Plan::AggNet {
                    key: "objsub".into(),
                    amount: "usd".into(),
                    tol: 0,
                },
                Plan::Exact {
                    amount: "native".into(),
                },
                Plan::Signal {
                    signals: "tokens".into(),
                    amount: "usd".into(),
                    tol: 0,
                    cap: 256,
                },
                Plan::Flow {
                    amount: "usd".into(),
                    day: "day".into(),
                    tokens: "tokens".into(),
                    penalty: 1000.0,
                    window: 30,
                    cost: CostSpec::default(),
                },
            ],
        }
    }

    fn unmatched_ids(rep: &Report) -> Vec<ExtId> {
        let unmatched_gids: BTreeSet<u64> = rep
            .groups
            .iter()
            .filter(|g| g.status == Status::Live && g.origin == "unmatched" && g.size == 1)
            .map(|g| g.group_id)
            .collect();
        rep.assignments
            .iter()
            .filter(|(_, gid)| unmatched_gids.contains(gid))
            .map(|(id, _)| *id)
            .collect()
    }

    fn matched_groups(rep: &Report) -> Vec<&GroupOut> {
        rep.groups
            .iter()
            .filter(|g| g.origin != "unmatched")
            .collect()
    }

    #[test]
    fn exact_pair_matches_and_conserves() {
        let mut s = Session::new(schema());
        s.upsert(1, row(100, 1, 0, 100, &[])).unwrap();
        s.upsert(2, row(-100, 2, 0, -100, &[])).unwrap();
        s.upsert(3, row(7, 3, 0, 7, &[])).unwrap(); // unmatched
        let rep = s.solve(&full_pipeline()).unwrap();
        assert_eq!(rep.assignments.len(), 3);
        assert_eq!(unmatched_ids(&rep), vec![3]);
        // 1 and 2 share a group.
        let g1 = rep.assignments.iter().find(|(id, _)| *id == 1).unwrap().1;
        let g2 = rep.assignments.iter().find(|(id, _)| *id == 2).unwrap().1;
        assert_eq!(g1, g2);
    }

    #[test]
    fn signal_bridge_groups_by_token() {
        let mut s = Session::new(schema());
        // Three legs sharing token 42 that net to zero: a reference bridge.
        // Distinct objsub so agg_net does not grab them first.
        s.upsert(1, row(100, 1, 1, 0, &[42])).unwrap();
        s.upsert(2, row(-60, 2, 2, 0, &[42])).unwrap();
        s.upsert(3, row(-40, 3, 3, 0, &[42])).unwrap();
        let rep = s.solve(&full_pipeline()).unwrap();
        assert_eq!(unmatched_ids(&rep).len(), 0);
        assert_eq!(rep.groups.len(), 1);
        assert_eq!(rep.groups[0].origin, "signal_group");
    }

    #[test]
    fn agg_net_accepts_balanced_bucket() {
        let mut s = Session::new(schema());
        s.upsert(1, row(100, 1, 7, 0, &[])).unwrap();
        s.upsert(2, row(-100, 2, 7, 0, &[])).unwrap();
        let rep = s.solve(&full_pipeline()).unwrap();
        assert_eq!(matched_groups(&rep).len(), 1);
        assert_eq!(matched_groups(&rep)[0].origin, "agg_net");
    }

    #[test]
    fn branch_predicate_routes_without_materializing_column() {
        let plan = Plan::Branch {
            pred: BoolRef::Expr(BoolExpr::Gt(
                Box::new("objsub".into()),
                Box::new(ScalarExpr::Lit(0).into()),
            )),
            and_then: Box::new(Plan::AggNet {
                key: "objsub".into(),
                amount: "usd".into(),
                tol: 0,
            }),
            or_else: Box::new(Plan::Exact {
                amount: "usd".into(),
            }),
        };
        let mut s = Session::new(schema());
        s.upsert(1, row(100, 1, 1, 0, &[])).unwrap();
        s.upsert(2, row(-100, 2, 1, 0, &[])).unwrap();
        s.upsert(3, row(50, 3, 0, 0, &[])).unwrap();
        s.upsert(4, row(-50, 4, 0, 0, &[])).unwrap();
        let rep = s.solve(&plan).unwrap();
        assert_eq!(matched_groups(&rep).len(), 2);
        assert!(rep.groups.iter().any(|g| g.origin == "agg_net"));
        assert!(rep.groups.iter().any(|g| g.origin == "exact_1to1"));
    }

    #[test]
    fn partition_keeps_currencies_apart() {
        let plan = Plan::Partition {
            by: "objsub".into(), // reuse objsub column as a partition key
            inner: Box::new(Plan::Exact {
                amount: "native".into(),
            }),
        };
        let mut s = Session::new(schema());
        // Same native amount, different partitions -> must NOT pair.
        s.upsert(1, row(0, 1, 1, 50, &[])).unwrap();
        s.upsert(2, row(0, 2, 2, -50, &[])).unwrap();
        let rep = s.solve(&plan).unwrap();
        assert_eq!(matched_groups(&rep).len(), 0);
        assert_eq!(unmatched_ids(&rep).len(), 2);
    }

    #[test]
    fn unknown_column_is_an_error() {
        let s = Session::new(schema());
        let plan = Plan::Exact {
            amount: "nope".into(),
        };
        assert_eq!(s.solve(&plan), Err(ApiError::UnknownColumn("nope".into())));
    }

    #[test]
    fn arity_mismatch_rejected() {
        let mut s = Session::new(schema());
        let bad = Row::new(vec![Cell::Num(1)]);
        assert!(matches!(
            s.upsert(1, bad),
            Err(ApiError::SchemaArity { .. })
        ));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn plan_json_round_trips() {
        // The plan is the wire format an agent/host ships across the boundary.
        let plan = full_pipeline();
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"op\":\"agg_net\""));
        assert!(json.contains("\"op\":\"flow\""));
        let back: Plan = serde_json::from_str(&json).unwrap();
        // Re-serialize and compare: stable round-trip.
        assert_eq!(json, serde_json::to_string(&back).unwrap());

        // And it still runs after a boundary round-trip.
        let mut s = Session::new(schema());
        s.upsert(1, row(100, 1, 0, 100, &[])).unwrap();
        s.upsert(2, row(-100, 2, 0, -100, &[])).unwrap();
        let rep = s.solve(&back).unwrap();
        assert_eq!(rep.assignments.len(), 2);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn flow_cost_defaults_when_omitted() {
        // A serialized Flow node without a `cost` field fills the default
        // cascade, so existing plans keep working and the data-driven cost is
        // backward compatible.
        let json = r#"{"op":"flow","amount":"usd","day":"day","tokens":"tokens","penalty":1000.0,"window":30}"#;
        let plan: Plan = serde_json::from_str(json).unwrap();
        match plan {
            Plan::Flow { cost, .. } => assert_eq!(cost, CostSpec::default()),
            _ => panic!("expected flow"),
        }
    }

    #[cfg(feature = "serde")]
    #[test]
    fn custom_cost_spec_round_trips_and_steers() {
        // A hand-authored cost: only a shared token may pair, exact amount is
        // forbidden. Two equal-amount rows with no shared token must NOT match.
        let plan = Plan::Seq {
            steps: vec![Plan::Flow {
                amount: "usd".into(),
                day: "day".into(),
                tokens: "tokens".into(),
                penalty: 1000.0,
                window: -1,
                cost: CostSpec {
                    tiers: vec![CostTier {
                        when: vec![Cond::TokenShared],
                        base: 1.0,
                        day_slope: 0.0,
                        max_day: None,
                    }],
                },
            }],
        };
        let json = serde_json::to_string(&plan).unwrap();
        assert_eq!(plan, serde_json::from_str::<Plan>(&json).unwrap());

        let mut s = Session::new(schema());
        s.upsert(1, row(100, 1, 0, 100, &[])).unwrap(); // equal amount,
        s.upsert(2, row(-100, 2, 0, -100, &[])).unwrap(); // no shared token
        let rep = s.solve(&plan).unwrap();
        assert_eq!(
            matched_groups(&rep).len(),
            0,
            "amount-only forbidden by this cost"
        );
        assert_eq!(unmatched_ids(&rep).len(), 2);

        // Give them a shared token and the same cost now pairs them.
        s.upsert(1, row(100, 1, 0, 100, &[7])).unwrap();
        s.upsert(2, row(-100, 2, 0, -100, &[7])).unwrap();
        let rep = s.solve(&plan).unwrap();
        assert_eq!(rep.groups.len(), 1);
    }

    #[test]
    fn generic_recon_over_plain_items() {
        // Recon<E> drives any Strategy over any item type -- here plain i64
        // amounts with an exact-pair strategy, no Row/Schema/Plan involved.
        use crate::strategy::exact_1to1;
        let strat = exact_1to1(|a: &i64| Some(a.unsigned_abs()), |a: &i64| *a);
        let mut r: Recon<i64> = Recon::new(strat);
        r.upsert(1, 50);
        r.upsert(2, -50);
        r.upsert(3, 9);
        r.solve().unwrap();
        let rep = r.report();
        // the pair plus a live singleton for the unmatched 9
        assert_eq!(rep.groups.len(), 2);
        let pair = rep.groups.iter().find(|g| g.size == 2).unwrap();
        assert_eq!(rep.groups.iter().filter(|g| g.size == 1).count(), 1);
        // freeze survives a re-solve; breakup returns members to the pool.
        let g = pair.group_id;
        r.freeze(g).unwrap();
        r.upsert(4, 9);
        r.solve().unwrap();
        assert!(
            r.report()
                .groups
                .iter()
                .any(|x| x.group_id == g && x.status == Status::Frozen)
        );
    }

    fn ws_conserves(ws: &Workspace) {
        let rep = ws.report();
        // Every row is in exactly one group (no separate residual set).
        let n = rep.assignments.len();
        assert_eq!(n, ws.len(), "every row is in exactly one group");
    }

    #[test]
    fn workspace_solve_freeze_breakup() {
        let mut ws = Workspace::new(schema(), full_pipeline()).unwrap();
        ws.upsert(1, row(100, 1, 10, 100, &[])).unwrap();
        ws.upsert(2, row(-100, 2, 10, -100, &[])).unwrap();
        ws.upsert(3, row(50, 3, 20, 50, &[])).unwrap();
        ws.upsert(4, row(-50, 4, 20, -50, &[])).unwrap();
        // Before solving, every fresh id stands as a live singleton group.
        assert_eq!(ws.report().groups.len(), 4);
        assert!(
            ws.report()
                .groups
                .iter()
                .all(|g| g.size == 1 && g.status == Status::Live)
        );
        ws_conserves(&ws);

        ws.solve().unwrap();
        let rep = ws.report();
        assert_eq!(rep.groups.len(), 2);
        ws_conserves(&ws);

        // Freeze one group, then break up the other and re-solve.
        let g0 = rep.groups[0].group_id;
        let g1 = rep.groups[1].group_id;
        ws.freeze(g0).unwrap();
        ws.breakup(g1).unwrap();
        // g1's members are now live singletons; g0 still grouped + frozen.
        let rep = ws.report();
        assert_eq!(rep.groups.len(), 3);
        assert_eq!(
            rep.groups
                .iter()
                .filter(|g| g.status == Status::Frozen)
                .count(),
            1
        );
        assert_eq!(rep.groups.iter().filter(|g| g.size == 1).count(), 2);
        ws_conserves(&ws);

        // Re-solve: frozen group survives with its id; the pool reforms.
        ws.solve().unwrap();
        let rep = ws.report();
        assert!(
            rep.groups
                .iter()
                .any(|g| g.group_id == g0 && g.status == Status::Frozen)
        );
        assert_eq!(rep.groups.len(), 2);
        ws_conserves(&ws);
    }

    #[test]
    fn workspace_remove_keeps_conservation() {
        let mut ws = Workspace::new(schema(), full_pipeline()).unwrap();
        ws.upsert(1, row(100, 1, 0, 100, &[])).unwrap();
        ws.upsert(2, row(-100, 2, 0, -100, &[])).unwrap();
        ws.solve().unwrap();
        assert_eq!(ws.report().groups.len(), 1);
        // Removing one member dissolves the pair; survivor becomes a live
        // singleton.
        ws.remove(2);
        let rep = ws.report();
        assert_eq!(rep.groups.len(), 1);
        assert_eq!(rep.groups[0].size, 1);
        assert!(rep.groups[0].status == Status::Live);
        assert_eq!(rep.assignments, vec![(1, rep.groups[0].group_id)]);
        ws_conserves(&ws);
    }

    #[test]
    fn workspace_unknown_group_errors() {
        let mut ws = Workspace::new(schema(), full_pipeline()).unwrap();
        assert_eq!(ws.freeze(99), Err(ApiError::UnknownGroup(99)));
        assert_eq!(ws.breakup(99), Err(ApiError::UnknownGroup(99)));
    }

    #[test]
    fn workspace_manual_group_and_ungroup() {
        let mut ws = Workspace::new(schema(), full_pipeline()).unwrap();
        // Four rows that the pipeline would not pair (distinct objsub, no net).
        ws.upsert(1, row(100, 1, 11, 100, &[])).unwrap();
        ws.upsert(2, row(-90, 2, 22, -90, &[])).unwrap();
        ws.upsert(3, row(40, 3, 33, 40, &[])).unwrap();
        ws.upsert(4, row(-50, 4, 44, -50, &[])).unwrap();
        ws.solve().unwrap();

        // A degenerate (single-row) manual group is refused.
        assert_eq!(
            ws.group(&[1], 100, "manual"),
            Err(ApiError::DegenerateGroup)
        );
        assert_eq!(
            ws.group(&[1, 999], 0, "manual"),
            Err(ApiError::UnknownId(999))
        );

        // Manually match two residual rows; the group is frozen and net is
        // exactly what the caller supplied.
        let gid = ws.group(&[1, 2], 10, "manual").unwrap();
        let rep = ws.report();
        let g = rep.groups.iter().find(|g| g.group_id == gid).unwrap();
        assert!(g.status == Status::Frozen && g.origin == "manual" && g.size == 2 && g.net == 10);
        ws_conserves(&ws);

        // A re-solve must not disturb the manual (frozen) group.
        ws.solve().unwrap();
        assert!(ws.report().groups.iter().any(|g| g.group_id == gid));

        // Stealing a row out of a frozen group is refused.
        assert_eq!(
            ws.group(&[1, 3], 0, "manual"),
            Err(ApiError::FrozenMember(1))
        );
        assert_eq!(ws.ungroup(&[1]), Err(ApiError::FrozenMember(1)));

        // Ungroup sends live-group rows back to live singletons.
        ws.ungroup(&[3, 4]).unwrap();
        let rep = ws.report();
        let singletons: BTreeSet<ExtId> = rep
            .groups
            .iter()
            .filter(|g| g.size == 1 && g.status == Status::Live)
            .flat_map(|g| {
                rep.assignments
                    .iter()
                    .filter(move |(_, gid)| *gid == g.group_id)
                    .map(|(id, _)| *id)
            })
            .collect();
        assert!(singletons.contains(&3) && singletons.contains(&4));
        ws_conserves(&ws);
    }

    #[test]
    fn workspace_frozen_singleton_survives_solve() {
        // Freezing a live singleton is the "accept an exception" path: it
        // becomes a persistent frozen group, untouched by re-solve, id stable.
        let mut ws = Workspace::new(schema(), full_pipeline()).unwrap();
        ws.upsert(1, row(7, 1, 0, 7, &[])).unwrap(); // never pairs
        ws.solve().unwrap();
        let rep = ws.report();
        let g = rep.groups.iter().find(|g| g.size == 1).unwrap();
        let gid = g.group_id;
        assert_eq!(g.status, Status::Live);
        assert_eq!(g.origin, "unmatched");

        ws.freeze_singletons(&[1]);
        let g = ws
            .report()
            .groups
            .into_iter()
            .find(|g| g.group_id == gid)
            .unwrap();
        assert!(g.size == 1 && g.status == Status::Frozen);

        // A re-solve leaves the frozen singleton untouched, id stable.
        ws.solve().unwrap();
        assert!(
            ws.report()
                .groups
                .iter()
                .any(|g| g.group_id == gid && g.status == Status::Frozen && g.size == 1)
        );
        ws_conserves(&ws);
    }

    #[test]
    fn workspace_live_singleton_id_is_ephemeral() {
        // Live singletons are re-minted every solve: nothing should reference a
        // live singleton id across a solve.
        let mut ws = Workspace::new(schema(), full_pipeline()).unwrap();
        ws.upsert(1, row(7, 1, 0, 7, &[])).unwrap();
        ws.solve().unwrap();
        let id1 = ws
            .report()
            .groups
            .into_iter()
            .find(|g| g.size == 1)
            .unwrap()
            .group_id;
        ws.solve().unwrap();
        let id2 = ws
            .report()
            .groups
            .into_iter()
            .find(|g| g.size == 1)
            .unwrap()
            .group_id;
        assert_ne!(id1, id2, "live singleton ids are ephemeral across solves");
        // Crucially, the stale id was not *reassigned* to a different group, so a
        // host that cached it fails loudly (UnknownGroup) instead of silently
        // mis-targeting whatever now holds that slot.
        assert!(ws.report().groups.iter().all(|g| g.group_id != id1));
        assert_eq!(ws.breakup(id1), Err(ApiError::UnknownGroup(id1)));
        assert_eq!(ws.freeze(id1), Err(ApiError::UnknownGroup(id1)));

        // Freezing pins the id: it persists across the next solve.
        ws.freeze_singletons(&[1]);
        let frozen_id = ws
            .report()
            .groups
            .into_iter()
            .find(|g| g.size == 1)
            .unwrap()
            .group_id;
        ws.solve().unwrap();
        assert!(ws.report().groups.iter().any(|g| g.group_id == frozen_id));
    }

    // A tiny deterministic LCG so the equivalence tests are reproducible
    // without pulling in a `rand` dependency.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 17
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    // The two rows of pair `p`, each carrying a token and native magnitude
    // unique to that pair, so the only viable flow partner for one leg is the
    // other leg. With every pair isolated, the min-cost-flow optimum is
    // *unique* (no equal-cost alternative arcs), so a warm re-solve and a fresh
    // cold solve must agree group-for-group, not merely on objective.
    fn pair_rows(p: u64, shard: i64) -> ((ExtId, Row), (ExtId, Row)) {
        let mag = (p as i64 + 1) * 101; // distinct magnitude per pair
        let token = 1_000 + p; // distinct token per pair
        let day = (p % 20) as i64;
        let a = (2 * p + 1, row(mag, day, shard, mag, &[token]));
        let b = (2 * p + 2, row(-mag, day + 1, shard, -mag, &[token]));
        (a, b)
    }

    // The multi-member matched-group partition (sets of members, ids sorted).
    fn report_partition<'a, I>(assignments: &[(ExtId, u64)], multi: I) -> BTreeSet<Vec<ExtId>>
    where
        I: IntoIterator<Item = &'a u64>,
    {
        let multi: BTreeSet<u64> = multi.into_iter().copied().collect();
        let mut by_gid: BTreeMap<u64, Vec<ExtId>> = BTreeMap::new();
        for &(id, gid) in assignments {
            if multi.contains(&gid) {
                by_gid.entry(gid).or_default().push(id);
            }
        }
        by_gid
            .into_values()
            .map(|mut m| {
                m.sort_unstable();
                m
            })
            .collect()
    }

    fn warm_partition(rep: &WorkspaceReport) -> BTreeSet<Vec<ExtId>> {
        let multi: Vec<u64> = rep
            .groups
            .iter()
            .filter(|g| g.size >= 2)
            .map(|g| g.group_id)
            .collect();
        report_partition(&rep.assignments, &multi)
    }

    fn cold_partition(
        schema: &Schema,
        plan: &Plan,
        rows: &BTreeMap<ExtId, Row>,
    ) -> BTreeSet<Vec<ExtId>> {
        let mut s = Session::new(schema.clone());
        for (&id, r) in rows {
            s.upsert(id, r.clone()).unwrap();
        }
        let rep = s.solve(plan).unwrap();
        let multi: Vec<u64> = rep
            .groups
            .iter()
            .filter(|g| g.size >= 2)
            .map(|g| g.group_id)
            .collect();
        report_partition(&rep.assignments, &multi)
    }

    fn flow_only_plan() -> Plan {
        Plan::Flow {
            amount: "usd".into(),
            day: "day".into(),
            tokens: "tokens".into(),
            penalty: 1000.0,
            window: 30,
            cost: CostSpec::default(),
        }
    }

    // Drive a random sequence of single-row upserts/removes drawn from a fixed
    // universe of unique-optimum pairs through the warm Workspace, comparing the
    // matched partition to a fresh cold Session at every step.
    fn run_equiv(plan: Plan, shard_of: impl Fn(u64) -> i64, seed: u64) {
        let schema = schema();
        let mut rng = Lcg(seed);
        let mut ws = Workspace::new(schema.clone(), plan.clone()).unwrap();
        let mut rows: BTreeMap<ExtId, Row> = BTreeMap::new();
        let pairs: u64 = 24;

        for _ in 0..300 {
            // Pick one leg of a random pair and toggle its presence.
            let p = rng.below(pairs);
            let shard = shard_of(p);
            let (a, b) = pair_rows(p, shard);
            let leg = if rng.below(2) == 0 { a } else { b };
            let (id, r) = leg;
            if rows.contains_key(&id) && rng.below(2) == 0 {
                rows.remove(&id);
                ws.remove(id);
            } else {
                rows.insert(id, r.clone());
                ws.upsert(id, r).unwrap();
            }
            ws.solve().unwrap();

            let rep = ws.report();
            assert_eq!(rep.assignments.len(), ws.len(), "conservation");
            let warm = warm_partition(&rep);
            let cold = cold_partition(&schema, &plan, &rows);
            assert_eq!(
                warm, cold,
                "warm flow grouping diverged from a fresh cold solve"
            );
        }
    }

    #[test]
    fn warm_flow_matches_cold_solve_under_random_edits() {
        // Warm-vs-cold equivalence (global, un-partitioned shard): warm interactive
        // Workspace == fresh cold Session, group-for-group, across a random
        // upsert/remove sequence. Each warm solve also runs the in-leaf debug
        // determinism guard (warm objective == cold objective).
        run_equiv(flow_only_plan(), |_p| 0, 0x1234_5678_9abc_def0);
    }

    #[test]
    fn warm_flow_partitioned_matches_cold() {
        // Same equivalence with a partitioned plan, exercising shard-key
        // recovery: pairs are sharded by `objsub` (3 shards) and each shard
        // keeps its own warm Matcher keyed by the accumulated partition column.
        let plan = Plan::Partition {
            by: "objsub".into(),
            inner: Box::new(flow_only_plan()),
        };
        run_equiv(plan, |p| (p % 3) as i64, 0xfeed_face_cafe_d00d);
    }

    #[test]
    fn warm_flow_objective_stable_under_contention() {
        // Heavy-contention data (many equal magnitudes, a tiny token space) over
        // the full cascade with random upsert/remove/freeze. The optimal
        // *matching* is degenerate here, but the in-leaf determinism guard
        // (warm objective == cold objective) must hold on every solve, and
        // conservation must never break.
        let schema = schema();
        let plan = full_pipeline();
        let mut rng = Lcg(0x0bad_c0de_dead_beef);
        let mut ws = Workspace::new(schema, plan).unwrap();
        let mut next_id: ExtId = 1;
        let mut live_ids: Vec<ExtId> = Vec::new();

        let contended = |rng: &mut Lcg| {
            let mag = (1 + rng.below(6)) as i64 * 10;
            let sign = if rng.below(2) == 0 { 1 } else { -1 };
            let amt = sign * mag;
            let day = rng.below(40) as i64;
            let token = rng.below(8);
            row(amt, day, 7_000 + amt, amt, &[token])
        };

        for _ in 0..200 {
            match rng.below(4) {
                0 if !live_ids.is_empty() => {
                    let pick = rng.below(live_ids.len() as u64) as usize;
                    let id = live_ids.remove(pick);
                    ws.remove(id);
                }
                1 => {
                    ws.freeze_clean(0);
                }
                2 => {
                    let rep = ws.report();
                    let singles: Vec<ExtId> = rep
                        .groups
                        .iter()
                        .filter(|g| g.size == 1 && g.status == Status::Live)
                        .flat_map(|g| {
                            rep.assignments
                                .iter()
                                .filter(move |(_, gid)| *gid == g.group_id)
                                .map(|(id, _)| *id)
                        })
                        .take(2)
                        .collect();
                    ws.freeze_singletons(&singles);
                }
                _ => {
                    let id = next_id;
                    next_id += 1;
                    let r = contended(&mut rng);
                    ws.upsert(id, r).unwrap();
                    live_ids.push(id);
                }
            }
            ws.solve().unwrap();
            assert_eq!(ws.report().assignments.len(), ws.len());
        }
    }
}
