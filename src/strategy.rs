//! The strategy algebra: parse a bag of entries into a partition of groups.
//!
//! Reconciliation here is **partitioning by identity**, nothing more. The input
//! is a bag of [`Entry`]s (a caller-owned [`Id`] plus an opaque payload `E`); a
//! [`Strategy`] pulls some entries into [`Group`]s and leaves the rest as
//! residual. Compose with [`seq`], shard with [`partition_by`], gate with
//! [`accept_if`]. The output is just a list of groups.
//!
//! There is **no privileged numeraire**. Every number a leaf needs (an amount to
//! net, a value to search, a flow supply) is a closure `Fn(&Entry<E>) -> i64`
//! over the payload — so multi-currency is just different closures, and there is
//! no `pivot`, no `original`, no fractional allocation. A group owns whole
//! entries (by id); the framework conserves **identity**: every input id lands
//! in exactly one group or in residual, nothing lost, nothing invented.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Deref;

pub mod flow;
pub use flow::{FlowSpec, flow};

/// Caller-owned, stable identity for one entry/row/lot.
pub type Id = u64;

/// One entry in the bag: a stable [`Id`] and an opaque payload. Derefs to the
/// payload, so a closure `|e| e.amount` reaches a field of `E` while `e.id`
/// still names the identity.
#[derive(Clone, Debug)]
pub struct Entry<E> {
    pub id: Id,
    pub data: E,
}

impl<E> Entry<E> {
    pub fn new(id: Id, data: E) -> Self {
        Entry { id, data }
    }
}

impl<E> Deref for Entry<E> {
    type Target = E;
    fn deref(&self) -> &E {
        &self.data
    }
}

/// A resolved group: a set of whole member entries (by id) plus provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub members: Vec<Id>,
    /// Which primitive produced it (the machine tag).
    pub origin: String,
    /// Optional human-facing explanation, stamped by [`labeled`].
    pub reason: Option<String>,
}

impl Group {
    fn new(members: Vec<Id>, origin: impl Into<String>) -> Self {
        Group {
            members,
            origin: origin.into(),
            reason: None,
        }
    }

    pub fn size(&self) -> usize {
        self.members.len()
    }

    /// A content-addressed identity, stable across solves: a function of the
    /// sorted member id set, so the *same* grouping always yields the *same*
    /// key regardless of how it was discovered.
    pub fn key(&self) -> u64 {
        let mut ids = self.members.clone();
        ids.sort_unstable();
        let mut h = 0xcbf29ce484222325u64;
        for id in ids {
            for b in id.to_le_bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
        }
        h
    }
}

/// What a strategy returns: the groups it pulled and the residual it left.
/// `groups`' member ids and `residual`'s ids together partition the input ids.
#[derive(Debug)]
pub struct Resolution<E> {
    pub groups: Vec<Group>,
    pub residual: Vec<Entry<E>>,
}

/// A reconciliation strategy: a pure function from a bag to a partition.
pub trait Strategy<E> {
    fn run(&self, bag: Vec<Entry<E>>) -> Resolution<E>;
}

impl<E> Strategy<E> for Box<dyn Strategy<E>> {
    fn run(&self, bag: Vec<Entry<E>>) -> Resolution<E> {
        (**self).run(bag)
    }
}

// ---------------------------------------------------------------------------
// GroupView — what an acceptance closure sees
// ---------------------------------------------------------------------------

/// A borrowed, read-only view of a candidate group, shown to an acceptance
/// closure. It lends back the member entries so the closure can compute
/// whatever it likes; numeric tests pass the lane they care about, e.g.
/// `|g| g.net(|e| e.amount).abs() <= 5`. There is no tolerance type — the author
/// writes the inequality inline and picks the lane by closure.
pub struct GroupView<'a, E> {
    members: Vec<&'a Entry<E>>,
}

impl<'a, E> GroupView<'a, E> {
    fn new(members: Vec<&'a Entry<E>>) -> Self {
        GroupView { members }
    }

