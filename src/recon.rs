//! The stateful reconciliation facade: [`Recon`].
//!
//! [`Recon<E>`] is the long-lived, editable workspace a host drives around any
//! [`Strategy`](crate::strategy::Strategy). It is algebra-free: you hand it a
//! strategy and a primary-amount extractor, then drive the interactive loop and
//! read back an allocation-hypergraph [`Report`]. Nothing here knows about
//! columns, schemas, or wire formats.
//!
//! ## Two orthogonal axes
//!
//! Every group is one allocation hyperedge over input ids, and its behaviour is
//! fixed by two independent axes — never by a string:
//!
//! - lifecycle ([`Status`]): `Proposed` groups are the machine's current
//!   opinion, owned by [`solve`](Recon::solve) and recomputed wholesale each
//!   time. `Pinned` groups are a human's decision; `solve` keeps them verbatim.
//! - `Label`: pure provenance for display — a strategy label, a lone `Residual`
//!   lot, or a `Manual` decision. Control flow never branches on it.
//!
//! The interactive surface decomposes into four orthogonal families:
//!
//! - ledger: [`upsert`](Recon::upsert), [`remove`](Recon::remove);
//! - machine: [`solve`](Recon::solve);
//! - lifecycle: [`pin`](Recon::pin), [`pin_where`](Recon::pin_where),
//!   [`unpin`](Recon::unpin);
//! - partition: [`merge`](Recon::merge), [`detach`](Recon::detach),
//!   [`dissolve`](Recon::dissolve).
//!
//! Conservation is enforced at the boundary: a solve verifies that every input
//! id's allocations sum to its original amount, so a bad strategy degrades to a
//! bad proposal, never a broken ledger.

use crate::ExtId;
pub use crate::error::ApiError;
pub use crate::report::{AllocationOut, Component, GroupOut, ProjectionError, Report, Status};
use crate::strategy::{Allocation, Item, Strategy};

use std::collections::{BTreeMap, BTreeSet};

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
// Group state: two orthogonal axes
// ---------------------------------------------------------------------------

/// A group's provenance — for display only, never branched on.
#[derive(Clone)]
enum Label {
    /// A machine label from a strategy leaf (e.g. `"exact_1to1"`).
    Strategy(String),
    /// A lone, unplaced lot.
    Residual,
    /// A human decision's label.
    Manual(String),
}

impl Label {
    /// The wire `origin` string. `Residual` renders as `"unmatched"` so a
    /// client can present lone lots without inspecting `size`.
    fn origin(&self) -> &str {
        match self {
            Label::Strategy(s) | Label::Manual(s) => s,
            Label::Residual => "unmatched",
        }
    }
}

struct GroupRec {
    id: u64,
    /// The lifecycle axis — the *only* one [`Recon::solve`] reads. `Proposed`
    /// groups are rebuilt each solve; `Pinned` groups are kept verbatim.
    lifecycle: Status,
    label: Label,
    reason: Option<String>,
    allocations: Vec<Allocation>,
}

impl GroupRec {
    fn is_pinned(&self) -> bool {
        self.lifecycle == Status::Pinned
    }

    fn net(&self) -> i64 {
        self.allocations.iter().map(|a| a.amount).sum()
    }

    fn size(&self) -> usize {
        self.allocations.len()
    }

    fn contains(&self, id: ExtId) -> bool {
        self.allocations.iter().any(|a| a.id == id)
    }

    fn status(&self) -> Status {
        self.lifecycle
    }
}

/// An immutable view of one group, used by [`Recon::pin_where`] predicates and
/// [`Recon::groups`] inspection. Its metrics reuse the strategy layer's
/// vocabulary ([`clean`](GroupView::clean), [`size`](GroupView::size)).
pub struct GroupView<'a> {
    rec: &'a GroupRec,
}

impl GroupView<'_> {
    pub fn id(&self) -> u64 {
        self.rec.id
    }
    /// Residual net in the numeraire; zero means the group balances.
    pub fn net(&self) -> i64 {
        self.rec.net()
    }
    pub fn size(&self) -> usize {
        self.rec.size()
    }
    /// A lone, unplaced lot (one allocation).
    pub fn is_singleton(&self) -> bool {
        self.rec.size() == 1
    }
    /// A genuine match (two or more allocations).
    pub fn is_match(&self) -> bool {
        self.rec.size() >= 2
    }
    pub fn is_pinned(&self) -> bool {
        self.rec.is_pinned()
    }
    pub fn contains(&self, id: ExtId) -> bool {
        self.rec.contains(id)
    }
    pub fn contains_any(&self, ids: &[ExtId]) -> bool {
        ids.iter().any(|&id| self.rec.contains(id))
    }
    /// Whether the net balances within an absolute `tol` (minor units). The
    /// host-facing "clean match" pin predicate; the wire ABI passes a plain
    /// integer slack.
    pub fn clean(&self, tol: i64) -> bool {
        self.net().abs() <= tol
    }
    pub fn origin(&self) -> &str {
        self.rec.label.origin()
    }
    pub fn reason(&self) -> Option<&str> {
        self.rec.reason.as_deref()
    }
    pub fn allocations(&self) -> &[Allocation] {
        &self.rec.allocations
    }
}

