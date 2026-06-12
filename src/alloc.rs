//! allocation as coupling — the dual of [`crate::strategy`].
//!
//! Reconciliation parses a bag into a *partition* and conserves **identity**.
//! Allocation re-expresses a coarse cost on a finer basis and conserves
//! **value**. A [`Measure`] is a sparse quantity over named axes; the algebra is
//! a few verbs and one idea.
//!
//! - **reshape** — [`Measure::rekey`] rewrites each cell's key and re-sums
//!   (subsumes marginalize / roll-up / relabel);
//! - **couple** — [`Measure::combine`] aligns shared axes and broadcasts the
//!   rest, with the scalar op a closure (`|a, b| a + b`);
//! - **down-project** — [`Measure::allocate`] splits a coarse cost across a
//!   driver, exact to the penny;
//! - **select** — [`Measure::select`] / [`Measure::partition`] / [`Measure::slice`]
//!   query a cube; [`Measure::group_by`] routes a per-group rule and recombines.
//!
//! The one idea is **[`ANY`]**: a reserved coord meaning "unresolved on this
//! axis". Every residual — a missing driver, an undriven dimension,
//! sub-threshold dust — is mass parked on `ANY`, *in-cube*. Two inverse moves
//! shuttle mass along the grain: [`Measure::rake`] refines `ANY` → real detail,
//! [`Measure::vacuum`] surrenders detail → `ANY`.
//!
//! There is no type-level conserved/unconserved distinction — everything is one
//! open `Measure`. Conservation is a property you assert (`a.total()`), not one
//! the compiler enforces: `combine` with a non-additive closure, or a dropped
//! `select`, will change the total. Caller beware. The cyclic (reciprocal) case
//! reaches for [`fixed_point`], as recon does.
//!
//! ```
//! use florecon::alloc::*;
//!
//! let rent = Measure::build(&["geog", "time"], &[
//!     (&[("geog", 1), ("time", 1)], 1000),
//!     (&[("geog", 2), ("time", 1)], 500),   // no driver here
//! ]);
//! let rev = Measure::build(&["geog", "product", "time"], &[
//!     (&[("geog", 1), ("product", 10), ("time", 1)], 30),
//!     (&[("geog", 1), ("product", 11), ("time", 1)], 70),
//! ]);
//!
//! let a = rent.allocate(&rev);
//! assert_eq!(a.total(), rent.total());          // conserves
//! assert_eq!(a.get(&Key::of(&[("geog", 1), ("product", 10), ("time", 1)])), 300);
//! assert_eq!(a.pending().total(), 500);          // geog 2 parked on ANY, in-cube
//! ```

use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A dimension name, e.g. `"geog"`.
pub type Axis = &'static str;
/// A label within an axis. Opaque — the caller interns its own coords.
pub type Coord = u64;
/// Quantity in minor units. Integer so conservation is exact.
pub type Money = i128;

/// Reserved catch-all coord: "unresolved on this axis". Residual mass parks
/// here ([`Measure::allocate`], [`Measure::vacuum`]); [`Measure::rake`] resolves
/// it. Inspect it with [`Measure::pending`].
pub const ANY: Coord = u64::MAX;

/// A cell coordinate: a sorted set of `(axis, coord)` pairs.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Key(Vec<(Axis, Coord)>);

impl Key {
    /// Build a key from pairs (order-insensitive; stored sorted).
    pub fn of(pairs: &[(Axis, Coord)]) -> Key {
        let mut v = pairs.to_vec();
        v.sort_unstable();
        v.dedup();
        Key(v)
    }

    /// The `(axis, coord)` pairs (sorted by axis).
    pub fn coords(&self) -> &[(Axis, Coord)] {
        &self.0
    }

    /// The coord on `axis`, if present.
    pub fn get(&self, axis: Axis) -> Option<Coord> {
        self.0.iter().find(|(a, _)| *a == axis).map(|(_, c)| *c)
    }

    /// Keep only the coords on `axes` (the building block of `marginalize`).
    pub fn project(&self, axes: &[Axis]) -> Key {
        self.project_set(&axes.iter().copied().collect())
    }

    /// Set (or add) the coord on `axis`. With [`ANY`] this parks the axis; with
    /// a real coord, or a hierarchy map, this rolls it up.
    pub fn with(&self, axis: Axis, coord: Coord) -> Key {
        let mut m: BTreeMap<Axis, Coord> = self.0.iter().copied().collect();
        m.insert(axis, coord);
        Key(m.into_iter().collect())
    }

    /// Drop an axis entirely.
    pub fn without(&self, axis: Axis) -> Key {
        Key(self.0.iter().copied().filter(|(a, _)| *a != axis).collect())
    }

