//! Composable reconciliation as combinators over a bag of entries.
//!
//! A reconciliation strategy *parses groups out of an unordered bag*: it
//! consumes the entries it can resolve and returns the rest. The shape is
//!
//! ```text
//! Strategy : Bag -> (Groups, residual Bag)
//! ```
//!
//! with one invariant every strategy preserves:
//!
//! ```text
//! groups ⊎ residual = input          (conservation: disjoint, nothing lost)
//! ```
//!
//! Primitives ([`exact_1to1`], [`agg_net`], [`signal_group`], [`flow`]) are the
//! leaves; combinators ([`seq`], [`partition_by`], [`when`]) compose them. A
//! whole pipeline is just an expression:
//!
//! ```ignore
//! partition_by(unit, partition_by(ccy, seq(vec![
//!     agg_net(objsub, amount, tol),   // macro nets accepted wholesale
//!     exact_1to1(amount_key, amount), // clean 1-to-1 pairs
//!     signal_group(tokens, amount, tol, cap), // reference bridge
//!     flow(spec),                     // engine arbitrates the rest
//! ])))
//! ```
//!
//! The committing primitives (`agg_net`, `exact_1to1`, `signal_group`) pull the
//! rows they are certain about; [`flow`] is the global *arbiter* for the
//! ambiguous residual where strategies would otherwise compete.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

// The incremental min-cost-flow matcher is just the arbiter behind the `flow`
// strategy leaf, so it lives here as one strategy among many. Kept in its own
// file; `flow::Group` stays distinct from this module's own `Group`.
pub mod flow;
pub use flow::{Allocation, ExtId, FlowSpec, flow};

use std::hash::Hash;
use std::marker::PhantomData;

/// One allocation lot in the bag: a caller-owned row/lot id, its original
/// signed amount in the currently active numeraire, its currently available
/// signed residual amount in that same numeraire, and payload. Strategies never
/// choose a money column themselves; the plan/workspace boundary initializes
/// the primary amount, and [`pivot`] is the only combinator that temporarily
/// switches the active numeraire for a subtree.
///
/// `original` is stable within the active numeraire and `amount` is the
/// shrinking residual. This lets later strategies classify leftovers by
/// materiality, e.g. "soak this residual if it is under 2% of the original
/// line".
#[derive(Clone)]
pub struct Item<E> {
    pub id: ExtId,
    pub original: i64,
    pub amount: i64,
    pub data: E,
}

impl<E> Item<E> {
    pub fn new(id: ExtId, amount: i64, data: E) -> Self {
        Item {
            id,
            original: amount,
            amount,
            data,
        }
    }
}

/// A resolved group of matched lot allocations.
#[derive(Debug, Clone)]
pub struct Group {
    pub members: Vec<Allocation>,
    /// Which primitive produced it.
    pub origin: String,
    /// Residual in the canonical numeraire; zero means it nets out.
    pub net: i64,
    /// Optional human-facing explanation of *why* the group formed, distinct
    /// from the machine `origin`. Stamped by the [`labeled`] combinator (the
    /// author tag) and surfaced to clients on the report. `None` for an
    /// unlabeled group; residual singletons are never labeled.
    pub reason: Option<String>,
}

impl Group {
    pub fn member_ids(&self) -> Vec<ExtId> {
        self.members.iter().map(|a| a.id).collect()
    }

    /// Number of member allocations.
    pub fn size(&self) -> usize {
        self.members.len()
    }

    /// Magnitude of the residual net (zero means the group balances exactly).
    pub fn abs_net(&self) -> i64 {
        self.net.abs()
    }

    /// Largest member allocation magnitude (the dominant leg).
    pub fn max_abs(&self) -> i64 {
        self.members
            .iter()
            .map(|a| a.amount.abs())
            .max()
            .unwrap_or(0)
    }

    /// Smallest non-zero member allocation magnitude; `0` if every leg is zero.
    pub fn min_abs(&self) -> i64 {
        self.members
            .iter()
            .map(|a| a.amount.abs())
            .filter(|&v| v > 0)
            .min()
            .unwrap_or(0)
    }

    /// The minority side count: `min(#positive, #negative)` legs. A clean 1:1
    /// pair is `1`; an all-one-sign wash is `0`. The usual structural gate for
    /// "both books are really represented".
    pub fn min_side(&self) -> usize {
        let pos = self.members.iter().filter(|a| a.amount > 0).count();
        let neg = self.members.iter().filter(|a| a.amount < 0).count();
        pos.min(neg)
    }

    /// Whether the group's net balances within `tol`, measured against the
    /// bucket's leg magnitudes (smallest leg for [`Tol::Rel`], largest for
    /// [`Tol::RelMax`]). The natural predicate for an [`accept_if`] gate.
    pub fn clean(&self, tol: Tol) -> bool {
        self.abs_net() <= tol.slack_for(self.members.iter().map(|a| a.amount))
    }
}

/// An acceptance tolerance for a netting primitive. `Abs` is a fixed slack in
/// the active numeraire; `Rel`/`RelMax` are proportional — `bps` basis points
/// of a reference leg, but never below `floor`. Relative tolerance is the common
/// reconciliation idiom ("within 0.1% of the line"); it stays integer-exact, so
/// conservation is untouched. The two relative forms differ only in the
/// reference leg: `Rel` scales off the **smallest** non-zero leg (conservative —
/// a tiny leg can't drag a big bucket into "balanced"), `RelMax` off the
/// **largest** leg (lenient — "within `bps` of the trade").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(untagged))]
pub enum Tol {
    Abs(i64),
    Rel { bps: i64, floor: i64 },
    RelMax { bps: i64, floor: i64 },
}

impl Tol {
    /// The effective integer slack given a single reference `scale` magnitude.
    /// Both relative forms apply the same `bps`-of-`scale`-but-never-below-floor
    /// rule; they differ only in *which* leg [`Tol::slack_for`] feeds as `scale`.
    pub fn slack(&self, scale: i64) -> i64 {
        match *self {
            Tol::Abs(t) => t,
            Tol::Rel { bps, floor } | Tol::RelMax { bps, floor } => {
                let rel = (scale.unsigned_abs() as i128 * bps.max(0) as i128 / 10_000) as i64;
                rel.max(floor.max(0))
            }
        }
    }

    /// The slack for a whole bucket, picking the reference leg from the member
    /// `legs` per the variant: smallest non-zero leg for [`Tol::Rel`], largest
    /// for [`Tol::RelMax`], irrelevant for [`Tol::Abs`]. This is the single place
    /// scale selection lives, so every netting leaf and [`Group::clean`] agree.
    pub fn slack_for(&self, legs: impl Iterator<Item = i64>) -> i64 {
        let scale = match self {
            Tol::Abs(_) => 0,
            Tol::Rel { .. } => legs.map(i64::abs).filter(|&v| v > 0).min().unwrap_or(0),
            Tol::RelMax { .. } => legs.map(i64::abs).max().unwrap_or(0),
        };
        self.slack(scale)
    }
}

impl From<i64> for Tol {
    fn from(t: i64) -> Self {
        Tol::Abs(t)
    }
}

/// What a strategy returns: the groups it pulled and the residual it left.
pub struct Resolution<E> {
    pub groups: Vec<Group>,
    pub residual: Vec<Item<E>>,
}

/// A reconciliation strategy: pull groups from a bag, return the residual.
///
/// `run` takes `&self` and is a **pure** function of the bag: every strategy
/// recomputes from scratch, holds no cross-call state, and is therefore
/// trivially reproducible and shard-parallel. (The min-cost-flow engine is still
/// incremental internally, but [`flow`] rebuilds it cold each run, so that is an
/// implementation detail the strategy layer never threads.)
pub trait Strategy<E> {
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E>;
}

impl<E> Strategy<E> for Box<dyn Strategy<E>> {
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        (**self).run(bag)
    }
}

// ---------------------------------------------------------------------------
// Combinators
// ---------------------------------------------------------------------------

struct Seq<E> {
    steps: Vec<Box<dyn Strategy<E>>>,
}

impl<E> Strategy<E> for Seq<E> {
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        #[cfg(not(target_arch = "wasm32"))]
        let timed = std::env::var_os("FLORECON_TIME").is_some();
        #[cfg(target_arch = "wasm32")]
        let timed = false;
        let mut groups = Vec::new();
        let mut residual = bag;
        for (i, step) in self.steps.iter().enumerate() {
            let n_in = residual.len();
            // Instant is only touched when profiling; wasm has no clock source.
            let t = timed.then(std::time::Instant::now);
            let r = step.run(residual);
            if let Some(t) = t {
                eprintln!(
                    "  seq step {i}: {n_in:>7} in -> {:>7} grouped, {:>7} residual  [{:>6.1} ms]",
                    r.groups.iter().map(|g| g.members.len()).sum::<usize>(),
                    r.residual.len(),
                    t.elapsed().as_secs_f64() * 1000.0,
                );
            }
            groups.extend(r.groups);
            residual = r.residual;
        }
        Resolution { groups, residual }
    }
}

/// Cascade: run each strategy on the previous one's residual, accumulating
/// groups. This is the macro -> flow -> ... pipeline as a fold.
pub fn seq<E: 'static>(steps: Vec<Box<dyn Strategy<E>>>) -> Box<dyn Strategy<E>> {
    Box::new(Seq { steps })
}

struct Labeled<E> {
    tag: String,
    inner: Box<dyn Strategy<E>>,
}

impl<E> Strategy<E> for Labeled<E> {
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        let mut r = self.inner.run(bag);
        for g in &mut r.groups {
            g.reason = Some(match g.reason.take() {
                Some(detail) => format!("{}: {}", self.tag, detail),
                None => self.tag.clone(),
            });
        }
        r
    }
}

/// Stamp an author `tag` onto every group a subtree produces (in its `reason`
/// field), prepending to any detail an inner label already set. Labeling is
/// orthogonal to *what* forms the group, so it is a combinator rather than a
/// field on every node: wrap a stage to name it ("S3a exact", "intercompany
/// netting"). Residual lots are not groups, so they are never labeled.
pub fn labeled<E: 'static>(
    tag: impl Into<String>,
    inner: Box<dyn Strategy<E>>,
) -> Box<dyn Strategy<E>> {
    Box::new(Labeled {
        tag: tag.into(),
        inner,
    })
}

struct Filter<E, FP> {
    pred: FP,
    inner: Box<dyn Strategy<E>>,
}

impl<E, FP> Strategy<E> for Filter<E, FP>
where
    E: Clone,
    FP: Fn(&Group) -> bool,
{
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        // Snapshot the input so a rejected group can be dissolved back into the
        // residual: a group only carries `Allocation { id, amount }`, having
        // shed the payload `E`, so reconstructing a residual lot needs the
        // original row's data (and `original`, for downstream materiality).
        let src: HashMap<ExtId, Item<E>> = bag.iter().map(|i| (i.id, i.clone())).collect();
        let mut r = self.inner.run(bag);

        let mut kept = Vec::with_capacity(r.groups.len());
        // Restored lots are merged into the residual by id (a partial match can
        // leave an id in *both* a group and the residual); index the existing
        // residual so a rejected portion folds back onto its surviving sibling
        // rather than appearing as a duplicate lot.
        let mut residual_ix: HashMap<ExtId, usize> = r
            .residual
            .iter()
            .enumerate()
            .map(|(ix, item)| (item.id, ix))
            .collect();
        for g in r.groups.drain(..) {
            if (self.pred)(&g) {
                kept.push(g);
                continue;
            }
            // Reject: return every member's allocated portion to the residual,
            // so `kept ⊎ residual = input` still holds in summed (id, amount).
            for a in &g.members {
                match residual_ix.get(&a.id) {
                    Some(&ix) => r.residual[ix].amount += a.amount,
                    None => {
                        if let Some(orig) = src.get(&a.id) {
                            residual_ix.insert(a.id, r.residual.len());
                            r.residual.push(Item {
                                id: a.id,
                                original: orig.original,
                                amount: a.amount,
                                data: orig.data.clone(),
                            });
                        }
                    }
                }
            }
        }
        Resolution {
            groups: kept,
            residual: r.residual,
        }
    }
}

/// Gate an inner strategy's output: keep ("accept") only the groups for which
/// `pred` returns `true`, and dissolve every rejected group back into the
/// residual so downstream stages (or the [`flow`] arbiter) can reconsider those
/// lots. Conservation is preserved — a rejected group's member allocations are
/// returned as residual (merged onto any surviving same-id lot) rather than
/// dropped.
///
/// This is the knob for *shaping* what a subtree is allowed to commit: reject
/// over-large groups (`g.members.len() <= cap`), require both sides to be
/// substantial (the minority-sign side must exceed a count), bound the net, and
/// so on. The predicate sees the whole [`Group`] (its member allocations,
/// `origin`, and `net`), so any structural test is expressible.
///
/// ```ignore
/// // Accept only groups <= 12 lots whose smaller side exceeds 2; reject the
/// // rest back to residual for a later stage.
/// accept_if(
///     |g| g.size() <= 12 && g.min_side() > 2,
///     flow(spec),
/// )
/// ```
pub fn accept_if<E: Clone + 'static, FP>(
    pred: FP,
    inner: Box<dyn Strategy<E>>,
) -> Box<dyn Strategy<E>>
where
    FP: Fn(&Group) -> bool + 'static,
{
    Box::new(Filter { pred, inner })
}

struct Material<E> {
    tol: Tol,
    inner: Box<dyn Strategy<E>>,
}

impl<E: Clone> Strategy<E> for Material<E> {
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        // Snapshot the input to recover each row's `original` (the materiality
        // reference) and its payload for the dissolve, exactly as `Filter` does.
        let src: HashMap<ExtId, Item<E>> = bag.iter().map(|i| (i.id, i.clone())).collect();
        let mut r = self.inner.run(bag);

