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
//! leaves; combinators ([`seq`], [`partition_by`], [`branch`]) compose them. A
//! whole pipeline is just an expression:
//!
//! ```ignore
//! partition_by(unit, partition_by(ccy, seq(vec![
//!     agg_net(objsub, amount, tol),   // macro nets accepted wholesale
//!     exact_1to1(amount_key, amount), // clean 1-to-1 pairs
//!     signal_group(tokens, amount, tol, cap), // reference bridge
//!     flow(model),                    // engine arbitrates the rest
//! ])))
//! ```
//!
//! The committing primitives (`agg_net`, `exact_1to1`, `signal_group`) pull the
//! rows they are certain about; [`flow`] is the global *arbiter* for the
//! ambiguous residual where strategies would otherwise compete.

use crate::engine::SolveStatus;
use crate::flow::{Allocation, ExtId, Matcher, Model};
use std::collections::{BTreeSet, HashMap};
use std::hash::Hash;
use std::marker::PhantomData;

/// One lot in the bag: a caller-owned row/lot id, its original signed line
/// amount, its currently available signed amount, and payload. A row-partition
/// workload may start with [`Item::row`] (no canonical lot amount yet); a
/// lot-aware subtree starts with [`Item::lot`] and may split a lot by emitting a
/// consumed allocation to a group and a remainder lot to residual.
///
/// `original` is stable within a lot pipeline and `amount` is the shrinking
/// residual. This lets later strategies classify leftovers by materiality, e.g.
/// "soak this residual if it is under 2% of the original line".
pub struct Item<E> {
    pub id: ExtId,
    pub original: i64,
    pub amount: i64,
    /// True when `original`/`amount` are the canonical lot amount for this item.
    /// Legacy row-partition callers may leave this false and let leaves compute
    /// their amount from `data` via the usual amount closures.
    pub lot: bool,
    pub data: E,
}

impl<E> Item<E> {
    pub fn row(id: ExtId, data: E) -> Self {
        Item {
            id,
            original: 0,
            amount: 0,
            lot: false,
            data,
        }
    }

    pub fn lot(id: ExtId, amount: i64, data: E) -> Self {
        Item {
            id,
            original: amount,
            amount,
            lot: true,
            data,
        }
    }

    fn effective_amount<FA>(&self, amount: &FA) -> i64
    where
        FA: Fn(&E) -> i64,
    {
        if self.lot {
            self.amount
        } else {
            amount(&self.data)
        }
    }