    /// The member entries.
    pub fn members(&self) -> impl Iterator<Item = &'a Entry<E>> + '_ {
        self.members.iter().copied()
    }

    /// Number of members.
    pub fn size(&self) -> usize {
        self.members.len()
    }

    /// Signed sum of the chosen lane — the residual the group would commit.
    pub fn net(&self, amount: impl Fn(&Entry<E>) -> i64) -> i64 {
        self.members.iter().map(|e| amount(e)).sum()
    }

    /// Sum of lane magnitudes, `Σ|lane|` — the matched/moved volume.
    pub fn gross(&self, amount: impl Fn(&Entry<E>) -> i64) -> i64 {
        self.members.iter().map(|e| amount(e).abs()).sum()
    }

    /// Largest lane magnitude; `0` if empty.
    pub fn max_leg(&self, amount: impl Fn(&Entry<E>) -> i64) -> i64 {
        self.members.iter().map(|e| amount(e).abs()).max().unwrap_or(0)
    }

    /// Smallest non-zero lane magnitude; `0` if every leg is zero.
    pub fn min_leg(&self, amount: impl Fn(&Entry<E>) -> i64) -> i64 {
        self.members
            .iter()
            .map(|e| amount(e).abs())
            .filter(|&v| v > 0)
            .min()
            .unwrap_or(0)
    }

    /// The minority-side count `min(#positive, #negative)` on the chosen lane.
    /// A clean 1:1 pair is `1`; an all-one-sign wash is `0`.
    pub fn min_side(&self, amount: impl Fn(&Entry<E>) -> i64) -> usize {
        let pos = self.members.iter().filter(|e| amount(e) > 0).count();
        let neg = self.members.iter().filter(|e| amount(e) < 0).count();
        pos.min(neg)
    }
}

// ---------------------------------------------------------------------------
// Helpers shared by leaves: split a consumed bag into groups + residual.
// ---------------------------------------------------------------------------

/// Given the matched id-clusters computed from `&bag`, build the groups and
/// move every un-clustered entry into residual.
fn split<E>(bag: Vec<Entry<E>>, matched: Vec<Vec<Id>>, origin: &str) -> Resolution<E> {
    let in_group: HashSet<Id> = matched.iter().flatten().copied().collect();
    let groups = matched
        .into_iter()
        .map(|ids| Group::new(ids, origin))
        .collect();
    let residual = bag.into_iter().filter(|e| !in_group.contains(&e.id)).collect();
    Resolution { groups, residual }
}

// ---------------------------------------------------------------------------
// Combinators
// ---------------------------------------------------------------------------

struct Seq<E>(Vec<Box<dyn Strategy<E>>>);

impl<E> Strategy<E> for Seq<E> {
    fn run(&self, bag: Vec<Entry<E>>) -> Resolution<E> {
        let mut groups = Vec::new();
        let mut residual = bag;
        for step in &self.0 {
            let r = step.run(residual);
            groups.extend(r.groups);
            residual = r.residual;
        }
        Resolution { groups, residual }
    }
}

/// Cascade: each step runs on the prior step's residual.
pub fn seq<E: 'static>(steps: Vec<Box<dyn Strategy<E>>>) -> Box<dyn Strategy<E>> {
    Box::new(Seq(steps))
}

struct Identity;
impl<E> Strategy<E> for Identity {
    fn run(&self, bag: Vec<Entry<E>>) -> Resolution<E> {
        Resolution {
            groups: Vec::new(),
            residual: bag,
        }
    }
}

/// The do-nothing strategy: everything passes through as residual.
pub fn identity<E: 'static>() -> Box<dyn Strategy<E>> {
    Box::new(Identity)
}

struct Labeled<E> {
    tag: String,
    inner: Box<dyn Strategy<E>>,
}
impl<E> Strategy<E> for Labeled<E> {
    fn run(&self, bag: Vec<Entry<E>>) -> Resolution<E> {
        let mut r = self.inner.run(bag);
        for g in &mut r.groups {
            g.reason = Some(self.tag.clone());
        }
        r
    }
}