        let mut kept = Vec::with_capacity(r.groups.len());
        let mut residual_ix: HashMap<ExtId, usize> = r
            .residual
            .iter()
            .enumerate()
            .map(|(ix, item)| (item.id, ix))
            .collect();
        for g in r.groups.drain(..) {
            // Moved volume: sum of |leg| across the group's allocations.
            let moved: i64 = g.members.iter().map(|a| a.amount.abs()).sum();
            // Reference base: sum of |original| over the group's *distinct*
            // member ids (original is per-row, so a repeated id counts once).
            let mut seen: HashSet<ExtId> = HashSet::new();
            let base: i64 = g
                .members
                .iter()
                .filter(|a| seen.insert(a.id))
                .filter_map(|a| src.get(&a.id))
                .map(|i| i.original.abs())
                .sum();
            // `Tol::slack` with our own `base` as the scale: `Abs(t)` ignores it
            // and returns `t`; `Rel`/`RelMax` return `bps`-of-`base` (floored).
            // The two relative forms coincide here -- the reference is the total
            // original, not a single leg -- so there is no smallest/largest pick.
            if moved > self.tol.slack(base) {
                kept.push(g);
                continue;
            }
            // Immaterial: dissolve every leg back to residual (merged by id), so
            // `kept ⊎ residual = input` in summed (id, amount). A later stage gets
            // to rematch those lots.
            for a in &g.members {
                match residual_ix.get(&a.id) {
                    Some(&ix) => r.residual[ix].amount += a.amount,
                    None => {
                        if let Some(orig) = src.get(&a.id) {
                            residual_ix.insert(a.id, r.residual.len());
                            r.residual.push(Item {
                                id: a.id,
                                original: orig.original,
                                amount: a.amount,
                                data: orig.data.clone(),
                            });
                        }
                    }
                }
            }
        }
        Resolution {
            groups: kept,
            residual: r.residual,
        }
    }
}

/// Drop **immaterial** groups: keep a group only if its *moved volume*
/// `M = Σ |leg|` (the sum of its allocation magnitudes) exceeds the tolerance,
/// otherwise dissolve it back into the residual (conserving) for a later stage
/// to reconsider. The mirror of [`whole_net`]: that keeps a small *break*
/// **inside** a group, this kicks a small *match* **out**; [`soak_small`]
/// absorbs a small *residual* into a bucket — three distinct materiality cells.
///
/// Tolerance basis ([`Tol`], §6):
/// - `Abs(t)` — keep iff `M > t`. (Already expressible as
///   `accept_if(|g| Σ|leg| > t, ..)`; offered here so authors don't switch
///   primitives for the absolute case.)
/// - `Rel { bps, floor }` — keep iff `M > max(floor, bps·R/10_000)`, where the
///   reference `R = Σ |original|` is summed over the group's **distinct member
///   ids**. This is the load-bearing case: a `Group` sheds each row's
///   `original`, so "is this match small *relative to the rows' original size*?"
///   is out of an [`accept_if`] predicate's reach. `RelMax` behaves identically
///   — the reference is the total original, not a single leg.
///
/// `material` measures slices against each row's **birth** size, not the
/// residual entering this stage: a row already 99% cleared upstream whose final
/// sliver matches cleanly here reads as immaterial and returns to residual. It
/// is a general per-group gate (no internal coalesce) — compose [`coalesce`] /
/// [`settle`] before it for settlement-granularity, or run it on raw [`flow`]
/// arcs for arc-granularity.
pub fn material<E: Clone + 'static>(tol: Tol, inner: Box<dyn Strategy<E>>) -> Box<dyn Strategy<E>> {
    Box::new(Material { tol, inner })
}

struct WholeNet<E> {
    tol: Tol,
    inner: Box<dyn Strategy<E>>,
}

impl<E: Clone> Strategy<E> for WholeNet<E> {
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        // Snapshot inputs to rematerialize *whole* lines: a group carries only
        // `Allocation { id, amount }`, so reclaiming a line at its full size
        // needs `original` and the payload `E`.
        let src: HashMap<ExtId, Item<E>> = bag.iter().map(|i| (i.id, i.clone())).collect();
        let r = self.inner.run(bag);

        // Index the inner residual by id so a line's ground tail can be reclaimed
        // (folded back into the whole line) or left in place if its cluster
        // dissolves. Merge duplicates defensively.
        let mut resid: HashMap<ExtId, Item<E>> = HashMap::new();
        for it in r.residual {
            resid
                .entry(it.id)
                .and_modify(|e| e.amount += it.amount)
                .or_insert(it);
        }

        // A line is atomic in the whole-line paradigm, so groups that share a
        // member id are one settlement: collapse them into a single cluster
        // (this is why a residual "going to another group" can't survive -- it
        // becomes the *same* group). Within a cluster, the only tails left are
        // tails to ground, which reclaim unambiguously.
        let comps = group_components(&r.groups);

        let mut out_groups: Vec<Group> = Vec::new();
        let mut out_residual: Vec<Item<E>> = Vec::new();

        for comp in comps {
            // Member ids across every group in the cluster (a line may appear in
            // more than one of them; dedup, keep first-seen then sort by id).
            let mut member_ids: Vec<ExtId> = Vec::new();
            let mut seen: HashSet<ExtId> = HashSet::new();
            for &gi in &comp {
                for a in &r.groups[gi].members {
                    if seen.insert(a.id) {
                        member_ids.push(a.id);
                    }
                }
            }
            member_ids.sort_unstable();

            // Whole-line amounts are originals; the cluster net is judged on the
            // *whole* lines (reclaimed tails included), not the matched parts.
            let wholes: Vec<(ExtId, i64)> = member_ids
                .iter()
                .filter_map(|&id| src.get(&id).map(|i| (id, i.original)))
                .collect();
            let net: i64 = wholes.iter().map(|&(_, o)| o).sum();
            let tol = self.tol.slack_for(wholes.iter().map(|&(_, o)| o));

            if net.abs() <= tol {
                // Accept the cluster as one whole-line settlement, keeping `net`
                // as the (in-tolerance) break. Reclaim every member's ground tail.
                for &(id, _) in &wholes {
                    resid.remove(&id);
                }
                // Preserve the inner origin/reason for a lone group; a genuine
                // multi-group merge becomes a settlement cluster.
                let (origin, reason) = if comp.len() == 1 {
                    let g = &r.groups[comp[0]];
                    (g.origin.clone(), g.reason.clone())
                } else {
                    ("settlement".to_string(), None)
                };
                out_groups.push(Group {
                    members: wholes
                        .iter()
                        .map(|&(id, o)| Allocation { id, amount: o })
                        .collect(),
                    origin,
                    net,
                    reason,
                });
            } else {
                // Dissolve: every member line returns to ground *whole*.
                for &(id, o) in &wholes {
                    resid.remove(&id);
                    if let Some(it) = src.get(&id) {
                        out_residual.push(Item {
                            id,
                            original: it.original,
                            amount: o,
                            data: it.data.clone(),
                        });
                    }
                }
            }
        }
        // Ground-only lots (lines that never entered a group) pass through.
        out_residual.extend(resid.into_values());
        out_residual.sort_by_key(|i| i.id);
        Resolution {
            groups: out_groups,
            residual: out_residual,
        }
    }
}

/// Commit groups of **whole lines** whose net clears within `tol` -- the
/// traditional N:M tolerance match, on a matcher's discovered grouping.
///
/// Where [`flow`] splits a line at the unit level (matched part + residual tail)
/// and leaves net-zero groups, `whole_net` works the other way: it takes the
/// inner's grouping, makes every member line **whole** (reclaiming its ground
/// tail), and accepts the cluster iff `|net| <= tol` -- keeping that net as the
/// visible, in-tolerance break *inside* the matched group. Clusters over
/// tolerance dissolve, every line returning to residual whole.
///
/// Because a line is atomic here, groups that share a member id are one
/// settlement: `whole_net` coalesces them first, so a line's tail can only ever
/// go to **ground** (never to a sibling group), and the reclaim is
/// unambiguous. Conservation holds -- each id ends up wholly in one accepted
/// group or wholly in residual. `tol` picks its reference leg per [`Tol`]
/// (smallest leg for `Rel`, largest for `RelMax`, fixed for `Abs`).
pub fn whole_net<E: Clone + 'static>(
    tol: impl Into<Tol>,
    inner: Box<dyn Strategy<E>>,
) -> Box<dyn Strategy<E>> {
    Box::new(WholeNet {
        tol: tol.into(),
        inner,
    })
}

struct Coalesce<E> {
    origin: String,
    inner: Box<dyn Strategy<E>>,
}

/// Union-find over `n` group indices: link two groups whenever they share a
/// member id, then read off connected components in first-seen order.
fn group_components(groups: &[Group]) -> Vec<Vec<usize>> {
    let mut parent: Vec<usize> = (0..groups.len()).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]]; // path-halving
            x = parent[x];
        }
        x
    }
    // First group index seen for each member id; subsequent sightings union.
    let mut first: HashMap<ExtId, usize> = HashMap::new();
    for (gi, g) in groups.iter().enumerate() {
        for a in &g.members {
            match first.get(&a.id) {
                Some(&fj) => {
                    let (ra, rb) = (find(&mut parent, gi), find(&mut parent, fj));
                    if ra != rb {
                        parent[ra] = rb;
                    }
                }
                None => {
                    first.insert(a.id, gi);
                }
            }
        }
    }
    // Bucket indices by root, preserving the order roots first appear so the
    // output is deterministic and independent of HashMap iteration order.
    let mut order: Vec<usize> = Vec::new();
    let mut buckets: HashMap<usize, Vec<usize>> = HashMap::new();
    for gi in 0..groups.len() {
        let r = find(&mut parent, gi);
        if !buckets.contains_key(&r) {
            order.push(r);
        }
        buckets.entry(r).or_default().push(gi);
    }
    order
        .into_iter()
        .map(|r| buckets.remove(&r).unwrap())
        .collect()
}

/// The settlement view of an allocation hypergraph: connected-component
/// regroup with per-id edge summing. Groups that share any member id merge into
/// one cluster; within a cluster each id's allocations sum to one clean edge.
/// Empty (fully-cancelled) clusters drop. Every cluster is stamped with
/// `origin`; a lone group keeps its inner `reason`, a merged cluster gets a
/// synthesized one. The implementation behind [`coalesce`].
fn coalesce_groups(groups: &[Group], origin: &str) -> Vec<Group> {
    let comps = group_components(groups);
    let mut out = Vec::with_capacity(comps.len());
    for comp in &comps {
        let mut by_id: BTreeMap<ExtId, i64> = BTreeMap::new();
        for &gi in comp {
            for a in &groups[gi].members {
                *by_id.entry(a.id).or_insert(0) += a.amount;
            }
        }
        let members: Vec<Allocation> = by_id
            .into_iter()
            .filter(|&(_, amount)| amount != 0)
            .map(|(id, amount)| Allocation { id, amount })
            .collect();
        if members.is_empty() {
            continue;
        }
        let net = members.iter().map(|a| a.amount).sum();
        let reason = if comp.len() == 1 {
            groups[comp[0]].reason.clone()
        } else {
            Some(format!("coalesced {} groups", comp.len()))
        };
        out.push(Group {
            members,
            origin: origin.to_string(),
            net,
            reason,
        });
    }
    out
}

impl<E> Strategy<E> for Coalesce<E> {
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        let r = self.inner.run(bag);
        // Residual is untouched -- coalesce only regroups what was already
        // matched, stamping every cluster with `origin`.
        Resolution {
            groups: coalesce_groups(&r.groups, &self.origin),
            residual: r.residual,
        }
    }
}

/// Collapse an inner strategy's allocation-hyperedge groups into their
/// **connected components**: groups that share any member id are merged into a
/// single coarse group, with each member id's allocations summed so the result
/// is one clean edge per row. The residual is **never touched**.
///
/// The [`flow`] arbiter (and partial matchers in general) produce an allocation
/// *hypergraph* — a row can be split across several groups, and groups interlock
/// through shared rows. That is the right representation for conservation and
/// for the optimizer, but it is awkward to action by hand. `coalesce` turns it
/// into the coarser "settlement cluster" view a human reconciles against: every
/// set of rows transitively tied together by the matcher becomes one group,
/// uniformly stamped with `origin`.
///
/// `coalesce` is a pure group→group transform: its invariant is
/// `residual_out == residual_in`, and the regrouped allocations are the same
/// multiset as the input groups'. It never moves material between groups and
/// residual; to *commit* whole-line settlements within tolerance (and dissolve
/// the rest), compose with [`whole_net`]. A lone group keeps its inner `reason`.
pub fn coalesce<E: 'static>(
    origin: impl Into<String>,
    inner: Box<dyn Strategy<E>>,
) -> Box<dyn Strategy<E>> {
    Box::new(Coalesce {
        origin: origin.into(),
        inner,
    })
}

/// The settlement view over [`flow`]: discover the matching as raw arcs, then
/// [`coalesce`] them into connected-component settlements (one clean,
/// per-id-summed edge per row, net 0, `origin = "flow"`).
///
/// This is the blessed "I just want the groups" path -- the common composition
/// of the [`flow`] primitive with the `coalesce` grouping authority -- and is
/// exactly what `flow` returned before its raw arcs were exposed. Reach past it
/// to bare [`flow`] only when you want the arcs themselves.
pub fn settle<E: Clone + 'static>(spec: FlowSpec<E>) -> Box<dyn Strategy<E>> {
    coalesce("flow", flow(spec))
}

struct FixedPoint<E> {
    inner: Box<dyn Strategy<E>>,
    max_passes: usize,
}

/// A stable fingerprint of remaining work: the sorted multiset of
/// `(id, current amount)`. Two residuals with the same fingerprint represent
/// identical outstanding work, so a pass that reproduces it has reached a fixed
/// point. Amount is included so a pass that only *re-prices* a residual lot
/// (e.g. a partial match upstream) still counts as progress.
fn residual_fingerprint<E>(items: &[Item<E>]) -> Vec<(ExtId, i64)> {
    let mut v: Vec<(ExtId, i64)> = items.iter().map(|i| (i.id, i.amount)).collect();
    v.sort_unstable();
    v
}

impl<E> Strategy<E> for FixedPoint<E> {
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        let mut groups = Vec::new();
        let mut residual = bag;
        let mut fp = residual_fingerprint(&residual);
        for _ in 0..self.max_passes {
            if residual.is_empty() {
                break;
            }
            let r = self.inner.run(std::mem::take(&mut residual));
            groups.extend(r.groups);
            residual = r.residual;
            let next = residual_fingerprint(&residual);
            // A pass that left the outstanding work unchanged is a no-op: the
            // loop has converged. (A pass can only reproduce the same
            // fingerprint by leaving the residual untouched, since grouped ids
            // leave the residual entirely.)
            if next == fp {
                break;
            }
            fp = next;
        }
        Resolution { groups, residual }
    }
}

