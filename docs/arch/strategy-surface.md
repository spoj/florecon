# Strategy surface: a small orthogonal combinator algebra

Status: **DESIGN**. A holistic review and redesign of `src/strategy/` (combinators,
primitives, the `flow` model) informed by a real intercompany matcher (the Python
prototype that drove the old data-plan DSL) and the freedoms the native plugin
architecture now gives us.

The thesis: **the plugin is native Rust compiled to one wasm, so predicates and
costs are closures, not serialized data.** That single fact collapses an entire
expression sub-DSL (`P.le/eq/and_/or_/mul/lit/col/cost_spec/tier/ABS_NET/MAX_ABS/
SIZE/MIN_SIDE/TOKEN_SHARED/AMOUNT_EQUAL`) into ordinary Rust. What remains is a
small algebra of *structural* combinators over a conserved bag, plus a handful of
matchers. This doc fixes that algebra.

---

## 1. Two layers, one rule

Everything a recon plugin does splits cleanly into two layers. Keeping the split
sharp is what keeps each layer small.

**Projection** (`RowView -> Row`, plus `id`/`primary`): all normalization and
feature prep. Code-hashing keys, choosing a numeraire, tokenizing references,
mapping opposite GL accounts to a shared key, day ordinals, materiality
originals. This is *per-row* and has nothing to do with combinators. In the
Python prototype this was the pandas half (`norm_code`, `token_text`,
`epoch_day`, `objsub_match_key`, `choose_trx`, `coalesce_input_rows`).

**Strategy**: the combinator algebra over the bag of projected rows. This doc.

**The rule that divides them:** *if it derives a row's own fields, it is
projection; if it partitions a bag into groups, it is strategy.* Consequences:

- Tokenization is **projection**, never a strategy. (Answered earlier: a
  `tokenize_with(...)` *strategy* is a category error — by the time a bag
  exists the raw text is gone; only the derived `tokens` survive on `Row`.) The
  customization seam already lives at the strategy edge as the closures
  `signal_group(|r| r.tokens.clone(), …)` and `FlowSpec::match_keys`.
- Split-booking pre-aggregation (`coalesce_input_rows`) is **host/projection**:
  the strategy algebra cannot merge two `ExtId`s into one — every `Item` is a
  row. Pre-coalesce upstream of `upsert`.
- Opposite-account bridging (`2013RTD ↔ 2013PTD`, `1101 ↔ 4404`) is
  **projection**: emit a shared `objsub_match` key; the strategy just buckets on
  it.

---

## 2. The conserved bag (unchanged foundation)

These types are the algebra's vocabulary and stay as-is:

```rust
struct Item<E> { id: ExtId, original: i64, amount: i64, data: E }  // a lot in the active numeraire
struct Group  { members: Vec<Allocation>, origin: String, net: i64, reason: Option<String> }
struct Resolution<E> { groups: Vec<Group>, residual: Vec<Item<E>> }
trait  Strategy<E> { fn run(&mut self, bag: Vec<Item<E>>) -> Resolution<E>; }
```

Invariant every node preserves: **`groups ⊎ residual = input`** (disjoint in
summed `(id, amount)`; nothing lost, nothing invented). `original` is the
materiality scale; `amount` is the shrinking residual. `pivot` is the only node
that switches the active numeraire for a subtree. This is the spine and it is
already right — the redesign is entirely about the *combinator surface* sitting
on top.

`Tol` (absolute / relative-bps-with-floor) is the one shared value type and
should be the **only** way any node expresses a tolerance (see §6.D).

---

## 3. Design principles

1. **Closures over data.** Every predicate, key, order, cost is `Fn`. No
   expression IR. The host-facing data-DSL is dead; this is its native
   replacement.
2. **Orthogonal families by *where they act*.** A node touches the bag on the
   way in, matches, reshapes groups on the way out, or terminates. Four
   families, four shapes. A node belongs to exactly one.
3. **Core is orthogonal; recipes live in the plugin.** `safe_flow`,
   `safe_agg`, `net_within_tol`, `all_cur` are *compositions*, not primitives.
   They belong in plugin-local helpers, proving the core is sufficient.