    fn project_set(&self, axes: &BTreeSet<Axis>) -> Key {
        Key(self
            .0
            .iter()
            .copied()
            .filter(|(a, _)| axes.contains(a))
            .collect())
    }

    fn merge(&self, other: &Key) -> Key {
        let mut m: BTreeMap<Axis, Coord> = self.0.iter().copied().collect();
        for (a, c) in &other.0 {
            m.entry(a).or_insert(*c);
        }
        Key(m.into_iter().collect())
    }

    fn with_any(&self, axes: &BTreeSet<Axis>) -> Key {
        let mut m: BTreeMap<Axis, Coord> = self.0.iter().copied().collect();
        for &a in axes {
            m.insert(a, ANY);
        }
        Key(m.into_iter().collect())
    }

    fn has_any(&self) -> bool {
        self.0.iter().any(|&(_, c)| c == ANY)
    }
}

/// A sparse quantity over named axes.
#[derive(Clone, Debug, Default)]
pub struct Measure {
    axes: BTreeSet<Axis>,
    cells: HashMap<Key, Money>,
}

impl Measure {
    /// An empty measure over `axes` (axes also grow as cells are added).
    pub fn with_axes(axes: BTreeSet<Axis>) -> Measure {
        Measure {
            axes,
            cells: HashMap::new(),
        }
    }

    /// Declare axes, then a list of `(coords, value)`.
    pub fn build(axes: &[Axis], cells: &[(&[(Axis, Coord)], Money)]) -> Measure {
        let mut m = Measure::with_axes(axes.iter().copied().collect());
        for (pairs, v) in cells {
            m.add(Key::of(pairs), *v);
        }
        m
    }

    /// The axes this measure ranges over.
    pub fn axes(&self) -> &BTreeSet<Axis> {
        &self.axes
    }

    /// Value at a cell (0 if absent).
    pub fn get(&self, k: &Key) -> Money {
        self.cells.get(k).copied().unwrap_or(0)
    }

    /// The sparse support: only nonzero cells.
    pub fn cells(&self) -> impl Iterator<Item = (&Key, Money)> {
        self.cells.iter().map(|(k, v)| (k, *v))
    }

    /// Number of stored (nonzero) cells.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Whether the support is empty.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Grand total over every cell.
    pub fn total(&self) -> Money {
        self.cells.values().sum()
    }

    fn add(&mut self, k: Key, v: Money) {
        if v == 0 {
            return;
        }
        for &(a, _) in &k.0 {
            self.axes.insert(a);
        }
        use std::collections::hash_map::Entry;
        match self.cells.entry(k) {
            Entry::Occupied(mut o) => {
                *o.get_mut() += v;
                if *o.get() == 0 {
                    o.remove();
                }
            }
            Entry::Vacant(va) => {
                va.insert(v);
            }
        }
    }

    // ── reshape ───────────────────────────────────────────────────────────

    /// Rewrite every cell's key and re-sum collisions. The one reshaping
    /// primitive: `marginalize` drops axes (`|k| k.project(keep)`), roll-up
    /// remaps a coord (`|k| k.with(dim, month(c))`), parking sets [`ANY`].
    pub fn rekey(&self, f: impl Fn(&Key) -> Key) -> Measure {
        let mut out = Measure::default();
        for (k, v) in &self.cells {
            out.add(f(k), *v);
        }
        out
    }

    /// Σ over the axes not in `keep`. Sugar for `rekey(|k| k.project(keep))`.
    pub fn marginalize(&self, keep: &[Axis]) -> Measure {
        let keep: BTreeSet<Axis> = keep.iter().copied().collect();
        self.rekey(|k| k.project_set(&keep))
    }

    // ── couple ────────────────────────────────────────────────────────────

    /// Pointwise: align shared axes (a join), broadcast disjoint ones, fill an
    /// absent side with `0`, prune zeros. The scalar op is yours:
    /// `a.combine(&b, |x, y| x + y)`. The anything-goes lane — only `+` on
    /// aligned axes conserves `total`; other ops/shapes won't.
    pub fn combine(&self, rhs: &Measure, f: impl Fn(Money, Money) -> Money) -> Measure {
        let shared: BTreeSet<Axis> = self.axes.intersection(&rhs.axes).copied().collect();
        let out_axes: BTreeSet<Axis> = self.axes.union(&rhs.axes).copied().collect();
        let lb = self.bucket(&shared);
        let rb = rhs.bucket(&shared);
        let mut out = Measure::with_axes(out_axes);

        let shared_keys: BTreeSet<&Key> = lb.keys().chain(rb.keys()).collect();
        for s in shared_keys {
            match (lb.get(s), rb.get(s)) {
                (Some(ls), Some(rs)) => {
                    for (lk, a) in ls {
                        for (rk, b) in rs {
                            out.add(lk.merge(rk), f(*a, *b));
                        }
                    }
                }
                (Some(ls), None) if rhs.axes.is_subset(&shared) => {
                    for (lk, a) in ls {
                        out.add(lk.clone(), f(*a, 0));
                    }
                }
                (None, Some(rs)) if self.axes.is_subset(&shared) => {
                    for (rk, b) in rs {
                        out.add(rk.clone(), f(0, *b));
                    }
                }
                _ => {}
            }
        }
        out
    }