    fn stamp_amount<FA>(&mut self, amount: &FA)
    where
        FA: Fn(&E) -> i64,
    {
        self.amount = self.effective_amount(amount);
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
}

impl Group {
    pub fn member_ids(&self) -> Vec<ExtId> {
        self.members.iter().map(|a| a.id).collect()
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
/// stateful [`flow`] leaf keeps a live [`Matcher`], and [`partition_by`] holds
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
        let timed = std::env::var_os("FLORECON_TIME").is_some();
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

struct PartitionBy<E, K, FK> {
    key: FK,
    /// Builds a fresh child subtree the first time a shard key is seen.
    factory: Box<dyn Fn() -> Box<dyn Strategy<E>>>,
    /// One independent child per shard key. Each child owns its own state
    /// (notably its own warm flow [`Matcher`]), so per-shard warm-start is
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
            let child = children.entry(k).or_insert_with(|| factory());
            let r = child.run(items);
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
/// a distinct `Matcher` that only ever sees that shard's rows.
pub fn partition_by<E: 'static, K, FK, FF>(key: FK, factory: FF) -> Box<dyn Strategy<E>>
where
    K: Hash + Eq + Clone + 'static,
    FK: Fn(&E) -> K + 'static,
    FF: Fn() -> Box<dyn Strategy<E>> + 'static,
{
    Box::new(PartitionBy {
        key,
        factory: Box::new(factory),
        children: HashMap::new(),
    })
}

struct Branch<E, FP> {
    pred: FP,
    and_then: Box<dyn Strategy<E>>,
    or_else: Box<dyn Strategy<E>>,
}

impl<E, FP> Strategy<E> for Branch<E, FP>
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
        // Always run both children, even on empty input, so stateful leaves such
        // as `flow` can observe rows that departed their branch and drop stale
        // warm state.
        let mut a = self.and_then.run(yes);
        let b = self.or_else.run(no);
        a.groups.extend(b.groups);
        a.residual.extend(b.residual);
        a
    }
}

/// Route the bag by a predicate, run different child subtrees on the two sides,
/// then join their groups/residuals. This is a structural split (unlike
/// [`seq`], which cascades over residual, and unlike [`partition_by`], which
/// runs the same child per shard). Conservation follows from the disjoint split
/// and from each child conserving its own side.
pub fn branch<E: 'static, FP>(
    pred: FP,
    and_then: Box<dyn Strategy<E>>,
    or_else: Box<dyn Strategy<E>>,
) -> Box<dyn Strategy<E>>
where
    FP: Fn(&E) -> bool + 'static,
{
    Box::new(Branch {
        pred,
        and_then,
        or_else,
    })
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

struct ExactOneToOne<E, FK, FA> {
    key: FK,
    amount: FA,
    _e: PhantomData<E>,
}

impl<E, FK, FA> Strategy<E> for ExactOneToOne<E, FK, FA>
where
    FK: Fn(&E) -> Option<u64>,
    FA: Fn(&E) -> i64,
{
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
        let mut buckets: HashMap<u64, Vec<Item<E>>> = HashMap::new();
        let mut residual = Vec::new();
        for mut item in bag {
            item.stamp_amount(&self.amount);
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
pub fn exact_1to1<E: 'static, FK, FA>(key: FK, amount: FA) -> Box<dyn Strategy<E>>
where
    FK: Fn(&E) -> Option<u64> + 'static,
    FA: Fn(&E) -> i64 + 'static,
{
    Box::new(ExactOneToOne {
        key,
        amount,
        _e: PhantomData,
    })
}

struct AggNet<E, FK, FA> {
    key: FK,
    amount: FA,
    tol: i64,
    _e: PhantomData<E>,
}

impl<E, FK, FA> Strategy<E> for AggNet<E, FK, FA>
where
    FK: Fn(&E) -> u64,
    FA: Fn(&E) -> i64,
{
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
        let mut buckets: HashMap<u64, Vec<Item<E>>> = HashMap::new();
        for mut item in bag {
            item.stamp_amount(&self.amount);
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
            if items.len() >= 2 && sum.abs() <= self.tol && signs.0 && signs.1 {
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
                });
            } else {
                residual.extend(items);
            }
        }
        Resolution { groups, residual }
    }
}

/// Accept a whole aggregation bucket (e.g. an `objsub`, or a balance-sheet-level
/// set) when it nets to zero within `tol`. The macro net-to-zero pre-filter:
/// confirmation, not optimization.
pub fn agg_net<E: 'static, FK, FA>(key: FK, amount: FA, tol: i64) -> Box<dyn Strategy<E>>
where
    FK: Fn(&E) -> u64 + 'static,
    FA: Fn(&E) -> i64 + 'static,
{
    Box::new(AggNet {
        key,
        amount,
        tol,
        _e: PhantomData,
    })
}

struct RunningZero<E, FO, FA> {
    order: FO,
    amount: FA,
    tol: i64,
    _e: PhantomData<E>,
}

impl<E, FO, FA> Strategy<E> for RunningZero<E, FO, FA>
where
    FO: Fn(&E) -> i64,
    FA: Fn(&E) -> i64,
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
        for mut item in bag {
            item.stamp_amount(&self.amount);
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
pub fn running_zero<E: 'static, FO, FA>(order: FO, amount: FA, tol: i64) -> Box<dyn Strategy<E>>
where
    FO: Fn(&E) -> i64 + 'static,
    FA: Fn(&E) -> i64 + 'static,
{
    Box::new(RunningZero {
        order,
        amount,
        tol,
        _e: PhantomData,
    })
}

struct SignalGroup<E, FS, FA> {
    signals: FS,
    amount: FA,
    tol: i64,
    cap: usize,
    _e: PhantomData<E>,
}

impl<E, FS, FA> Strategy<E> for SignalGroup<E, FS, FA>
where
    FS: Fn(&E) -> Vec<u64>,
    FA: Fn(&E) -> i64,
{
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
        let bag: Vec<Item<E>> = bag
            .into_iter()
            .map(|mut i| {
                i.stamp_amount(&self.amount);
                i
            })
            .collect();
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
            if sum.abs() <= self.tol && has_pos && has_neg {
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
pub fn signal_group<E: 'static, FS, FA>(
    signals: FS,
    amount: FA,
    tol: i64,
    cap: usize,
) -> Box<dyn Strategy<E>>
where
    FS: Fn(&E) -> Vec<u64> + 'static,
    FA: Fn(&E) -> i64 + 'static,
{
    Box::new(SignalGroup {
        signals,
        amount,
        tol,
        cap,
        _e: PhantomData,
    })
}

/// The global arbiter leaf, kept *warm*. It owns one min-cost-flow [`Matcher`]
/// and the id set currently loaded into it (`present`). Each `run` applies only
/// the membership delta — upsert new ids, remove departed ones — then re-solves,
/// reusing the cached simplex basis, so a no-op recalc costs microseconds rather
/// than a full cold solve. A *fresh* leaf (first run, or one rebuilt per solve
/// by the batch [`Session`](crate::plan::Session) path) simply has an empty
/// `present`, so its first solve *is* the cold solve — warm vs cold is decided
/// purely by whether the caller keeps the compiled strategy alive. Sharding is
/// the caller's job: [`partition_by`] gives each shard its own `Flow`, so this
/// leaf only ever sees one shard's rows.
#[derive(Clone)]
struct FlowTx<Tx> {
    tx: Tx,
    amount: i64,
}

#[derive(Clone)]
struct FlowLotModel<M> {
    inner: M,
}

impl<M> Model for FlowLotModel<M>
where
    M: Model,
{
    type Tx = FlowTx<M::Tx>;

    fn base_amount(&self, tx: &Self::Tx) -> i64 {
        tx.amount
    }
    fn penalty(&self, tx: &Self::Tx) -> f64 {
        self.inner.penalty(&tx.tx)
    }
    fn block_key(&self, tx: &Self::Tx) -> i64 {
        self.inner.block_key(&tx.tx)
    }
    fn window(&self) -> i64 {
        self.inner.window()
    }
    fn cost(&self, a: &Self::Tx, b: &Self::Tx) -> Option<f64> {
        self.inner.cost_lot(&a.tx, a.amount, &b.tx, b.amount)
    }
    fn match_keys(&self, tx: &Self::Tx) -> Vec<u64> {
        self.inner.match_keys_lot(&tx.tx, tx.amount)
    }
}

#[derive(Clone, PartialEq, Eq)]
struct FlowSig {
    amount: i64,
    penalty_bits: u64,
    key: i64,
    keys: Vec<u64>,
}

struct Flow<M: Model> {
    model: M,
    matcher: Matcher<FlowLotModel<M>>,
    loaded: HashMap<ExtId, FlowSig>,
}

impl<M> Flow<M>
where
    M: Model,
{
    fn flow_amount(&self, item: &Item<M::Tx>) -> i64 {
        if item.lot {
            item.amount
        } else {
            self.model.base_amount(&item.data)
        }
    }

    fn flow_tx(&self, item: &Item<M::Tx>) -> FlowTx<M::Tx>
    where
        M::Tx: Clone,
    {
        FlowTx {
            tx: item.data.clone(),
            amount: self.flow_amount(item),
        }
    }

    fn flow_sig(&self, item: &Item<M::Tx>) -> FlowSig {
        let mut keys = self.model.match_keys(&item.data);
        keys.sort_unstable();
        FlowSig {
            amount: self.flow_amount(item),
            penalty_bits: self.model.penalty(&item.data).to_bits(),
            key: self.model.block_key(&item.data),
            keys,
        }
    }
}

impl<M> Strategy<M::Tx> for Flow<M>
where
    M: Model + Clone,
    M::Tx: Clone,
{
    fn run(&mut self, bag: Vec<Item<M::Tx>>) -> Resolution<M::Tx> {
        let timed = std::env::var_os("FLORECON_TIME").is_some();
        let want: BTreeSet<ExtId> = bag.iter().map(|i| i.id).collect();
        // id -> lot references (no clones unless we actually upsert). The
        // matcher conserves the lot's *current* amount, not a fresh amount
        // recomputed from original row data, so partial residuals compose
        // through `seq`.
        let data: HashMap<ExtId, &Item<M::Tx>> = bag.iter().map(|i| (i.id, i)).collect();
        let sigs: HashMap<ExtId, FlowSig> = bag.iter().map(|i| (i.id, self.flow_sig(i))).collect();

        // Diff want vs loaded. Upsert both new ids and same-id rows whose
        // current lot amount or candidate-generation signature changed; this is
        // what keeps warm lot recalc correct when an upstream step changes the
        // residual amount of an id that remains present.
        let mut upserts: Vec<ExtId> = sigs
            .iter()
            .filter_map(|(&id, sig)| (self.loaded.get(&id) != Some(sig)).then_some(id))
            .collect();
        upserts.sort_by_key(|&id| flow_upsert_rank(id));
        let drops: Vec<ExtId> = self
            .loaded
            .keys()
            .copied()
            .filter(|id| !want.contains(id))
            .collect();

        // Instant is only touched when profiling; wasm has no clock source.
        let tb = timed.then(std::time::Instant::now);
        for id in upserts {
            if let Some(item) = data.get(&id) {
                self.matcher.upsert(id, self.flow_tx(item));
            }
        }
        for id in drops {
            self.matcher.remove(id);
        }
        let build = tb.map(|t| t.elapsed().as_secs_f64() * 1000.0);
        let ts = timed.then(std::time::Instant::now);
        let status = self.matcher.solve(); // warm when `present` was non-empty.
        if let (Some(build), Some(ts)) = (build, ts) {
            eprintln!(
                "    flow: delta {build:>6.1} ms ({} arcs), solve {:>6.1} ms",
                self.matcher.arc_count(),
                ts.elapsed().as_secs_f64() * 1000.0,
            );
        }
        debug_assert_eq!(status, SolveStatus::Optimal);
        self.loaded = sigs;

        // Determinism guard: in debug builds (or when FLORECON_VERIFY_WARM is
        // set) rebuild a fresh cold matcher on the same id set and assert the
        // warm solution matches. The always-true invariant is the optimal
        // *objective* (matched costs + unmatched penalties): a min-cost-flow
        // optimum is unique in cost but can be degenerate in *which* equal-cost
        // arcs carry flow, so the grouping is only guaranteed identical when the
        // optimum is unique. This catches the real failure mode — a warm
        // re-solve drifting to a different (or worse) objective — without
        // false-positiving on benign tie re-grouping (see the
        // `warm_flow_matches_cold_*` equivalence tests).
        if cfg!(debug_assertions) || std::env::var_os("FLORECON_VERIFY_WARM").is_some() {
            let mut cold = Matcher::new(FlowLotModel {
                inner: self.model.clone(),
            });
            let mut ids: Vec<ExtId> = data.keys().copied().collect();
            ids.sort_unstable();
            for id in ids {
                if let Some(item) = data.get(&id) {
                    cold.upsert(
                        id,
                        FlowTx {
                            tx: item.data.clone(),
                            amount: if item.lot {
                                item.amount
                            } else {
                                self.model.base_amount(&item.data)
                            },
                        },
                    );
                }
            }
            cold.solve();
            let (warm_obj, cold_obj) = (self.matcher.objective(), cold.objective());
            assert!(
                (warm_obj - cold_obj).abs() < 1e-6,
                "warm flow solve diverged from a fresh cold rebuild: \
                 warm objective {warm_obj} != cold objective {cold_obj}"
            );
        }

        let groups = self
            .matcher
            .allocation_groups()
            .into_iter()
            .map(|g| Group {
                members: g.members,
                origin: "flow".to_string(),
                net: g.net_base,
            })
            .collect();
        let unmatched: HashMap<ExtId, i64> = self
            .matcher
            .unmatched_allocations()
            .into_iter()
            .map(|a| (a.id, a.amount))
            .collect();
        let residual = bag
            .into_iter()
            .filter_map(|mut i| {
                unmatched.get(&i.id).map(|&amount| {
                    i.amount = amount;
                    i
                })
            })
            .collect();
        Resolution { groups, residual }
    }
}

/// The global arbiter: hand the residual to the min-cost-flow engine, which
/// resolves competing candidates into one consistent grouping. This is where
/// *proposing* signals (reference + amount + date, via the `Model`) become a
/// committed partition. The returned leaf is *stateful* — it keeps its matcher
/// warm across solves — but that is invisible to the caller: a one-shot solve
/// just runs it once.
pub fn flow<M>(model: M) -> Box<dyn Strategy<M::Tx>>
where
    M: Model + Clone + 'static,
    M::Tx: Clone + 'static,
{
    Box::new(Flow {
        matcher: Matcher::new(FlowLotModel {
            inner: model.clone(),
        }),
        model,
        loaded: HashMap::new(),
    })
}

/// Establish a canonical lot amount for a subtree. `inner` sees lots whose
/// `original` and current `amount` are initialized from `amount(&data)`, and any
/// residual it returns keeps flowing outward with the same `original` value.
/// This is the adapter that lets the upper plan layer opt into allocation
/// semantics while legacy row-grouping plans continue to use amount closures on
/// the leaves.
struct Lots<E, FA> {
    amount: FA,
    inner: Box<dyn Strategy<E>>,
}

impl<E, FA> Strategy<E> for Lots<E, FA>
where
    FA: Fn(&E) -> i64,
{
    fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E> {
        let bag = bag
            .into_iter()
            .map(|mut item| {
                if !item.lot {
                    let a = (self.amount)(&item.data);
                    item.original = a;
                    item.amount = a;
                    item.lot = true;
                }
                item
            })
            .collect();
        self.inner.run(bag)
    }
}

pub fn lots<E: 'static, FA>(amount: FA, inner: Box<dyn Strategy<E>>) -> Box<dyn Strategy<E>>
where
    FA: Fn(&E) -> i64 + 'static,
{
    Box::new(Lots { amount, inner })
}

#[derive(Clone, Copy)]
pub enum SoakMode {
    Singleton,
    Bucket,
}

struct SoakSmall<E, FK> {
    max_bps: Option<i64>,
    max_abs: Option<i64>,
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
            if is_small_residual(&item, self.max_bps, self.max_abs) {
                match self.mode {
                    SoakMode::Singleton => groups.push(Group {
                        members: vec![Allocation {
                            id: item.id,
                            amount: item.amount,
                        }],
                        origin: self.origin.clone(),
                        net: item.amount,
                    }),
                    SoakMode::Bucket => buckets.entry((self.key)(&item)).or_default().push(item),
                }
            } else {
                residual.push(item);
            }
        }
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
                origin: format!("{}:{}", self.origin, k.to_string()),
                net,
            });
        }
        Resolution { groups, residual }
    }
}

