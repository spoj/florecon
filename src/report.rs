use crate::flow::ExtId;
use std::collections::{BTreeMap, BTreeSet, HashMap};

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
    /// Number of allocation rows in this group. For whole-row strategies this is
    /// also the number of business rows; for lot strategies a row id may appear
    /// in multiple groups across the report.
    pub size: usize,
    /// The single recalc-status axis: `live` or `frozen`. (Matched vs unmatched
    /// is a client projection over the allocation hypergraph, not core state.)
    pub status: Status,
}

/// One signed lot allocation in a [`Report`]. This is the incidence edge of the
/// allocation hypergraph: row/lot id `id` contributes signed `amount` to
/// `group_id`. The same id may appear more than once when a row is split across
/// a matched group and a residual/variance group.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AllocationOut {
    pub id: ExtId,
    pub group_id: u64,
    pub amount: i64,
}

/// The allocation-native reconciliation result. This is the single source of
/// truth that crosses the Rust/WASM boundary: `allocations` are hypergraph
/// incidences and `groups` are their metadata. Row-level groupings are
/// projections and should be chosen explicitly by clients (see helper methods
/// below, or roll your own projection from `allocations`).
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Report {
    pub groups: Vec<GroupOut>,
    pub allocations: Vec<AllocationOut>,
}

/// A row/group connected component of the allocation hypergraph. A simple
/// whole-row report has one group id per component; split lots connect multiple
/// group ids through one or more row ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    pub rows: Vec<ExtId>,
    pub groups: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionError {
    /// A row id participates in more than one group, so a strict one-row-one-
    /// group assignment projection would lose information.
    SplitRow { id: ExtId, groups: Vec<u64> },
}

impl std::fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProjectionError::SplitRow { id, groups } => {
                write!(f, "row {id} is split across groups {groups:?}")
            }
        }
    }
}

impl std::error::Error for ProjectionError {}

impl Report {
    /// Project to legacy strict assignments only when every row id appears in
    /// exactly one group. This refuses split lots rather than picking a lossy
    /// primary group.
    pub fn strict_assignments(&self) -> Result<Vec<(ExtId, u64)>, ProjectionError> {
        let mut by_id: BTreeMap<ExtId, BTreeSet<u64>> = BTreeMap::new();
        for a in &self.allocations {
            by_id.entry(a.id).or_default().insert(a.group_id);
        }
        let mut out = Vec::with_capacity(by_id.len());
        for (id, groups) in by_id {
            if groups.len() != 1 {
                return Err(ProjectionError::SplitRow {
                    id,
                    groups: groups.into_iter().collect(),
                });
            }
            out.push((id, *groups.iter().next().unwrap()));
        }
        Ok(out)
    }

    /// Connected components of the bipartite graph `row id <-> group id`.
    /// This is the safest generic row-level projection for split lot reports:
    /// it represents entanglement without choosing a fake primary group.
    pub fn connected_components(&self) -> Vec<Component> {
        let mut row_to_groups: HashMap<ExtId, Vec<u64>> = HashMap::new();
        let mut group_to_rows: HashMap<u64, Vec<ExtId>> = HashMap::new();
        for a in &self.allocations {
            row_to_groups.entry(a.id).or_default().push(a.group_id);
            group_to_rows.entry(a.group_id).or_default().push(a.id);
        }

        let mut seen_rows = BTreeSet::new();
        let mut seen_groups = BTreeSet::new();
        let mut out = Vec::new();
        for &start in row_to_groups.keys() {
            if seen_rows.contains(&start) {
                continue;
            }
            let mut rows = BTreeSet::new();
            let mut groups = BTreeSet::new();
            let mut row_stack = vec![start];
            let mut group_stack = Vec::new();
            while !row_stack.is_empty() || !group_stack.is_empty() {
                while let Some(r) = row_stack.pop() {
                    if !seen_rows.insert(r) {
                        continue;
                    }
                    rows.insert(r);
                    if let Some(gs) = row_to_groups.get(&r) {
                        for &g in gs {
                            group_stack.push(g);
                        }
                    }
                }
                while let Some(g) = group_stack.pop() {
                    if !seen_groups.insert(g) {
                        continue;
                    }
                    groups.insert(g);
                    if let Some(rs) = group_to_rows.get(&g) {
                        for &r in rs {
                            row_stack.push(r);
                        }
                    }
                }
            }
            out.push(Component {
                rows: rows.into_iter().collect(),
                groups: groups.into_iter().collect(),
            });
        }
        out.sort_by_key(|c| {
            (
                c.rows.first().copied().unwrap_or(0),
                c.groups.first().copied().unwrap_or(0),
            )
        });
        out
    }
}
