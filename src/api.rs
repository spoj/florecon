//! Layer 3 — the consumption API.
//!
//! This is the surface external hosts (a PyO3 wheel, a wasm-bindgen module, an
//! agent emitting config) drive. It turns the closure-based combinators of
//! [`crate::strategy`] into a **data-driven, serializable** pipeline so nothing
//! but plans and results ever cross a language boundary.
//!
//! Three pieces:
//! - [`Plan`] — the strategy tree *as data* (no host callbacks). Serializable,
//!   so an agent can author it and a native interpreter runs it.
//! - [`Session`] — a long-lived handle that owns the rows natively; hosts cross
//!   the boundary only with coarse deltas ([`Session::upsert`] /
//!   [`Session::remove`]) and a [`Session::solve`] call.
//! - [`Report`] — the relational result (`assignments` + `groups` + `residual`).
//!
//! Conservation is enforced at the boundary: [`Session::solve`] verifies that
//! every input id lands in exactly one group or in the residual, so a malformed
//! plan can never silently lose or double-count mass.

use crate::recon::{ExtId, Model};
use crate::strategy::{
    Item, Strategy, agg_net, exact_1to1, flow, partition_by, seq, signal_group, windowed,
};
use std::collections::BTreeMap;

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
#[derive(Debug, Clone)]
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
    },
}

/// Errors from compiling or running a [`Plan`].
#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    UnknownColumn(String),
    SchemaArity { expected: usize, got: usize },
    /// The pipeline did not partition the input: some id was lost or assigned
    /// to more than one group. Should be impossible — a bug guard.
    ConservationViolated { input: usize, accounted: usize },
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
        let shared = {
            let bt = b.tokens(self.tokens);
            a.tokens(self.tokens).iter().any(|t| bt.contains(t))
        };
        let amt_match = {
            let (na, nb) = (a.int(self.native), b.int(self.native));
            na != 0 && na.abs() == nb.abs()
        };
        let dd = (a.int(self.day) - b.int(self.day)).abs() as f64 * 0.001;
        // Confidence tiers: a shared reference token is the strongest signal,
        // exact native amount next; otherwise forbid the pair.
        if shared {
            Some(0.5 + dd)
        } else if amt_match {
            Some(1.0 + dd)
        } else {
            None
        }
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
        } => flow(PlanModel {
            amount: schema.index(amount)?,
            day: schema.index(day)?,
            native: schema.index(native)?,
            tokens: schema.index(tokens)?,
            penalty: *penalty,
            window: *window,
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
}