fn is_small_residual<E>(item: &Item<E>, max_bps: Option<i64>, max_abs: Option<i64>) -> bool {
    if item.amount == 0 {
        return false;
    }
    if let Some(max_abs) = max_abs
        && item.amount.abs() <= max_abs.abs()
    {
        return true;
    }
    if let Some(max_bps) = max_bps
        && item.original != 0
    {
        // Avoid overflow on large line values by comparing after promoting to
        // i128. `max_bps` is basis points, so 200 means 2%.
        let lhs = item.amount.abs() as i128 * 10_000;
        let rhs = item.original.abs() as i128 * max_bps.max(0) as i128;
        return lhs <= rhs;
    }
    false
}

/// Consume residual lots whose current amount is immaterial versus their
/// original line amount and/or an absolute threshold. Singleton mode produces
/// one variance group per residual; bucket mode groups small residuals by `key`.
pub fn soak_small<E: 'static, K, FK>(
    max_bps: Option<i64>,
    max_abs: Option<i64>,
    mode: SoakMode,
    origin: impl Into<String>,
    key: FK,
) -> Box<dyn Strategy<E>>
where
    K: Hash + Eq + Clone + ToString + 'static,
    FK: Fn(&Item<E>) -> K + 'static,
{
    Box::new(SoakSmall {
        max_bps,
        max_abs,
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
            match self.mode {
                SoakMode::Singleton => groups.push(Group {
                    members: vec![Allocation {
                        id: item.id,
                        amount: item.amount,
                    }],
                    origin: self.origin.clone(),
                    net: item.amount,
                }),
                SoakMode::Bucket => buckets.entry((self.key)(&item)).or_default().push(item),
            }
        }
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
                origin: format!("{}:{}", self.origin, k.to_string()),
                net,
            });
        }
        Resolution {
            groups,
            residual: Vec::new(),
        }
    }
}