4. **One name per concept.** No aliases (`filter`/`accept_if`), no sugar that
   isn't pulling real weight.
5. **Information closures need is reachable.** Group-shape metrics
   (`size`, `min_side`, `abs_net`, `max_abs`) are methods on `Group`, so gate
   closures read like the predicates they replace.

---

## 4. The surface, by family

Type shapes:

- **Leaf** `Bag -> (Groups, Residual)` — a matcher.
- **Bag combinator** `Bag -> Bag'` then run child — routes/orders the *input*.
- **Group combinator** `Groups -> (Groups', Residual)` — reshapes a child's
  *output*.
- **Soaker** `Bag -> Groups` — terminal classifier of the residual tail.

```
            bag combinators                    leaves                group combinators
input ──▶ seq/partition_by/             ──▶ exact_1to1/agg_net/  ──▶ labeled/accept_if/   ──▶ groups
          partition_by_with/when/            signal_group/             coalesce/whole_net/material
          windowed/pivot/fixed_point         running_zero/flow
                                                                   soakers terminate ──▶ groups (∅ residual)
```

### 4.1 Bag combinators (route & order the input)

| Combinator | Signature (sketch) | Role |
|---|---|---|
| `seq` | `seq(Vec<Strategy>)` | cascade: each step runs on the prior residual |
| `partition_by` | `partition_by(key: Fn(&E)->K, factory: Fn()->Strategy)` | shard by key equality; **one warm child per shard**, uniform subtree |
| `partition_by_with` *(new)* | `partition_by_with(key: Fn(&E)->K, factory: Fn(&K)->Strategy)` | as above, but the **factory gets the key** so plain Rust picks a per-key subtree |
| `when` *(new)* | `when(pred: Fn(&E)->bool, inner)` | route matching items into `inner`; non-matching (and `inner`'s residual) pass through |
| `windowed` | `windowed(order: Fn(&E)->i64, width, inner)` | sort + sweep bands with carry; locality for the cheap leaves |
| `pivot` | `pivot(amount: Fn(&E)->i64, inner)` | run `inner` in a different numeraire, translate back |
| `fixed_point` | `fixed_point(inner, max_passes)` | iterate `inner` on its own residual to convergence |
| `identity` *(new)* | `identity()` | no-op passthrough; the unit of `seq` |

**Two loop shapes.** `fixed_point(inner, n)` is *convergence-driven*: it re-runs
one warm `inner` on its own residual until the residual stops changing (or `n`
passes elapse). The complementary shape is *schedule-driven* — run a fixed
sequence of stages whose parameters vary by index, the canonical use being an
**expanding-window** ladder (match same-day, then ±1wk, then ±1mo), each stage
committing its confident matches and handing the residual down. That is just
`seq` over a built range, with the closure capturing the pass index:

```rust
seq((0..n).map(|i| whole_net(0, flow(spec.window(7 << i)))).collect())
```

A one-line `for_n(n, |i| …)` sugar over that `seq` reads better and signals
"schedule, not converge", but it is *sugar*, not a core primitive — the
expressive content already lives in `seq`. Reach for `fixed_point` when the
parameters are fixed and you iterate to stability; reach for the scheduled
`seq`/`for_n` when the parameters must change per pass (and rebuild a fresh
child each pass, so a warm basis is never reused against re-priced edges).

**`branch` is removed; there is no `cond`.** Predicate routing is expressed two
ways, chosen by whether you want *cascade* or *hard partition*:

- **Cascade (the default idiom):** `seq(when(p1, a), when(p2, b), …)`. Each guard
  routes its matching items into a subtree; everything else — including a
  subtree's own residual — flows on to the next step. This is exactly what the
  Python plan's `branch(pred, A, P.seq())` did (it continued into later
  `agg_net`s on the leftovers), so `seq + when` is the faithful, more readable
  expression. `when(p, inner)` is the only one-sided primitive; `identity()` is
  its do-nothing arm.