/// Iterate `inner` on its own residual until it reaches a fixed point -- a pass
/// that changes nothing more -- or `max_passes` elapse, accumulating every
/// group found along the way. Conservation holds by construction: each pass
/// conserves, and only the residual is re-fed while groups are locked in.
///
/// State inside `inner` **persists across passes** (the warm flow basis,
/// per-shard [`partition_by`] children, ...): the loop reuses the same compiled
/// subtree rather than rebuilding it. That is sound because every node treats
/// its incoming bag as the *authoritative present-set* and reconciles against
/// what it previously held -- the same discipline that makes warm re-solve
/// correct -- so re-running a node on its own (shrunken) residual is
/// reentrant-safe: departed ids are dropped, surviving ids are re-priced, and a
/// globally-optimal leaf like `flow` simply reproduces its residual and the loop
/// converges. `max_passes` is a hard bound; reaching it returns the best result
/// so far (still conserving), so a pathological non-convergent `inner` is
/// bounded rather than unbounded.
pub fn fixed_point<E: 'static>(
    inner: Box<dyn Strategy<E>>,
    max_passes: usize,
) -> Box<dyn Strategy<E>> {
    Box::new(FixedPoint {
        inner,
        max_passes: max_passes.max(1),
    })
}

/// Builds a per-shard child subtree from the shard key (see [`partition_by`] /
/// [`partition_by_with`]).
type ShardFactory<E, K> = dyn Fn(&K) -> Box<dyn Strategy<E>>;

struct PartitionBy<E, K, FK> {
    key: FK,
    /// Builds a child subtree for a shard key. Receives the shard key, so
    /// [`partition_by_with`] can choose a per-key subtree; [`partition_by`]
    /// passes a key-ignoring factory. A fresh child is built per shard per run
    /// (the strategy is stateless).
    factory: Box<ShardFactory<E, K>>,
}

impl<E, K, FK> Strategy<E> for PartitionBy<E, K, FK>
where
    K: Hash + Eq + Clone,
    FK: Fn(&E) -> K,
{
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        let mut shards: HashMap<K, Vec<Item<E>>> = HashMap::new();
        for item in bag {
            shards.entry((self.key)(&item.data)).or_default().push(item);
        }
        let mut groups = Vec::new();
        let mut residual = Vec::new();
        for (k, items) in shards {
            let r = (self.factory)(&k).run(items);
            groups.extend(r.groups);
            residual.extend(r.residual);
        }
        Resolution { groups, residual }
    }
}

/// Fork/join: split the bag by a key and run an independent child subtree on
/// each shard, then merge. `factory` builds a child the first time a shard key
/// is seen; each child keeps its own (warm) state across solves. This is how
/// sharding (e.g. by bilateral pair or by currency) is expressed — and what
/// makes per-shard warm-start fall out for free, since each shard's flow leaf is
/// a distinct warm flow leaf that only ever sees that shard's rows.
pub fn partition_by<E: 'static, K, FK, FF>(key: FK, factory: FF) -> Box<dyn Strategy<E>>
where
    K: Hash + Eq + Clone + 'static,
    FK: Fn(&E) -> K + 'static,
    FF: Fn() -> Box<dyn Strategy<E>> + 'static,
{
    Box::new(PartitionBy {
        key,
        factory: Box::new(move |_k| factory()),
    })
}

/// [`partition_by`] with a **key-aware** factory: shard by key equality exactly
/// as `partition_by`, but the factory receives the shard key, so plain Rust
/// picks a per-key subtree (e.g. an AR/AP shard runs a different cascade than a
/// GA shard). Routing stays hard-disjoint with per-shard warm state — an item
/// lands in exactly one key-chosen subtree and never cascades into a sibling.
/// (For *cascade* routing where leftovers flow on, compose [`when`] in a
/// [`seq`] instead.)
pub fn partition_by_with<E: 'static, K, FK, FF>(key: FK, factory: FF) -> Box<dyn Strategy<E>>
where
    K: Hash + Eq + Clone + 'static,
    FK: Fn(&E) -> K + 'static,
    FF: Fn(&K) -> Box<dyn Strategy<E>> + 'static,
{
    Box::new(PartitionBy {
        key,
        factory: Box::new(factory),
    })
}

struct When<E, FP> {
    pred: FP,
    inner: Box<dyn Strategy<E>>,
}

impl<E, FP> Strategy<E> for When<E, FP>
where
    FP: Fn(&E) -> bool,
{
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        let mut yes = Vec::new();
        let mut no = Vec::new();
        for item in bag {
            if (self.pred)(&item.data) {
                yes.push(item);
            } else {
                no.push(item);
            }
        }
        // Always run the child, even on empty input, so a stateful leaf such as
        // `flow` observes rows that departed the guard and drops stale warm
        // state. Non-matching items pass straight through as residual, joined
        // with whatever matching items the child could not resolve.
        let mut r = self.inner.run(yes);
        r.residual.extend(no);
        r
    }
}

/// Route the items matching `pred` into `inner`; everything else passes straight
/// through as residual. `inner`'s own residual (matching items it could not
/// resolve) joins the passthrough, so inside a [`seq`] the leftovers cascade to
/// the next step. This is the one-sided guard — the everyday way to apply a
/// subtree to a subset (only prior-close rows, only rows with a non-zero trx
/// amount) while leaving the rest for later stages.
///
/// For *hard-disjoint* per-key routing with warm shards (an item lands in
/// exactly one key-chosen subtree, no cascade), use [`partition_by_with`]; for a
/// two-way split, just sequence two guards: `seq(vec![when(p, a), when(not_p,
/// b)])`.
pub fn when<E: 'static, FP>(pred: FP, inner: Box<dyn Strategy<E>>) -> Box<dyn Strategy<E>>
where
    FP: Fn(&E) -> bool + 'static,
{
    Box::new(When { pred, inner })
}

struct Identity;

impl<E> Strategy<E> for Identity {
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        Resolution {
            groups: Vec::new(),
            residual: bag,
        }
    }
}

/// The no-op strategy: pulls no groups, returns the whole bag as residual. It is
/// the unit of [`seq`] (an empty `seq` behaves identically) and the do-nothing
/// arm of a guard. Rarely written directly; handy as a default subtree.
pub fn identity<E: 'static>() -> Box<dyn Strategy<E>> {
    Box::new(Identity)
}

struct Windowed<E, FO> {
    order: FO,
    width: i64,
    inner: Box<dyn Strategy<E>>,
    _e: PhantomData<E>,
}

impl<E, FO> Strategy<E> for Windowed<E, FO>
where
    FO: Fn(&E) -> i64,
{
    fn run(&self, mut bag: Vec<Item<E>>) -> Resolution<E> {
        // Soft locality, not hard segmentation: sort by `order`, sweep in bands
        // of `width`, and run `inner` on each band together with a carry of
        // still-matchable items from earlier bands. An item gets a full window
        // of look-back and look-ahead before it is flushed to residual, so a
        // match whose endpoints' order keys differ (a card payment vs its
        // transactions) is still found -- without letting a coincidental far
        // match form. `width` is the tolerance for imperfect ordering.
        let w = self.width.max(1);
        bag.sort_by_key(|i| (self.order)(&i.data));
        let mut groups = Vec::new();
        let mut residual = Vec::new();
        let mut carry: Vec<Item<E>> = Vec::new();
        let mut it = bag.into_iter().peekable();
        while let Some(first) = it.peek() {
            let band_bottom = (self.order)(&first.data);
            let mut band = Vec::new();
            while let Some(item) = it.peek() {
                if (self.order)(&item.data) < band_bottom + w {
                    band.push(it.next().unwrap());
                } else {
                    break;
                }
            }
            // flush carry items too old to match anything from here on
            let mut keep = Vec::new();
            for item in carry.drain(..) {
                if (self.order)(&item.data) + w >= band_bottom {
                    keep.push(item);
                } else {
                    residual.push(item);
                }
            }
            keep.extend(band);
            let r = self.inner.run(keep);
            groups.extend(r.groups);
            carry = r.residual; // unmatched -> look ahead into later bands
        }
        residual.extend(carry);
        Resolution { groups, residual }
    }
}

/// Order-then-windowed-search: bound where a committing `inner` strategy looks
/// by proximity over an `order` key, with `width` as the tolerance for
/// imperfect ordering. This gives the deterministic primitives the same
/// locality the [`flow`] arbiter gets from its block/window, cutting both false
/// positives (a coincidental equal amount a year away) and work. `running_zero`
/// is the strict special case (window = since the last balance clear).
pub fn windowed<E: 'static, FO>(
    order: FO,
    width: i64,
    inner: Box<dyn Strategy<E>>,
) -> Box<dyn Strategy<E>>
where
    FO: Fn(&E) -> i64 + 'static,
{
    Box::new(Windowed {
        order,
        width,
        inner,
        _e: PhantomData,
    })
}

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

struct ExactOneToOne<E, FK> {
    key: FK,
    _e: PhantomData<E>,
}

impl<E, FK> Strategy<E> for ExactOneToOne<E, FK>
where
    FK: Fn(&E) -> Option<u64>,
{
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        let mut buckets: HashMap<u64, Vec<Item<E>>> = HashMap::new();
        let mut residual = Vec::new();
        for item in bag {
            match (self.key)(&item.data) {
                Some(k) if item.amount != 0 => buckets.entry(k).or_default().push(item),
                _ => residual.push(item),
            }
        }
        let mut groups = Vec::new();
        for (_k, items) in buckets {
            // Pair opposite signs of equal magnitude within the bucket.
            // pos/neg stacks per magnitude
            type Signed<E> = (Vec<Item<E>>, Vec<Item<E>>);
            let mut by_mag: HashMap<i64, Signed<E>> = HashMap::new();
            for item in items {
                let a = item.amount;
                let slot = by_mag.entry(a.abs()).or_default();
                if a > 0 {
                    slot.0.push(item);
                } else {
                    slot.1.push(item);
                }
            }
            for (_mag, (mut pos, mut neg)) in by_mag {
                // Pair deterministically by id so the *identity* of the surplus
                // left for downstream leaves (and the flow arbiter) is stable
                // across re-solves. Without this, HashMap/stack order would pick
                // a different equal-magnitude row to leave each solve, changing
                // the flow input set and defeating warm-start.
                pos.sort_unstable_by_key(|i| i.id);
                neg.sort_unstable_by_key(|i| i.id);
                let pairs = pos.len().min(neg.len());
                for _ in 0..pairs {
                    let p = pos.pop().unwrap();
                    let n = neg.pop().unwrap();
                    groups.push(Group {
                        members: vec![
                            Allocation {
                                id: p.id,
                                amount: p.amount,
                            },
                            Allocation {
                                id: n.id,
                                amount: n.amount,
                            },
                        ],
                        origin: "exact_1to1".to_string(),
                        net: 0,
                        reason: Some("exact 1:1 pair".to_string()),
                    });
                }
                residual.extend(pos);
                residual.extend(neg);
            }
        }
        Resolution { groups, residual }
    }
}

/// Pull opposite-sign pairs of equal magnitude sharing a key (e.g. native
/// currency + amount). The cheapest, highest-precision matcher; clears clean
/// 1-to-1s before anything expensive runs. `key` returns `None` to opt out.
pub fn exact_1to1<E: 'static, FK>(key: FK) -> Box<dyn Strategy<E>>
where
    FK: Fn(&E) -> Option<u64> + 'static,
{
    Box::new(ExactOneToOne {
        key,
        _e: PhantomData,
    })
}

struct AggNet<E, FK> {
    key: FK,
    tol: Tol,
    _e: PhantomData<E>,
}

impl<E, FK> Strategy<E> for AggNet<E, FK>
where
    FK: Fn(&E) -> u64,
{
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        let mut buckets: HashMap<u64, Vec<Item<E>>> = HashMap::new();
        for item in bag {
            buckets
                .entry((self.key)(&item.data))
                .or_default()
                .push(item);
        }
        let mut groups = Vec::new();
        let mut residual = Vec::new();
        for (_k, items) in buckets {
            let sum: i64 = items.iter().map(|i| i.amount).sum();
            let signs = items.iter().fold((false, false), |(p, n), i| {
                let a = i.amount;
                (p || a > 0, n || a < 0)
            });
            // Relative tolerance picks its reference leg per the `Tol` variant.
            let tol = self.tol.slack_for(items.iter().map(|i| i.amount));
            if items.len() >= 2 && sum.abs() <= tol && signs.0 && signs.1 {
                groups.push(Group {
                    members: items
                        .iter()
                        .map(|i| Allocation {
                            id: i.id,
                            amount: i.amount,
                        })
                        .collect(),
                    origin: "agg_net".to_string(),
                    net: sum,
                    reason: Some("aggregate net".to_string()),
                });
            } else {
                residual.extend(items);
            }
        }
        Resolution { groups, residual }
    }
}

/// Accept a whole aggregation bucket (e.g. an `objsub`, or a balance-sheet-level
/// set) when it nets to zero within `tol` (absolute or relative; see [`Tol`]).
/// The macro net-to-zero pre-filter: confirmation, not optimization.
pub fn agg_net<E: 'static, FK>(key: FK, tol: impl Into<Tol>) -> Box<dyn Strategy<E>>
where
    FK: Fn(&E) -> u64 + 'static,
{
    Box::new(AggNet {
        key,
        tol: tol.into(),
        _e: PhantomData,
    })
}

struct RunningZero<E, FO> {
    order: FO,
    tol: i64,
    _e: PhantomData<E>,
}

impl<E, FO> Strategy<E> for RunningZero<E, FO>
where
    FO: Fn(&E) -> i64,
{
    fn run(&self, mut bag: Vec<Item<E>>) -> Resolution<E> {
        // Order the bag (finance bags are a timeline), then walk the running
        // balance. Each time it returns to zero, everything since the last zero
        // is a closed clearing segment -- e.g. a payment that settles all
        // outstanding items up to its date.
        bag.sort_by_key(|i| (self.order)(&i.data));
        let mut groups = Vec::new();
        let mut seg: Vec<Item<E>> = Vec::new();
        let mut acc: i64 = 0;
        for item in bag {
            acc += item.amount;
            seg.push(item);
            if acc.abs() <= self.tol && seg.len() >= 2 {
                groups.push(Group {
                    members: seg
                        .iter()
                        .map(|i| Allocation {
                            id: i.id,
                            amount: i.amount,
                        })
                        .collect(),
                    origin: "running_zero".to_string(),
                    net: acc,
                    reason: Some("running-balance zero".to_string()),
                });
                seg.clear();
                acc = 0;
            }
        }
        Resolution {
            groups,
            residual: seg, // trailing, never-cleared tail
        }
    }
}