    // ── down-project ────────────────────────────────────────────────────────

    /// Split this pool across `driver`, normalized within the shared axes, onto
    /// `self.axes ∪ driver.axes`. Fused multiply-divide with largest-remainder
    /// rounding per fiber, so each pool cell's split is penny-exact. Pool mass in
    /// a shared cell the driver doesn't cover **parks on [`ANY`]** across the new
    /// axes — in-cube, so `total()` is conserved.
    pub fn allocate(&self, driver: &Measure) -> Measure {
        let shared: BTreeSet<Axis> = self.axes.intersection(&driver.axes).copied().collect();
        let out_axes: BTreeSet<Axis> = self.axes.union(&driver.axes).copied().collect();
        let new_axes: BTreeSet<Axis> = driver.axes.difference(&self.axes).copied().collect();
        let pool_b = self.bucket(&shared);
        let drv_b = driver.bucket(&shared);
        let mut out = Measure::with_axes(out_axes);

        for (s, prows) in &pool_b {
            let drows = drv_b
                .get(s)
                .filter(|d| d.iter().map(|(_, w)| *w).sum::<i128>() > 0);
            let Some(drows) = drows else {
                for (pk, amt) in prows {
                    out.add(pk.with_any(&new_axes), *amt); // no driver -> park
                }
                continue;
            };
            let weights: Vec<Money> = drows.iter().map(|(_, w)| *w).collect();
            let wtotal: i128 = weights.iter().sum();
            let dkeys: Vec<&Key> = drows.iter().map(|(dk, _)| dk).collect();
            for (pk, amt) in prows {
                let shares = largest_remainder(*amt, &weights, wtotal, &dkeys);
                for ((dk, _), share) in drows.iter().zip(&shares) {
                    out.add(pk.merge(dk), *share);
                }
            }
        }
        out
    }

    // ── select / route ──────────────────────────────────────────────────────

    /// The sub-cube of cells matching `keep` (a `where`); the complement is
    /// dropped. Lossy by design.
    pub fn select(&self, keep: impl Fn(&Key, Money) -> bool) -> Measure {
        let mut out = Measure::with_axes(self.axes.clone());
        for (k, v) in &self.cells {
            if keep(k, *v) {
                out.add(k.clone(), *v);
            }
        }
        out
    }

    /// Split into `(matching, rest)` — exhaustive and disjoint, so
    /// `a.combine(&b, |x, y| x + y)` rebuilds the original. You hold both halves;
    /// drop one and you leak mass.
    pub fn partition(&self, pred: impl Fn(&Key, Money) -> bool) -> (Measure, Measure) {
        let mut yes = Measure::with_axes(self.axes.clone());
        let mut no = Measure::with_axes(self.axes.clone());
        for (k, v) in &self.cells {
            if pred(k, *v) {
                yes.add(k.clone(), *v);
            } else {
                no.add(k.clone(), *v);
            }
        }
        (yes, no)
    }

    /// Query a cube: keep only cells matching these coords, then drop those
    /// (now-constant) axes. E.g. `cube.slice(&[("metric", REV), ("year", 2024)])`
    /// yields the revenue cube for that year over the remaining axes.
    pub fn slice(&self, at: &[(Axis, Coord)]) -> Measure {
        let at = at.to_vec();
        let drop: BTreeSet<Axis> = at.iter().map(|(a, _)| *a).collect();
        let keep: Vec<Axis> = self
            .axes
            .iter()
            .copied()
            .filter(|a| !drop.contains(a))
            .collect();
        self.select(move |k, _| at.iter().all(|&(a, c)| k.get(a) == Some(c)))
            .marginalize(&keep)
    }