- **Hard partition (no cross-talk, warm shards):** `partition_by_with(key, |k|
  …)`. Use it only when an item must land in *exactly one* key-chosen subtree
  with its own warm state and never cascade into a sibling.

We deliberately reject a first-match `cond([(pred, inner), …])`: its priority/
fallthrough semantics are a *race* (order matters, overlapping predicates
silently shadow), which nothing in the domain needs. Equality-partition
(`partition_by_with`) and cascade (`seq + when`) cover the real cases without the
race footgun.

### 4.2 Leaves (matchers)

| Leaf | Signature | What it pulls |
|---|---|---|
| `exact_1to1` | `exact_1to1(key: Fn(&E)->Option<u64>)` | opposite-sign equal-magnitude **pairs** sharing `key` |
| `agg_net` | `agg_net(key: Fn(&E)->u64, tol)` | a **whole bucket** that nets to zero within `tol` |
| `signal_group` | `signal_group(signals: Fn(&E)->Vec<u64>, tol, cap)` | multi-key (token) buckets that net; greedy specific-first, `cap`-bounded |
| `running_zero` | `running_zero(order: Fn(&E)->i64, tol)` | ordered running-balance **clearing segments** |
| `flow` | `flow(FlowSpec<E>)` | the global **min-cost-flow arbiter** over the ambiguous remainder; emits the matching as **raw arcs** (one 2-member net-0 group per positive-flow arc), *not* settlements |

These four-plus-one share one shape — *bucket, then accept-if-balanced* —
differing only in how buckets form (key / multi-key / order / proximity-graph)
and the acceptance rule (pairwise / whole-net / running / optimized). That shared
shape is worth documenting but **not** worth collapsing into a god-leaf: the
disjoint-vs-overlapping and pairwise-vs-whole distinctions are the point.

`exact_1to1_any()` is **removed** — it is `exact_1to1(|_| Some(0))`; the sugar
earns nothing.

### 4.3 Group combinators (reshape the output)

| Combinator | Signature | Role |
|---|---|---|
| `labeled` | `labeled(tag, inner)` | stamp a human `reason` on every group a subtree forms |
| `accept_if` | `accept_if(pred: Fn(&Group)->bool, inner)` | gate groups; **dissolve rejects back to residual** (conserving) |
| `coalesce` | `coalesce(origin, inner)` | fuse interlocking groups (shared member) into settlement clusters — the **settlement authority** |
| `settle` | `settle(spec) = coalesce("flow", flow(spec))` | the blessed settlement view over `flow`: discover arcs, fold them into clusters |
| `whole_net` | `whole_net(tol, inner)` | commit clusters of **whole lines** whose net clears within `tol`; the break stays **inside** the group. `whole_net(0, inner)` commits only self-contained (net-exactly-0) clusters |
| `material` | `material(tol, inner)` | drop **immaterial** groups: keep iff moved volume `M=Σ\|leg\|` exceeds `tol` (`Rel` measures `M` against `Σ\|original\|`), else dissolve back to residual |

`filter` is **removed** as a redundant alias of `accept_if`. Pick the name that
states the semantics (we *accept* groups passing the predicate and dissolve the
rest); `filter` wrongly suggests filtering *items*.

`trim`/`snap` (edge-materiality reshapers) are **removed**: speculative surface
with a sharp edge (per-row-original trimming could sever a shared edge into a
lopsided net-≠0 group). The partial-lot reshaping niche will be revisited from
real use-cases; for now `whole_net` (commit-whole-or-dissolve) and the `soak`
family (absorb residual into auditable buckets) cover the demonstrated needs.

`flow` is a **strict primitive**: it emits the matching as raw arcs and nothing
else. `coalesce` is the single **settlement authority** that folds an allocation
hypergraph (a row split across arcs/groups) into the coarser, human-actionable
cluster view; `settle` is the one-token `coalesce("flow", flow(spec))` sugar for
the common case. This keeps connected-components logic in exactly one place
(`coalesce`) instead of duplicated inside `flow`.

`whole_net` is the **commit gate** for a *discovered* grouping, and together with
`flow` it exposes the library's two matching paradigms:

- **Transportation (`flow`):** a line is *divisible*. `flow` splits amounts at
  the unit level, every matched cluster nets to **0**, and the difference is a
  separate **residual** lot. `whole_net(0, flow(..))` keeps only clusters that
  cleared *completely* — a near-miss like `+100 / -97` goes **entirely** to
  residual.
- **Netting (whole-line):** a line is *atomic*. `agg_net`/`signal_group` (§4.2)
  bucket whole lines by a **key** and accept iff the bucket nets within `tol`,
  the break staying **inside** the group as its `net`. `whole_net(tol, inner)`
  brings that acceptance to a *discovered* grouping: it makes every member line
  whole (reclaiming its residual tail) and accepts the cluster iff
  `|net| <= tol`, so `+100 / -97` becomes one matched group with net `+3`.

Because a line is atomic in the netting view, groups that share a member id are
**one settlement**: `whole_net` coalesces them first, so a line's tail can only
ever go to **ground** (never to a sibling group) and the reclaim is unambiguous;
conservation holds (each id ends up wholly in one group or wholly in residual).
`net == 0` is **not** sufficient for wholeness (a group can net to zero while a
member bleeds into residual), which is exactly why this is a distinct primitive
and not an `accept_if(|g| g.abs_net()==0)` gate — a `Group` sheds each row's
`original` and never sees the residual, so "is this line whole?" is out of a
`Fn(&Group)->bool`'s scope. Tolerance basis follows `Tol` (§6): `Abs` fixed,
`Rel` against the **smallest** leg, `RelMax` against the **largest** — there is
no total-of-legs basis.

**Choosing the matcher:** use `agg_net` when a **key** already defines the group;
use `whole_net(tol, flow(spec))` for the **"flow discovers, netting commits"**
idiom when the grouping must be inferred from amounts/proximity; use
`whole_net(0, flow(spec))` when only a *complete* clear counts; use `settle` when
you just want the discovered settlements without a tolerance commit. `flow` +
`soak` (net-0 group, the break as a labelled residual bucket) and `whole_net`
(break kept *inside* the matched group) are the two valid bookkeeping choices for
the same mismatch — *separate break lot* vs *matched-with-break*.

### 4.4 Soakers (terminate the tail)

| Soaker | Signature | Role |
|---|---|---|
| `soak_all` | `soak_all(mode, origin, key)` | consume **every** non-zero residual into singleton/bucket classes |
| `soak_small` | `soak_small(tol, mode, origin, key)` | consume only residual **immaterial vs its `original`** (uses `Tol`) |
| `soak_if` | `soak_if(pred: Fn(&Item)->bool, mode, origin, key)` | the predicate-general parent the other two specialize |

`SoakMode { Singleton, Bucket }`. Non-zero group `net` is *expected* here — a
soaker is a classifier (variance / write-off / unmatched), not a matcher. Place
last in a `seq`.

**Change vs the just-restored version:** `soak_small` takes a `Tol`
(`impl Into<Tol>`), not bespoke `max_bps`/`max_abs`. The threshold is
`tol.slack(item.original)` — the exact materiality idiom, and now consistent
with every other tolerance in the algebra. `is_small_residual` and the twin
`Option<i64>` knobs disappear.

### 4.5 `Group` metrics (the closure ergonomics fix)

The Python plan's group-level expression atoms become methods, so `accept_if`
closures read naturally:

```rust
impl Group {
    fn size(&self) -> usize;       // P.SIZE      — member count
    fn abs_net(&self) -> i64;      // P.ABS_NET   — |net|
    fn max_abs(&self) -> i64;      // P.MAX_ABS   — largest |member amount|
    fn min_side(&self) -> usize;   // P.MIN_SIDE  — min(#pos, #neg) by amount sign
    fn clean(&self, tol: Tol) -> bool; // |net| within tol of the bucket scale
}
```

The `safe_flow` gate

```python
P.and_(P.le(P.SIZE, 100), P.and_(P.le(P.MIN_SIDE, 2), net_within_tol()))
```

becomes