/// Consume every remaining non-zero residual lot into singleton or bucketed
/// groups. This is a terminal classifier, not a matcher: non-zero group nets are
/// expected and represent unmatched/variance/writeoff classes.
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

// ---------------------------------------------------------------------------
// Flow determinism helper
// ---------------------------------------------------------------------------

/// Deterministic, dataset-robust upsert rank for a flow node. The network
/// simplex's pivot count depends on the order nodes/arcs are introduced; a
/// monotone (ascending or descending) id order is a pathological sequence on
/// real data. Ordering by a stable scramble of the id (a SplitMix64 finalizer)
/// keeps the order a *pure function of the id* — so cold and warm solves agree
/// and results are reproducible across builds — while spreading augmenting
/// paths to avoid the worst case. Measured: ~2x fewer pivots vs ascending id on
/// the interco sample.
fn flow_upsert_rank(id: ExtId) -> u64 {
    let mut z = id.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bag(items: &[(ExtId, i64)]) -> Vec<Item<i64>> {
        items.iter().map(|&(id, a)| Item::lot(id, a, a)).collect()
    }
    fn ids(g: &Group) -> Vec<ExtId> {
        let mut m = g.member_ids();
        m.sort();
        m
    }
    fn conserves(input: usize, r: &Resolution<i64>) {
        let g: usize = r.groups.iter().map(|g| g.members.len()).sum();
        assert_eq!(g + r.residual.len(), input, "conservation violated");
    }

    #[test]
    fn exact_pairs_and_leaves_residual() {
        // amounts: +5, -5, +5, +3 ; key by |amount| so signs pair within magnitude
        let b = bag(&[(1, 5), (2, -5), (3, 5), (4, 3)]);
        let mut s = exact_1to1(|a: &i64| Some(a.unsigned_abs()), |a: &i64| *a);
        let r = s.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 1); // one +5/-5 pair (id 2 pairs with a +5)
        assert!(r.groups[0].member_ids().contains(&2));
        // one +5 left unpaired plus the +3 -> 2 residual
        assert_eq!(r.residual.len(), 2);
    }

    #[test]
    fn agg_accepts_netting_bucket() {
        let b = bag(&[(1, 100), (2, -60), (3, -40), (4, 7)]);
        // all in one bucket; nets to 7 -> with tol 0 it should NOT accept
        let mut s = agg_net(|_a: &i64| 0u64, |a: &i64| *a, 0);
        let r = s.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 0);
        // with tol 10 it accepts the whole bucket
        let b = bag(&[(1, 100), (2, -60), (3, -40), (4, 7)]);
        let mut s = agg_net(|_a: &i64| 0u64, |a: &i64| *a, 10);
        let r = s.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].members.len(), 4);
    }

    #[test]
    fn signal_groups_net_and_cascade() {
        // ids 1,2 share token 10 and net; id 3 alone; pipeline then leaves 3.
        let b = bag(&[(1, 50), (2, -50), (3, 9)]);
        let mut s = signal_group(
            |a: &i64| if *a == 9 { vec![] } else { vec![10] },
            |a: &i64| *a,
            0,
            16,
        );
        let r = s.run(b);
        conserves(3, &r);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(ids(&r.groups[0]), vec![1, 2]);
        assert_eq!(r.residual.len(), 1);
    }

    #[test]
    fn branch_routes_to_different_children_and_conserves() {
        let b = bag(&[(1, 5), (2, -5), (3, 7), (4, -7)]);
        let mut s = branch(
            |a: &i64| a.unsigned_abs() == 5,
            agg_net(|_a: &i64| 1u64, |a: &i64| *a, 0),
            agg_net(|_a: &i64| 2u64, |a: &i64| *a, 0),
        );
        let r = s.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 2);
        assert_eq!(r.residual.len(), 0);
    }

    #[test]
    fn windowed_blocks_far_matches() {
        // +5@1 and -5@100: global exact_1to1 would pair them; windowed (w=3)
        // must not -- they are too far apart in the ordering.
        let b = vec![Item::lot(1, 5, (1i64, 5i64)), Item::lot(2, -5, (100, -5))];
        let inner = exact_1to1(
            |d: &(i64, i64)| Some(d.1.unsigned_abs()),
            |d: &(i64, i64)| d.1,
        );
        let r = {
            let mut w = windowed(|d: &(i64, i64)| d.0, 3, inner);
            w.run(b)
        };
        assert_eq!(r.groups.len(), 0);
        assert_eq!(r.residual.len(), 2);
    }

    #[test]
    fn windowed_finds_near_match_across_band_boundary() {
        // +5@4 and -5@7 fall in different bands (w=3) but within one window of
        // each other; the carry/look-ahead must still pair them.
        let b = vec![Item::lot(1, 5, (4i64, 5i64)), Item::lot(2, -5, (7, -5))];
        let inner = exact_1to1(
            |d: &(i64, i64)| Some(d.1.unsigned_abs()),
            |d: &(i64, i64)| d.1,
        );
        let r = {
            let mut w = windowed(|d: &(i64, i64)| d.0, 3, inner);
            w.run(b)
        };
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.residual.len(), 0);
    }

    #[test]
    fn running_zero_segments_at_balance_clears() {
        // timeline: +100, -100 | +50, -30, -20  -> two clearing segments
        let b = vec![
            Item::lot(1, 100, (1i64, 100i64)),
            Item::lot(2, -100, (2, -100)),
            Item::lot(3, 50, (3, 50)),
            Item::lot(4, -30, (4, -30)),
            Item::lot(5, -20, (5, -20)),
        ];
        let mut s = running_zero(|d: &(i64, i64)| d.0, |d: &(i64, i64)| d.1, 0);
        let r = s.run(b);
        let g: usize = r.groups.iter().map(|g| g.members.len()).sum();
        assert_eq!(g + r.residual.len(), 5);
        assert_eq!(r.groups.len(), 2);
        assert_eq!(r.groups[0].member_ids(), vec![1, 2]);
        assert_eq!(r.groups[1].member_ids(), vec![3, 4, 5]);
    }

    #[test]
    fn running_zero_leaves_uncleared_tail() {
        let b = vec![
            Item::lot(1, 100, (1i64, 100i64)),
            Item::lot(2, -100, (2, -100)),
            Item::lot(3, 7, (3, 7)), // never clears
        ];
        let mut s = running_zero(|d: &(i64, i64)| d.0, |d: &(i64, i64)| d.1, 0);
        let r = s.run(b);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.residual.len(), 1);
        assert_eq!(r.residual[0].id, 3);
    }

    #[test]
    fn seq_then_partition_compose() {
        let mut pipeline = partition_by(
            |a: &i64| a.signum().unsigned_abs(), // silly key just to exercise sharding
            || {
                seq(vec![exact_1to1(
                    |a: &i64| Some(a.unsigned_abs()),
                    |a: &i64| *a,
                )])
            },
        );
        let b = bag(&[(1, 4), (2, -4), (3, 4), (4, -4)]);
        let r = pipeline.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 2);
    }
}