/// Stamp a human `reason` on every group a subtree forms.
pub fn labeled<E: 'static>(tag: impl Into<String>, inner: Box<dyn Strategy<E>>) -> Box<dyn Strategy<E>> {
    Box::new(Labeled {
        tag: tag.into(),
        inner,
    })
}

struct When<E, FP> {
    pred: FP,
    inner: Box<dyn Strategy<E>>,
}
impl<E, FP: Fn(&Entry<E>) -> bool> Strategy<E> for When<E, FP> {
    fn run(&self, bag: Vec<Entry<E>>) -> Resolution<E> {
        let (matching, mut rest): (Vec<_>, Vec<_>) = bag.into_iter().partition(|e| (self.pred)(e));
        let r = self.inner.run(matching);
        rest.extend(r.residual);
        Resolution {
            groups: r.groups,
            residual: rest,
        }
    }
}

/// Route entries satisfying `pred` into `inner`; everything else (and `inner`'s
/// own residual) passes through.
pub fn when<E: 'static, FP>(pred: FP, inner: Box<dyn Strategy<E>>) -> Box<dyn Strategy<E>>
where
    FP: Fn(&Entry<E>) -> bool + 'static,
{
    Box::new(When { pred, inner })
}

struct PartitionBy<E, K, FK, FF> {
    key: FK,
    factory: FF,
    _k: std::marker::PhantomData<(K, fn() -> E)>,
}
impl<E, K, FK, FF> Strategy<E> for PartitionBy<E, K, FK, FF>
where
    K: Ord + Clone,
    FK: Fn(&Entry<E>) -> K,
    FF: Fn(&K) -> Box<dyn Strategy<E>>,
{
    fn run(&self, bag: Vec<Entry<E>>) -> Resolution<E> {
        let mut shards: BTreeMap<K, Vec<Entry<E>>> = BTreeMap::new();
        for e in bag {
            shards.entry((self.key)(&e)).or_default().push(e);
        }
        let mut groups = Vec::new();
        let mut residual = Vec::new();
        for (k, ents) in shards {
            let r = (self.factory)(&k).run(ents);
            groups.extend(r.groups);
            residual.extend(r.residual);
        }
        Resolution { groups, residual }
    }
}

/// Shard the bag by `key` (over the whole entry) and run a per-shard subtree
/// built by `factory`. The key-ignoring case is `|_| inner()`.
pub fn partition_by<E: 'static, K, FK, FF>(key: FK, factory: FF) -> Box<dyn Strategy<E>>
where
    K: Ord + Clone + 'static,
    FK: Fn(&Entry<E>) -> K + 'static,
    FF: Fn(&K) -> Box<dyn Strategy<E>> + 'static,
{
    Box::new(PartitionBy {
        key,
        factory,
        _k: std::marker::PhantomData,
    })
}

struct Windowed<E, FO> {
    order: FO,
    width: i64,
    inner: Box<dyn Strategy<E>>,
}
impl<E, FO: Fn(&Entry<E>) -> i64> Strategy<E> for Windowed<E, FO> {
    fn run(&self, bag: Vec<Entry<E>>) -> Resolution<E> {
        // Tile the order axis into non-overlapping windows of `width`; run
        // `inner` independently per window (locality for the cheap leaves).
        let w = self.width.max(1);
        let mut tiles: BTreeMap<i64, Vec<Entry<E>>> = BTreeMap::new();
        for e in bag {
            let t = (self.order)(&e).div_euclid(w);
            tiles.entry(t).or_default().push(e);
        }
        let mut groups = Vec::new();
        let mut residual = Vec::new();
        for (_t, ents) in tiles {
            let r = self.inner.run(ents);
            groups.extend(r.groups);
            residual.extend(r.residual);
        }
        Resolution { groups, residual }
    }
}