```rust
accept_if(|g| g.size() <= 100 && g.min_side() <= 2 && g.clean(tol), inner)
```

---

## 5. `flow`: `Model` trait → `FlowSpec` builder

`Model` is the one leaf that breaks the closure idiom — a trait + associated
type where everything else takes closures, and its name pretends to be a domain
concept when it is just "the five hooks `flow` needs." Replace it with a closure
builder consistent with the rest of the algebra:

```rust
pub struct FlowSpec<E> {                    // closures behind Arc so Clone is cheap
    penalty:    Arc<dyn Fn(&E) -> f64>,             // cost of leaving a lot unmatched
    block_key:  Arc<dyn Fn(&E) -> i64>,             // 1-D proximity ordering (e.g. day)
    window:     i64,                                 // proximity radius; <0 = exact-join only
    match_keys: Arc<dyn Fn(&E, i64) -> Vec<u64>>,    // exact-join keys (tokens, amount bridges)
    cost:       Arc<dyn Fn(&E, i64, &E, i64) -> Option<f64>>, // lot-aware; None forbids the pair
}

impl<E> FlowSpec<E> {
    fn new() -> Self;                       // penalty 0, block_key 0, window -1, no keys, cost None
    fn penalty(self, f64) -> Self;          // constant …
    fn penalty_fn(self, Fn(&E)->f64) -> Self;   // … or per-lot
    fn window(self, i64) -> Self;
    fn block_key(self, Fn(&E)->i64) -> Self;
    fn match_keys(self, Fn(&E)->Vec<u64>) -> Self;        // amount-independent convenience
    fn match_keys_lot(self, Fn(&E,i64)->Vec<u64>) -> Self; // full lot-aware form
    fn cost(self, Fn(&E,&E)->Option<f64>) -> Self;         // amount-independent convenience
    fn cost_lot(self, Fn(&E,i64,&E,i64)->Option<f64>) -> Self;
}

pub fn flow<E: Clone + 'static>(spec: FlowSpec<E>) -> Box<dyn Strategy<E>>;
```

Notes:

- **Lot form is canonical, row form is sugar.** The trait carried both
  `cost`/`cost_lot` and `match_keys`/`match_keys_lot` for default-method
  ergonomics. The builder stores the lot-aware closure and `.cost(...)` simply
  wraps an amount-ignoring one. Same defaults, no trait machinery.
- **`Arc<dyn Fn>` for `Clone`.** `flow`'s warm-vs-cold determinism guard rebuilds
  a cold leaf via `spec.clone()`; `Arc` makes that a pointer bump and is strictly
  better than today's `M: Clone` deep-clone for any spec holding real data.
- **Cost:** one indirect call per candidate arc instead of a monomorphized
  inline. Real but in line with the rest of the algebra's dispatch, and `cost`
  is O(candidate arcs). Acceptable; the consistency win dominates.
- The leaf internals (`Entry`, `by_key`, warm basis, readback) are unchanged —
  only the `model.foo(tx)` calls become `spec.foo(tx)` closure calls.

A tiered-cost helper (the Python `cost_spec(tier(...))` shape) is genuinely
useful but **domain sugar**, so it ships as an optional SDK helper, not core:

```rust
// florecon::flow_util
tiered(&[
    (&[Cond::TokenShared, Cond::AmountEqual], 1.5, slope),
    (&[Cond::TokenShared],                    2.0, slope),
    (&[Cond::AmountEqual],                     4.5, slope*10.0),
]) -> impl Fn(&E,&E)->Option<f64>
```

---

## 6. Removals, renames, and unifications (the diff)

