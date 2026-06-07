use crate::ExtId;

/// Errors from compiling or running the public API.
#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
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
    /// A manual op would disturb a frozen (signed-off) member; unpin first.
    FrozenMember(ExtId),
    /// A structural edit targeted a pinned (signed-off) group; unpin first.
    FrozenGroup(u64),
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
    UnknownAllocation { group_id: u64, id: ExtId },
}

impl ApiError {
    /// A stable machine code for the wire error envelope.
    pub fn code(&self) -> &'static str {
        match self {
            ApiError::ConservationViolated { .. } => "conservation_violated",
            ApiError::UnknownGroup(_) => "unknown_group",
            ApiError::UnknownId(_) => "unknown_id",
            ApiError::FrozenMember(_) => "frozen_member",
            ApiError::FrozenGroup(_) => "frozen_group",
            ApiError::DegenerateGroup => "degenerate_group",
            ApiError::InsufficientLiveAmount { .. } => "insufficient_live_amount",
            ApiError::UnknownAllocation { .. } => "unknown_allocation",
        }
    }

    /// The row id this error is about, if any (lets a client highlight it).
    pub fn id(&self) -> Option<ExtId> {
        match *self {
            ApiError::ConservationViolated { id, .. }
            | ApiError::UnknownId(id)
            | ApiError::FrozenMember(id)
            | ApiError::InsufficientLiveAmount { id, .. }
            | ApiError::UnknownAllocation { id, .. } => Some(id),
            _ => None,
        }
    }

    /// The group id this error is about, if any.
    pub fn group_id(&self) -> Option<u64> {
        match *self {
            ApiError::UnknownGroup(g)
            | ApiError::FrozenGroup(g)
            | ApiError::UnknownAllocation { group_id: g, .. } => Some(g),
            _ => None,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
                write!(f, "row {id} is in a pinned group; unpin it first")
            }
            ApiError::FrozenGroup(g) => {
                write!(f, "group {g} is pinned; unpin it first")
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
        }
    }
}

impl std::error::Error for ApiError {}
