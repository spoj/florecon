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
//!   / `freeze` / `breakup` / …); [`Workspace`] is its [`Row`] + [`Plan`]
//!   specialization and [`Session`] is the stateless one-shot form.
//! - [`Report`] — the relational result (`assignments` + `groups` + `residual`).
//!
//! Conservation is enforced at the boundary: a solve verifies that every input
//! id lands in exactly one group or in the residual, so a malformed plan can
//! never silently lose or double-count mass.

use crate::flow::{ExtId, Model};
use crate::strategy::{
    Item, Strategy, agg_net, exact_1to1, flow, partition_by, seq, signal_group, windowed,
};
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// Typed records
// ---------------------------------------------------------------------------

/// A typed column value. Integers carry money (minor units), dates (days), and
/// partition/bucket keys; token lists carry pre-hashed out-of-band signals
/// (reference tokens) computed host-side.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Value {
    Int(i64),
    Tokens(Vec<u64>),
}

/// Column layout shared by every row in a [`Session`].
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Schema {
    cols: Vec<String>,
}

impl Schema {
    pub fn new<I, S>(cols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Schema {
            cols: cols.into_iter().map(Into::into).collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.cols.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cols.is_empty()
    }

    fn index(&self, name: &str) -> Result<usize, ApiError> {
        self.cols
            .iter()
            .position(|c| c == name)
            .ok_or_else(|| ApiError::UnknownColumn(name.to_string()))
    }
}

/// One row's column values, positional against the session [`Schema`].
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Row {
    pub values: Vec<Value>,
}

impl Row {
    pub fn new(values: Vec<Value>) -> Self {
        Row { values }
    }

    fn int(&self, idx: usize) -> i64 {
        match self.values.get(idx) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        }
    }

    fn tokens(&self, idx: usize) -> Vec<u64> {
        match self.values.get(idx) {
            Some(Value::Tokens(t)) => t.clone(),
            _ => Vec::new(),
        }
    }
}

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
    /// Fork/join shard by an integer column, run `inner` per shard.
    Partition { by: String, inner: Box<Plan> },
    /// Run `inner` within a sliding window over an integer order column.
    Windowed {
        order: String,
        width: i64,
        inner: Box<Plan>,
    },
    /// Accept an aggregation bucket (integer `key`) that nets to zero in `tol`.
    AggNet {
        key: String,
        amount: String,
        tol: i64,
    },
    /// Pair opposite-sign rows with equal magnitude on `amount`.
    Exact { amount: String },
    /// Group rows that share an out-of-band token signal and net to zero.
    Signal {
        signals: String,
        amount: String,
        tol: i64,
        cap: usize,
    },
    /// The min-cost-flow arbiter over the residual.
    Flow {
        /// Numeraire column conserved by the network.
        amount: String,
        /// Date/order column (days) for proximity candidate generation.
        day: String,
        /// Native-amount column used for exact-amount candidates and cost.
        native: String,
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
    /// The two rows have equal, non-zero absolute native amount.
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

/// Errors from compiling or running a [`Plan`].
#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    UnknownColumn(String),
    SchemaArity { expected: usize, got: usize },
    /// The pipeline did not partition the input: some id was lost or assigned
    /// to more than one group. Should be impossible — a bug guard.
    ConservationViolated { input: usize, accounted: usize },
    /// A group id referenced by an interactive op does not exist.
    UnknownGroup(u64),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::UnknownColumn(c) => write!(f, "unknown column: {c}"),
            ApiError::SchemaArity { expected, got } => {
                write!(f, "row arity {got} != schema arity {expected}")
            }
            ApiError::ConservationViolated { input, accounted } => {
                write!(f, "conservation violated: {accounted} accounted of {input}")
            }
            ApiError::UnknownGroup(g) => write!(f, "unknown group id: {g}"),
        }
    }
}

impl std::error::Error for ApiError {}

// ---------------------------------------------------------------------------
// Flow model bound to columns
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct PlanModel {
    amount: usize,
    day: usize,
    native: usize,
    tokens: usize,
    penalty: f64,
    window: i64,
    cost: CostSpec,
}