| Action | Item | Rationale |
|---|---|---|
| **remove** | `Model` trait | → `FlowSpec` closure builder (§5) |
| **remove** | `exact_1to1_any` | sugar over `exact_1to1(\|_\| Some(0))` |
| **remove** | `filter` | redundant alias of `accept_if` |
| **remove** | `branch` | replaced by cascade (`seq + when`) and hard-partition (`partition_by_with`); see §4.1 |
| **rename** | `flow(model)` | now `flow(spec: FlowSpec)` |
| **add** | `when(pred, inner)` | the one-sided guard the prototype used 3× (cascade routing) |
| **add** | `identity()` | the unit of `seq`; replaces empty `seq()` idiom |
| **add** | `partition_by_with(key, \|k\| …)` | `partition_by` with a key-aware factory: per-key subtree, hard-disjoint warm shards |
| **add** | `Group::{size,abs_net,max_abs,min_side,clean}` | closure ergonomics; replaces the group-expr atoms |
| **change** | `soak_small(max_bps,max_abs,…)` → `soak_small(tol,…)` | one tolerance type everywhere (§6.D) |
| **reject** | `cond([(pred, inner), …])` | first-match *race* semantics (order-dependent, shadowing); not needed |
| **keep** | `seq, partition_by, windowed, pivot, fixed_point` | the structural spine, already orthogonal |
| **keep** | `agg_net, signal_group, running_zero, exact_1to1, flow` | distinct matchers, shared shape documented |
| **keep** | `labeled, accept_if, coalesce` | the post-matching group algebra |
| **add** | `settle(spec)` | `coalesce("flow", flow(spec))` sugar: the settlement view of the arcs `flow` now emits raw |
| **add** | `whole_net(tol, inner)` | commit whole-line clusters within tolerance; `whole_net(0, ..)` = self-contained only |
| **add** | `material(tol, inner)` | drop groups whose moved volume `Σ\|leg\|` is immaterial (`Rel`: vs `Σ\|original\|`); the relative-to-original gate `accept_if` can't express |
| **remove** | `trim, snap` | speculative edge-reshapers with a lopsided-severing sharp edge; revisit from real partial-lot use-cases |
| **remove** | `whole_only` | exactly `whole_net(0, inner)`; the sugar earns nothing |
| **reshape** | `flow(spec)` | now a strict primitive returning **raw arcs**; grouping moves to `coalesce`/`settle` |
| **keep** | `soak_all, soak_if, SoakMode` | terminal classifiers |

### 6.D One open semantic decision: relative-tolerance scale

`Tol::Rel { bps, floor }` currently scales bps off the bucket's **smallest**
non-zero leg. The prototype's `net_within_tol` scaled off **`MAX_ABS`** (the
largest leg) — far more permissive. With soakers and `clean()` both routing
through `Tol`, pick one and document it:

- **smallest leg** (current): conservative; a tiny leg can't drag a big bucket
  into "balanced".
- **largest leg** (prototype): lenient; matches "within 1bp of the trade".

Recommendation: keep **smallest-leg** as `Tol::Rel` (the safe default) and, if
the lenient form is needed, add an explicit `Tol::RelMax { bps, floor }` rather
than silently changing the scale. One value type, two named scale references, no
hidden behavior.

---

## 7. The intercompany plugin on the new surface

The whole Python waterfall — minus the dead expression DSL — as native helpers
composed from the orthogonal core. This is the sufficiency proof.