    /// Route a per-group rule and **recombine automatically**: partition cells by
    /// their coord on `axis`, apply `f(coord, subcube)` to each group, and sum
    /// the results back. The recombine is structural, so it can't be forgotten —
    /// "company 1 by headcount, company 2 by floor space, then merge".
    pub fn group_by(&self, axis: Axis, f: impl Fn(Coord, &Measure) -> Measure) -> Measure {
        let mut groups: BTreeMap<Coord, Measure> = BTreeMap::new();
        for (k, v) in &self.cells {
            let c = k.get(axis).unwrap_or(ANY);
            groups
                .entry(c)
                .or_insert_with(|| Measure::with_axes(self.axes.clone()))
                .add(k.clone(), *v);
        }
        let mut out = Measure::default();
        for (c, sub) in &groups {
            out = out.combine(&f(*c, sub), |a, b| a + b);
        }
        out
    }

    // ── residual: refine / coarsen along the grain ──────────────────────────

    /// The in-cube residual view: cells unresolved ([`ANY`]) on some axis.
    pub fn pending(&self) -> Measure {
        self.select(|k, _| k.has_any())
    }

    /// Refine the [`ANY`] catch-all on `dim`: redistribute every cell unresolved
    /// on `dim` across `dim`'s real coords by `driver` (conditioned on the other
    /// axes), folding the result back in. Cells the driver can't place stay
    /// parked. Chain one rake per dimension, each with its own rule.
    pub fn rake(&self, dim: Axis, driver: &Measure) -> Measure {
        let (pending, resolved) = self.partition(|k, _| k.get(dim) == Some(ANY));
        if pending.is_empty() {
            return resolved;
        }
        let raked = pending.rekey(|k| k.without(dim)).allocate(driver);
        resolved.combine(&raked, |a, b| a + b)
    }

    /// The inverse of [`rake`](Self::rake): surrender detail to [`ANY`] for
    /// sub-threshold cells. `order` lists the dims you will give up,
    /// **least-strict first**; dims omitted are protected (e.g. time, company,
    /// GL). Per dim: cells `>= eps` keep their grain; the rest coarsen to `ANY`
    /// and re-sum, so dust merges. Merged cells that clear `eps` graduate and
    /// stay; the rest fall through to the next dim. Conserves `total()`.
    pub fn vacuum(&self, eps: Money, order: &[Axis]) -> Measure {
        let mut current = self.clone();
        for &dim in order {
            if !current.axes.contains(&dim) {
                continue;
            }
            let (small, big) = current.partition(|_, v| v.abs() < eps);
            let coarsened = small.rekey(|k| k.with(dim, ANY));
            current = big.combine(&coarsened, |a, b| a + b);
        }
        current
    }

    fn bucket(&self, shared: &BTreeSet<Axis>) -> HashMap<Key, Vec<(Key, Money)>> {
        let mut m: HashMap<Key, Vec<(Key, Money)>> = HashMap::new();
        for (k, v) in &self.cells {
            m.entry(k.project_set(shared))
                .or_default()
                .push((k.clone(), *v));
        }
        m
    }
}

/// Floor each share of `amt` by weight, then hand the leftover units to the
/// largest fractional remainders (ties broken by key) so the parts sum to
/// exactly `amt`. Works for negative `amt`.
fn largest_remainder(amt: Money, weights: &[Money], wtotal: i128, keys: &[&Key]) -> Vec<Money> {
    let n = weights.len();
    let mut base = vec![0i128; n];
    let mut rem = vec![0i128; n];
    let mut sum_base = 0i128;
    for i in 0..n {
        let num = amt
            .checked_mul(weights[i])
            .expect("allocate: weight*amount overflow");
        base[i] = num.div_euclid(wtotal);
        rem[i] = num.rem_euclid(wtotal);
        sum_base += base[i];
    }
    let deficit = (amt - sum_base) as usize; // in [0, n)
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| rem[b].cmp(&rem[a]).then_with(|| keys[a].cmp(keys[b])));
    for &i in idx.iter().take(deficit) {
        base[i] += 1;
    }
    base
}

/// Iterate `step` from `init` until `converged(next, prev)` or `max_passes`
/// elapse. The reciprocal-allocation combinator: curry the structure into
/// `step` (an endofunction `S -> S`) and pass a value-norm convergence test.
/// Mirrors [`crate::strategy::fixed_point`] over a continuous state.
pub fn fixed_point<S>(
    init: S,
    step: impl Fn(&S) -> S,
    converged: impl Fn(&S, &S) -> bool,
    max_passes: usize,
) -> S {
    let mut state = init;
    for _ in 0..max_passes {
        let next = step(&state);
        if converged(&next, &state) {
            return next;
        }
        state = next;
    }
    state
}

#[cfg(test)]
mod tests;