/// Restrict matching to non-overlapping windows of `width` over an `order`.
/// Matches do not span window boundaries — widen `width` (or use `flow`'s own
/// window) for cross-boundary reach.
pub fn windowed<E: 'static, FO>(order: FO, width: i64, inner: Box<dyn Strategy<E>>) -> Box<dyn Strategy<E>>
where
    FO: Fn(&Entry<E>) -> i64 + 'static,
{
    Box::new(Windowed { order, width, inner })
}

struct FixedPoint<E> {
    inner: Box<dyn Strategy<E>>,
    max_passes: usize,
}
fn id_fingerprint<E>(items: &[Entry<E>]) -> Vec<Id> {
    let mut v: Vec<Id> = items.iter().map(|e| e.id).collect();
    v.sort_unstable();
    v
}
impl<E> Strategy<E> for FixedPoint<E> {
    fn run(&self, bag: Vec<Entry<E>>) -> Resolution<E> {
        let mut groups = Vec::new();
        let mut residual = bag;
        let mut fp = id_fingerprint(&residual);
        for _ in 0..self.max_passes {
            if residual.is_empty() {
                break;
            }
            let r = self.inner.run(residual);
            groups.extend(r.groups);
            residual = r.residual;
            let now = id_fingerprint(&residual);
            if now == fp {
                break;
            }
            fp = now;
        }
        Resolution { groups, residual }
    }
}

/// Iterate `inner` on its own residual until the residual stops changing (or
/// `max_passes` elapse).
pub fn fixed_point<E: 'static>(inner: Box<dyn Strategy<E>>, max_passes: usize) -> Box<dyn Strategy<E>> {
    Box::new(FixedPoint { inner, max_passes })
}

struct AcceptIf<E, FP> {
    pred: FP,
    inner: Box<dyn Strategy<E>>,
}
impl<E: Clone, FP: Fn(&GroupView<E>) -> bool> Strategy<E> for AcceptIf<E, FP> {
    fn run(&self, bag: Vec<Entry<E>>) -> Resolution<E> {
        let src: HashMap<Id, Entry<E>> = bag.iter().map(|e| (e.id, e.clone())).collect();
        let r = self.inner.run(bag);
        let mut groups = Vec::new();
        let mut residual = r.residual;
        for g in r.groups {
            let view = GroupView::new(g.members.iter().filter_map(|id| src.get(id)).collect());
            if (self.pred)(&view) {
                groups.push(g);
            } else {
                for id in g.members {
                    if let Some(e) = src.get(&id) {
                        residual.push(e.clone());
                    }
                }
            }
        }
        Resolution { groups, residual }
    }
}

/// Gate an inner strategy's groups by a closure over a [`GroupView`]; rejected
/// groups dissolve back to residual (whole). This is the one acceptance concept.
pub fn accept_if<E: Clone + 'static, FP>(pred: FP, inner: Box<dyn Strategy<E>>) -> Box<dyn Strategy<E>>
where
    FP: Fn(&GroupView<E>) -> bool + 'static,
{
    Box::new(AcceptIf { pred, inner })
}

struct Soak {
    origin: String,
}
impl<E> Strategy<E> for Soak {
    fn run(&self, bag: Vec<Entry<E>>) -> Resolution<E> {
        if bag.is_empty() {
            return Resolution {
                groups: Vec::new(),
                residual: Vec::new(),
            };
        }
        let members = bag.iter().map(|e| e.id).collect();
        Resolution {
            groups: vec![Group::new(members, self.origin.clone())],
            residual: Vec::new(),
        }
    }
}

/// Consume every entry it receives into one group. Shape its cardinality with
/// [`partition_by`] (`partition_by(|e| e.id, |_| soak(o))` = singletons) and
/// filter it with [`when`].
pub fn soak<E: 'static>(origin: impl Into<String>) -> Box<dyn Strategy<E>> {
    Box::new(Soak { origin: origin.into() })
}