impl Model for PlanModel {
    type Tx = Row;

    fn base_amount(&self, tx: &Row) -> i64 {
        tx.int(self.amount)
    }
    fn penalty(&self, _tx: &Row) -> f64 {
        self.penalty
    }
    fn block_key(&self, tx: &Row) -> i64 {
        tx.int(self.day)
    }
    fn window(&self) -> i64 {
        self.window
    }
    fn match_keys(&self, tx: &Row) -> Vec<u64> {
        // Reference tokens bridge the two books; the native amount lets exact
        // equal-magnitude rows become candidates even without a shared token.
        let mut keys = tx.tokens(self.tokens);
        let n = tx.int(self.native);
        if n != 0 {
            keys.push(n.unsigned_abs());
        }
        keys
    }
    fn cost(&self, a: &Row, b: &Row) -> Option<f64> {
        let token_shared = {
            let bt = b.tokens(self.tokens);
            a.tokens(self.tokens).iter().any(|t| bt.contains(t))
        };
        let amount_equal = {
            let (na, nb) = (a.int(self.native), b.int(self.native));
            na != 0 && na.abs() == nb.abs()
        };
        let dd = (a.int(self.day) - b.int(self.day)).abs();
        // First tier whose conditions hold and whose date gap is in range wins;
        // a pair matched by no tier is forbidden (no arc).
        for tier in &self.cost.tiers {
            let holds = tier.when.iter().all(|c| match c {
                Cond::TokenShared => token_shared,
                Cond::AmountEqual => amount_equal,
            });
            if !holds {
                continue;
            }
            if let Some(md) = tier.max_day
                && dd > md
            {
                continue; // out of range for this tier; try a looser one
            }
            return Some(tier.base + tier.day_slope * dd as f64);
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Compiler: Plan -> Strategy<Row>
// ---------------------------------------------------------------------------

fn compile(plan: &Plan, schema: &Schema) -> Result<Box<dyn Strategy<Row>>, ApiError> {
    Ok(match plan {
        Plan::Seq { steps } => {
            let mut compiled = Vec::with_capacity(steps.len());
            for s in steps {
                compiled.push(compile(s, schema)?);
            }
            seq(compiled)
        }
        Plan::Partition { by, inner } => {
            let k = schema.index(by)?;
            partition_by(move |r: &Row| r.int(k), compile(inner, schema)?)
        }
        Plan::Windowed {
            order,
            width,
            inner,
        } => {
            let o = schema.index(order)?;
            windowed(move |r: &Row| r.int(o), *width, compile(inner, schema)?)
        }
        Plan::AggNet { key, amount, tol } => {
            let (k, a) = (schema.index(key)?, schema.index(amount)?);
            agg_net(move |r: &Row| r.int(k) as u64, move |r: &Row| r.int(a), *tol)
        }
        Plan::Exact { amount } => {
            let a = schema.index(amount)?;
            exact_1to1(
                move |r: &Row| {
                    let v = r.int(a);
                    if v != 0 { Some(v.unsigned_abs()) } else { None }
                },
                move |r: &Row| r.int(a),
            )
        }
        Plan::Signal {
            signals,
            amount,
            tol,
            cap,
        } => {
            let (s, a) = (schema.index(signals)?, schema.index(amount)?);
            signal_group(
                move |r: &Row| r.tokens(s),
                move |r: &Row| r.int(a),
                *tol,
                *cap,
            )
        }
        Plan::Flow {
            amount,
            day,
            native,
            tokens,
            penalty,
            window,
            cost,
        } => flow(PlanModel {
            amount: schema.index(amount)?,
            day: schema.index(day)?,
            native: schema.index(native)?,
            tokens: schema.index(tokens)?,
            penalty: *penalty,
            window: *window,
            cost: cost.clone(),
        }),
    })
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

/// One reconciled group in a [`Report`].
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GroupOut {
    pub group_id: u64,
    pub origin: String,
    /// Residual in the numeraire; zero means it nets out.
    pub net: i64,
    pub size: usize,
}

/// The relational result of [`Session::solve`]. `assignments` ⊎ `residual`
/// partitions the input ids exactly (verified before this is returned).
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Report {
    /// `(id, group_id)`, one row per matched id.
    pub assignments: Vec<(ExtId, u64)>,
    pub groups: Vec<GroupOut>,
    /// Ids left unmatched.
    pub residual: Vec<ExtId>,
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// A long-lived reconciliation handle. Owns the rows natively; hosts cross the
/// boundary only with coarse deltas and plan submissions.
#[derive(Default)]
pub struct Session {
    schema: Schema,
    rows: BTreeMap<ExtId, Row>,
}

impl Session {
    pub fn new(schema: Schema) -> Self {
        Session {
            schema,
            rows: BTreeMap::new(),
        }
    }

    /// Build a session from a schema and a batch of rows (the batch boundary
    /// mode: the whole shard crosses once, e.g. from a WASM host).
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

    /// Insert or replace a row. One coarse boundary crossing per edit.
    pub fn upsert(&mut self, id: ExtId, row: Row) -> Result<(), ApiError> {
        if row.values.len() != self.schema.len() {
            return Err(ApiError::SchemaArity {
                expected: self.schema.len(),
                got: row.values.len(),
            });
        }
        self.rows.insert(id, row);
        Ok(())
    }

    /// Remove a row if present.
    pub fn remove(&mut self, id: ExtId) {
        self.rows.remove(&id);
    }

    /// Run a plan over the current rows and return the partitioned result.
    /// Verifies conservation before returning.
    pub fn solve(&self, plan: &Plan) -> Result<Report, ApiError> {
        let strategy = compile(plan, &self.schema)?;
        // Materialize in id order for deterministic candidate generation.
        let bag: Vec<Item<Row>> = self
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
        let mut group_out = Vec::with_capacity(groups.len());
        for (gid, g) in groups.into_iter().enumerate() {
            let gid = gid as u64;
            for &m in &g.members {
                assignments.push((m, gid));
            }
            group_out.push(GroupOut {
                group_id: gid,
                origin: g.origin.to_string(),
                net: g.net,
                size: g.members.len(),
            });
        }
        assignments.sort();

        let mut residual: Vec<ExtId> = res.residual.into_iter().map(|i| i.id).collect();
        residual.sort_unstable();

        // Conservation airlock: assigned ⊎ residual must equal the input ids,
        // with no id counted twice.
        let accounted = assignments.len() + residual.len();
        if accounted != input {
            return Err(ApiError::ConservationViolated { input, accounted });
        }

        Ok(Report {
            assignments,
            groups: group_out,
            residual,
        })
    }
}

// ---------------------------------------------------------------------------
// Batch request (the portable wire shape for a whole-shard solve)
// ---------------------------------------------------------------------------

/// A self-contained batch solve: schema + rows + plan. This is the JSON a WASM
/// or other batch host ships across the boundary in one coarse crossing.
#[cfg(feature = "serde")]
#[derive(Debug, Clone)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SolveRequest {
    pub schema: Schema,
    pub rows: Vec<(ExtId, Row)>,
    pub plan: Plan,
}

#[cfg(feature = "serde")]
impl SolveRequest {
    /// Build the session and run the plan, partition check included.
    pub fn run(self) -> Result<Report, ApiError> {
        let session = Session::from_rows(self.schema, self.rows)?;
        session.solve(&self.plan)
    }
}

// ---------------------------------------------------------------------------
// Workspace — the interactive, stateful surface
// ---------------------------------------------------------------------------

/// One group in a [`Workspace`], live or frozen. `group_id` is stable so a
/// front-end can reference it across operations.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct WsGroup {
    pub group_id: u64,
    pub origin: String,
    pub net: i64,
    pub size: usize,
    /// Frozen groups are locked: a re-solve never disturbs them.
    pub frozen: bool,
}

struct GroupRec {
    id: u64,
    members: Vec<ExtId>,
    origin: String,
    net: i64,
    frozen: bool,
}

/// The interactive result: stable group ids, frozen flags, and the residual.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct WorkspaceReport {
    pub assignments: Vec<(ExtId, u64)>,
    pub groups: Vec<WsGroup>,
    pub residual: Vec<ExtId>,
}

/// A long-lived, editable reconciliation workspace over items of type `E`,
/// driven by any [`Strategy`]. This is the one stateful facade; [`Workspace`]
/// is its `Row` + [`Plan`] specialization and a typed Rust caller can drive
/// `Recon<MyTx>` directly with a strategy built from the combinators.
///
/// It supports the interactive loop a UI drives: [`solve`](Recon::solve)
/// recomputes the unfrozen pool; [`freeze`](Recon::freeze) locks a group an
/// analyst trusts so re-solves leave it alone; [`breakup`](Recon::breakup)
/// dissolves a group back to the pool. The conservation invariant — every item
/// id is in exactly one group or in the residual — holds after every operation.
pub struct Recon<E> {
    strategy: Box<dyn Strategy<E>>,
    items: BTreeMap<ExtId, E>,
    groups: Vec<GroupRec>,
    residual: BTreeSet<ExtId>,
    next_id: u64,
}

impl<E: Clone> Recon<E> {
    /// Create an empty workspace driven by `strategy`.
    pub fn new(strategy: Box<dyn Strategy<E>>) -> Self {
        Recon {
            strategy,
            items: BTreeMap::new(),
            groups: Vec::new(),
            residual: BTreeSet::new(),
            next_id: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Insert or replace an item. A new id starts life in the residual; the
    /// caller re-solves to fold it into groups.
    pub fn upsert(&mut self, id: ExtId, item: E) {
        if self.items.insert(id, item).is_none() && !self.in_group(id) {
            self.residual.insert(id);
        }
    }

    /// Remove an item from the workspace and from wherever it currently sits.
    pub fn remove(&mut self, id: ExtId) {
        self.items.remove(&id);
        self.residual.remove(&id);
        for g in &mut self.groups {
            g.members.retain(|&m| m != id);
        }
        // A group reduced below two members can no longer net; dissolve it.
        let mut orphaned = Vec::new();
        self.groups.retain(|g| {
            if g.members.len() < 2 {
                orphaned.extend(g.members.iter().copied());
                false
            } else {
                true
            }
        });
        self.residual.extend(orphaned);
    }

    fn in_group(&self, id: ExtId) -> bool {
        self.groups.iter().any(|g| g.members.contains(&id))
    }

    /// Recompute the unfrozen pool: drop all live groups, run the strategy over
    /// every item not locked in a frozen group, and install fresh live groups.
    /// Frozen groups are left untouched.
    pub fn solve(&mut self) -> Result<(), ApiError> {
        let frozen_members: BTreeSet<ExtId> = self
            .groups
            .iter()
            .filter(|g| g.frozen)
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
        let pool = bag.len();
        let res = self.strategy.run(bag);

        self.groups.retain(|g| g.frozen);
        let mut new_groups = res.groups;
        new_groups.sort_by_key(|g| g.members.iter().copied().min().unwrap_or(0));
        let mut accounted = 0;
        for g in new_groups {
            accounted += g.members.len();
            self.groups.push(GroupRec {
                id: self.next_id,
                members: g.members,
                origin: g.origin.to_string(),
                net: g.net,
                frozen: false,
            });
            self.next_id += 1;
        }
        self.residual = res.residual.into_iter().map(|i| i.id).collect();
        accounted += self.residual.len();
        if accounted != pool {
            return Err(ApiError::ConservationViolated {
                input: pool,
                accounted,
            });
        }
        Ok(())
    }

    /// Lock a group so future solves leave it intact.
    pub fn freeze(&mut self, group_id: u64) -> Result<(), ApiError> {
        self.group_mut(group_id)?.frozen = true;
        Ok(())
    }

    /// Freeze every live group whose net is within `tol` (a clean group).
    /// Returns how many were newly frozen.
    pub fn freeze_clean(&mut self, tol: i64) -> usize {
        let mut n = 0;
        for g in &mut self.groups {
            if !g.frozen && g.net.abs() <= tol {
                g.frozen = true;
                n += 1;
            }
        }
        n
    }

    /// Unlock a frozen group; the next solve may reshape it.
    pub fn unfreeze(&mut self, group_id: u64) -> Result<(), ApiError> {
        self.group_mut(group_id)?.frozen = false;
        Ok(())
    }

    /// Dissolve a group (live or frozen); its members return to the residual
    /// until the next explicit solve.
    pub fn breakup(&mut self, group_id: u64) -> Result<(), ApiError> {
        let pos = self
            .groups
            .iter()
            .position(|g| g.id == group_id)
            .ok_or(ApiError::UnknownGroup(group_id))?;
        let g = self.groups.remove(pos);
        self.residual.extend(g.members);
        Ok(())
    }

    fn group_mut(&mut self, group_id: u64) -> Result<&mut GroupRec, ApiError> {
        self.groups
            .iter_mut()
            .find(|g| g.id == group_id)
            .ok_or(ApiError::UnknownGroup(group_id))
    }

    /// Snapshot the current state as a relational report.
    pub fn report(&self) -> WorkspaceReport {
        let mut assignments = Vec::new();
        let mut groups = Vec::with_capacity(self.groups.len());
        for g in &self.groups {
            for &m in &g.members {
                assignments.push((m, g.id));
            }
            groups.push(WsGroup {
                group_id: g.id,
                origin: g.origin.clone(),
                net: g.net,
                size: g.members.len(),
                frozen: g.frozen,
            });
        }
        assignments.sort();
        groups.sort_by_key(|g| g.group_id);
        WorkspaceReport {
            assignments,
            groups,
            residual: self.residual.iter().copied().collect(),
        }
    }
}

/// The interactive [`Plan`]-driven workspace over [`Row`]s: a [`Recon<Row>`]
/// plus its [`Schema`] (for arity validation). This is what the WASM `dispatch`
/// surface drives.
pub struct Workspace {
    schema: Schema,
    inner: Recon<Row>,
}

impl Workspace {
    /// Compile `plan` against `schema` and create an empty workspace. Fails if
    /// the plan references an unknown column.
    pub fn new(schema: Schema, plan: Plan) -> Result<Self, ApiError> {
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

    /// Insert or replace a row (arity-checked against the schema).
    pub fn upsert(&mut self, id: ExtId, row: Row) -> Result<(), ApiError> {
        if row.values.len() != self.schema.len() {
            return Err(ApiError::SchemaArity {
                expected: self.schema.len(),
                got: row.values.len(),
            });
        }
        self.inner.upsert(id, row);
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

    /// Freeze every live group whose net is within `tol`. Returns the count.
    pub fn freeze_clean(&mut self, tol: i64) -> usize {
        self.inner.freeze_clean(tol)
    }

    /// Unlock a frozen group; the next solve may reshape it.
    pub fn unfreeze(&mut self, group_id: u64) -> Result<(), ApiError> {
        self.inner.unfreeze(group_id)
    }

    /// Dissolve a group; its members return to the residual.
    pub fn breakup(&mut self, group_id: u64) -> Result<(), ApiError> {
        self.inner.breakup(group_id)
    }

    /// Snapshot the current state as a relational report.
    pub fn report(&self) -> WorkspaceReport {
        self.inner.report()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Schema {
        Schema::new(["usd", "day", "objsub", "native", "tokens"])
    }

    fn row(usd: i64, day: i64, objsub: i64, native: i64, tokens: &[u64]) -> Row {
        Row::new(vec![
            Value::Int(usd),
            Value::Int(day),
            Value::Int(objsub),
            Value::Int(native),
            Value::Tokens(tokens.to_vec()),
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
                    native: "native".into(),
                    tokens: "tokens".into(),
                    penalty: 1000.0,
                    window: 30,
                    cost: CostSpec::default(),
                },
            ],
        }
    }

    #[test]
    fn exact_pair_matches_and_conserves() {
        let mut s = Session::new(schema());
        s.upsert(1, row(100, 1, 0, 100, &[])).unwrap();
        s.upsert(2, row(-100, 2, 0, -100, &[])).unwrap();
        s.upsert(3, row(7, 3, 0, 7, &[])).unwrap(); // unmatched
        let rep = s.solve(&full_pipeline()).unwrap();
        assert_eq!(rep.assignments.len(), 2);
        assert_eq!(rep.residual, vec![3]);
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
        assert_eq!(rep.residual.len(), 0);
        assert_eq!(rep.groups.len(), 1);
        assert_eq!(rep.groups[0].origin, "signal_group");
    }

    #[test]
    fn agg_net_accepts_balanced_bucket() {
        let mut s = Session::new(schema());
        s.upsert(1, row(100, 1, 7, 0, &[])).unwrap();
        s.upsert(2, row(-100, 2, 7, 0, &[])).unwrap();
        let rep = s.solve(&full_pipeline()).unwrap();
        assert_eq!(rep.groups.len(), 1);
        assert_eq!(rep.groups[0].origin, "agg_net");
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
        assert_eq!(rep.groups.len(), 0);
        assert_eq!(rep.residual.len(), 2);
    }

    #[test]
    fn unknown_column_is_an_error() {
        let s = Session::new(schema());
        let plan = Plan::Exact {
            amount: "nope".into(),
        };
        assert_eq!(
            s.solve(&plan),
            Err(ApiError::UnknownColumn("nope".into()))
        );
    }

    #[test]
    fn arity_mismatch_rejected() {
        let mut s = Session::new(schema());
        let bad = Row::new(vec![Value::Int(1)]);
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
        let json = r#"{"op":"flow","amount":"usd","day":"day","native":"native","tokens":"tokens","penalty":1000.0,"window":30}"#;
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
                native: "native".into(),
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
        assert_eq!(rep.groups.len(), 0, "amount-only forbidden by this cost");
        assert_eq!(rep.residual.len(), 2);

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
        assert_eq!(rep.groups.len(), 1);
        assert_eq!(rep.residual, vec![3]);
        // freeze survives a re-solve; breakup returns members to the pool.
        let g = rep.groups[0].group_id;
        r.freeze(g).unwrap();
        r.upsert(4, 9);
        r.solve().unwrap();
        assert!(r.report().groups.iter().any(|x| x.group_id == g && x.frozen));
    }

    fn ws_conserves(ws: &Workspace) {
        let rep = ws.report();
        let n = rep.assignments.len() + rep.residual.len();
        assert_eq!(n, ws.len(), "every row is in exactly one group or residual");
    }

    #[test]
    fn workspace_solve_freeze_breakup() {
        let mut ws = Workspace::new(schema(), full_pipeline()).unwrap();
        ws.upsert(1, row(100, 1, 10, 100, &[])).unwrap();
        ws.upsert(2, row(-100, 2, 10, -100, &[])).unwrap();
        ws.upsert(3, row(50, 3, 20, 50, &[])).unwrap();
        ws.upsert(4, row(-50, 4, 20, -50, &[])).unwrap();
        // Before solving, everything is residual.
        assert_eq!(ws.report().groups.len(), 0);
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
        // g1's members are back in residual; g0 still grouped.
        let rep = ws.report();
        assert_eq!(rep.groups.len(), 1);
        assert!(rep.groups[0].frozen);
        assert_eq!(rep.residual.len(), 2);
        ws_conserves(&ws);

        // Re-solve: frozen group survives with its id; the pool reforms.
        ws.solve().unwrap();
        let rep = ws.report();
        assert!(rep.groups.iter().any(|g| g.group_id == g0 && g.frozen));
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
        // Removing one member dissolves the now-singleton group.
        ws.remove(2);
        let rep = ws.report();
        assert_eq!(rep.groups.len(), 0);
        assert_eq!(rep.residual, vec![1]);
        ws_conserves(&ws);
    }

    #[test]
    fn workspace_unknown_group_errors() {
        let mut ws = Workspace::new(schema(), full_pipeline()).unwrap();
        assert_eq!(ws.freeze(99), Err(ApiError::UnknownGroup(99)));
        assert_eq!(ws.breakup(99), Err(ApiError::UnknownGroup(99)));
    }
}
