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
    /// The pipeline did not partition the input: some id was lost or assigned
    /// to more than one group. Should be impossible — a bug guard.
    ConservationViolated {
        input: usize,
        accounted: usize,
    },
    /// A group id referenced by an interactive op does not exist.
    UnknownGroup(u64),
    /// A manual op referenced an id that is not in the workspace.
    UnknownId(ExtId),
    /// A manual op would disturb a frozen (signed-off) group; unfreeze first.
    FrozenMember(ExtId),
    /// A manual group needs at least two distinct members.
    DegenerateGroup,
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
            ApiError::ConservationViolated { input, accounted } => {
                write!(f, "conservation violated: {accounted} accounted of {input}")
            }
            ApiError::UnknownGroup(g) => write!(f, "unknown group id: {g}"),
            ApiError::UnknownId(id) => write!(f, "unknown row id: {id}"),
            ApiError::FrozenMember(id) => {
                write!(f, "row {id} is in a frozen group; unfreeze it first")
            }
            ApiError::DegenerateGroup => write!(f, "a manual group needs at least two rows"),
            ApiError::BadCell { col, want } => write!(f, "column {col}: expected a {want}"),
        }
    }
}

impl std::error::Error for ApiError {}