// ---------------------------------------------------------------------------
// Leaves (matchers)
// ---------------------------------------------------------------------------

struct Exact1to1<E, FK, FA> {
    key: FK,
    amount: FA,
    _e: std::marker::PhantomData<fn() -> E>,
}
impl<E, FK, FA> Strategy<E> for Exact1to1<E, FK, FA>
where
    FK: Fn(&Entry<E>) -> Option<u64>,
    FA: Fn(&Entry<E>) -> i64,
{
    fn run(&self, bag: Vec<Entry<E>>) -> Resolution<E> {
        // Bucket by key, then within each bucket pair an opposite-sign entry of
        // equal magnitude. Deterministic: stable by id.
        let mut by_key: BTreeMap<u64, Vec<&Entry<E>>> = BTreeMap::new();
        for e in &bag {
            if let Some(k) = (self.key)(e) {
                by_key.entry(k).or_default().push(e);
            }
        }
        let mut matched: Vec<Vec<Id>> = Vec::new();
        for (_k, ents) in by_key {
            // magnitude -> (positive ids, negative ids), each id-sorted.
            let mut pos: BTreeMap<i64, Vec<Id>> = BTreeMap::new();
            let mut neg: BTreeMap<i64, Vec<Id>> = BTreeMap::new();
            let mut sorted = ents;
            sorted.sort_by_key(|e| e.id);
            for e in sorted {
                let a = (self.amount)(e);
                if a > 0 {
                    pos.entry(a).or_default().push(e.id);
                } else if a < 0 {
                    neg.entry(-a).or_default().push(e.id);
                }
            }
            for (mag, p) in pos {
                if let Some(n) = neg.get(&mag) {
                    let k = p.len().min(n.len());
                    for i in 0..k {
                        matched.push(vec![p[i], n[i]]);
                    }
                }
            }
        }
        split(bag, matched, "exact_1to1")
    }
}

/// Pair an entry with an equal-and-opposite entry (on the `amount` lane) that
/// shares a `key`. `key` returns `None` to opt an entry out.
pub fn exact_1to1<E: 'static, FK, FA>(key: FK, amount: FA) -> Box<dyn Strategy<E>>
where
    FK: Fn(&Entry<E>) -> Option<u64> + 'static,
    FA: Fn(&Entry<E>) -> i64 + 'static,
{
    Box::new(Exact1to1 {
        key,
        amount,
        _e: std::marker::PhantomData,
    })
}

struct AggNet<E, FK, FP> {
    key: FK,
    accept: FP,
    _e: std::marker::PhantomData<fn() -> E>,
}
impl<E, FK, FP> Strategy<E> for AggNet<E, FK, FP>
where
    FK: Fn(&Entry<E>) -> u64,
    FP: Fn(&GroupView<E>) -> bool,
{
    fn run(&self, bag: Vec<Entry<E>>) -> Resolution<E> {
        let mut by_key: BTreeMap<u64, Vec<&Entry<E>>> = BTreeMap::new();
        for e in &bag {
            by_key.entry((self.key)(e)).or_default().push(e);
        }
        let mut matched: Vec<Vec<Id>> = Vec::new();
        for (_k, ents) in by_key {
            let view = GroupView::new(ents.clone());
            if (self.accept)(&view) {
                matched.push(ents.iter().map(|e| e.id).collect());
            }
        }
        split(bag, matched, "agg_net")
    }
}

/// Bucket entries by `key`; keep each bucket the `accept` closure approves
/// (typically "nets to ~0 on some lane"). Rejected buckets dissolve to residual.
pub fn agg_net<E: 'static, FK, FP>(key: FK, accept: FP) -> Box<dyn Strategy<E>>
where
    FK: Fn(&Entry<E>) -> u64 + 'static,
    FP: Fn(&GroupView<E>) -> bool + 'static,
{
    Box::new(AggNet {
        key,
        accept,
        _e: std::marker::PhantomData,
    })
}