// ---------------------------------------------------------------------------
// Workspace — the interactive, stateful surface
// ---------------------------------------------------------------------------

/// A long-lived, editable reconciliation workspace over items of type `E`,
/// driven by any [`Strategy`]. A typed Rust caller can drive `Recon<MyTx>`
/// directly; the plugin SDK builds on it unchanged.
///
/// [`solve`](Recon::solve) recomputes the proposed pool; [`pin`](Recon::pin)
/// locks a group an analyst trusts so re-solves leave its edges alone;
/// [`dissolve`](Recon::dissolve) breaks one back to singletons. The report is an
/// allocation hypergraph; row-level grouping is an explicit projection.
pub struct Recon<E> {
    strategy: Box<dyn Strategy<E>>,
    primary: Box<dyn Fn(&E) -> i64>,
    items: BTreeMap<ExtId, E>,
    groups: Vec<GroupRec>,
    /// Monotonic group-id allocator. **Never reset, never reused** — this is what
    /// makes proposed-group id ephemerality *safe*: each solve dissolves the
    /// proposed pool and re-mints its groups with brand-new ids, so a stale id
    /// held by a host across a solve can never silently land on a *different*
    /// group. It either still names the same pinned group (pinned ids are
    /// stable) or fails loudly as [`ApiError::UnknownGroup`].
    next_id: u64,
}

impl<E: Clone> Recon<E> {
    /// Create an empty workspace driven by `strategy`, with `primary` extracting
    /// the conserved numeraire from a row.
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
    /// the rows, the groups (pinned decisions included), and the monotonic id
    /// allocator. The next [`solve`](Self::solve) recomputes the proposed pool
    /// under the new strategy; pinned groups are preserved verbatim with stable
    /// ids. The next [`solve`](Self::solve) recomputes the proposed pool under
    /// the new strategy (strategies are stateless, so every solve recomputes
    /// from the rows); pinned groups are preserved verbatim with stable ids.
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

    // -- internal group construction ----------------------------------------

    /// Push a fresh proposed residual singleton holding `alloc`.
    fn push_singleton(&mut self, alloc: Allocation) {
        self.groups.push(GroupRec {
            id: self.next_id,
            lifecycle: Status::Proposed,
            label: Label::Residual,
            reason: None,
            allocations: vec![alloc],
        });
        self.next_id += 1;
    }

    /// Restore the proposed-layer shape after a partition edit: drop emptied
    /// proposed groups and relabel any that fell to a single allocation as
    /// `Residual`. Pinned groups and existing singletons keep their ids — no
    /// churn between solves.
    fn normalize_proposed(&mut self) {
        self.groups.retain_mut(|g| {
            if g.is_pinned() {
                return true;
            }
            match g.allocations.len() {
                0 => false,
                1 => {
                    g.label = Label::Residual;
                    true
                }
                _ => true,
            }
        });
    }

    // -- ledger -------------------------------------------------------------

    /// Insert or replace an item. A new id starts life as a proposed residual
    /// singleton; the caller re-solves to fold it into matches.
    pub fn upsert(&mut self, id: ExtId, item: E) {
        let is_new = !self.items.contains_key(&id);
        let amount = (self.primary)(&item);
        self.items.insert(id, item);
        if is_new {
            self.push_singleton(Allocation { id, amount });
        }
    }

    /// Remove items from the workspace in a single pass.
    ///
    /// Removing a row is the *one* operation allowed to touch a pinned group,
    /// because conservation forces it — a pinned group may not reference a
    /// deleted id. The rule is explicit and total: if `remove` would alter a
    /// pinned group, that whole group is **demoted** to proposed singletons (its
    /// inputs changed, so the decision is void). No half-pinned survivors, no
    /// silent net rewrites.
    pub fn remove(&mut self, ids: &[ExtId]) {
        if ids.is_empty() {
            return;
        }
        let victims: BTreeSet<ExtId> = ids.iter().copied().collect();
        for id in &victims {
            self.items.remove(id);
        }
        let mut scattered = Vec::new();
        self.groups.retain_mut(|g| {
            if !g.allocations.iter().any(|a| victims.contains(&a.id)) {
                return true; // untouched
            }
            if g.is_pinned() {
                // Demote the whole pinned group: its inputs changed.
                scattered.extend(
                    g.allocations
                        .iter()
                        .filter(|a| !victims.contains(&a.id))
                        .cloned(),
                );
                return false;
            }
            g.allocations.retain(|a| !victims.contains(&a.id));
            true // shape restored by normalize_proposed below
        });
        for a in scattered {
            self.push_singleton(a);
        }
        self.normalize_proposed();
    }

