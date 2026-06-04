use crate::flow::ExtId;

/// Errors from compiling or running the public API.
#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    UnknownColumn(String),
    BadExpr(String),
    SchemaArity {
        expected: usize,
        got: usize,
    },
    /// Amount conservation failed: some input id's allocations do not sum to
    /// its original amount (a row was partly/fully lost, or split incorrectly).
    /// In the allocation-native (lot hypergraph) model this is the conserved
    /// invariant — row *presence* is not, since a zero-amount row legitimately
    /// produces no allocation. Should be impossible — a bug guard.
    ConservationViolated {
        id: ExtId,
        original: i64,
        accounted: i64,
    },
    /// A group id referenced by an interactive op does not exist.
    UnknownGroup(u64),
    /// A manual op referenced an id that is not in the workspace.
    UnknownId(ExtId),
    /// A manual op would disturb a frozen (signed-off) group; unfreeze first.
    FrozenMember(ExtId),
    /// A manual group needs at least two distinct members.
    DegenerateGroup,
    /// A requested allocation amount is not available in the live (unfrozen)
    /// pool for that row id.
    InsufficientLiveAmount {
        id: ExtId,
        requested: i64,
        available: i64,
    },
    /// A group does not contain a live allocation for the requested row id.
    UnknownAllocation {
        group_id: u64,
        id: ExtId,
    },
    /// A bare cell did not match its column kind (e.g. a string in a `Number`
    /// column). `col` is the column index; `want` the expected scalar.
    BadCell {
        col: usize,
        want: &'static str,
    },
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::UnknownColumn(c) => write!(f, "unknown column: {c}"),
            ApiError::BadExpr(e) => write!(f, "bad expression: {e}"),
            ApiError::SchemaArity { expected, got } => {
                write!(f, "row arity {got} != schema arity {expected}")
            }
            ApiError::ConservationViolated {
                id,
                original,
                accounted,
            } => write!(
                f,
                "conservation violated: row {id} allocations sum to {accounted}, expected {original}"
            ),
            ApiError::UnknownGroup(g) => write!(f, "unknown group id: {g}"),
            ApiError::UnknownId(id) => write!(f, "unknown row id: {id}"),
            ApiError::FrozenMember(id) => {
                write!(f, "row {id} is in a frozen group; unfreeze it first")
            }
            ApiError::DegenerateGroup => write!(f, "a manual group needs at least two rows"),
            ApiError::InsufficientLiveAmount {
                id,
                requested,
                available,
            } => write!(
                f,
                "row {id}: requested live amount {requested}, only {available} available"
            ),
            ApiError::UnknownAllocation { group_id, id } => {
                write!(f, "group {group_id} has no live allocation for row {id}")
            }
            ApiError::BadCell { col, want } => write!(f, "column {col}: expected a {want}"),
        }
    }
}

impl std::error::Error for ApiError {}