struct SignalGroup<E, FS, FP> {
    signals: FS,
    accept: FP,
    cap: usize,
    _e: std::marker::PhantomData<fn() -> E>,
}
impl<E, FS, FP> Strategy<E> for SignalGroup<E, FS, FP>
where
    FS: Fn(&Entry<E>) -> Vec<u64>,
    FP: Fn(&GroupView<E>) -> bool,
{
    fn run(&self, bag: Vec<Entry<E>>) -> Resolution<E> {
        // token -> entry indices that carry it.
        let mut by_tok: HashMap<u64, Vec<usize>> = HashMap::new();
        for (i, e) in bag.iter().enumerate() {
            for t in (self.signals)(e) {
                by_tok.entry(t).or_default().push(i);
            }
        }
        // Specific-first: rarer tokens first, then token value for determinism.
        let mut toks: Vec<(u64, usize)> = by_tok.iter().map(|(&t, v)| (t, v.len())).collect();
        toks.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        let mut used = vec![false; bag.len()];
        let mut matched: Vec<Vec<Id>> = Vec::new();
        for (t, _) in toks {
            let cand: Vec<usize> = by_tok[&t].iter().copied().filter(|&i| !used[i]).collect();
            if cand.len() < 2 || cand.len() > self.cap {
                continue;
            }
            let view = GroupView::new(cand.iter().map(|&i| &bag[i]).collect());
            if (self.accept)(&view) {
                for &i in &cand {
                    used[i] = true;
                }
                matched.push(cand.iter().map(|&i| bag[i].id).collect());
            }
        }
        split(bag, matched, "signal_group")
    }
}

/// Group entries that share a free-text `signal` token, greedy specific-first
/// and bounded by `cap`, keeping a bucket iff `accept` approves it.
pub fn signal_group<E: 'static, FS, FP>(signals: FS, accept: FP, cap: usize) -> Box<dyn Strategy<E>>
where
    FS: Fn(&Entry<E>) -> Vec<u64> + 'static,
    FP: Fn(&GroupView<E>) -> bool + 'static,
{
    Box::new(SignalGroup {
        signals,
        accept,
        cap,
        _e: std::marker::PhantomData,
    })
}

struct Cumulative<E, FO, FP> {
    order: FO,
    accept: FP,
    _e: std::marker::PhantomData<fn() -> E>,
}
impl<E, FO, FP> Strategy<E> for Cumulative<E, FO, FP>
where
    FO: Fn(&Entry<E>) -> i64,
    FP: Fn(&GroupView<E>) -> bool,
{
    fn run(&self, bag: Vec<Entry<E>>) -> Resolution<E> {
        let mut order: Vec<&Entry<E>> = bag.iter().collect();
        order.sort_by_key(|e| ((self.order)(e), e.id));
        let mut matched: Vec<Vec<Id>> = Vec::new();
        let mut seg: Vec<&Entry<E>> = Vec::new();
        for e in order {
            seg.push(e);
            let view = GroupView::new(seg.clone());
            if seg.len() >= 2 && (self.accept)(&view) {
                matched.push(seg.iter().map(|e| e.id).collect());
                seg.clear();
            }
        }
        split(bag, matched, "cumulative")
    }
}

/// Sort by `order` and close a running segment the moment the segment-so-far
/// satisfies `accept` (e.g. its running net clears).
pub fn cumulative<E: 'static, FO, FP>(order: FO, accept: FP) -> Box<dyn Strategy<E>>
where
    FO: Fn(&Entry<E>) -> i64 + 'static,
    FP: Fn(&GroupView<E>) -> bool + 'static,
{
    Box::new(Cumulative {
        order,
        accept,
        _e: std::marker::PhantomData,
    })
}

