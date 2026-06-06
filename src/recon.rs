//! The stateful reconciliation facade: [`Recon`].
//!
//! [`Recon<E>`] is the long-lived, editable workspace a host drives around any
//! [`Strategy`](crate::strategy::Strategy). It is algebra-free: you hand it a
//! strategy and a primary-amount extractor, then `upsert` / `remove` / `solve`
//! / `freeze` / `breakup` / `group` and read back an allocation-hypergraph
//! [`Report`]. The plugin SDK builds on this directly; nothing here knows about
//! columns, schemas, or wire formats.
//!
//! Conservation is enforced at the boundary: a solve verifies that every input
//! id's allocations sum to its original amount, so a bad strategy degrades to a
//! bad proposal, never a broken ledger.

use crate::ExtId;
pub use crate::error::ApiError;
pub use crate::report::{AllocationOut, Component, GroupOut, ProjectionError, Report, Status};
pub use crate::strategy::Tol;

use crate::strategy::{Item, Strategy};
use std::collections::{BTreeMap, BTreeSet};

/// Allocation request used by allocation-native manual workspace operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AllocationSpec {
    pub id: ExtId,
    pub amount: i64,
}

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
    reason: Option<String>,
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

    /// Swap the compiled strategy and primary-amount extractor in place, keeping
    /// the rows, the groups (frozen decisions included), and the monotonic id
    /// allocator. The next [`solve`](Self::solve) recomputes the live pool under
    /// the new strategy; frozen groups are preserved verbatim with stable ids.
    /// Backs [`Workspace::replan`], which lets a caller iterate on a plan without
    /// re-ingesting rows or re-applying frozen decisions. The freshly compiled
    /// strategy starts cold (no warm flow state) — correct, since a changed plan
    /// invalidates the old basis anyway.
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
            reason: None,
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
            reason: None,
        });
        self.next_id += 1;
    }

    /// Insert or replace an item. A new id starts life as a live singleton
    /// group; the caller re-solves to fold it into matches.
    pub fn upsert(&mut self, id: ExtId, item: E) {
        // A new id (insert returned None) cannot already be in a group: every
        // grouped id is, by invariant, present in `items` (live singletons
        // early-return on `items.get`; frozen/match groups are built from items
        // and `remove` prunes them). So the old `&& !self.in_group(id)` guard
        // was always true here — and, since it scans every group, it made a
        // bulk init O(n^2) (each of n upserts scanning the growing singleton
        // pool). Dropping it keeps init O(n log n) with identical semantics.
        if self.items.insert(id, item).is_none() {
            self.push_live_singleton(id);
        }
    }

    /// Remove an item from the workspace and from its group. A match that loses
    /// a member dissolves; its survivor returns to a fresh live singleton.
    pub fn remove(&mut self, id: ExtId) {
        self.remove_many(&[id]);
    }

    /// Remove many items in a single pass over the groups. Removing ids one at a
    /// time is O(groups) per id (each `remove` scans every group), so a bulk
    /// delete of m ids over n groups is O(n*m) — quadratic when m ~ n. This does
    /// one `retain_mut` over the groups regardless of how many ids are dropped,
    /// with identical end-state semantics to looping `remove`.
    pub fn remove_many(&mut self, ids: &[ExtId]) {
        if ids.is_empty() {
            return;
        }
        let victims: BTreeSet<ExtId> = ids.iter().copied().collect();
        for id in &victims {
            self.items.remove(id);
        }
        let mut orphaned = Vec::new();
        self.groups.retain_mut(|g| {
            // Untouched groups pass through without rebuilding net.
            if !g.allocations.iter().any(|a| victims.contains(&a.id)) {
                return true;
            }
            g.allocations.retain(|a| !victims.contains(&a.id));
            g.net = g.allocations.iter().map(|a| a.amount).sum();
            if g.allocations.is_empty() {
                false
            } else if g.size() == 1 {
                // A match reduced to one allocation can no longer net; its
                // survivor returns to the live pool as a fresh singleton.
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

    #[allow(dead_code)]
    fn in_group(&self, id: ExtId) -> bool {
        self.groups.iter().any(|g| g.contains(id))
    }

    /// Recompute the live pool: dissolve every live group (singletons included)
    /// into a flat pool, run the strategy, and install fresh live groups plus a
    /// live singleton for each leftover. Frozen groups are kept verbatim with
    /// stable ids.
    pub fn solve(&mut self) -> Result<(), ApiError> {
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
                reason: g.reason,
            });
            self.next_id += 1;
        }
        // Every residual lot becomes its own live allocation group. Do not drop
        // a residual merely because the same row id was partly allocated above:
        // the report is a hypergraph, not a row partition.
        for item in res.residual {
            self.singleton_from_item(item);
        }
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
        conservation_airlock(&originals, &allocated)?;
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
    pub fn group(&mut self, ids: &[ExtId], net: i64, origin: &str, reason: Option<String>) -> Result<u64, ApiError> {
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
            reason,
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
            reason,
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
                reason: g.reason.clone(),
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