    // -- machine ------------------------------------------------------------

    /// Recompute the proposed pool: dissolve every proposed group into a flat
    /// bag (minus pinned mass), run the strategy, and install fresh proposed
    /// groups plus a residual singleton for each leftover. Pinned groups are
    /// kept verbatim with stable ids.
    pub fn solve(&mut self) -> Result<(), ApiError> {
        let mut pinned: BTreeMap<ExtId, i64> = BTreeMap::new();
        for g in self.groups.iter().filter(|g| g.is_pinned()) {
            for a in &g.allocations {
                *pinned.entry(a.id).or_insert(0) += a.amount;
            }
        }
        let bag: Vec<Item<E>> = self
            .items
            .iter()
            .filter_map(|(id, item)| {
                let original = (self.primary)(item);
                let rem = original - pinned.get(id).copied().unwrap_or(0);
                (rem != 0).then(|| Item {
                    id: *id,
                    original,
                    amount: rem,
                    data: item.clone(),
                })
            })
            .collect();
        let res = self.strategy.run(bag);

        // Dissolve the proposed layer; keep pinned groups verbatim.
        self.groups.retain(|g| g.is_pinned());
        let mut new_groups = res.groups;
        new_groups.sort_by_key(|g| g.members.iter().map(|a| a.id).min().unwrap_or(0));
        for g in new_groups {
            self.groups.push(GroupRec {
                id: self.next_id,
                lifecycle: Status::Proposed,
                label: Label::Strategy(g.origin),
                reason: g.reason,
                allocations: g.members,
            });
            self.next_id += 1;
        }
        // Every residual lot becomes its own proposed singleton. Do not drop a
        // residual merely because the same row id was partly allocated above:
        // the report is a hypergraph, not a row partition.
        for item in res.residual {
            self.push_singleton(Allocation {
                id: item.id,
                amount: item.amount,
            });
        }
        self.check_conservation()
    }

    fn check_conservation(&self) -> Result<(), ApiError> {
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
        conservation_airlock(&originals, &allocated)
    }

    // -- lifecycle ----------------------------------------------------------

    /// Pin one group so future solves keep it intact. Valid on singletons too:
    /// pinning a residual singleton records an accepted unmatched exception.
    pub fn pin(&mut self, group_id: u64) -> Result<(), ApiError> {
        self.group_mut(group_id)?.lifecycle = Status::Pinned;
        Ok(())
    }