struct SubsetSum<E, FA> {
    amount: FA,
    band: i64,
    max_group: usize,
    seed: u64,
    _e: std::marker::PhantomData<fn() -> E>,
}
fn tie(id: Id, seed: u64) -> u64 {
    let mut h = seed ^ 0xcbf29ce484222325;
    for b in id.to_le_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
impl<E, FA> Strategy<E> for SubsetSum<E, FA>
where
    FA: Fn(&Entry<E>) -> i64,
{
    fn run(&self, bag: Vec<Entry<E>>) -> Resolution<E> {
        // Atomic many-to-one clearing: for each positive anchor (descending,
        // seeded tie-break), find a subset of still-free negatives summing to
        // within `band` of the anchor. Bounded DFS (max_group small).
        let mut used: HashSet<Id> = HashSet::new();
        let mut anchors: Vec<&Entry<E>> = bag.iter().filter(|e| (self.amount)(e) > 0).collect();
        anchors.sort_by(|a, b| {
            (self.amount)(b)
                .cmp(&(self.amount)(a))
                .then(tie(a.id, self.seed).cmp(&tie(b.id, self.seed)))
        });
        let negs: Vec<(&Entry<E>, i64)> =
            bag.iter().filter(|e| (self.amount)(e) < 0).map(|e| (e, -(self.amount)(e))).collect();
        let mut matched: Vec<Vec<Id>> = Vec::new();
        for anchor in anchors {
            if used.contains(&anchor.id) {
                continue;
            }
            let target = (self.amount)(anchor);
            let mut pool: Vec<(Id, i64)> = negs
                .iter()
                .filter(|(e, _)| !used.contains(&e.id))
                .map(|(e, v)| (e.id, *v))
                .collect();
            pool.sort_by(|a, b| b.1.cmp(&a.1).then(tie(a.0, self.seed).cmp(&tie(b.0, self.seed))));
            if let Some(subset) = subset_search(&pool, target, self.band, self.max_group) {
                used.insert(anchor.id);
                let mut ids = vec![anchor.id];
                for s in subset {
                    used.insert(s);
                    ids.push(s);
                }
                matched.push(ids);
            }
        }
        split(bag, matched, "subset_sum")
    }
}

/// Find a subset of `pool` (id, value>0) summing to within `band` of `target`,
/// of size <= `max_group - 1`. Greedy-ordered bounded DFS, first hit wins.
fn subset_search(pool: &[(Id, i64)], target: i64, band: i64, max_group: usize) -> Option<Vec<Id>> {
    let max_pick = max_group.saturating_sub(1);
    if max_pick == 0 {
        return None;
    }
    fn dfs(
        pool: &[(Id, i64)],
        start: usize,
        remaining_picks: usize,
        target: i64,
        band: i64,
        acc: &mut Vec<Id>,
    ) -> bool {
        if target.abs() <= band && !acc.is_empty() {
            return true;
        }
        if remaining_picks == 0 || target < -band {
            return false;
        }
        for i in start..pool.len() {
            let (id, v) = pool[i];
            if v > target + band {
                continue; // pruned: overshoots even the band
            }
            acc.push(id);
            if dfs(pool, i + 1, remaining_picks - 1, target - v, band, acc) {
                return true;
            }
            acc.pop();
        }
        false
    }
    let mut acc = Vec::new();
    if dfs(pool, 0, max_pick, target, band, &mut acc) {
        Some(acc)
    } else {
        None
    }
}

/// Atomic whole-lot many-to-one clearing: each positive anchor draws a subset of
/// negatives summing within `band`. `band` is a *search* parameter (candidate
/// pruning), not an acceptance test — gate precisely with a downstream
/// [`accept_if`]. Seeded and reproducible.
pub fn subset_sum<E: 'static, FA>(amount: FA, band: i64, max_group: usize, seed: u64) -> Box<dyn Strategy<E>>
where
    FA: Fn(&Entry<E>) -> i64 + 'static,
{
    Box::new(SubsetSum {
        amount,
        band,
        max_group,
        seed,
        _e: std::marker::PhantomData,
    })
}

#[cfg(test)]
mod tests;
