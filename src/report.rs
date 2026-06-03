use crate::flow::ExtId;

/// The single recalc-status axis of a group. Only a human operator flips a
/// group between these in a workspace: `live` is the machine's current opinion
/// (subject to recalc), `frozen` is the operator's decision (inviolable).
/// Stateless batch solves return live proposals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Status {
    Live,
    Frozen,
}

/// One reconciled group in a [`Report`].
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GroupOut {
    pub group_id: u64,
    pub origin: String,
    /// Residual in the numeraire; zero means it nets out.
    pub net: i64,
    pub size: usize,
    /// The single recalc-status axis: `live` or `frozen`. (Matched vs unmatched
    /// is arity — `size` — not status.)
    pub status: Status,
}

/// The relational partition result: every input id appears in exactly one
/// group. There is no separate residual bucket; an unmatched row is a live
/// singleton group with origin `"unmatched"`.
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Report {
    /// `(id, group_id)`, one row per input id.
    pub assignments: Vec<(ExtId, u64)>,
    pub groups: Vec<GroupOut>,
}
