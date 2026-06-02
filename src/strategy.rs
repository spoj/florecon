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
//! leaves; combinators ([`seq`], [`partition_by`]) compose them. A whole
//! pipeline is just an expression:
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
use crate::flow::{ExtId, Model, Matcher};
use std::collections::HashMap;
use std::hash::Hash;
use std::marker::PhantomData;

/// One entry in the bag: a caller-owned id plus its payload.
pub struct Item<E> {
    pub id: ExtId,
    pub data: E,
}

/// A resolved group of matched entries.
#[derive(Debug, Clone)]
pub struct Group {
    pub members: Vec<ExtId>,
    /// Which primitive produced it.
    pub origin: &'static str,
    /// Residual in the canonical numeraire; zero means it nets out.
    pub net: i64,
}

/// What a strategy returns: the groups it pulled and the residual it left.
pub struct Resolution<E> {
    pub groups: Vec<Group>,
    pub residual: Vec<Item<E>>,
}

/// A reconciliation strategy: pull groups from a bag, return the residual.
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
        let mut groups = Vec::new();
        let mut residual = bag;
        for step in &self.steps {
            let r = step.run(residual);
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
    inner: Box<dyn Strategy<E>>,
    _k: PhantomData<K>,
}

impl<E, K, FK> Strategy<E> for PartitionBy<E, K, FK>
where
    K: Hash + Eq,
    FK: Fn(&E) -> K,
{
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        let mut shards: HashMap<K, Vec<Item<E>>> = HashMap::new();
        for item in bag {
            shards.entry((self.key)(&item.data)).or_default().push(item);
        }
        let mut groups = Vec::new();
        let mut residual = Vec::new();
        for (_k, items) in shards {
            let r = self.inner.run(items);
            groups.extend(r.groups);
            residual.extend(r.residual);
        }
        Resolution { groups, residual }
    }
}