    /// Pin every proposed group matching `pred`; returns how many were newly
    /// pinned. The one bulk-pin primitive — "pin clean matches", "pin these
    /// singletons", and anything else are just predicates:
    ///
    /// ```ignore
    /// recon.pin_where(|g| g.is_match() && g.clean(tol));        // clean matches
    /// recon.pin_where(|g| g.is_singleton() && g.contains_any(&ids)); // singletons
    /// ```
    pub fn pin_where(&mut self, pred: impl Fn(&GroupView<'_>) -> bool) -> usize {
        let mut n = 0;
        for i in 0..self.groups.len() {
            let g = &self.groups[i];
            if !g.is_pinned() && pred(&GroupView { rec: g }) {
                self.groups[i].lifecycle = Status::Pinned;
                n += 1;
            }
        }
        n
    }

    /// Release a pinned group; the next solve may reshape it.
    pub fn unpin(&mut self, group_id: u64) -> Result<(), ApiError> {
        self.group_mut(group_id)?.lifecycle = Status::Proposed;
        Ok(())
    }

    // -- partition ----------------------------------------------------------

    /// Assert a pinned group over exact allocation amounts, claimed from the
    /// proposed pool (splitting existing allocations if needed). Atomic:
    /// availability is validated for every id before anything is pulled, so a
    /// failure mutates nothing. Pinned groups are never disturbed — mass already
    /// pinned elsewhere is unavailable (an honest [`ApiError::InsufficientLiveAmount`],
    /// never a silent refusal or double count).
    pub fn merge(
        &mut self,
        specs: &[Allocation],
        label: &str,
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
        // Validate up front against the current pool (no mutation yet).
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
        // Commit: each take touches only its own id, so per-id availability
        // validated above still holds.
        let mut allocations = Vec::new();
        for (id, amount) in want {
            allocations.extend(self.take_live(id, amount));
        }
        self.normalize_proposed();
        let id = self.next_id;
        self.next_id += 1;
        self.groups.push(GroupRec {
            id,
            lifecycle: Status::Pinned,
            label: Label::Manual(label.to_string()),
            reason,
            allocations,
        });
        Ok(id)
    }

    /// Pull specific row allocations out of one proposed group, returning each
    /// to a residual singleton. Refuses pinned groups ([`unpin`](Recon::unpin)
    /// first).
    pub fn detach(&mut self, group_id: u64, ids: &[ExtId]) -> Result<(), ApiError> {
        let pos = self
            .groups
            .iter()
            .position(|g| g.id == group_id)
            .ok_or(ApiError::UnknownGroup(group_id))?;
        if self.groups[pos].is_pinned() {
            return Err(ApiError::FrozenGroup(group_id));
        }
        let want: BTreeSet<ExtId> = ids.iter().copied().collect();
        for &id in &want {
            if !self.groups[pos].contains(id) {
                return Err(ApiError::UnknownAllocation { group_id, id });
            }
        }
        let mut detached = Vec::new();
        self.groups[pos].allocations.retain(|a| {
            if want.contains(&a.id) {
                detached.push(*a);
                false
            } else {
                true
            }
        });
        for a in detached {
            self.push_singleton(a);
        }
        self.normalize_proposed();
        Ok(())
    }

    /// Dissolve a whole proposed group back into residual singletons. Refuses
    /// pinned groups ([`unpin`](Recon::unpin) first).
    pub fn dissolve(&mut self, group_id: u64) -> Result<(), ApiError> {
        let pos = self
            .groups
            .iter()
            .position(|g| g.id == group_id)
            .ok_or(ApiError::UnknownGroup(group_id))?;
        if self.groups[pos].is_pinned() {
            return Err(ApiError::FrozenGroup(group_id));
        }
        let g = self.groups.remove(pos);
        for a in g.allocations {
            self.push_singleton(a);
        }
        Ok(())
    }

    /// Total proposed (unpinned) mass for `id` on the given `sign`.
    fn live_available(&self, id: ExtId, sign: i64) -> i64 {
        self.groups
            .iter()
            .filter(|g| !g.is_pinned())
            .flat_map(|g| &g.allocations)
            .filter(|a| a.id == id && a.amount.signum() == sign)
            .map(|a| a.amount)
            .sum()
    }

    /// Pull up to `amount` (signed) of `id`'s proposed mass, splitting
    /// allocations as needed. Touches only `id`; never pinned groups. Assumes
    /// the caller validated availability (see [`merge`](Recon::merge)).
    fn take_live(&mut self, id: ExtId, amount: i64) -> Vec<Allocation> {
        let sign = amount.signum();
        let mut remaining = amount.abs();
        let mut pulled = Vec::new();
        for g in &mut self.groups {
            if g.is_pinned() || remaining == 0 {
                continue;
            }
            let mut keep = Vec::with_capacity(g.allocations.len());
            for mut a in g.allocations.drain(..) {
                if a.id == id && a.amount.signum() == sign && remaining > 0 {
                    let take = remaining.min(a.amount.abs());
                    remaining -= take;
                    pulled.push(Allocation {
                        id,
                        amount: sign * take,
                    });
                    a.amount -= sign * take;
                    if a.amount != 0 {
                        keep.push(a);
                    }
                } else {
                    keep.push(a);
                }
            }
            g.allocations = keep;
        }
        pulled
    }

    fn group_mut(&mut self, group_id: u64) -> Result<&mut GroupRec, ApiError> {
        self.groups
            .iter_mut()
            .find(|g| g.id == group_id)
            .ok_or(ApiError::UnknownGroup(group_id))
    }

    // -- read ---------------------------------------------------------------

    /// Inspect every group (for custom predicates or reporting).
    pub fn groups(&self) -> impl Iterator<Item = GroupView<'_>> {
        self.groups.iter().map(|g| GroupView { rec: g })
    }

