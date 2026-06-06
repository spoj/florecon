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

use std::collections::{BTreeMap, BTreeSet, HashMap};

// The incremental min-cost-flow matcher is just the arbiter behind the `flow`
// strategy leaf, so it lives here as one strategy among many. Kept in its own
// file; `flow::Group` stays distinct from this module's own `Group`.
pub mod flow;
pub use flow::{flow, Allocation, ExtId, FlowSpec};

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
        self.members.iter().map(|a| a.amount.abs()).max().unwrap_or(0)
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
/// `run` takes `&mut self`, so a node *may* carry state across calls (e.g. the
/// stateful [`flow`] leaf keeps a live warm basis, and [`partition_by`] holds
/// one warm child per shard). Statefulness
/// is an opt-in capability, not a mandate: the cheap leaves (`agg_net`,
/// `exact_1to1`, `signal_group`, …) ignore `&mut` and recompute, staying
/// stateless by convention. A node that *does* hold state owes a warm-vs-cold
/// determinism guarantee (see [`flow`]'s cross-check).
pub trait Strategy<E> {
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E>;
}

impl<E> Strategy<E> for Box<dyn Strategy<E>> {
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
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
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
        #[cfg(not(target_arch = "wasm32"))]
        let timed = std::env::var_os("FLORECON_TIME").is_some();
        #[cfg(target_arch = "wasm32")]
        let timed = false;
        let mut groups = Vec::new();
        let mut residual = bag;
        for (i, step) in self.steps.iter_mut().enumerate() {
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
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
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
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
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
    order.into_iter().map(|r| buckets.remove(&r).unwrap()).collect()
}

impl<E> Strategy<E> for Coalesce<E> {
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
        let r = self.inner.run(bag);
        let groups = r.groups;

        // Connected components over the groups; sum each member id's edges
        // within a component into one clean allocation per row. Residual is
        // untouched — coalesce only regroups what was already matched.
        let comps = group_components(&groups);
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
            // Every cluster carries the coalesce `origin` uniformly. A lone group
            // (nothing merged) keeps its inner `reason`; a merged cluster gets a
            // synthesized one.
            let reason = if comp.len() == 1 {
                groups[comp[0]].reason.clone()
            } else {
                Some(format!("coalesced {} groups", comp.len()))
            };
            out.push(Group {
                members,
                origin: self.origin.clone(),
                net,
                reason,
            });
        }
        Resolution {
            groups: out,
            residual: r.residual,
        }
    }
}

/// Which way a small edge moves under [`trim`] / [`snap`].
enum EdgeOp {
    /// Cut the small edge to the floor (residual).
    Trim,
    /// Fold the small edge onto the row's dominant edge.
    Snap,
}

struct EdgeReshape<E> {
    op: EdgeOp,
    tol: Tol,
    inner: Box<dyn Strategy<E>>,
}

impl<E: Clone> Strategy<E> for EdgeReshape<E> {
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
        // A group `Allocation` carries neither the row's `original` (the Tol
        // scale) nor its payload (needed to (re)materialize a residual lot), so
        // snapshot the input once. Both `trim` and `snap` need the scale.
        let src: HashMap<ExtId, Item<E>> = bag.iter().map(|i| (i.id, i.clone())).collect();
        let r = self.inner.run(bag);
        let groups = r.groups;
        let residual = r.residual;

        // Flatten every incidence into one edge table: each row's group edges
        // plus its single floor (residual) edge. `trim`/`snap` differ only in
        // where a sub-Tol edge's mass goes; both preserve per-id totals, so
        // `groups ⊎ residual = input` holds in summed `(id, amount)`.
        #[derive(Clone, Copy)]
        enum Loc {
            Group(usize),
            Floor,
        }
        struct Edge {
            id: ExtId,
            loc: Loc,
            amount: i64,
        }
        let mut edges: Vec<Edge> = Vec::new();
        for (gi, g) in groups.iter().enumerate() {
            for a in &g.members {
                edges.push(Edge {
                    id: a.id,
                    loc: Loc::Group(gi),
                    amount: a.amount,
                });
            }
        }
        // Merge residual to one floor edge per id (usually already one).
        let mut floor_ix: HashMap<ExtId, usize> = HashMap::new();
        for it in &residual {
            match floor_ix.get(&it.id) {
                Some(&ei) => edges[ei].amount += it.amount,
                None => {
                    floor_ix.insert(it.id, edges.len());
                    edges.push(Edge {
                        id: it.id,
                        loc: Loc::Floor,
                        amount: it.amount,
                    });
                }
            }
        }