/// Fork/join: split the bag by a key, run `inner` on each shard independently,
/// then merge. Embarrassingly parallel; this is how sharding (e.g. by bilateral
/// pair or by currency) is expressed.
pub fn partition_by<E: 'static, K, FK>(key: FK, inner: Box<dyn Strategy<E>>) -> Box<dyn Strategy<E>>
where
    K: Hash + Eq + 'static,
    FK: Fn(&E) -> K + 'static,
{
    Box::new(PartitionBy {
        key,
        inner,
        _k: PhantomData,
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
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        let mut buckets: HashMap<u64, Vec<Item<E>>> = HashMap::new();
        let mut residual = Vec::new();
        for item in bag {
            match (self.key)(&item.data) {
                Some(k) if (self.amount)(&item.data) != 0 => {
                    buckets.entry(k).or_default().push(item)
                }
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
                let a = (self.amount)(&item.data);
                let slot = by_mag.entry(a.abs()).or_default();
                if a > 0 {
                    slot.0.push(item);
                } else {
                    slot.1.push(item);
                }
            }
            for (_mag, (mut pos, mut neg)) in by_mag {
                let pairs = pos.len().min(neg.len());
                for _ in 0..pairs {
                    let p = pos.pop().unwrap();
                    let n = neg.pop().unwrap();
                    groups.push(Group {
                        members: vec![p.id, n.id],
                        origin: "exact_1to1",
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
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        let mut buckets: HashMap<u64, Vec<Item<E>>> = HashMap::new();
        for item in bag {
            buckets.entry((self.key)(&item.data)).or_default().push(item);
        }
        let mut groups = Vec::new();
        let mut residual = Vec::new();
        for (_k, items) in buckets {
            let sum: i64 = items.iter().map(|i| (self.amount)(&i.data)).sum();
            let signs = items.iter().fold((false, false), |(p, n), i| {
                let a = (self.amount)(&i.data);
                (p || a > 0, n || a < 0)
            });
            if items.len() >= 2 && sum.abs() <= self.tol && signs.0 && signs.1 {
                groups.push(Group {
                    members: items.iter().map(|i| i.id).collect(),
                    origin: "agg_net",
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
            acc += (self.amount)(&item.data);
            seg.push(item);
            if acc.abs() <= self.tol && seg.len() >= 2 {
                groups.push(Group {
                    members: seg.iter().map(|i| i.id).collect(),
                    origin: "running_zero",
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
    fn run(&self, bag: Vec<Item<E>>) -> Resolution<E> {
        let n = bag.len();
        let amt: Vec<i64> = bag.iter().map(|i| (self.amount)(&i.data)).collect();
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
                    members: members.iter().map(|&i| bag[i].id).collect(),
                    origin: "signal_group",
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

struct Flow<M> {
    model: M,
}

impl<M> Strategy<M::Tx> for Flow<M>
where
    M: Model + Clone,
    M::Tx: Clone,
{
    fn run(&self, bag: Vec<Item<M::Tx>>) -> Resolution<M::Tx> {
        let mut rec = Matcher::new(self.model.clone());
        for item in &bag {
            rec.upsert(item.id, item.data.clone());
        }
        let status = rec.solve();
        debug_assert_eq!(status, SolveStatus::Optimal);
        let groups = rec
            .groups()
            .into_iter()
            .map(|g| Group {
                members: g.members,
                origin: "flow",
                net: g.net_base,
            })
            .collect();
        let unmatched: std::collections::HashSet<ExtId> = rec.unmatched().into_iter().collect();
        let residual = bag
            .into_iter()
            .filter(|i| unmatched.contains(&i.id))
            .collect();
        Resolution { groups, residual }
    }
}

/// The global arbiter: hand the residual to the min-cost-flow engine, which
/// resolves competing candidates into one consistent grouping. This is where
/// *proposing* signals (reference + amount + date, via the `Model`) become a
/// committed partition.
pub fn flow<M>(model: M) -> Box<dyn Strategy<M::Tx>>
where
    M: Model + Clone + 'static,
    M::Tx: Clone + 'static,
{
    Box::new(Flow { model })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bag(items: &[(ExtId, i64)]) -> Vec<Item<i64>> {
        items.iter().map(|&(id, a)| Item { id, data: a }).collect()
    }
    fn ids(g: &Group) -> Vec<ExtId> {
        let mut m = g.members.clone();
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
        let s = exact_1to1(|a: &i64| Some(a.unsigned_abs()), |a: &i64| *a);
        let r = s.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 1); // one +5/-5 pair (id 2 pairs with a +5)
        assert!(r.groups[0].members.contains(&2));
        // one +5 left unpaired plus the +3 -> 2 residual
        assert_eq!(r.residual.len(), 2);
    }

    #[test]
    fn agg_accepts_netting_bucket() {
        let b = bag(&[(1, 100), (2, -60), (3, -40), (4, 7)]);
        // all in one bucket; nets to 7 -> with tol 0 it should NOT accept
        let s = agg_net(|_a: &i64| 0u64, |a: &i64| *a, 0);
        let r = s.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 0);
        // with tol 10 it accepts the whole bucket
        let b = bag(&[(1, 100), (2, -60), (3, -40), (4, 7)]);
        let s = agg_net(|_a: &i64| 0u64, |a: &i64| *a, 10);
        let r = s.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].members.len(), 4);
    }

    #[test]
    fn signal_groups_net_and_cascade() {
        // ids 1,2 share token 10 and net; id 3 alone; pipeline then leaves 3.
        let b = bag(&[(1, 50), (2, -50), (3, 9)]);
        let s = signal_group(
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
    fn windowed_blocks_far_matches() {
        // +5@1 and -5@100: global exact_1to1 would pair them; windowed (w=3)
        // must not -- they are too far apart in the ordering.
        let b = vec![
            Item { id: 1, data: (1i64, 5i64) },
            Item { id: 2, data: (100, -5) },
        ];
        let inner = exact_1to1(
            |d: &(i64, i64)| Some(d.1.unsigned_abs()),
            |d: &(i64, i64)| d.1,
        );
        let r = windowed(|d: &(i64, i64)| d.0, 3, inner).run(b);
        assert_eq!(r.groups.len(), 0);
        assert_eq!(r.residual.len(), 2);
    }

    #[test]
    fn windowed_finds_near_match_across_band_boundary() {
        // +5@4 and -5@7 fall in different bands (w=3) but within one window of
        // each other; the carry/look-ahead must still pair them.
        let b = vec![
            Item { id: 1, data: (4i64, 5i64) },
            Item { id: 2, data: (7, -5) },
        ];
        let inner = exact_1to1(
            |d: &(i64, i64)| Some(d.1.unsigned_abs()),
            |d: &(i64, i64)| d.1,
        );
        let r = windowed(|d: &(i64, i64)| d.0, 3, inner).run(b);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.residual.len(), 0);
    }

    #[test]
    fn running_zero_segments_at_balance_clears() {
        // timeline: +100, -100 | +50, -30, -20  -> two clearing segments
        let b = vec![
            Item { id: 1, data: (1i64, 100i64) },
            Item { id: 2, data: (2, -100) },
            Item { id: 3, data: (3, 50) },
            Item { id: 4, data: (4, -30) },
            Item { id: 5, data: (5, -20) },
        ];
        let s = running_zero(|d: &(i64, i64)| d.0, |d: &(i64, i64)| d.1, 0);
        let r = s.run(b);
        let g: usize = r.groups.iter().map(|g| g.members.len()).sum();
        assert_eq!(g + r.residual.len(), 5);
        assert_eq!(r.groups.len(), 2);
        assert_eq!(r.groups[0].members, vec![1, 2]);
        assert_eq!(r.groups[1].members, vec![3, 4, 5]);
    }

    #[test]
    fn running_zero_leaves_uncleared_tail() {
        let b = vec![
            Item { id: 1, data: (1i64, 100i64) },
            Item { id: 2, data: (2, -100) },
            Item { id: 3, data: (3, 7) }, // never clears
        ];
        let s = running_zero(|d: &(i64, i64)| d.0, |d: &(i64, i64)| d.1, 0);
        let r = s.run(b);
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.residual.len(), 1);
        assert_eq!(r.residual[0].id, 3);
    }

    #[test]
    fn seq_then_partition_compose() {
        let pipeline = partition_by(
            |a: &i64| a.signum().unsigned_abs(), // silly key just to exercise sharding
            seq(vec![exact_1to1(|a: &i64| Some(a.unsigned_abs()), |a: &i64| *a)]),
        );
        let b = bag(&[(1, 4), (2, -4), (3, 4), (4, -4)]);
        let r = pipeline.run(b);
        conserves(4, &r);
        assert_eq!(r.groups.len(), 2);
    }
}