    /// Snapshot the current allocation hypergraph as a sorted, deterministic
    /// [`Report`].
    pub fn report(&self) -> Report {
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
                origin: g.label.origin().to_string(),
                net: g.net(),
                size: g.size(),
                status: g.status(),
                reason: g.reason.clone(),
            });
        }
        allocations.sort_by_key(|a| (a.id, a.group_id));
        groups.sort_by_key(|g| g.group_id);
        Report {
            groups,
            allocations,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::exact_1to1;

    /// A workspace over bare `i64` rows: the value *is* the conserved amount,
    /// and the strategy pairs opposite-equal amounts (a clean 1-to-1).
    fn recon() -> Recon<i64> {
        Recon::new(exact_1to1(|_: &Item<i64>| Some(0)), |&a| a)
    }

    fn alloc(id: ExtId, amount: i64) -> Allocation {
        Allocation { id, amount }
    }

    fn group_of(r: &Recon<i64>, id: ExtId) -> u64 {
        r.groups().find(|g| g.contains(id)).unwrap().id()
    }

    #[test]
    fn solve_nets_a_clean_pair_and_conserves() {
        let mut r = recon();
        r.upsert(1, 100);
        r.upsert(2, -100);
        r.solve().unwrap();
        let rep = r.report();
        assert!(rep.groups.iter().any(|g| g.size == 2 && g.net == 0));
    }

    #[test]
    fn pinned_mass_is_excluded_from_solve() {
        let mut r = recon();
        r.upsert(1, 50);
        r.solve().unwrap(); // 50 alone -> residual singleton
        let gid = group_of(&r, 1);
        r.pin(gid).unwrap();
        r.upsert(2, -50);
        r.solve().unwrap();
        // Row 1 is pinned out of the bag, so it cannot net with row 2.
        assert!(
            r.groups()
                .any(|g| g.id() == gid && g.is_pinned() && g.contains(1))
        );
        assert!(r.groups().any(|g| g.contains(2) && !g.is_pinned()));
    }

    #[test]
    fn merge_is_atomic_and_pins() {
        let mut r = recon();
        r.upsert(1, 100);
        r.upsert(2, -100);
        let gid = r
            .merge(&[alloc(1, 100), alloc(2, -100)], "m", None)
            .unwrap();
        assert!(
            r.groups()
                .any(|g| g.id() == gid && g.is_pinned() && g.size() == 2)
        );
        // The mass is now pinned; a second claim finds nothing live.
        let err = r.merge(&[alloc(1, 100), alloc(2, -100)], "m", None);
        assert!(matches!(
            err,
            Err(ApiError::InsufficientLiveAmount { id: 1, .. })
        ));
    }

    #[test]
    fn detach_and_dissolve_refuse_pinned() {
        let mut r = recon();
        r.upsert(1, 100);
        r.upsert(2, -100);
        let gid = r
            .merge(&[alloc(1, 100), alloc(2, -100)], "m", None)
            .unwrap();
        assert!(matches!(r.dissolve(gid), Err(ApiError::FrozenGroup(_))));
        assert!(matches!(r.detach(gid, &[1]), Err(ApiError::FrozenGroup(_))));
        // Unpin first, then both succeed.
        r.unpin(gid).unwrap();
        r.detach(gid, &[1]).unwrap();
        assert!(r.groups().any(|g| g.contains(1) && g.is_singleton()));
    }

    #[test]
    fn remove_demotes_a_touched_pinned_group() {
        let mut r = recon();
        r.upsert(1, 100);
        r.upsert(2, -100);
        let gid = r
            .merge(&[alloc(1, 100), alloc(2, -100)], "m", None)
            .unwrap();
        r.remove(&[1]);
        // The pinned group is gone (its inputs changed); the survivor is a
        // proposed singleton, never a silently half-pinned group.
        assert!(r.groups().all(|g| g.id() != gid));
        assert!(r.groups().any(|g| g.contains(2) && !g.is_pinned()));
        assert!(r.groups().all(|g| !g.contains(1)));
    }

    #[test]
    fn pin_where_pins_clean_matches_only() {
        let mut r = recon();
        r.upsert(1, 100);
        r.upsert(2, -100);
        r.upsert(3, 50);
        r.solve().unwrap();
        let n = r.pin_where(|g| g.is_match() && g.clean(0));
        assert_eq!(n, 1); // the {1,2} net-zero match, not the lone 3
        assert!(
            r.groups()
                .any(|g| g.is_pinned() && g.contains(1) && g.contains(2))
        );
        assert!(r.groups().any(|g| g.contains(3) && !g.is_pinned()));
    }

    #[test]
    fn detached_singleton_keeps_its_id_no_churn() {
        let mut r = recon();
        r.upsert(1, 70);
        r.solve().unwrap();
        let before = group_of(&r, 1);
        // A no-op partition edit elsewhere must not renumber this singleton.
        r.upsert(2, 30);
        let after = group_of(&r, 1);
        assert_eq!(before, after, "untouched singleton id churned");
    }
}