        let mut by_id: HashMap<ExtId, Vec<usize>> = HashMap::new();
        for (ei, e) in edges.iter().enumerate() {
            by_id.entry(e.id).or_default().push(ei);
        }

        for (id, idxs) in &by_id {
            // Tol scale is the row's own `original` — the materiality idiom:
            // "an edge under x% of the line is immaterial".
            let scale = src.get(id).map(|i| i.original).unwrap_or(0);
            let slack = self.tol.slack(scale);
            match self.op {
                EdgeOp::Trim => {
                    // Cut every small *group* edge to the floor. The floor is
                    // never a source; create it if the row had no residual.
                    for &ei in idxs {
                        let small = edges[ei].amount != 0 && edges[ei].amount.abs() <= slack;
                        if matches!(edges[ei].loc, Loc::Group(_)) && small {
                            let amt = edges[ei].amount;
                            edges[ei].amount = 0;
                            match floor_ix.get(id) {
                                Some(&fi) => edges[fi].amount += amt,
                                None => {
                                    floor_ix.insert(*id, edges.len());
                                    edges.push(Edge {
                                        id: *id,
                                        loc: Loc::Floor,
                                        amount: amt,
                                    });
                                }
                            }
                        }
                    }
                }
                EdgeOp::Snap => {
                    // Fold every non-dominant small edge onto the row's dominant
                    // edge (largest magnitude; floor eligible both ways). The
                    // dominant never folds into itself, so a lone clean edge is
                    // always left intact. Ties resolve to the first (smallest)
                    // edge index for determinism.
                    let mut dom = idxs[0];
                    for &ei in &idxs[1..] {
                        if edges[ei].amount.unsigned_abs() > edges[dom].amount.unsigned_abs() {
                            dom = ei;
                        }
                    }
                    for &ei in idxs {
                        if ei == dom {
                            continue;
                        }
                        let small = edges[ei].amount != 0 && edges[ei].amount.abs() <= slack;
                        if small {
                            let amt = edges[ei].amount;
                            edges[ei].amount = 0;
                            edges[dom].amount += amt;
                        }
                    }
                }
            }
        }

        // Rebuild groups from surviving group edges (origin/reason preserved,
        // member order kept) and the residual from the floor edges.
        let mut members_by_g: Vec<Vec<Allocation>> = groups.iter().map(|_| Vec::new()).collect();
        let mut floor: BTreeMap<ExtId, i64> = BTreeMap::new();
        for e in &edges {
            match e.loc {
                Loc::Group(gi) => {
                    if e.amount != 0 {
                        members_by_g[gi].push(Allocation {
                            id: e.id,
                            amount: e.amount,
                        });
                    }
                }
                Loc::Floor => {
                    *floor.entry(e.id).or_insert(0) += e.amount;
                }
            }
        }
        let mut out_groups = Vec::new();
        for (g, members) in groups.into_iter().zip(members_by_g) {
            if members.is_empty() {
                continue;
            }
            let net = members.iter().map(|a| a.amount).sum();
            out_groups.push(Group {
                members,
                origin: g.origin,
                net,
                reason: g.reason,
            });
        }
        let mut out_residual = Vec::new();
        for (id, amount) in floor {
            if amount == 0 {
                continue;
            }
            if let Some(orig) = src.get(&id) {
                out_residual.push(Item {
                    id,
                    original: orig.original,
                    amount,
                    data: orig.data.clone(),
                });
            }
        }
        Resolution {
            groups: out_groups,
            residual: out_residual,
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
/// multiset as the input groups'. To move material between groups and residual,
/// compose with [`trim`] or [`snap`]. A lone group keeps its inner `reason`.
pub fn coalesce<E: 'static>(
    origin: impl Into<String>,
    inner: Box<dyn Strategy<E>>,
) -> Box<dyn Strategy<E>> {
    Box::new(Coalesce {
        origin: origin.into(),
        inner,
    })
}

/// **Trim** sub-`tol` edges to the floor: every group allocation whose
/// magnitude is within `tol` (measured against its row's `original`, the
/// materiality idiom) is cut and leaked to the residual. One-directional — mass
/// only ever moves matched→residual.
///
/// Cutting a *bridging* edge (a row shared by two or more groups) disconnects
/// those groups, so `trim` before [`coalesce`] yields smaller islands and more
/// residual, while `coalesce` before `trim` runs the threshold against the
/// already-summed cluster edges (fewer fall below `tol`) for larger islands and
/// little residual.
///
/// Post-condition: every surviving group edge is material (`> tol`).
/// Conservation holds — a cut edge moves intact (same id, same amount) from its
/// group to the residual.
pub fn trim<E: Clone + 'static>(
    tol: impl Into<Tol>,
    inner: Box<dyn Strategy<E>>,
) -> Box<dyn Strategy<E>> {
    Box::new(EdgeReshape {
        op: EdgeOp::Trim,
        tol: tol.into(),
        inner,
    })
}