/// Order-aware clearing: sort the bag by `order` and close a group every time
/// the running balance returns to zero (within `tol`). Expresses
/// "balance-forward" semantics -- an entry that clears all outstanding balance
/// up to its date is exactly the one that brings the running balance back to
/// zero. Intermediate zero-crossings give the finest segmentation consistent
/// with the timeline; the never-cleared tail is left as residual.
pub fn running_zero<E: 'static, FO>(order: FO, tol: i64) -> Box<dyn Strategy<E>>
where
    FO: Fn(&E) -> i64 + 'static,
{
    Box::new(RunningZero {
        order,
        tol,
        _e: PhantomData,
    })
}

struct SignalGroup<E, FS> {
    signals: FS,
    tol: Tol,
    cap: usize,
    _e: PhantomData<E>,
}

impl<E, FS> Strategy<E> for SignalGroup<E, FS>
where
    FS: Fn(&E) -> Vec<u64>,
{
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        let n = bag.len();
        let amt: Vec<i64> = bag.iter().map(|i| i.amount).collect();
        let sigs: Vec<Vec<u64>> = bag.iter().map(|i| (self.signals)(&i.data)).collect();
        // signal -> member indices
        let mut index: HashMap<u64, Vec<usize>> = HashMap::new();
        for (i, s) in sigs.iter().enumerate() {
            for &k in s {
                index.entry(k).or_default().push(i);
            }
        }
        // Prefer specific (small) buckets first so a coincidental shared token
        // can't pre-empt a tight reference group.
        let mut order: Vec<(usize, u64)> = index.iter().map(|(k, v)| (v.len(), *k)).collect();
        order.sort_unstable();

        let mut used = vec![false; n];
        let mut groups = Vec::new();
        for (_len, k) in order {
            let members: Vec<usize> = index[&k].iter().copied().filter(|&i| !used[i]).collect();
            if members.len() < 2 || members.len() > self.cap {
                continue;
            }
            let sum: i64 = members.iter().map(|&i| amt[i]).sum();
            let has_pos = members.iter().any(|&i| amt[i] > 0);
            let has_neg = members.iter().any(|&i| amt[i] < 0);
            // Relative tolerance picks its reference leg per the `Tol` variant,
            // matching `agg_net`.
            let slack = self.tol.slack_for(members.iter().map(|&i| amt[i]));
            if sum.abs() <= slack && has_pos && has_neg {
                for &i in &members {
                    used[i] = true;
                }
                groups.push(Group {
                    members: members
                        .iter()
                        .map(|&i| Allocation {
                            id: bag[i].id,
                            amount: amt[i],
                        })
                        .collect(),
                    origin: "signal_group".to_string(),
                    net: sum,
                    reason: Some("shared reference".to_string()),
                });
            }
        }
        let residual = bag
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !used[*i])
            .map(|(_, item)| item)
            .collect();
        Resolution { groups, residual }
    }
}

/// Group by an out-of-band signal (e.g. hashed reference tokens that bridge two
/// books) and pull buckets that net to zero within `tol`. High precision: a
/// token *names* the group; netting only validates it. Greedy on most-specific
/// buckets first; ambiguous/over-large buckets (`> cap`) are left for [`flow`].
pub fn signal_group<E: 'static, FS>(
    signals: FS,
    tol: impl Into<Tol>,
    cap: usize,
) -> Box<dyn Strategy<E>>
where
    FS: Fn(&E) -> Vec<u64> + 'static,
{
    Box::new(SignalGroup {
        signals,
        tol: tol.into(),
        cap,
        _e: PhantomData,
    })
}

/// A pure seeded hash (splitmix64) of a 64-bit word. The *only* source of
/// "randomness" in the stochastic strategies, so every choice is a reproducible
/// function of row ids and an explicit seed -- never an RNG or the clock, which
/// keeps warm-vs-cold parity and golden replays intact.
fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}
fn seed_mix(a: u64, b: u64) -> u64 {
    splitmix64(a ^ splitmix64(b))
}

/// Candidate cap per anchor. Blocking ([`partition_by`]/[`windowed`]) should
/// keep pools well below this; beyond it the meet-in-the-middle halves blow up,
/// so a degenerate block is truncated to its largest-magnitude candidates.
const SUBSET_CAND_CAP: usize = 32;

struct SubsetSum<E> {
    tol: i64,
    max_group: usize,
    seed: u64,
    _e: PhantomData<E>,
}

impl<E> Strategy<E> for SubsetSum<E> {
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        let n = bag.len();
        let mut consumed = vec![false; n];
        let max_partners = self.max_group.saturating_sub(1);

        // Anchor order: largest |amount| first (decompose a big lot into smaller
        // partners), ties broken by a seeded hash so distinct seeds explore
        // distinct matchings under `restart`.
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|&i| {
            (
                std::cmp::Reverse(bag[i].amount.unsigned_abs()),
                seed_mix(bag[i].id, self.seed),
            )
        });

        let mut groups = Vec::new();
        for &ai in &order {
            if consumed[ai] || bag[ai].amount == 0 || max_partners == 0 {
                continue;
            }
            let target = bag[ai].amount.unsigned_abs() as i64;
            let want_sign = -bag[ai].amount.signum();

            // Opposite-sign, unconsumed lots no larger than the band (a single
            // partner above `target + tol` cannot belong to any in-band subset).
            let mut cands: Vec<(usize, ExtId, i64)> = (0..n)
                .filter(|&j| {
                    !consumed[j]
                        && j != ai
                        && bag[j].amount.signum() == want_sign
                        && bag[j].amount.unsigned_abs() as i64 <= target + self.tol
                })
                .map(|j| (j, bag[j].id, bag[j].amount.unsigned_abs() as i64))
                .collect();
            if cands.is_empty() {
                continue;
            }
            if cands.len() > SUBSET_CAND_CAP {
                cands.sort_by_key(|&(_, _, mag)| std::cmp::Reverse(mag));
                cands.truncate(SUBSET_CAND_CAP);
            }

            if let Some(chosen) = best_subset(target, self.tol, max_partners, &cands, self.seed) {
                consumed[ai] = true;
                let mut members = vec![Allocation {
                    id: bag[ai].id,
                    amount: bag[ai].amount,
                }];
                for &ci in &chosen {
                    let j = cands[ci].0;
                    consumed[j] = true;
                    members.push(Allocation {
                        id: bag[j].id,
                        amount: bag[j].amount,
                    });
                }
                let net = members.iter().map(|m| m.amount).sum();
                let size = members.len();
                groups.push(Group {
                    members,
                    origin: "subset-sum".to_string(),
                    net,
                    reason: Some(format!("subset sum of {size} lots")),
                });
            }
        }

        let residual = bag
            .into_iter()
            .zip(consumed)
            .filter_map(|(item, used)| (!used).then_some(item))
            .collect();
        Resolution { groups, residual }
    }
}

/// Meet-in-the-middle: find a subset of `cands` (each `(global_idx, id,
/// magnitude > 0)`) whose magnitudes sum within `tol` of `target`, of size at
/// most `k_max`, picked by *closest to `target`*, then *fewest lots*, then a
/// seeded canonical key. Returns the chosen positions into `cands`, or `None`
/// when nothing lands in the band. Splitting `m` lots into halves makes this
/// `O(2^(m/2))` rather than `O(2^m)`, and it works in value space so the target
/// can be arbitrarily large (unlike a DP table keyed by amount).
fn best_subset(
    target: i64,
    tol: i64,
    k_max: usize,
    cands: &[(usize, ExtId, i64)],
    seed: u64,
) -> Option<Vec<usize>> {
    if k_max == 0 || cands.is_empty() {
        return None;
    }
    let m = cands.len();
    let mid = m / 2;

    // Every subset of a half (capped at `k_max` lots): (sum, popcount, bitmask,
    // seeded canonical key). `xor` of per-id seeded hashes is order-independent.
    let enumerate = |lo: usize, len: usize| -> Vec<(i64, u32, u32, u64)> {
        let mut out = Vec::new();
        for mask in 0u32..(1u32 << len) {
            let pc = mask.count_ones();
            if pc as usize > k_max {
                continue;
            }
            let mut sum = 0i64;
            let mut key = 0u64;
            for b in 0..len {
                if mask & (1u32 << b) != 0 {
                    sum += cands[lo + b].2;
                    key ^= seed_mix(cands[lo + b].1, seed);
                }
            }
            out.push((sum, pc, mask, key));
        }
        out
    };

    let left = enumerate(0, mid);
    let mut right = enumerate(mid, m - mid);
    right.sort_by_key(|&(s, _, _, _)| s);
    let rsums: Vec<i64> = right.iter().map(|&(s, _, _, _)| s).collect();

    // best = (err, card, key, left_mask, right_mask), minimized lexicographically
    // on (err, card, key).
    let mut best: Option<(i64, u32, u64, u32, u32)> = None;
    for &(sl, cl, ml, kl) in &left {
        let lo = target - tol - sl;
        let hi = target + tol - sl;
        let start = rsums.partition_point(|&s| s < lo);
        for &(sr, cr, mr, kr) in &right[start..] {
            if sr > hi {
                break;
            }
            let card = cl + cr;
            if card == 0 || card as usize > k_max {
                continue;
            }
            let cand = ((sl + sr - target).abs(), card, kl ^ kr, ml, mr);
            if best.is_none_or(|b| (cand.0, cand.1, cand.2) < (b.0, b.1, b.2)) {
                best = Some(cand);
            }
        }
    }

    best.map(|(_, _, _, ml, mr)| {
        let mut picks = Vec::new();
        for b in 0..mid {
            if ml & (1u32 << b) != 0 {
                picks.push(b);
            }
        }
        for b in 0..(m - mid) {
            if mr & (1u32 << b) != 0 {
                picks.push(mid + b);
            }
        }
        picks
    })
}

/// **Atomic many-to-one clearing.** For each anchor lot (largest first), find a
/// subset of opposite-sign lots whose magnitudes sum within `tol` of the
/// anchor's, forming a clearing group of *whole* lots (no splitting) of size at
/// most `max_group`; the small break stays **inside** the group as its `net`
/// (`|net| <= tol`), like [`agg_net`]. Unmatched lots pass to residual; a later
/// stage gets to reconsider them.
///
/// This fills the gap [`flow`] cannot: flow splits amounts fractionally, so it
/// can never enforce "use this credit *wholly or not at all*". `subset_sum`
/// works in whole-lot selection space (meet-in-the-middle, see [`best_subset`]),
/// which is exactly the canonical "one payment clears several invoices" shape.
/// It sits between [`agg_net`] (nets a bucket you *already keyed*) and [`flow`]
/// (divisible): it *discovers* the clearing set by amount search.
///
/// **Seeded, not random.** Anchor-order ties and equally-good subsets break on a
/// pure hash of row ids and `seed`, so a run is reproducible and distinct seeds
/// surface distinct matchings -- feed it to [`restart`] to try several and keep
/// the best. There is no warm basis (each shard is recomputed cold), but the
/// recompute is deterministic. Subset search is exponential, so keep pools small
/// with blocking ([`partition_by`]/[`windowed`]); degenerate blocks are capped.
///
/// A high-recall *proposer*: pair it with a strict *verifier* ([`material`],
/// [`whole_net`], [`accept_if`]) that dissolves weak groups back to residual.
pub fn subset_sum<E: 'static>(tol: i64, max_group: usize, seed: u64) -> Box<dyn Strategy<E>> {
    Box::new(SubsetSum {
        tol,
        max_group,
        seed,
        _e: PhantomData,
    })
}

struct Restart<E, F> {
    n: usize,
    seed: u64,
    factory: F,
    _e: PhantomData<E>,
}

impl<E, F> Strategy<E> for Restart<E, F>
where
    E: Clone,
    F: Fn(u64) -> Box<dyn Strategy<E>>,
{
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        let runs = self.n.max(1);
        // Keep the run with the most matched volume, then the fewest residual
        // lots, then the earliest seed (strict-`>` replacement leaves ties with
        // the lower index).
        let mut best: Option<Resolution<E>> = None;
        let mut best_score = (i64::MIN, i64::MIN);
        for i in 0..runs {
            let s = seed_mix(self.seed, i as u64);
            let r = (self.factory)(s).run(bag.clone());
            let matched: i64 = r
                .groups
                .iter()
                .flat_map(|g| &g.members)
                .map(|a| a.amount.abs())
                .sum();
            let score = (matched, -(r.residual.len() as i64));
            if best.is_none() || score > best_score {
                best_score = score;
                best = Some(r);
            }
        }
        best.expect("restart runs at least once")
    }
}

/// Run a seeded family of `n` attempts and keep the **best** result — the most
/// matched volume (`Σ|leg|` across groups), ties broken by fewest residual lots
/// then earliest seed. `factory(seed)` builds a fresh inner per attempt, each
/// fed a distinct seed derived from `seed` (e.g. `|s| subset_sum(tol, 4, s)`),
/// so a stochastic proposer's random-restart search stays fully reproducible.
///
/// The outer half of the *propose / verify* pattern: a high-recall stochastic
/// inner explores; `restart` selects the best whole [`Resolution`]; a strict
/// verifier ([`material`]/[`whole_net`]) downstream still gates what commits.
/// Each attempt re-runs on a clone of the same bag, so `inner` need not be
/// warm-startable. `n == 0` is treated as one attempt.
pub fn restart<E, F>(n: usize, seed: u64, factory: F) -> Box<dyn Strategy<E>>
where
    E: Clone + 'static,
    F: Fn(u64) -> Box<dyn Strategy<E>> + 'static,
{
    Box::new(Restart {
        n,
        seed,
        factory,
        _e: PhantomData,
    })
}