```rust
// ---- plugin-local recipes (NOT core) -------------------------------------
const FLOOR: Tol = Tol::Abs(1_000);                  // $10 floor
const REL:   Tol = Tol::Rel { bps: 1, floor: 1_000 };

// "net within floor OR 1bp": just the relative Tol (floor is its minimum).
fn clean(g: &Group) -> bool { g.clean(REL) }

fn safe_agg(tag: &str, key: impl Fn(&Row)->u64 + 'static) -> Box<dyn Strategy<Row>> {
    accept_if(clean, labeled(tag, agg_net(key, REL)))
}

fn safe_flow(tag: &str, win: i64, max_size: usize, max_side: usize,
             cost: impl Fn(&Row,&Row)->Option<f64> + 'static) -> Box<dyn Strategy<Row>> {
    let spec = FlowSpec::new()
        .window(win).penalty(1000.0)
        .block_key(|r: &Row| r.day)
        .match_keys(|r| r.tokens.clone())
        .cost(cost);
    accept_if(
        move |g| g.size() <= max_size && g.min_side() <= max_side && g.clean(REL),
        coalesce(tag, labeled(tag, flow(spec.clone()))),
    )
}

fn transactional(prefix: &str) -> Box<dyn Strategy<Row>> {
    seq(vec![
        labeled(&fmt(prefix,"SIGNAL"),   signal_group(|r| r.tokens.clone(), FLOOR, 256)),
        windowed(|r| r.day, 10, safe_agg(&fmt(prefix,"OBJSUB"), |r| r.objsub_match)),
        windowed(|r| r.day, 10, safe_agg(&fmt(prefix,"UNIT"),   |r| r.unit)),
        labeled(&fmt(prefix,"EXACT"),    exact_1to1(|_| Some(0))),
        safe_flow(&fmt(prefix,"FLOW"), 15, 100, 2, tiered_cost(0.0)),
        windowed(|r| r.day, 10,
            safe_flow(&fmt(prefix,"SHORTFLOW"), 10, 100, 5, tiered_cost(0.05))),
    ])
}

// numeraire iteration = `pivot`, exactly as before
fn all_cur() -> Box<dyn Strategy<Row>> {
    seq(vec![
        when(|r: &Row| r.trx_amt != 0,
            partition_by(|r| r.trx_ccy, || pivot(|r| r.trx_amt, transactional("T_TRX")))),
        when(|r| r.trx_usd != 0, pivot(|r| r.trx_usd, transactional("T_USD"))),
        transactional("T_BSUSD"),
    ])
}

fn strategy() -> Box<dyn Strategy<Row>> {
    let structural = seq(vec![
        safe_agg("S1_GLOBAL_OBJSUB", |r| r.objsub_match),
        partition_by(|r| r.unit, || seq(vec![
            when(|r: &Row| r.prior_close, seq(vec![
                safe_agg("S0A_PRIOR_UNIT",   |r| r.unit),
                partition_by(|r| r.source_class, || safe_agg("S0B_PRIOR_SRC", |r| r.unit)),
                partition_by(|r| r.objsub_match, || safe_agg("S0C_PRIOR_OBJ", |r| r.unit)),
            ])),
            safe_agg("S3_UNIT", |r| r.unit),
            partition_by(|r| r.source_class, || safe_agg("S5_UNIT_SRC", |r| r.unit)),
            partition_by(|r| r.objsub_match, || safe_agg("S7_UNIT_OBJ", |r| r.unit)),
        ])),
    ]);
    let core = seq(vec![
        structural,
        soak_small(FLOOR, SoakMode::Bucket, "S8_SMALL", |i| i.data.source_class),
        partition_by(|r| r.unit, all_cur),
    ]);
    fixed_point(core, 4)
}
```

Everything domain-specific is a closure or a plugin-local `fn`; the core
contributes only orthogonal nodes. No expression IR, no `Model` impl, no
group-metric atoms.

---

## 8. Surface summary (the whole public API)

```
foundation   Item  Group  Resolution  Strategy  Tol  Allocation  ExtId
             Group::{member_ids,size,abs_net,max_abs,min_side,clean}

bag combs    seq  partition_by  partition_by_with  when  windowed  pivot  fixed_point  identity
leaves       exact_1to1  agg_net  signal_group  running_zero  flow(FlowSpec)  settle(FlowSpec)
group combs  labeled  accept_if  coalesce  whole_net  material
soakers      soak_all  soak_small  soak_if   (SoakMode)
flow         FlowSpec builder  (+ optional flow_util::tiered cost helper)
```

**22 constructor functions** (8 bag combinators + 5 group combinators + 5 leaves
+ `settle` flow-sugar + 3 soakers) **+ one builder** (`FlowSpec`). `material` is
the third materiality cell alongside `whole_net` (keep a small *break* inside)
and `soak_small` (absorb a small *residual*): it kicks a small *match* out,
measuring moved volume against each row's **original** — the relative case an
[`accept_if`] closure cannot express, since `Group` sheds `original`. `flow` is a
strict primitive returning raw arcs; `coalesce` is the sole settlement authority
and `settle` its one-token `flow` sugar. Every node obeys one closure idiom and
belongs to exactly one family.
```