/// **Snap** sub-`tol` edges onto the row's dominant edge instead of the floor.
/// For each row, every edge within `tol` (against the row's `original`) that is
/// not the row's largest-magnitude edge is folded into that dominant edge. The
/// **floor (residual) is an eligible edge both ways**, so one rule covers:
///
/// * tail under `tol`, matched dominant → the residual tail folds **into the
///   group** (completes the row),
/// * match under `tol`, residual dominant → the weak match folds **into the
///   floor** (gives it up),
/// * a small cross-edge → folds onto the row's main group (consolidates, with no
///   new residual).
///
/// The dominant edge never folds into itself, so a lone clean edge is always
/// left intact — `snap` never silently un-matches a material row. Post-condition
/// and conservation match [`trim`]; the two differ only in the sink.
pub fn snap<E: Clone + 'static>(
    tol: impl Into<Tol>,
    inner: Box<dyn Strategy<E>>,
) -> Box<dyn Strategy<E>> {
    Box::new(EdgeReshape {
        op: EdgeOp::Snap,
        tol: tol.into(),
        inner,
    })
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
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
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
    /// Builds a fresh child subtree the first time a shard key is seen. Receives
    /// the shard key, so [`partition_by_with`] can choose a per-key subtree;
    /// [`partition_by`] passes a key-ignoring factory.
    factory: Box<ShardFactory<E, K>>,
    /// One independent child per shard key. Each child owns its own state
    /// (notably its own warm flow basis), so per-shard warm-start is
    /// automatic and the flow leaf never needs to know it is sharded.
    children: HashMap<K, Box<dyn Strategy<E>>>,
}