#[derive(Clone)]
struct PivotMeta<E> {
    outer: Item<E>,
    alt_original: i64,
}

struct Pivot<E, FA> {
    amount: FA,
    inner: Box<dyn Strategy<E>>,
}

fn prorate(total: i64, part: i64, denom: i64) -> i64 {
    if denom == 0 || total == 0 || part == 0 {
        return 0;
    }
    let num = part as i128 * total as i128;
    let den = denom as i128;
    (num / den) as i64
}

impl<E, FA> Strategy<E> for Pivot<E, FA>
where
    E: Clone,
    FA: Fn(&E) -> i64,
{
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        let mut meta: BTreeMap<ExtId, PivotMeta<E>> = BTreeMap::new();
        let inner_bag: Vec<Item<E>> = bag
            .into_iter()
            .map(|outer| {
                let alt_original = (self.amount)(&outer.data);
                let alt_amount = prorate(alt_original, outer.amount, outer.original);
                let id = outer.id;
                let data = outer.data.clone();
                meta.insert(
                    id,
                    PivotMeta {
                        outer,
                        alt_original,
                    },
                );
                Item {
                    id,
                    original: alt_original,
                    amount: alt_amount,
                    data,
                }
            })
            .collect();
        let mut res = self.inner.run(inner_bag);

        // Conservation airlock. An id consumed into groups in pivot numeraire
        // can map back to 0 parent units when its parent amount is tiny
        // relative to its pivot amount (e.g. bs_usd = 1, trx_amt = 4: a 2/4
        // pivot match prorates to floor(1*2/4) = 0). That leaves a phantom
        // 0-mass member in a group and silently leaks the parent cent.
        //
        // Contract: a row consumed into a group must carry >= 1 parent unit in
        // groups, or be returned whole to residual for later primary-numeraire
        // matching. Detect ids whose summed group pivot mass rounds to 0 parent
        // units, drop their group edges (deterministically, lowest id first via
        // BTreeSet/BTreeMap), and fold that pivot mass back into residual.
        {
            let mut group_pivot: BTreeMap<ExtId, i64> = BTreeMap::new();
            for g in &res.groups {
                for a in &g.members {
                    *group_pivot.entry(a.id).or_insert(0) += a.amount;
                }
            }
            let mut dissolve: BTreeSet<ExtId> = BTreeSet::new();
            for (id, &gp) in &group_pivot {
                if gp == 0 {
                    continue;
                }
                let Some(m) = meta.get(id) else { continue };
                if prorate(m.outer.amount, gp, m.alt_original) == 0 {
                    dissolve.insert(*id);
                }
            }
            if !dissolve.is_empty() {
                // Pull every dissolved id's pivot mass out of groups...
                let mut moved: BTreeMap<ExtId, i64> = BTreeMap::new();
                for g in &mut res.groups {
                    g.members.retain(|a| {
                        if dissolve.contains(&a.id) {
                            *moved.entry(a.id).or_insert(0) += a.amount;
                            false
                        } else {
                            true
                        }
                    });
                    g.net = g.members.iter().map(|a| a.amount).sum();
                }
                res.groups.retain(|g| !g.members.is_empty());
                // ...and fold it back into residual (pivot numeraire). The
                // conversion below re-maps these to parent units exactly.
                for (id, amt) in moved {
                    if amt == 0 {
                        continue;
                    }
                    if let Some(item) = res.residual.iter_mut().find(|i| i.id == id) {
                        item.amount += amt;
                    } else if let Some(m) = meta.get(&id) {
                        res.residual.push(Item {
                            id,
                            original: m.alt_original,
                            amount: amt,
                            data: m.outer.data.clone(),
                        });
                    }
                }
            }
        }

        // Collect pivot-numeraire output parts per id in deterministic output
        // order: group members first, then residuals. Convert all parts for an
        // id together so their outer amounts sum exactly to the input outer
        // residual, with any integer rounding remainder assigned to the last
        // part for that id.
        let mut parts: BTreeMap<ExtId, Vec<(usize, Option<usize>, i64)>> = BTreeMap::new();
        for (gi, g) in res.groups.iter().enumerate() {
            for (mi, a) in g.members.iter().enumerate() {
                parts
                    .entry(a.id)
                    .or_default()
                    .push((gi, Some(mi), a.amount));
            }
        }
        for (ri, item) in res.residual.iter().enumerate() {
            parts
                .entry(item.id)
                .or_default()
                .push((ri, None, item.amount));
        }

        let mut group_amounts: Vec<Vec<i64>> = res
            .groups
            .iter()
            .map(|g| vec![0; g.members.len()])
            .collect();
        let mut residual_amounts: Vec<i64> = vec![0; res.residual.len()];
        // Ids the inner matcher actually returned (groups ∪ residual). Any
        // incoming id missing from this set was dropped by inner -- its lane
        // amount forward-floored to 0 and a zero-dropping leaf (e.g. `flow`)
        // discarded it. Such rows are unmatchable in this numeraire and must be
        // returned whole to residual below, or their parent mass leaks.
        let accounted: BTreeSet<ExtId> = parts.keys().copied().collect();
        for (id, ps) in parts {
            let Some(m) = meta.get(&id) else { continue };
            let mut converted = Vec::with_capacity(ps.len());
            let mut sum = 0i64;
            for (_, _, amt) in &ps {
                let v = prorate(m.outer.amount, *amt, m.alt_original);
                converted.push(v);
                sum += v;
            }
            if let Some(last) = converted.last_mut() {
                *last += m.outer.amount - sum;
            }
            for ((idx, mi, _), v) in ps.into_iter().zip(converted) {
                if let Some(mi) = mi {
                    group_amounts[idx][mi] = v;
                } else {
                    residual_amounts[idx] = v;
                }
            }
        }

        let groups = res
            .groups
            .into_iter()
            .enumerate()
            .filter_map(|(gi, mut g)| {
                for (mi, a) in g.members.iter_mut().enumerate() {
                    a.amount = group_amounts[gi][mi];
                }
                // Drop zero-mass members (e.g. a pivot target of 0, whose row
                // carries no pivot mass and whose parent mass flows to residual
                // via remainder-to-last). A 0 member adds nothing to net or
                // conservation, so dropping it is safe and keeps groups honest.
                g.members.retain(|a| a.amount != 0);
                if g.members.is_empty() {
                    return None;
                }
                g.net = g.members.iter().map(|a| a.amount).sum();
                Some(g)
            })
            .collect();
        let mut residual: Vec<Item<E>> = res
            .residual
            .into_iter()
            .enumerate()
            .filter_map(|(ri, mut i)| {
                let m = meta.remove(&i.id)?;
                i.original = m.outer.original;
                i.amount = residual_amounts[ri];
                (i.amount != 0).then_some(i)
            })
            .collect();
        // Conservation closure: re-emit any incoming id the inner matcher
        // dropped entirely (see `accounted`). It is unmatched in this numeraire,
        // so return it to residual at its full incoming parent amount. `meta`
        // still owns every such id (only group/residual ids were removed above).
        for (id, m) in meta {
            if !accounted.contains(&id) && m.outer.amount != 0 {
                residual.push(Item {
                    id,
                    original: m.outer.original,
                    amount: m.outer.amount,
                    data: m.outer.data,
                });
            }
        }
        Resolution { groups, residual }
    }
}

/// Temporarily switch the active numeraire for `inner`, then translate every
/// produced allocation and residual back to the caller's numeraire.
pub fn pivot<E: Clone + 'static, FA>(
    amount: FA,
    inner: Box<dyn Strategy<E>>,
) -> Box<dyn Strategy<E>>
where
    FA: Fn(&E) -> i64 + 'static,
{
    Box::new(Pivot { amount, inner })
}

// ---------------------------------------------------------------------------
// Soakers — terminal classifiers for the residual tail
// ---------------------------------------------------------------------------
//
// A soaker is *not* a matcher: it consumes leftover residual lots into groups
// whose non-zero `net` is expected and meaningful (a variance, a write-off, an
// "unmatched" class). Where the committing primitives pull rows they are
// *certain* net, soakers terminate the cascade by classifying what is left.
// `Singleton` mode emits one group per residual lot; `Bucket` mode collects
// residuals sharing a `key` into one labelled class.

/// How a soaker shapes the residual it consumes: one group per lot, or one
/// group per `key` bucket.
#[derive(Clone, Copy)]
pub enum SoakMode {
    Singleton,
    Bucket,
}

/// Emit the soaked `items` as either singleton groups or one bucketed group,
/// stamping `origin` (suffixed with the bucket key in `Bucket` mode).
fn soak_emit<E, K>(
    groups: &mut Vec<Group>,
    buckets: &mut HashMap<K, Vec<Item<E>>>,
    mode: SoakMode,
    origin: &str,
    key: Option<K>,
    item: Item<E>,
) where
    K: Hash + Eq,
{
    match mode {
        SoakMode::Singleton => groups.push(Group {
            members: vec![Allocation {
                id: item.id,
                amount: item.amount,
            }],
            origin: origin.to_string(),
            net: item.amount,
            reason: None,
        }),
        SoakMode::Bucket => buckets.entry(key.unwrap()).or_default().push(item),
    }
}

/// Flush the per-key buckets accumulated by [`soak_emit`] into one group each.
fn soak_flush<E, K: ToString>(
    groups: &mut Vec<Group>,
    buckets: HashMap<K, Vec<Item<E>>>,
    origin: &str,
) {
    for (k, items) in buckets {
        let net: i64 = items.iter().map(|i| i.amount).sum();
        groups.push(Group {
            members: items
                .iter()
                .map(|i| Allocation {
                    id: i.id,
                    amount: i.amount,
                })
                .collect(),
            origin: format!("{}:{}", origin, k.to_string()),
            net,
            reason: None,
        });
    }
}

struct SoakSmall<E, FK> {
    tol: Tol,
    key: FK,
    mode: SoakMode,
    origin: String,
    _e: PhantomData<E>,
}

impl<E, K, FK> Strategy<E> for SoakSmall<E, FK>
where
    K: Hash + Eq + Clone + ToString,
    FK: Fn(&Item<E>) -> K,
{
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        let mut groups = Vec::new();
        let mut residual = Vec::new();
        let mut buckets: HashMap<K, Vec<Item<E>>> = HashMap::new();
        for item in bag {
            // Immaterial == within `tol` measured against the lot's own
            // `original` (the materiality scale). A zero-amount lot is never
            // soaked — there is nothing to classify.
            let immaterial = item.amount != 0 && item.amount.abs() <= self.tol.slack(item.original);
            if immaterial {
                let k = matches!(self.mode, SoakMode::Bucket).then(|| (self.key)(&item));
                soak_emit(&mut groups, &mut buckets, self.mode, &self.origin, k, item);
            } else {
                residual.push(item);
            }
        }
        soak_flush(&mut groups, buckets, &self.origin);
        Resolution { groups, residual }
    }
}

/// Consume residual lots whose current amount is **immaterial versus their own
/// `original`** line amount, measured by `tol` (absolute, or relative bps of the
/// original — see [`Tol`]). Material lots pass through untouched as residual.
/// `Singleton` mode produces one variance group per soaked lot; `Bucket` mode
/// groups soaked lots by `key`. This is the "soak the rounding tail" classifier
/// you place at the end of a cascade, before [`soak_all`].
///
/// Note the scale is the lot's `original`, not its current `amount`, so this is
/// only meaningful after an upstream leaf has *shrunk* a lot's residual (a
/// partial `flow`/`pivot` match): on a fresh bag `amount == original` and a
/// relative `tol` is a no-op. An absolute `tol` applies regardless.
pub fn soak_small<E: 'static, K, FK>(
    tol: impl Into<Tol>,
    mode: SoakMode,
    origin: impl Into<String>,
    key: FK,
) -> Box<dyn Strategy<E>>
where
    K: Hash + Eq + Clone + ToString + 'static,
    FK: Fn(&Item<E>) -> K + 'static,
{
    Box::new(SoakSmall {
        tol: tol.into(),
        key,
        mode,
        origin: origin.into(),
        _e: PhantomData,
    })
}

struct SoakIf<E, FP, FK> {
    pred: FP,
    key: FK,
    mode: SoakMode,
    origin: String,
    _e: PhantomData<E>,
}

impl<E, K, FP, FK> Strategy<E> for SoakIf<E, FP, FK>
where
    K: Hash + Eq + Clone + ToString,
    FP: Fn(&Item<E>) -> bool,
    FK: Fn(&Item<E>) -> K,
{
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        let mut groups = Vec::new();
        let mut residual = Vec::new();
        let mut buckets: HashMap<K, Vec<Item<E>>> = HashMap::new();
        for item in bag {
            if item.amount != 0 && (self.pred)(&item) {
                let k = matches!(self.mode, SoakMode::Bucket).then(|| (self.key)(&item));
                soak_emit(&mut groups, &mut buckets, self.mode, &self.origin, k, item);
            } else {
                residual.push(item);
            }
        }
        soak_flush(&mut groups, buckets, &self.origin);
        Resolution { groups, residual }
    }
}

/// Consume residual lots for which `pred` holds (and whose amount is non-zero)
/// into singleton or bucketed classes; everything else passes through as
/// residual. The general soaker that [`soak_small`] (predicate = "immaterial")
/// and [`soak_all`] (predicate = "true") specialize: reach for it when the
/// classification rule is neither pure materiality nor "everything", e.g. soak
/// only one sign, or only lots flagged by a payload field.
pub fn soak_if<E: 'static, K, FP, FK>(
    pred: FP,
    mode: SoakMode,
    origin: impl Into<String>,
    key: FK,
) -> Box<dyn Strategy<E>>
where
    K: Hash + Eq + Clone + ToString + 'static,
    FP: Fn(&Item<E>) -> bool + 'static,
    FK: Fn(&Item<E>) -> K + 'static,
{
    Box::new(SoakIf {
        pred,
        key,
        mode,
        origin: origin.into(),
        _e: PhantomData,
    })
}

struct SoakAll<E, FK> {
    key: FK,
    mode: SoakMode,
    origin: String,
    _e: PhantomData<E>,
}

impl<E, K, FK> Strategy<E> for SoakAll<E, FK>
where
    K: Hash + Eq + Clone + ToString,
    FK: Fn(&Item<E>) -> K,
{
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        let mut groups = Vec::new();
        let mut buckets: HashMap<K, Vec<Item<E>>> = HashMap::new();
        for item in bag {
            if item.amount == 0 {
                continue;
            }
            let k = matches!(self.mode, SoakMode::Bucket).then(|| (self.key)(&item));
            soak_emit(&mut groups, &mut buckets, self.mode, &self.origin, k, item);
        }
        soak_flush(&mut groups, buckets, &self.origin);
        Resolution {
            groups,
            residual: Vec::new(),
        }
    }
}

/// Consume *every* remaining non-zero residual lot into singleton or bucketed
/// groups, leaving an empty residual. A terminal classifier, not a matcher:
/// non-zero group nets are expected and represent unmatched / variance /
/// write-off classes. Place it last in a [`seq`] to give every leftover lot a
/// home.
pub fn soak_all<E: 'static, K, FK>(
    mode: SoakMode,
    origin: impl Into<String>,
    key: FK,
) -> Box<dyn Strategy<E>>
where
    K: Hash + Eq + Clone + ToString + 'static,
    FK: Fn(&Item<E>) -> K + 'static,
{
    Box::new(SoakAll {
        key,
        mode,
        origin: origin.into(),
        _e: PhantomData,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bag(items: &[(ExtId, i64)]) -> Vec<Item<i64>> {
        items.iter().map(|&(id, a)| Item::new(id, a, a)).collect()
    }
    fn ids(g: &Group) -> Vec<ExtId> {
        let mut m = g.member_ids();
        m.sort();
        m
    }
    fn conserves<E>(input: usize, r: &Resolution<E>) {
        let g: usize = r.groups.iter().map(|g| g.members.len()).sum();
        assert_eq!(g + r.residual.len(), input, "conservation violated");
    }

    #[test]
    fn agg_net_relative_tolerance_scales_with_smallest_leg() {
        // Net residual of 9 against a smallest leg of 10_000: 10 bps = 10, so it
        // is accepted; 5 bps = 5, so it is rejected. Absolute tol would need to
        // know the magnitude up front; Rel derives it from the bucket.
        let b = bag(&[(1, 10_000), (2, -9_991)]);
        let s = agg_net(|_a: &i64| 0u64, Tol::Rel { bps: 10, floor: 0 });
        let r = s.run(b);
        conserves(2, &r);
        assert_eq!(r.groups.len(), 1, "9 <= 10 (10bps of 10_000)");

        let b = bag(&[(1, 10_000), (2, -9_991)]);
        let s = agg_net(|_a: &i64| 0u64, Tol::Rel { bps: 5, floor: 0 });
        let r = s.run(b);
        conserves(2, &r);
        assert_eq!(r.groups.len(), 0, "9 > 5 (5bps of 10_000)");
    }

    #[test]
    fn agg_net_relative_floor_applies_to_tiny_buckets() {
        // 10 bps of 100 is 0, but the floor of 3 lets a residual of 2 net.
        let b = bag(&[(1, 100), (2, -98)]);
        let s = agg_net(|_a: &i64| 0u64, Tol::Rel { bps: 10, floor: 3 });
        let r = s.run(b);
        conserves(2, &r);
        assert_eq!(r.groups.len(), 1);
    }

    #[test]
    fn labeled_stamps_reason_on_groups_but_not_residual() {
        let b = bag(&[(1, 5), (2, -5), (3, 7)]);
        let s = labeled("S3a exact", exact_1to1(|_| Some(0)));
        let r = s.run(b);
        conserves(3, &r);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(
            r.groups[0].reason.as_deref(),
            Some("S3a exact: exact 1:1 pair")
        );
        // The leftover row is residual, not a group, so it carries no label.
        assert_eq!(r.residual.len(), 1);
        assert_eq!(r.residual[0].id, 3);
    }

    #[test]
    fn labeled_prepends_to_inner_detail() {
        // An inner label is preserved as detail when an outer label wraps it.
        let b = bag(&[(1, 5), (2, -5)]);
        let s = labeled("outer", labeled("inner", exact_1to1(|_| Some(0))));
        let r = s.run(b);
        assert_eq!(
            r.groups[0].reason.as_deref(),
            Some("outer: inner: exact 1:1 pair")
        );
    }

    #[test]
    fn exact_pairs_and_leaves_residual() {
        let b = bag(&[(1, 5), (2, -5), (3, 5), (4, 3)]);
        let s = exact_1to1(|_| Some(0));
        let r = s.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 1);
        assert!(r.groups[0].member_ids().contains(&2));
        assert_eq!(r.residual.len(), 2);
    }

    #[test]
    fn agg_accepts_netting_bucket() {
        let b = bag(&[(1, 100), (2, -60), (3, -40), (4, 7)]);
        let s = agg_net(|_a: &i64| 0u64, 0);
        let r = s.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 0);
        let b = bag(&[(1, 100), (2, -60), (3, -40), (4, 7)]);
        let s = agg_net(|_a: &i64| 0u64, 10);
        let r = s.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].members.len(), 4);
    }

    #[test]
    fn signal_groups_net_and_cascade() {
        let b = bag(&[(1, 50), (2, -50), (3, 9)]);
        let s = signal_group(
            |a: &i64| if *a == 9 { vec![] } else { vec![10] },
            Tol::Abs(0),
            16,
        );
        let r = s.run(b);
        conserves(3, &r);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(ids(&r.groups[0]), vec![1, 2]);
        assert_eq!(r.residual.len(), 1);
    }

    #[test]
    fn signal_groups_accept_relative_tol() {
        // Bucket nets to 5 against a smallest leg of 1000. 10 bps = 1, so the
        // residual 5 is rejected; 60 bps = 6 accepts it. Absolute tol would
        // have to know the leg magnitude up front; Rel derives it from the
        // bucket, matching `agg_net`.
        let b = bag(&[(1, 1000), (2, -995)]);
        let tight = signal_group(|_: &i64| vec![7u64], Tol::Rel { bps: 10, floor: 0 }, 16);
        let r = tight.run(b.clone());
        assert_eq!(r.groups.len(), 0);
        assert_eq!(r.residual.len(), 2);

        let loose = signal_group(|_: &i64| vec![7u64], Tol::Rel { bps: 60, floor: 0 }, 16);
        let r = loose.run(b);
        conserves(2, &r);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(ids(&r.groups[0]), vec![1, 2]);
    }

    /// A deliberately non-maximal leaf: it groups *at most one* opposite-sign
    /// equal-magnitude pair per call and returns everything else as residual.
    /// One `run` is not enough to clear a fully matchable bag, so it is the
    /// honest probe for the fixed-point loop's repeat-until-stable contract.
    struct OnePair;
    impl Strategy<i64> for OnePair {
        fn run(&self, bag: Vec<Item<i64>>) -> Resolution<i64> {
            for i in 0..bag.len() {
                for j in (i + 1)..bag.len() {
                    if bag[i].amount == -bag[j].amount && bag[i].amount != 0 {
                        let mut residual = Vec::new();
                        let mut members = Vec::new();
                        for (k, item) in bag.into_iter().enumerate() {
                            if k == i || k == j {
                                members.push(Allocation {
                                    id: item.id,
                                    amount: item.amount,
                                });
                            } else {
                                residual.push(item);
                            }
                        }
                        let g = Group {
                            members,
                            origin: "onepair".into(),
                            net: 0,
                            reason: None,
                        };
                        return Resolution {
                            groups: vec![g],
                            residual,
                        };
                    }
                }
            }
            Resolution {
                groups: vec![],
                residual: bag,
            }
        }
    }

    #[test]
    fn fixed_point_drives_a_non_maximal_leaf_to_completion() {
        // A single pass of OnePair clears exactly one pair.
        let once = OnePair;
        let r = once.run(bag(&[(1, 5), (2, -5), (3, 7), (4, -7)]));
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.residual.len(), 2);

        // Wrapped in fixed_point, it iterates until nothing more matches.
        let fp = fixed_point(Box::new(OnePair), 16);
        let r = fp.run(bag(&[(1, 5), (2, -5), (3, 7), (4, -7)]));
        conserves(4, &r);
        assert_eq!(r.groups.len(), 2, "both pairs found across passes");
        assert_eq!(r.residual.len(), 0);
    }

    #[test]
    fn fixed_point_leaves_unmatchable_residual_and_terminates() {
        // 3 and 4 (+7, +3) can never pair: the loop must converge, not spin.
        let fp = fixed_point(Box::new(OnePair), 16);
        let r = fp.run(bag(&[(1, 5), (2, -5), (3, 7), (4, 3)]));
        conserves(4, &r);
        assert_eq!(r.groups.len(), 1);
        let mut left: Vec<ExtId> = r.residual.iter().map(|i| i.id).collect();
        left.sort();
        assert_eq!(left, vec![3, 4]);
    }

    #[test]
    fn fixed_point_respects_the_pass_cap() {
        // With a 1-pass cap it behaves exactly like a single OnePair run.
        let fp = fixed_point(Box::new(OnePair), 1);
        let r = fp.run(bag(&[(1, 5), (2, -5), (3, 7), (4, -7)]));
        conserves(4, &r);
        assert_eq!(r.groups.len(), 1, "cap of 1 means one pass");
        assert_eq!(r.residual.len(), 2);
    }

    #[test]
    fn when_cascade_routes_to_different_children_and_conserves() {
        // `seq(when(pred, a), b)` is the cascade replacement for the old
        // two-way `branch`: the |·|==5 pair nets in the first child, the rest
        // flow on to the second.
        let b = bag(&[(1, 5), (2, -5), (3, 7), (4, -7)]);
        let s = seq(vec![
            when(|a: &i64| a.unsigned_abs() == 5, agg_net(|_a: &i64| 1u64, 0)),
            agg_net(|_a: &i64| 2u64, 0),
        ]);
        let r = s.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 2);
        assert_eq!(r.residual.len(), 0);
    }

    #[test]
    fn partition_by_with_picks_a_per_key_subtree() {
        // Key 0 nets its bucket; key 1 gets identity() and passes through.
        let b = bag(&[(1, 5), (2, -5), (3, 7), (4, -7)]);
        let s = partition_by_with(
            |a: &i64| (a.unsigned_abs() == 5) as u8,
            |k: &u8| {
                if *k == 1 {
                    agg_net(|_a: &i64| 0u64, 0)
                } else {
                    identity()
                }
            },
        );
        let r = s.run(b);
        conserves(4, &r);
        // Only the ±5 shard (key 1) nets; the ±7 shard (key 0) is identity.
        assert_eq!(r.groups.len(), 1);
        let mut rem: Vec<ExtId> = r.residual.iter().map(|i| i.id).collect();
        rem.sort_unstable();
        assert_eq!(rem, vec![3, 4]);
    }

    #[test]
    fn windowed_blocks_far_matches() {
        let b = vec![Item::new(1, 5, (1i64, 5i64)), Item::new(2, -5, (100, -5))];
        let inner = exact_1to1(|_| Some(0));
        let r = {
            let w = windowed(|d: &(i64, i64)| d.0, 3, inner);
            w.run(b)
        };
        assert_eq!(r.groups.len(), 0);
        assert_eq!(r.residual.len(), 2);
    }

    #[test]
    fn windowed_finds_near_match_across_band_boundary() {
        let b = vec![Item::new(1, 5, (4i64, 5i64)), Item::new(2, -5, (7, -5))];
        let inner = exact_1to1(|_| Some(0));
        let r = {
            let w = windowed(|d: &(i64, i64)| d.0, 3, inner);
            w.run(b)
        };
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.residual.len(), 0);
    }

    #[test]
    fn running_zero_segments_at_balance_clears() {
        let b = vec![
            Item::new(1, 100, (1i64, 100i64)),
            Item::new(2, -100, (2, -100)),
            Item::new(3, 50, (3, 50)),
            Item::new(4, -30, (4, -30)),
            Item::new(5, -20, (5, -20)),
        ];
        let s = running_zero(|d: &(i64, i64)| d.0, 0);
        let r = s.run(b);
        conserves(5, &r);
        assert_eq!(r.groups.len(), 2);
        assert_eq!(r.groups[0].member_ids(), vec![1, 2]);
        assert_eq!(r.groups[1].member_ids(), vec![3, 4, 5]);
    }

    #[test]
    fn running_zero_leaves_uncleared_tail() {
        let b = vec![
            Item::new(1, 100, (1i64, 100i64)),
            Item::new(2, -100, (2, -100)),
            Item::new(3, 7, (3, 7)),
        ];
        let s = running_zero(|d: &(i64, i64)| d.0, 0);
        let r = s.run(b);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.residual.len(), 1);
        assert_eq!(r.residual[0].id, 3);
    }

    #[test]
    fn seq_then_partition_compose() {
        let pipeline = partition_by(
            |a: &i64| a.signum().unsigned_abs(),
            || seq(vec![exact_1to1(|_| Some(0))]),
        );
        let b = bag(&[(1, 4), (2, -4), (3, 4), (4, -4)]);
        let r = pipeline.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 2);
    }

    #[test]
    fn accept_if_rejects_groups_back_to_residual() {
        // exact_1to1 forms two equal-magnitude pairs; accept only the pair whose
        // magnitude is 5. The rejected pair (magnitude 7) must reappear in the
        // residual, fully intact, so nothing is lost.
        let b = bag(&[(1, 5), (2, -5), (3, 7), (4, -7)]);
        let s = accept_if(
            |g: &Group| g.members.iter().all(|a| a.amount.abs() == 5),
            exact_1to1(|_| Some(0)),
        );
        let r = s.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(ids(&r.groups[0]), vec![1, 2]);
        let mut left: Vec<ExtId> = r.residual.iter().map(|i| i.id).collect();
        left.sort();
        assert_eq!(left, vec![3, 4]);
        // The rejected lots keep their amounts, so a downstream stage sees them
        // exactly as the inner strategy received them.
        for i in &r.residual {
            assert_eq!(i.amount.abs(), 7);
        }
    }

    // Inner that fully matches a small slice of two big rows: a +M/-M pair drawn
    // from rows whose `original` is `big`, leaving the rest as ground residual.
    struct Slice {
        moved: i64,
    }
    impl Strategy<i64> for Slice {
        fn run(&self, bag: Vec<Item<i64>>) -> Resolution<i64> {
            let m: HashMap<ExtId, Item<i64>> = bag.into_iter().map(|i| (i.id, i)).collect();
            let groups = vec![Group {
                members: vec![
                    Allocation {
                        id: 1,
                        amount: self.moved,
                    },
                    Allocation {
                        id: 2,
                        amount: -self.moved,
                    },
                ],
                origin: "flow".into(),
                net: 0,
                reason: None,
            }];
            let mut t1 = m[&1].clone();
            t1.amount = m[&1].original - self.moved;
            let mut t2 = m[&2].clone();
            t2.amount = m[&2].original + self.moved;
            let residual = [t1, t2].into_iter().filter(|i| i.amount != 0).collect();
            Resolution { groups, residual }
        }
    }

    #[test]
    fn material_rel_prunes_a_sliver_of_a_big_row() {
        // Rows born at 1000/-1000; flow cleared only 30/-30. Moved volume M=60,
        // reference R=2000. 5% of R is 100, so 60 <= 100 -> immaterial, dissolved
        // back to residual whole.
        let s = material(
            Tol::Rel { bps: 500, floor: 0 },
            Box::new(Slice { moved: 30 }),
        );
        let r = s.run(bag(&[(1, 1000), (2, -1000)]));
        assert!(r.groups.is_empty(), "60 <= 5% of 2000");
        let mut left: Vec<(ExtId, i64)> = r.residual.iter().map(|i| (i.id, i.amount)).collect();
        left.sort();
        assert_eq!(left, vec![(1, 1000), (2, -1000)], "rows return whole");
    }

    #[test]
    fn material_rel_keeps_a_substantial_match() {
        // Same rows, but 300/-300 cleared. M=600 > 5% of 2000 (=100), so the
        // match is material and survives; only the uncleared tail is residual.
        let s = material(
            Tol::Rel { bps: 500, floor: 0 },
            Box::new(Slice { moved: 300 }),
        );
        let r = s.run(bag(&[(1, 1000), (2, -1000)]));
        assert_eq!(r.groups.len(), 1, "600 > 5% of 2000");
        assert_eq!(ids(&r.groups[0]), vec![1, 2]);
        // Conservation in amount: matched 300 + residual 700 per side = original.
        let by_id = |id: ExtId| {
            r.residual
                .iter()
                .filter(|i| i.id == id)
                .map(|i| i.amount)
                .sum::<i64>()
        };
        assert_eq!(by_id(1), 700);
        assert_eq!(by_id(2), -700);
    }

    #[test]
    fn material_abs_prunes_below_fixed_floor_ignoring_original() {
        // Abs measures moved volume against a fixed magnitude, not original.
        // M=60: dropped at Abs(100), kept at Abs(50).
        let s = material(Tol::Abs(100), Box::new(Slice { moved: 30 }));
        let r = s.run(bag(&[(1, 1000), (2, -1000)]));
        assert!(r.groups.is_empty(), "60 <= 100");

        let s = material(Tol::Abs(50), Box::new(Slice { moved: 30 }));
        let r = s.run(bag(&[(1, 1000), (2, -1000)]));
        assert_eq!(r.groups.len(), 1, "60 > 50");
    }

    #[test]
    fn material_dissolve_merges_onto_existing_residual_and_conserves() {
        // The dissolved legs fold onto the rows' surviving residual tails rather
        // than appearing as duplicate lots: one residual entry per id, summing
        // to the original.
        let s = material(Tol::Abs(1000), Box::new(Slice { moved: 30 }));
        let r = s.run(bag(&[(1, 1000), (2, -1000)]));
        assert!(r.groups.is_empty());
        assert_eq!(r.residual.len(), 2, "no duplicate lots");
        let mut left: Vec<(ExtId, i64)> = r.residual.iter().map(|i| (i.id, i.amount)).collect();
        left.sort();
        assert_eq!(left, vec![(1, 1000), (2, -1000)]);
    }

    #[test]
    fn subset_sum_clears_one_against_many_whole_lots() {
        // +100 anchor clears the {-60, -40} subset exactly; the -25 stays whole
        // in residual (atomic: never split to top up the match).
        let s = subset_sum(0, 8, 0);
        let r = s.run(bag(&[(1, 100), (2, -60), (3, -40), (4, -25)]));
        conserves(4, &r);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].origin, "subset-sum");
        assert_eq!(r.groups[0].net, 0);
        assert_eq!(ids(&r.groups[0]), vec![1, 2, 3]);
        assert_eq!(r.residual.len(), 1);
        assert_eq!(r.residual[0].id, 4);
        assert_eq!(r.residual[0].amount, -25, "unmatched lot stays whole");
    }

    #[test]
    fn subset_sum_keeps_break_inside_within_tol() {
        // -98 subset against +100: net +2 stays inside the group when tol >= 2.
        let s = subset_sum(2, 8, 0);
        let r = s.run(bag(&[(1, 100), (2, -60), (3, -38)]));
        conserves(3, &r);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].net, 2);
        assert_eq!(ids(&r.groups[0]), vec![1, 2, 3]);
        assert!(r.residual.is_empty());

        // tol 1 < 2: no in-band subset, everything stays residual.
        let s = subset_sum(1, 8, 0);
        let r = s.run(bag(&[(1, 100), (2, -60), (3, -38)]));
        conserves(3, &r);
        assert!(r.groups.is_empty());
        assert_eq!(r.residual.len(), 3);
    }

    #[test]
    fn subset_sum_respects_the_group_size_cap() {
        // max_group = 2 admits only 1:1; +100 has no single partner here, so the
        // three-lot clear is forbidden and all rows stay residual.
        let s = subset_sum(0, 2, 0);
        let r = s.run(bag(&[(1, 100), (2, -60), (3, -40)]));
        conserves(3, &r);
        assert!(r.groups.is_empty());
        assert_eq!(r.residual.len(), 3);

        // Raising the cap lets the {-60,-40} subset clear the anchor.
        let s = subset_sum(0, 3, 0);
        let r = s.run(bag(&[(1, 100), (2, -60), (3, -40)]));
        assert_eq!(r.groups.len(), 1);
        assert_eq!(ids(&r.groups[0]), vec![1, 2, 3]);
    }

    #[test]
    fn subset_sum_is_reproducible_across_runs() {
        // Same seed -> byte-identical grouping, twice.
        let run = || {
            let s = subset_sum(0, 8, 7);
            let r = s.run(bag(&[(1, 100), (2, -50), (3, -50), (4, -100), (5, 100)]));
            r.groups.iter().map(|g| (ids(g), g.net)).collect::<Vec<_>>()
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn restart_keeps_the_attempt_that_matches_most() {
        // A toy inner that matches a clean pair only on odd seeds, nothing on
        // even. Across several seeds `restart` must surface the matching run.
        struct Toy {
            seed: u64,
        }
        impl Strategy<i64> for Toy {
            fn run(&self, bag: Vec<Item<i64>>) -> Resolution<i64> {
                if self.seed % 2 == 1 {
                    let members = bag
                        .iter()
                        .map(|i| Allocation {
                            id: i.id,
                            amount: i.amount,
                        })
                        .collect();
                    Resolution {
                        groups: vec![Group {
                            members,
                            origin: "toy".into(),
                            net: bag.iter().map(|i| i.amount).sum(),
                            reason: None,
                        }],
                        residual: vec![],
                    }
                } else {
                    Resolution {
                        groups: vec![],
                        residual: bag,
                    }
                }
            }
        }
        let s = restart(6, 0, |seed| Box::new(Toy { seed }));
        let r = s.run(bag(&[(1, 5), (2, -5)]));
        conserves(2, &r);
        assert_eq!(r.groups.len(), 1, "the matching seed wins");
        assert!(r.residual.is_empty());
    }

    #[test]
    fn restart_drives_subset_sum_to_a_full_clear() {
        // Two interleaved +100/{-50,-50} settlements plus a distractor. A strict
        // verifier keeps only exact clears; `restart` searches seeds for the
        // anchor order that clears the most. End state conserves regardless.
        let factory = |seed: u64| material(Tol::Abs(0), subset_sum(0, 8, seed));
        let s = restart(8, 42, factory);
        let r = s.run(bag(&[(1, 100), (2, -50), (3, -50), (4, -50), (5, 50)]));
        conserves(5, &r);
        // Whatever it commits nets exactly zero (the verifier's bar).
        assert!(r.groups.iter().all(|g| g.net == 0));
        let matched: i64 = r
            .groups
            .iter()
            .flat_map(|g| &g.members)
            .map(|a| a.amount.abs())
            .sum();
        let residual: i64 = r.residual.iter().map(|i| i.amount.abs()).sum();
        assert_eq!(matched + residual, 300, "conservation in amount");
    }

    #[test]
    fn whole_net_reclaims_tail_and_keeps_break_within_tol() {
        // Inner: +100 matched 97 against -97, with a +3 ground tail on line 1.
        struct Partial;
        impl Strategy<i64> for Partial {
            fn run(&self, bag: Vec<Item<i64>>) -> Resolution<i64> {
                let m: HashMap<ExtId, Item<i64>> = bag.into_iter().map(|i| (i.id, i)).collect();
                let groups = vec![Group {
                    members: vec![
                        Allocation { id: 1, amount: 97 },
                        Allocation { id: 2, amount: -97 },
                    ],
                    origin: "flow".into(),
                    net: 0,
                    reason: None,
                }];
                let mut tail = m[&1].clone();
                tail.amount = 3;
                Resolution {
                    groups,
                    residual: vec![tail],
                }
            }
        }
        // tol >= 3: reclaim the tail, match the whole lines, keep net +3 inside.
        let s = whole_net(Tol::Abs(5), Box::new(Partial));
        let r = s.run(bag(&[(1, 100), (2, -97)]));
        conserves(2, &r);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].net, 3);
        assert_eq!(ids(&r.groups[0]), vec![1, 2]);
        assert!(r.residual.is_empty()); // the +3 tail was reclaimed into the whole line

        // tol < 3: dissolve, both lines return to ground whole.
        let s = whole_net(Tol::Abs(2), Box::new(Partial));
        let r = s.run(bag(&[(1, 100), (2, -97)]));
        conserves(2, &r);
        assert!(r.groups.is_empty());
        let left: Vec<(ExtId, i64)> = r.residual.iter().map(|i| (i.id, i.amount)).collect();
        assert_eq!(left, vec![(1, 100), (2, -97)]); // whole lines, not the 97/3 split
    }

    #[test]
    fn whole_net_collapses_groups_sharing_a_line() {
        // Line 1 (+100) is split across two groups: +60 in A, +40 in B. They
        // share id 1, so the whole-line view is ONE settlement, not two.
        struct Split;
        impl Strategy<i64> for Split {
            fn run(&self, _bag: Vec<Item<i64>>) -> Resolution<i64> {
                let groups = vec![
                    Group {
                        members: vec![
                            Allocation { id: 1, amount: 60 },
                            Allocation { id: 2, amount: -60 },
                        ],
                        origin: "a".into(),
                        net: 0,
                        reason: None,
                    },
                    Group {
                        members: vec![
                            Allocation { id: 1, amount: 40 },
                            Allocation { id: 3, amount: -40 },
                        ],
                        origin: "b".into(),
                        net: 0,
                        reason: None,
                    },
                ];
                Resolution {
                    groups,
                    residual: vec![],
                }
            }
        }
        let s = whole_net(Tol::Abs(0), Box::new(Split));
        let r = s.run(bag(&[(1, 100), (2, -60), (3, -40)]));
        conserves(3, &r);
        // One merged settlement of whole lines, line 1 appearing once at +100.
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].origin, "settlement");
        assert_eq!(r.groups[0].net, 0);
        let mut mem: Vec<(ExtId, i64)> = r.groups[0]
            .members
            .iter()
            .map(|a| (a.id, a.amount))
            .collect();
        mem.sort();
        assert_eq!(mem, vec![(1, 100), (2, -60), (3, -40)]);
        assert!(r.residual.is_empty());
    }

    #[test]
    fn accept_if_size_cap_and_minority_side() {
        // A big one-to-many group (1 vs 4) and a clean small pair. Reject groups
        // bigger than 3 lots; the small pair survives, the big group dissolves.
        let b = bag(&[
            (1, 40),
            (2, -10),
            (3, -10),
            (4, -10),
            (5, -10),
            (6, 8),
            (7, -8),
        ]);
        let s = accept_if(
            |g: &Group| g.members.len() <= 3,
            agg_net(|a: &i64| if a.unsigned_abs() == 8 { 1u64 } else { 0u64 }, 0),
        );
        let r = s.run(b);
        conserves(7, &r);
        assert_eq!(r.groups.len(), 1, "only the small pair is accepted");
        assert_eq!(ids(&r.groups[0]), vec![6, 7]);
        assert_eq!(r.residual.len(), 5, "the over-large group is dissolved");
    }

    /// A leaf that emits a fixed, possibly-interlocking set of groups (members
    /// referenced by id), passing everything else to residual. Lets us drive
    /// `coalesce` with a known hypergraph regardless of any matcher's heuristics.
    struct EmitGroups(Vec<Vec<(ExtId, i64)>>);
    impl Strategy<i64> for EmitGroups {
        fn run(&self, bag: Vec<Item<i64>>) -> Resolution<i64> {
            let claimed: BTreeSet<ExtId> = self.0.iter().flatten().map(|&(id, _)| id).collect();
            let groups = self
                .0
                .iter()
                .map(|m| Group {
                    members: m
                        .iter()
                        .map(|&(id, amount)| Allocation { id, amount })
                        .collect(),
                    origin: "emit".into(),
                    net: m.iter().map(|&(_, a)| a).sum(),
                    reason: None,
                })
                .collect();
            let residual = bag
                .into_iter()
                .filter(|i| !claimed.contains(&i.id))
                .collect();
            Resolution { groups, residual }
        }
    }

    #[test]
    fn coalesce_merges_groups_that_share_a_row() {
        // Two groups interlock through row 2 (split 60/40): coalesce unions them
        // into one cluster and sums row 2's allocations back to 100.
        let inner = EmitGroups(vec![
            vec![(1, 100), (2, -60)],
            vec![(2, -40), (3, 100), (4, -100)],
        ]);
        let b = bag(&[(1, 100), (2, -100), (3, 100), (4, -100), (9, 7)]);
        let s = coalesce("settlement", Box::new(inner));
        let r = s.run(b);
        conserves(5, &r);
        assert_eq!(r.groups.len(), 1, "the two interlocking groups merge");
        let g = &r.groups[0];
        assert_eq!(g.origin, "settlement");
        assert_eq!(ids(g), vec![1, 2, 3, 4]);
        // Row 2's split allocations are summed into a single clean edge.
        let two = g.members.iter().find(|a| a.id == 2).unwrap();
        assert_eq!(two.amount, -100);
        assert_eq!(g.net, 0);
        // The untouched row stays in residual.
        assert_eq!(r.residual.len(), 1);
        assert_eq!(r.residual[0].id, 9);
    }

    #[test]
    fn coalesce_keeps_disjoint_groups_separate() {
        // Two groups with no shared row remain two distinct clusters, each
        // uniformly stamped with the coalesce origin.
        let inner = EmitGroups(vec![vec![(1, 5), (2, -5)], vec![(3, 7), (4, -7)]]);
        let b = bag(&[(1, 5), (2, -5), (3, 7), (4, -7)]);
        let s = coalesce("settlement", Box::new(inner));
        let r = s.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 2, "disjoint components stay separate");
        assert!(r.groups.iter().all(|g| g.origin == "settlement"));
    }

    #[test]
    fn settle_folds_flow_arcs_into_one_settlement() {
        // The partial-match shape `flow` exposes as two arcs (id 3 drawing from
        // ids 1 and 2): `settle` coalesces them into a single {1,2,3} cluster
        // with id 3 summed to one clean -250 edge. This is the settlement view
        // `flow` no longer bakes in.
        #[derive(Clone)]
        struct Tx {
            date: i64,
        }
        let spec = FlowSpec::<Tx>::new()
            .penalty(1_000_000.0)
            .window(3)
            .block_key(|t: &Tx| t.date)
            .cost_lot(|a: &Tx, a_amt, b: &Tx, b_amt| {
                Some(1.0 + (a_amt + b_amt).abs() as f64 * 0.1 + (a.date - b.date).abs() as f64)
            });
        let s = settle(spec);
        let r = s.run(vec![
            Item::new(1, 100, Tx { date: 0 }),
            Item::new(2, 200, Tx { date: 1 }),
            Item::new(3, -250, Tx { date: 0 }),
        ]);
        assert_eq!(r.groups.len(), 1, "arcs fold into one settlement");
        let g = &r.groups[0];
        assert_eq!(g.origin, "flow");
        assert_eq!(g.net, 0);
        assert_eq!(ids(g), vec![1, 2, 3]);
        let three = g.members.iter().find(|a| a.id == 3).unwrap();
        assert_eq!(three.amount, -250, "id 3's two arcs sum to one clean edge");
        assert_eq!(r.residual.iter().map(|i| i.amount).sum::<i64>(), 50);
    }

    #[test]
    fn pivot_converts_back_to_outer_amount() {
        let b = vec![Item::new(1, 110, (100i64,)), Item::new(2, -110, (-100i64,))];
        let s = pivot(|d: &(i64,)| d.0, exact_1to1(|_| Some(0)));
        let r = s.run(b);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].net, 0);
        assert_eq!(
            r.groups[0].members,
            vec![
                Allocation { id: 1, amount: 110 },
                Allocation {
                    id: 2,
                    amount: -110
                }
            ]
        );
    }

    // Partial-consumption inner: matches half of each row's pivot mass into one
    // shared group, leaves the remainder in residual. Used to exercise the
    // pivot conservation airlock.
    struct HalfMatch;
    impl Strategy<(i64,)> for HalfMatch {
        fn run(&self, bag: Vec<Item<(i64,)>>) -> Resolution<(i64,)> {
            let mut members = Vec::new();
            let mut residual = Vec::new();
            for it in bag {
                let half = it.amount / 2;
                members.push(Allocation {
                    id: it.id,
                    amount: half,
                });
                let mut r = it.clone();
                r.amount = it.amount - half;
                residual.push(r);
            }
            let net = members.iter().map(|a| a.amount).sum();
            let groups = vec![Group {
                members,
                origin: "half".into(),
                net,
                reason: None,
            }];
            Resolution { groups, residual }
        }
    }

    #[test]
    fn pivot_dissolves_rows_that_round_to_zero_parent() {
        // X (id 1): parent 1, pivot 4. Half-matched (2/4) prorates to
        // floor(1*2/4) = 0 parent units -> airlock dissolves X's group edge and
        // returns the whole cent to residual.
        // Z (id 2): parent 100, pivot 4. Half-matched (2/4) = 50 parent units,
        // representable -> kept in the group untouched.
        let b = vec![Item::new(1, 1, (4i64,)), Item::new(2, 100, (4i64,))];
        let s = pivot(|d: &(i64,)| d.0, Box::new(HalfMatch));
        let r = s.run(b);

        // Group retains only Z at 50; the phantom 0-mass X edge is gone.
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].members, vec![Allocation { id: 2, amount: 50 }]);

        // Residual: X whole at 1 (conservation preserved), Z remainder at 50.
        let mut res: Vec<(ExtId, i64)> = r.residual.iter().map(|i| (i.id, i.amount)).collect();
        res.sort();
        assert_eq!(res, vec![(1, 1), (2, 50)]);
    }

    #[test]
    fn pivot_zero_target_is_safe() {
        // X (id 1): parent 5, pivot 0 -- no pivot mass to match. No panic
        // (prorate guards the zero denominator), the full parent flows to
        // residual, and no phantom 0-mass member is left in the group.
        let b = vec![Item::new(1, 5, (0i64,)), Item::new(2, 100, (4i64,))];
        let s = pivot(|d: &(i64,)| d.0, Box::new(HalfMatch));
        let r = s.run(b);

        // Group keeps only Z; X carries no mass and is not present.
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].members, vec![Allocation { id: 2, amount: 50 }]);

        // Conservation: X's full 5 parent units sit in residual.
        let mut res: Vec<(ExtId, i64)> = r.residual.iter().map(|i| (i.id, i.amount)).collect();
        res.sort();
        assert_eq!(res, vec![(1, 5), (2, 50)]);
    }

    // Inner that drops zero-amount lots, exactly like `flow` and the soakers.
    // Used to exercise the pivot forward-floor conservation closure.
    struct DropZeros;
    impl Strategy<(i64,)> for DropZeros {
        fn run(&self, bag: Vec<Item<(i64,)>>) -> Resolution<(i64,)> {
            Resolution {
                groups: Vec::new(),
                residual: bag.into_iter().filter(|i| i.amount != 0).collect(),
            }
        }
    }

    #[test]
    fn pivot_reemits_forward_floored_rows() {
        // id 1: a parent residual of 1 (of original 100) at lane 3 forward-maps
        // to floor(3*1/100) = 0, so a zero-dropping inner discards it. The pivot
        // conservation closure must return it whole to residual at parent 1, or
        // the cent leaks and the recon airlock aborts. This is the exact leak in
        // `fixed_point(seq(pivot(l1), pivot(l2), pivot(l3, flow)))` when an
        // earlier lane leaves a sub-lane-unit residual.
        // id 2: parent 50 at lane 50 survives untouched.
        let b = vec![
            Item {
                id: 1,
                original: 100,
                amount: 1,
                data: (3i64,),
            },
            Item {
                id: 2,
                original: 50,
                amount: 50,
                data: (50i64,),
            },
        ];
        let s = pivot(|d: &(i64,)| d.0, Box::new(DropZeros));
        let r = s.run(b);
        assert!(r.groups.is_empty());
        let mut res: Vec<(ExtId, i64)> = r.residual.iter().map(|i| (i.id, i.amount)).collect();
        res.sort();
        assert_eq!(res, vec![(1, 1), (2, 50)]);
    }

    // --- soakers ---------------------------------------------------------

    // Items here carry their own `original` (= amount) via `bag`, except where
    // a materiality test needs a distinct original, built explicitly.

    #[test]
    fn soak_small_abs_threshold_singletons() {
        // amounts: two immaterial (<=5), one material.
        let s = soak_small(5, SoakMode::Singleton, "rounding", |_: &Item<i64>| 0u64);
        let r = s.run(bag(&[(1, 3), (2, -2), (3, 100)]));
        conserves(3, &r);
        // Two soaked singletons, one material lot left as residual.
        assert_eq!(r.groups.len(), 2);
        assert!(
            r.groups
                .iter()
                .all(|g| g.members.len() == 1 && g.origin == "rounding")
        );
        assert_eq!(r.residual.len(), 1);
        assert_eq!(r.residual[0].id, 3);
    }

    #[test]
    fn soak_small_bps_against_original() {
        // amount 10 on an original of 1000 = 100 bps (1%); soak under 200 bps.
        let items = vec![
            Item {
                id: 1,
                original: 1000,
                amount: 10,
                data: 0,
            }, // immaterial: 100 bps
            Item {
                id: 2,
                original: 1000,
                amount: 50,
                data: 0,
            }, // material:   500 bps
        ];
        let s = soak_small(
            Tol::Rel { bps: 200, floor: 0 },
            SoakMode::Singleton,
            "var",
            |_: &Item<i64>| 0u64,
        );
        let r = s.run(items);
        conserves(2, &r);
        assert_eq!(ids(&r.groups[0]), vec![1]);
        assert_eq!(r.residual.iter().map(|i| i.id).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn soak_small_bucket_groups_by_key() {
        // Soak immaterial lots, bucketing by sign of the amount.
        let key = |i: &Item<i64>| if i.amount > 0 { "pos" } else { "neg" };
        let s = soak_small(5, SoakMode::Bucket, "tail", key);
        let r = s.run(bag(&[(1, 3), (2, 4), (3, -2), (4, 100)]));
        conserves(4, &r);
        // One bucket per sign among the soaked lots; the material lot stays.
        assert_eq!(r.groups.len(), 2);
        assert!(r.groups.iter().all(|g| g.origin.starts_with("tail:")));
        assert_eq!(r.residual.iter().map(|i| i.id).collect::<Vec<_>>(), vec![4]);
    }

    #[test]
    fn soak_if_predicate_selects() {
        // Soak only negative residual lots; positives pass through.
        let s = soak_if(
            |i: &Item<i64>| i.amount < 0,
            SoakMode::Singleton,
            "shorts",
            |_: &Item<i64>| 0u64,
        );
        let r = s.run(bag(&[(1, 50), (2, -30), (3, -10)]));
        conserves(3, &r);
        let mut soaked: Vec<ExtId> = r.groups.iter().flat_map(ids).collect();
        soaked.sort();
        assert_eq!(soaked, vec![2, 3]);
        assert_eq!(r.residual.iter().map(|i| i.id).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn soak_all_terminates_residual() {
        let s = soak_all(SoakMode::Singleton, "unmatched", |_: &Item<i64>| 0u64);
        let r = s.run(bag(&[(1, 50), (2, -30), (3, 0)]));
        // Zero-amount lots are dropped (nothing to classify); the rest soak and
        // the residual is fully drained.
        assert!(r.residual.is_empty());
        let mut soaked: Vec<ExtId> = r.groups.iter().flat_map(ids).collect();
        soaked.sort();
        assert_eq!(soaked, vec![1, 2]);
        assert!(r.groups.iter().all(|g| g.net != 0));
    }

    #[test]
    fn soak_all_bucket_nets_per_key() {
        let key = |i: &Item<i64>| if i.amount > 0 { 1u64 } else { 2u64 };
        let s = soak_all(SoakMode::Bucket, "class", key);
        let r = s.run(bag(&[(1, 50), (2, 30), (3, -20)]));
        assert!(r.residual.is_empty());
        // pos bucket nets 80, neg bucket nets -20.
        let mut nets: Vec<i64> = r.groups.iter().map(|g| g.net).collect();
        nets.sort();
        assert_eq!(nets, vec![-20, 80]);
    }

    // --- Group metrics & Tol scale references ----------------------------

    fn group(members: &[(ExtId, i64)]) -> Group {
        let members: Vec<Allocation> = members
            .iter()
            .map(|&(id, amount)| Allocation { id, amount })
            .collect();
        let net = members.iter().map(|a| a.amount).sum();
        Group {
            members,
            origin: "test".into(),
            net,
            reason: None,
        }
    }

    #[test]
    fn group_metrics() {
        // legs: +1_000_000, -999_000, -1_200  -> net -200
        let g = group(&[(1, 1_000_000), (2, -999_000), (3, -1_200)]);
        assert_eq!(g.size(), 3);
        assert_eq!(g.abs_net(), 200);
        assert_eq!(g.max_abs(), 1_000_000);
        assert_eq!(g.min_abs(), 1_200);
        assert_eq!(g.min_side(), 1); // one positive leg, two negative
    }

    #[test]
    fn clean_rel_vs_relmax_pick_different_scale_legs() {
        // The partial-alloc example: net -200 against a {1_000_000, 999_000,
        // 1_200} bucket. tol = 5bps, $1.00 floor.
        let g = group(&[(1, 1_000_000), (2, -999_000), (3, -1_200)]);
        // Rel scales off the smallest leg (1_200): slack = max(100, 0) = 100.
        assert!(
            !g.clean(Tol::Rel { bps: 5, floor: 100 }),
            "200 > 100 -> dirty"
        );
        // RelMax scales off the largest leg (1_000_000): slack = max(100, 500).
        assert!(
            g.clean(Tol::RelMax { bps: 5, floor: 100 }),
            "200 <= 500 -> clean"
        );
        // Abs ignores the legs entirely.
        assert!(g.clean(Tol::Abs(200)));
        assert!(!g.clean(Tol::Abs(199)));
    }

    #[test]
    fn agg_net_relmax_accepts_what_rel_rejects() {
        // Same shape as a leaf bucket: the large leg lets RelMax accept a net
        // that Rel (smallest-leg) would reject.
        let b = bag(&[(1, 1_000_000), (2, -999_000), (3, -1_200)]);
        let rel = agg_net(|_: &i64| 0u64, Tol::Rel { bps: 5, floor: 100 });
        assert_eq!(rel.run(b.clone()).groups.len(), 0);
        let relmax = agg_net(|_: &i64| 0u64, Tol::RelMax { bps: 5, floor: 100 });
        let r = relmax.run(b);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].net, -200);
    }
}