impl<E, K, FK> Strategy<E> for PartitionBy<E, K, FK>
where
    K: Hash + Eq + Clone,
    FK: Fn(&E) -> K,
{
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
        let mut shards: HashMap<K, Vec<Item<E>>> = HashMap::new();
        for item in bag {
            shards.entry((self.key)(&item.data)).or_default().push(item);
        }
        // Re-run existing children whose shard received no items this solve with
        // an empty bag, so their warm state drops the departed rows instead of
        // retaining stale members until the shard happens to reappear.
        for k in self.children.keys() {
            shards.entry(k.clone()).or_default();
        }
        // Split the borrows: `factory` builds new children, `children` is the
        // map being mutated. Both fields, disjoint, so no clone of self.
        let factory = &self.factory;
        let children = &mut self.children;
        let mut groups = Vec::new();
        let mut residual = Vec::new();
        for (k, items) in shards {
            if !children.contains_key(&k) {
                children.insert(k.clone(), factory(&k));
            }
            let r = children.get_mut(&k).unwrap().run(items);
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
        children: HashMap::new(),
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
        children: HashMap::new(),
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
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
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
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
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
    fn run(&mut self, mut bag: Vec<Item<E>>) -> Resolution<E> {
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
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
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
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
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
    fn run(&mut self, mut bag: Vec<Item<E>>) -> Resolution<E> {
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
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
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
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
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
        let residual = res
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
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
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
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
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
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
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
        let mut s = agg_net(|_a: &i64| 0u64, Tol::Rel { bps: 10, floor: 0 });
        let r = s.run(b);
        conserves(2, &r);
        assert_eq!(r.groups.len(), 1, "9 <= 10 (10bps of 10_000)");

        let b = bag(&[(1, 10_000), (2, -9_991)]);
        let mut s = agg_net(|_a: &i64| 0u64, Tol::Rel { bps: 5, floor: 0 });
        let r = s.run(b);
        conserves(2, &r);
        assert_eq!(r.groups.len(), 0, "9 > 5 (5bps of 10_000)");
    }

    #[test]
    fn agg_net_relative_floor_applies_to_tiny_buckets() {
        // 10 bps of 100 is 0, but the floor of 3 lets a residual of 2 net.
        let b = bag(&[(1, 100), (2, -98)]);
        let mut s = agg_net(|_a: &i64| 0u64, Tol::Rel { bps: 10, floor: 3 });
        let r = s.run(b);
        conserves(2, &r);
        assert_eq!(r.groups.len(), 1);
    }

    #[test]
    fn labeled_stamps_reason_on_groups_but_not_residual() {
        let b = bag(&[(1, 5), (2, -5), (3, 7)]);
        let mut s = labeled("S3a exact", exact_1to1(|_| Some(0)));
        let r = s.run(b);
        conserves(3, &r);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].reason.as_deref(), Some("S3a exact: exact 1:1 pair"));
        // The leftover row is residual, not a group, so it carries no label.
        assert_eq!(r.residual.len(), 1);
        assert_eq!(r.residual[0].id, 3);
    }

    #[test]
    fn labeled_prepends_to_inner_detail() {
        // An inner label is preserved as detail when an outer label wraps it.
        let b = bag(&[(1, 5), (2, -5)]);
        let mut s = labeled("outer", labeled("inner", exact_1to1(|_| Some(0))));
        let r = s.run(b);
        assert_eq!(r.groups[0].reason.as_deref(), Some("outer: inner: exact 1:1 pair"));
    }

    #[test]
    fn exact_pairs_and_leaves_residual() {
        let b = bag(&[(1, 5), (2, -5), (3, 5), (4, 3)]);
        let mut s = exact_1to1(|_| Some(0));
        let r = s.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 1);
        assert!(r.groups[0].member_ids().contains(&2));
        assert_eq!(r.residual.len(), 2);
    }

    #[test]
    fn agg_accepts_netting_bucket() {
        let b = bag(&[(1, 100), (2, -60), (3, -40), (4, 7)]);
        let mut s = agg_net(|_a: &i64| 0u64, 0);
        let r = s.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 0);
        let b = bag(&[(1, 100), (2, -60), (3, -40), (4, 7)]);
        let mut s = agg_net(|_a: &i64| 0u64, 10);
        let r = s.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].members.len(), 4);
    }

    #[test]
    fn signal_groups_net_and_cascade() {
        let b = bag(&[(1, 50), (2, -50), (3, 9)]);
        let mut s = signal_group(|a: &i64| if *a == 9 { vec![] } else { vec![10] }, Tol::Abs(0), 16);
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
        let mut tight = signal_group(|_: &i64| vec![7u64], Tol::Rel { bps: 10, floor: 0 }, 16);
        let r = tight.run(b.clone());
        assert_eq!(r.groups.len(), 0);
        assert_eq!(r.residual.len(), 2);

        let mut loose = signal_group(|_: &i64| vec![7u64], Tol::Rel { bps: 60, floor: 0 }, 16);
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
        fn run(&mut self, bag: Vec<Item<i64>>) -> Resolution<i64> {
            for i in 0..bag.len() {
                for j in (i + 1)..bag.len() {
                    if bag[i].amount == -bag[j].amount && bag[i].amount != 0 {
                        let mut residual = Vec::new();
                        let mut members = Vec::new();
                        for (k, item) in bag.into_iter().enumerate() {
                            if k == i || k == j {
                                members.push(Allocation { id: item.id, amount: item.amount });
                            } else {
                                residual.push(item);
                            }
                        }
                        let g = Group { members, origin: "onepair".into(), net: 0, reason: None };
                        return Resolution { groups: vec![g], residual };
                    }
                }
            }
            Resolution { groups: vec![], residual: bag }
        }
    }

    #[test]
    fn fixed_point_drives_a_non_maximal_leaf_to_completion() {
        // A single pass of OnePair clears exactly one pair.
        let mut once = OnePair;
        let r = once.run(bag(&[(1, 5), (2, -5), (3, 7), (4, -7)]));
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.residual.len(), 2);

        // Wrapped in fixed_point, it iterates until nothing more matches.
        let mut fp = fixed_point(Box::new(OnePair), 16);
        let r = fp.run(bag(&[(1, 5), (2, -5), (3, 7), (4, -7)]));
        conserves(4, &r);
        assert_eq!(r.groups.len(), 2, "both pairs found across passes");
        assert_eq!(r.residual.len(), 0);
    }

    #[test]
    fn fixed_point_leaves_unmatchable_residual_and_terminates() {
        // 3 and 4 (+7, +3) can never pair: the loop must converge, not spin.
        let mut fp = fixed_point(Box::new(OnePair), 16);
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
        let mut fp = fixed_point(Box::new(OnePair), 1);
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
        let mut s = seq(vec![
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
        let mut s = partition_by_with(
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
            let mut w = windowed(|d: &(i64, i64)| d.0, 3, inner);
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
            let mut w = windowed(|d: &(i64, i64)| d.0, 3, inner);
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
        let mut s = running_zero(|d: &(i64, i64)| d.0, 0);
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
        let mut s = running_zero(|d: &(i64, i64)| d.0, 0);
        let r = s.run(b);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.residual.len(), 1);
        assert_eq!(r.residual[0].id, 3);
    }

    #[test]
    fn seq_then_partition_compose() {
        let mut pipeline = partition_by(
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
        let mut s = accept_if(
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

    #[test]
    fn accept_if_size_cap_and_minority_side() {
        // A big one-to-many group (1 vs 4) and a clean small pair. Reject groups
        // bigger than 3 lots; the small pair survives, the big group dissolves.
        let b = bag(&[(1, 40), (2, -10), (3, -10), (4, -10), (5, -10), (6, 8), (7, -8)]);
        let mut s = accept_if(
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
        fn run(&mut self, bag: Vec<Item<i64>>) -> Resolution<i64> {
            let claimed: BTreeSet<ExtId> =
                self.0.iter().flatten().map(|&(id, _)| id).collect();
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
            let residual = bag.into_iter().filter(|i| !claimed.contains(&i.id)).collect();
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
        let mut s = coalesce("settlement", Box::new(inner));
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
        let mut s = coalesce("settlement", Box::new(inner));
        let r = s.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 2, "disjoint components stay separate");
        assert!(r.groups.iter().all(|g| g.origin == "settlement"));
    }

    #[test]
    fn trim_cuts_small_edges_to_residual_and_splits_a_cluster() {
        // Row 2 bridges two settlements but only by a tiny 3-unit overlap (split
        // -97 / -3). `trim` before `coalesce` cuts the -3 edge to residual, so
        // the two groups no longer share a row and fall into separate clusters.
        let inner = EmitGroups(vec![
            vec![(1, 100), (2, -97)],
            vec![(2, -3), (3, 100), (4, -100)],
        ]);
        let b = bag(&[(1, 100), (2, -100), (3, 100), (4, -100)]);
        let mut s = coalesce("settlement", trim(Tol::Abs(10), Box::new(inner)));
        let r = s.run(b);
        // Per-id amount conservation (count conservation does not hold once a
        // row is split between a kept edge and a leaked residual).
        let mut acc: BTreeMap<ExtId, i64> = BTreeMap::new();
        for g in &r.groups {
            for a in &g.members {
                *acc.entry(a.id).or_default() += a.amount;
            }
        }
        for i in &r.residual {
            *acc.entry(i.id).or_default() += i.amount;
        }
        assert_eq!(acc.get(&2), Some(&-100), "row 2's mass is preserved");
        assert_eq!(r.groups.len(), 2, "weak tie trimmed -> two clusters");
        assert_eq!(r.residual.len(), 1);
        assert_eq!(r.residual[0].id, 2);
        assert_eq!(r.residual[0].amount, -3);

        // A *material* overlap is not trimmed, so the cluster still merges.
        let inner = EmitGroups(vec![
            vec![(1, 100), (2, -60)],
            vec![(2, -40), (3, 100), (4, -100)],
        ]);
        let b = bag(&[(1, 100), (2, -100), (3, 100), (4, -100)]);
        let mut s = coalesce("settlement", trim(Tol::Abs(10), Box::new(inner)));
        let r = s.run(b);
        assert_eq!(r.groups.len(), 1, "strong tie survives -> one cluster");
        assert!(r.residual.is_empty(), "nothing trimmed");
    }

    // Inner that matches row 1 against row 2 for `matched` units, leaving row
    // 1's `original - matched` tail (and any other row) in the residual.
    struct Partial {
        matched: i64,
    }
    impl Strategy<i64> for Partial {
        fn run(&mut self, bag: Vec<Item<i64>>) -> Resolution<i64> {
            let g = Group {
                members: vec![
                    Allocation { id: 1, amount: self.matched },
                    Allocation { id: 2, amount: -self.matched },
                ],
                origin: "partial".into(),
                net: 0,
                reason: None,
            };
            let residual = bag
                .into_iter()
                .filter_map(|mut i| match i.id {
                    2 => None,
                    1 => {
                        i.amount = i.original - self.matched;
                        (i.amount != 0).then_some(i)
                    }
                    _ => Some(i),
                })
                .collect();
            Resolution {
                groups: vec![g],
                residual,
            }
        }
    }

    #[test]
    fn snap_absorbs_small_tail_into_the_matched_group() {
        // Row 1 (original 100) matched 80; its 20 tail is the minority edge, so
        // it folds into the dominant group edge -> the row shows whole (100) and
        // the group nets the 20. No orphan residual singleton.
        let b = bag(&[(1, 100), (2, -80), (9, 7)]);
        let mut s = snap(Tol::Abs(25), Box::new(Partial { matched: 80 }));
        let r = s.run(b);
        assert_eq!(r.groups.len(), 1);
        let g = &r.groups[0];
        assert_eq!(g.members.iter().find(|a| a.id == 1).unwrap().amount, 100);
        assert_eq!(g.net, 20);
        assert_eq!(r.residual.len(), 1, "only the unrelated row is left");
        assert_eq!(r.residual[0].id, 9);
    }

    #[test]
    fn snap_leaks_small_match_when_the_residual_dominates() {
        // Row 1 (original 100) matched only 20; now the matched edge is the
        // minority and the 80 residual is dominant, so the weak match folds into
        // the floor -> the row goes wholly to residual, the group keeps only its
        // counterparty.
        let b = bag(&[(1, 100), (2, -20), (9, 7)]);
        let mut s = snap(Tol::Abs(25), Box::new(Partial { matched: 20 }));
        let r = s.run(b);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(ids(&r.groups[0]), vec![2], "row 1 left the match");
        let one = r.residual.iter().find(|i| i.id == 1).unwrap();
        assert_eq!(one.amount, 100, "row 1 is whole in residual");
    }

    #[test]
    fn snap_tol_scales_with_the_row_original() {
        // Relative Tol measures the tail against the row's own `original`. A 20
        // tail on a 100 line is 20%: below 30% it absorbs, above 10% it does not.
        let mut s = snap(Tol::Rel { bps: 3000, floor: 0 }, Box::new(Partial { matched: 80 }));
        let r = s.run(bag(&[(1, 100), (2, -80)]));
        assert_eq!(r.groups[0].members.iter().find(|a| a.id == 1).unwrap().amount, 100);
        assert!(r.residual.is_empty(), "20% tail under 30% -> absorbed");

        let mut s = snap(Tol::Rel { bps: 1000, floor: 0 }, Box::new(Partial { matched: 80 }));
        let r = s.run(bag(&[(1, 100), (2, -80)]));
        assert_eq!(r.residual.len(), 1, "20% tail over 10% -> left split");
        assert_eq!(r.residual[0].amount, 20);
    }

    #[test]
    fn pivot_converts_back_to_outer_amount() {
        let b = vec![Item::new(1, 110, (100i64,)), Item::new(2, -110, (-100i64,))];
        let mut s = pivot(|d: &(i64,)| d.0, exact_1to1(|_| Some(0)));
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
        fn run(&mut self, bag: Vec<Item<(i64,)>>) -> Resolution<(i64,)> {
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
        let mut s = pivot(|d: &(i64,)| d.0, Box::new(HalfMatch));
        let r = s.run(b);

        // Group retains only Z at 50; the phantom 0-mass X edge is gone.
        assert_eq!(r.groups.len(), 1);
        assert_eq!(
            r.groups[0].members,
            vec![Allocation { id: 2, amount: 50 }]
        );

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
        let mut s = pivot(|d: &(i64,)| d.0, Box::new(HalfMatch));
        let r = s.run(b);

        // Group keeps only Z; X carries no mass and is not present.
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].members, vec![Allocation { id: 2, amount: 50 }]);

        // Conservation: X's full 5 parent units sit in residual.
        let mut res: Vec<(ExtId, i64)> = r.residual.iter().map(|i| (i.id, i.amount)).collect();
        res.sort();
        assert_eq!(res, vec![(1, 5), (2, 50)]);
    }

    // --- soakers ---------------------------------------------------------

    // Items here carry their own `original` (= amount) via `bag`, except where
    // a materiality test needs a distinct original, built explicitly.

    #[test]
    fn soak_small_abs_threshold_singletons() {
        // amounts: two immaterial (<=5), one material.
        let mut s = soak_small(5, SoakMode::Singleton, "rounding", |_: &Item<i64>| 0u64);
        let r = s.run(bag(&[(1, 3), (2, -2), (3, 100)]));
        conserves(3, &r);
        // Two soaked singletons, one material lot left as residual.
        assert_eq!(r.groups.len(), 2);
        assert!(r.groups.iter().all(|g| g.members.len() == 1 && g.origin == "rounding"));
        assert_eq!(r.residual.len(), 1);
        assert_eq!(r.residual[0].id, 3);
    }

    #[test]
    fn soak_small_bps_against_original() {
        // amount 10 on an original of 1000 = 100 bps (1%); soak under 200 bps.
        let items = vec![
            Item { id: 1, original: 1000, amount: 10, data: 0 }, // immaterial: 100 bps
            Item { id: 2, original: 1000, amount: 50, data: 0 }, // material:   500 bps
        ];
        let mut s = soak_small(
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
        let mut s = soak_small(5, SoakMode::Bucket, "tail", key);
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
        let mut s = soak_if(
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
        let mut s = soak_all(SoakMode::Singleton, "unmatched", |_: &Item<i64>| 0u64);
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
        let mut s = soak_all(SoakMode::Bucket, "class", key);
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
        assert!(!g.clean(Tol::Rel { bps: 5, floor: 100 }), "200 > 100 -> dirty");
        // RelMax scales off the largest leg (1_000_000): slack = max(100, 500).
        assert!(g.clean(Tol::RelMax { bps: 5, floor: 100 }), "200 <= 500 -> clean");
        // Abs ignores the legs entirely.
        assert!(g.clean(Tol::Abs(200)));
        assert!(!g.clean(Tol::Abs(199)));
    }

    #[test]
    fn agg_net_relmax_accepts_what_rel_rejects() {
        // Same shape as a leaf bucket: the large leg lets RelMax accept a net
        // that Rel (smallest-leg) would reject.
        let b = bag(&[(1, 1_000_000), (2, -999_000), (3, -1_200)]);
        let mut rel = agg_net(|_: &i64| 0u64, Tol::Rel { bps: 5, floor: 100 });
        assert_eq!(rel.run(b.clone()).groups.len(), 0);
        let mut relmax = agg_net(|_: &i64| 0u64, Tol::RelMax { bps: 5, floor: 100 });
        let r = relmax.run(b);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].net, -200);
    }
}
