# Strategy surface: a small orthogonal combinator algebra

Status: **LANDED**. A holistic review and redesign of `src/strategy/` (combinators,
primitives, the `flow` model) informed by a real intercompany matcher (the Python
prototype that drove the old data-plan DSL) and the freedoms the native plugin
architecture now gives us. This doc describes the surface that ships in
`src/strategy/mod.rs`.

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
  `signal_group(|i| i.data.tokens.clone(), …)` and `FlowSpec::match_keys`.
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
trait  Strategy<E> { fn run(&self, bag: Vec<Item<E>>) -> Resolution<E>; }
```

Invariant every node preserves: **`groups ⊎ residual = input`** (disjoint in
summed `(id, amount)`; nothing lost, nothing invented). `original` is the
materiality scale; `amount` is the shrinking residual. `pivot` is the only node
that switches the active numeraire for a subtree. This is the spine and it is
already right — the redesign is entirely about the *combinator surface* sitting
on top.

**There is no tolerance *type*.** Acceptance and materiality are a single
concept: a closure over a [`GroupView`] (§4.5), the borrowed gate-time lens that
lends back the two things `Group`/`Allocation` shed — each member's birth-size
`original` and a borrow of its payload `&E`. The author writes the inequality
inline (`|g| g.net().abs() <= 5 * g.min_leg() / 10_000`); the smallest-leg vs
largest-leg vs total-original choice is just *which accessor you call*. No `Tol`,
no `Scale`, no tolerance helpers (see §6).

---

## 3. Design principles

1. **Closures over data.** Every predicate, key, order, cost is `Fn`. No
   expression IR. The host-facing data-DSL is dead; this is its native
   replacement.
2. **Orthogonal families by *where they act*.** A node touches the bag on the
   way in, matches, reshapes groups on the way out, or terminates. Four
   families, four shapes. A node belongs to exactly one.
3. **Core is orthogonal; recipes live in the plugin.** `safe_flow`,
   `safe_agg`, `clean`, `all_cur` are *compositions*, not primitives. They
   belong in plugin-local helpers, proving the core is sufficient.
4. **One name per concept.** No aliases, no sugar that isn't pulling real
   weight, and exactly one acceptance concept (a `Fn(&GroupView)`), not a
   tolerance type *plus* predicate gates.
5. **Information closures need is reachable.** Gate metrics (`net`, `gross`,
   `min_leg`, `max_leg`, `original_total`, `size`, `min_side`) are accessors on
   [`GroupView`], so an acceptance closure reads like the predicate it replaces —
   including *materiality*, which needs each row's `original`.

---

## 4. The surface, by family

Type shapes:

- **Leaf** `Bag -> (Groups, Residual)` — a matcher.
- **Bag combinator** `Bag -> Bag'` then run child — routes/orders the *input*.
- **Group combinator** `Groups -> (Groups', Residual)` — reshapes a child's
  *output*.
- **Soaker** `Bag -> Groups` — terminal classifier of the residual tail.

```
            bag combinators                    leaves                 group combinators
input ──▶ seq/partition_by/when/        ──▶ exact_1to1/agg_net/  ──▶ labeled/accept_if/   ──▶ groups
          windowed/pivot/fixed_point/       signal_group/             coalesce/reclaim
          restart/identity                  cumulative/subset_sum/
                                             flow
                                                                  soak terminates ──▶ groups (∅ residual)
```

Every row-inspecting closure (`key`, `pred`, `order`, `signals`, `amount`)
uniformly takes the whole **`&Item<E>`** — id, `original`, the shrinking
`amount`, and the payload. (Warm-start is gone, so keying/ordering on lot state
is safe; still prefer stable fields — `id`, `original`, payload — for shard keys,
since `amount` shrinks across `seq`/`fixed_point` passes and would make shard
assignment pass-dependent.) Every acceptance closure takes a **`&GroupView<E>`**.

### 4.1 Bag combinators (route & order the input)

| Combinator | Signature (sketch) | Role |
|---|---|---|
| `seq` | `seq(Vec<Strategy>)` | cascade: each step runs on the prior residual |
| `partition_by` | `partition_by(key: Fn(&Item<E>)->K, factory: Fn(&K)->Strategy)` | shard by key equality; the factory **receives the shard key** so it can pick a per-key subtree (key-ignoring case = `\|_\| inner()`). Hard-disjoint: an item lands in exactly one subtree, no cascade |
| `when` | `when(pred: Fn(&Item<E>)->bool, inner)` | route matching items into `inner`; non-matching (and `inner`'s residual) pass through |
| `windowed` | `windowed(order: Fn(&Item<E>)->i64, width, inner)` | sort + sweep bands with carry; locality for the cheap leaves |
| `pivot` | `pivot(amount: Fn(&Item<E>)->i64, inner)` | run `inner` in a different numeraire, translate back |
| `fixed_point` | `fixed_point(inner, max_passes)` | iterate `inner` on its own residual to convergence |
| `restart` | `restart(n, seed, factory: Fn(u64)->Strategy)` | run a seeded family of `n` attempts, keep the **best** (most matched volume); the outer half of *propose/verify* |
| `identity` | `identity()` | no-op passthrough; the unit of `seq` |

**Two loop shapes.** `fixed_point(inner, n)` is *convergence-driven*: it re-runs
`inner` on its own residual until the residual stops changing (or `n`
passes elapse). The complementary shape is *schedule-driven* — run a fixed
sequence of stages whose parameters vary by index, the canonical use being an
**expanding-window** ladder (match same-day, then ±1wk, then ±1mo), each stage
committing its confident matches and handing the residual down. That is just
`seq` over a built range, with the closure capturing the pass index:

```rust
seq((0..n).map(|i| accept_if(|g| g.net() == 0, flow(spec.window(7 << i)))).collect())
```

Reach for `fixed_point` when the parameters are fixed and you iterate to
stability; reach for the scheduled `seq` when the parameters must change per
pass. (Strategies are stateless, so every pass rebuilds cold anyway — there is no
warm basis to worry about carrying across re-priced edges.)

**`branch` is removed; there is no `cond`.** Predicate routing is expressed two
ways, chosen by whether you want *cascade* or *hard partition*:

- **Cascade (the default idiom):** `seq(when(p1, a), when(p2, b), …)`. Each guard
  routes its matching items into a subtree; everything else — including a
  subtree's own residual — flows on to the next step. This is exactly what the
  Python plan's `branch(pred, A, P.seq())` did (it continued into later
  `agg_net`s on the leftovers), so `seq + when` is the faithful, more readable
  expression. `when(p, inner)` is the only one-sided primitive; `identity()` is
  its do-nothing arm.
- **Hard partition (no cross-talk, disjoint shards):** `partition_by(key, |k|
  …)`. Use it when an item must land in *exactly one* key-chosen subtree and
  never cascade into a sibling. Since the factory gets the key, plain Rust picks
  the per-key subtree (an AR/AP shard runs a different cascade than a GA shard);
  the key-ignoring case is just `|_| inner()`.

We deliberately reject a first-match `cond([(pred, inner), …])`: its priority/
fallthrough semantics are a *race* (order matters, overlapping predicates
silently shadow), which nothing in the domain needs. Equality-partition
(`partition_by`) and cascade (`seq + when`) cover the real cases without the
race footgun.

### 4.2 Leaves (matchers)

| Leaf | Signature | What it pulls |
|---|---|---|
| `exact_1to1` | `exact_1to1(key: Fn(&Item<E>)->Option<u64>)` | opposite-sign equal-magnitude **pairs** sharing `key` (`None` opts out) |
| `agg_net` | `agg_net(key: Fn(&Item<E>)->u64, accept: Fn(&GroupView<E>)->bool)` | a **whole bucket** that `accept`s its net (intrinsic rule: ≥ 2 lots, both signs) |
| `signal_group` | `signal_group(signals: Fn(&Item<E>)->Vec<u64>, accept: Fn(&GroupView<E>)->bool, cap)` | multi-key (token) buckets the gate accepts; greedy specific-first, `cap`-bounded |
| `cumulative` | `cumulative(order: Fn(&Item<E>)->i64, accept: Fn(&GroupView<E>)->bool)` | ordered running-balance **clearing segments**: close a segment the moment the segment-so-far `accept`s |
| `subset_sum` | `subset_sum(band, max_group, seed)` | **atomic many-to-one clearing**: a whole-lot subset summing within `band` of an anchor (meet-in-the-middle); seeded |
| `flow` | `flow(FlowSpec<E>)` | the global **min-cost-flow arbiter** over the ambiguous remainder; emits the matching as **raw arcs** (one 2-member net-0 group per positive-flow arc), *not* settlements |

These share one shape — *bucket, then accept-if-balanced* — differing only in how
buckets form (key / multi-key / order / proximity-graph) and the acceptance rule
(pairwise / whole-net / running / optimized). `agg_net`, `signal_group`, and
`cumulative` carry their *intrinsic proposal rule* (a real net needs both books
represented; a segment needs ≥ 2 lots; a token *names* the group) and delegate
the **net judgement** to the `accept` closure over the bucket's [`GroupView`].
That shared shape is worth documenting but **not** worth collapsing into a
god-leaf: the disjoint-vs-overlapping and pairwise-vs-whole distinctions are the
point.

`subset_sum` is the **atomic** counterpart to divisible `flow`: it selects whole
lots (no fractional splitting), filling the canonical "one payment clears several
invoices" shape that a flow LP cannot express (it would split a credit to top up
the target). The small break stays **inside** the group as its `net`, like
`agg_net`. Crucially, **`band` is a search parameter, not an acceptance
tolerance** — it is the half-width of the value window the meet-in-the-middle
search explores around each anchor, and the candidate-pruning bound. A black-box
acceptance closure gives the search nothing to prune against, which is exactly
why this one stays a concrete integer. Keep `band` *generous* for recall and gate
what actually commits with a *strict* `accept_if` downstream — the propose/verify
split (loose band = recall, tight predicate = precision). Its search is
exponential, so it relies on blocking (`partition_by`/`windowed`) to keep pools
small; it is **seeded** (anchor-order and subset ties break on a pure hash of ids
+ `seed`), making it a reproducible *high-recall proposer* in the propose/verify
idiom — pair it with `restart` (§4.1) and a strict `accept_if` verifier.

### 4.3 Group combinators (reshape the output)

| Combinator | Signature | Role |
|---|---|---|
| `labeled` | `labeled(tag, inner)` | stamp a human `reason` on every group a subtree forms |
| `accept_if` | `accept_if(pred: Fn(&GroupView<E>)->bool, inner)` | gate groups; **dissolve rejects back to residual** (conserving) |
| `coalesce` | `coalesce(origin, inner)` | fuse interlocking groups (shared member) into settlement clusters — the **settlement authority**; `residual_out == residual_in` |
| `reclaim` | `reclaim(origin, inner)` | make a discovered grouping **whole-line**: coalesce shared-id groups, reclaim each line's ground tail, net measured on whole lines |

`accept_if` is the **only acceptance concept** in the library. The predicate sees
a [`GroupView`] — the member legs, each row's `original`, and its payload — so
net / materiality / structural / payload tests are all just closures, and they
compose:

```rust
// <= 12 lots, both sides really present, net within 5 bps of the smallest leg.
accept_if(
    |g| g.size() <= 12 && g.min_side() > 0 && g.net().abs() <= 5 * g.min_leg() / 10_000,
    flow(spec),
)
```

Because the gate reads `original` (via `g.original_total()`), the old `material`
combinator is gone — *immaterial-match* drop is just an `accept_if` over moved
volume vs birth size:

```rust
// keep a match only if its moved volume exceeds 2% of the rows' original size.
accept_if(|g| g.gross() * 50 > g.original_total(), inner)
```

`flow` is a **strict primitive**: it emits the matching as raw arcs and nothing
else. `coalesce` is the single **settlement authority** that folds an allocation
hypergraph (a row split across arcs/groups) into the coarser, human-actionable
cluster view; it is a pure group→group transform whose invariant is
`residual_out == residual_in`. This keeps connected-components logic in exactly
one place instead of duplicated inside `flow`. The blessed settlement view over
`flow` is one token of composition, no sugar needed:

```rust
coalesce("flow", flow(spec))   // discover arcs, fold them into clusters
```

`reclaim` is the library's two-paradigm hinge, the one residual→group move
`coalesce` is forbidden from making:

- **Transportation (`flow`):** a line is *divisible*. `flow` splits amounts at
  the unit level, every matched cluster nets to **0**, and the difference is a
  separate **residual** lot. `accept_if(|g| g.net() == 0, flow(..))` keeps only
  clusters that cleared *completely* — a near-miss like `+100 / -97` goes
  **entirely** to residual.
- **Netting (whole-line):** a line is *atomic*. `agg_net`/`signal_group` (§4.2)
  bucket whole lines by a **key** and accept iff the bucket's net passes the gate,
  the break staying **inside** the group as its `net`. `reclaim` brings that to a
  *discovered* grouping: it coalesces shared-id groups into one settlement
  cluster, then reclaims each member line's ground tail so every leg carries its
  full `original`, with the cluster `net` (the remaining break) measured on those
  whole lines. It is purely structural and commits *everything* — the gate is a
  *separate* `accept_if`, which sees the whole legs and dissolves an
  over-tolerance cluster back to residual *whole*.

So the classic N:M tolerance match is `accept_if` over `reclaim`:

```rust
// +100 / -97 becomes one matched group with net +3, if the break clears 5.
accept_if(|g| g.net().abs() <= 5, reclaim("settlement", inner))
```

Because a line is atomic in the netting view, groups that share a member id are
**one settlement**: `reclaim` coalesces them first, so a line's tail can only ever
go to **ground** (never to a sibling group) and the reclaim is unambiguous;
conservation holds (each id ends up wholly in one cluster — then wholly committed
or wholly dissolved by the gate — or wholly in residual). `net == 0` is **not**
sufficient for wholeness (a group can net to zero while a member bleeds into
residual), which is exactly why `reclaim` is a distinct structural primitive and
not an `accept_if(|g| g.net()==0)` gate: only `reclaim` reaches into the residual
to make lines whole. The relative-tolerance scale is no longer an enum knob —
gate against `g.min_leg()` (smallest leg, conservative), `g.max_leg()` (largest
leg, lenient), or `g.original_total()` (birth size); it is just which accessor
the closure calls.

`flow` + a net-0 gate (the break as a separate labelled residual lot, soaked) and
`reclaim` + a tolerance gate (break kept *inside* the matched group) are the two
valid bookkeeping choices for the same mismatch — *separate break lot* vs
*matched-with-break*.

### 4.4 Soaker (terminate the tail)

| Soaker | Signature | Role |
|---|---|---|
| `soak` | `soak(origin)` | consume **every** non-zero lot it receives into **one** group |

`soak` is deliberately the *only* soaker: it just collapses whatever it is handed
into one group whose non-zero `net` is expected and meaningful (variance,
write-off, "unmatched"). The two orthogonal knobs live where they belong —
*cardinality* is a `partition_by` concern and *which lots to soak* is a `when`
concern:

```rust
partition_by(|i: &Item<E>| i.id, |_| soak("unmatched"))           // one class per lot
partition_by(|i: &Item<E>| i.data.class, |_| soak("variance"))    // one class per key
when(                                                              // only the immaterial tail
    |i: &Item<E>| i.amount != 0 && i.amount.abs() <= i.original / 50,
    partition_by(|i: &Item<E>| i.id, |_| soak("rounding")),
)
```

This replaces the old `soak_all`/`soak_small`/`soak_if` trio and the
`SoakMode { Singleton, Bucket }` enum: materiality is a `when` predicate over the
`Item` (it sees `original`), singleton-vs-bucket is the `partition_by` key, and
"soak everything" is bare `soak`. Place it last in a `seq`.

### 4.5 `Group` vs `GroupView` (the closure ergonomics fix)

`Group` is the committed, payload-free **output** record. Its metrics describe a
*formed* group:

```rust
impl Group {
    fn member_ids(&self) -> Vec<ExtId>;
    fn size(&self) -> usize;       // member count
    fn abs_net(&self) -> i64;      // |net|
    fn max_abs(&self) -> i64;      // largest |member amount|
    fn min_abs(&self) -> i64;      // smallest non-zero |member amount|
    fn min_side(&self) -> usize;   // min(#pos, #neg) by amount sign
}
```

[`GroupView`] is the borrowed, gate-time **input** to an acceptance closure. It
lends back the two things `Allocation` sheds — each member's birth-size
`original` and a borrow of its payload `&E` — so a gate can judge *materiality*
and inspect the payload, neither of which a `Fn(&Group)` could reach:

```rust
struct MemberView<'a, E> { id: ExtId, amount: i64, original: i64, data: &'a E }

impl<'a, E> GroupView<'a, E> {
    fn net(&self) -> i64;            // signed Σ leg — the residual it would commit
    fn gross(&self) -> i64;          // Σ|leg| — matched/moved volume
    fn max_leg(&self) -> i64;        // largest leg magnitude
    fn min_leg(&self) -> i64;        // smallest non-zero leg magnitude
    fn original_total(&self) -> i64; // Σ|original| over distinct ids — materiality denominator
    fn size(&self) -> usize;
    fn min_side(&self) -> usize;
    fn members(&self) -> impl Iterator<Item = &MemberView<'a, E>>;
}
```

The Python `safe_flow` gate

```python
P.and_(P.le(P.SIZE, 100), P.and_(P.le(P.MIN_SIDE, 2), net_within_tol()))
```

becomes one inline closure — no tolerance type, the relative scale is just which
accessor you call:

```rust
accept_if(
    |g| g.size() <= 100 && g.min_side() <= 2 && g.net().abs() <= 5 * g.min_leg() / 10_000,
    inner,
)
```

The author owns the arithmetic, including `i128` widening when a leg is large
(`(5_i128 * g.min_leg() as i128 / 10_000) as i64`). That ownership is the price
of having *one* concept instead of an enum of pre-baked scales.

---

## 5. `flow`: `Model` trait → `FlowSpec` builder

`Model` was the one leaf that broke the closure idiom — a trait + associated
type where everything else takes closures, and its name pretended to be a domain
concept when it is just "the five hooks `flow` needs." It is replaced by a
closure builder consistent with the rest of the algebra:

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

(The `flow` builder closures take the payload `&E` plus the lot amount, not
`&Item<E>` — `flow` threads the conserved `amount` separately, so the cost/keys
hooks see `(&E, i64)`.)

Notes:

- **Lot form is canonical, row form is sugar.** The builder stores the lot-aware
  closure and `.cost(...)` simply wraps an amount-ignoring one. Same defaults, no
  trait machinery.
- **`Arc<dyn Fn>` for `Clone`.** `flow` clones its `FlowSpec` into a fresh cold
  build each `run` via `spec.clone()`; `Arc` makes that a pointer bump and is
  strictly better than a `M: Clone` deep-clone for any spec holding real data.
- **Cost:** one indirect call per candidate arc instead of a monomorphized
  inline. Real but in line with the rest of the algebra's dispatch, and `cost`
  is O(candidate arcs). Acceptable; the consistency win dominates.
- The leaf internals (`Entry`, `by_key`, the network build, readback) are
  unchanged — only the `model.foo(tx)` calls became `spec.foo(tx)` closure calls.

A tiered-cost helper (the Python `cost_spec(tier(...))` shape) is genuinely
useful but **domain sugar**, so it ships as an optional SDK helper, not core.

---

## 6. Removals, renames, and unifications (the landed diff)

| Action | Item | Rationale |
|---|---|---|
| **remove** | `Tol` enum (`Abs/Rel/RelMax`, `slack`, `slack_for`) | one acceptance concept: a `Fn(&GroupView)` closure (§2, §4.5). The relative scale is just which accessor you call |
| **remove** | `Model` trait | → `FlowSpec` closure builder (§5) |
| **remove** | `material(tol, inner)` | now `accept_if(\|g\| g.gross() … g.original_total(), inner)` — the gate sees `original` via `GroupView` |
| **remove** | `whole_net(tol, inner)` | split into structural `reclaim` + a separate `accept_if` gate (§4.3) |
| **remove** | `settle(spec)` | just write `coalesce("flow", flow(spec))` |
| **remove** | `partition_by_with` | merged into `partition_by`, which now **always** takes a key-aware factory `Fn(&K)->Strategy` (key-ignoring = `\|_\| inner()`) |
| **remove** | `soak_all`, `soak_small`, `soak_if`, `SoakMode` | collapsed into a single `soak(origin)`; cardinality = `partition_by`, filtering = `when` (§4.4) |
| **remove** | `Group::clean(tol)` | gates read `GroupView` accessors directly |
| **remove** | `exact_1to1_any`, `filter`, `whole_only`, `branch`, `trim`, `snap` | sugar / aliases / speculative reshapers that earned nothing |
| **rename** | `running_zero(order, tol)` → `cumulative(order, accept)` | per-segment `Fn(&GroupView)->bool`: close when the segment-so-far accepts |
| **rename** | `subset_sum(tol, …)` → `subset_sum(band, …)` | `band` is a **search** parameter (the MITM value window + prune bound), not an acceptance tolerance |
| **change** | every row closure `Fn(&E)` → `Fn(&Item<E>)` | warm-start is gone, so selectors may see the whole lot (id/original/amount/payload) |
| **change** | `agg_net`/`signal_group` tol arg → `accept: Fn(&GroupView)->bool` | the intrinsic proposal rule stays built in; the net judgement is the closure |
| **change** | `accept_if(Fn(&Group)) ` → `accept_if(Fn(&GroupView))` | the gate now sees `original` + payload |
| **add** | `GroupView`/`MemberView` | the borrowed gate-time lens (§4.5) |
| **add** | `reclaim(origin, inner)` | make a discovered grouping whole-line; the residual→group move `coalesce` is forbidden from making |
| **add** | `when(pred, inner)`, `identity()` | the one-sided guard and its unit |
| **add** | `restart(n, seed, factory)` | seeded random-restart over a stochastic proposer, keep the best |
| **keep** | `seq, partition_by, windowed, pivot, fixed_point` | the structural spine, already orthogonal |
| **keep** | `agg_net, signal_group, cumulative, subset_sum, exact_1to1, flow` | distinct matchers, shared shape documented |
| **keep** | `labeled, accept_if, coalesce` | the post-matching group algebra |
| **reshape** | `flow(spec)` | a strict primitive returning **raw arcs**; grouping moves to `coalesce`/`reclaim` |

---

## 7. The intercompany plugin on the new surface

The whole Python waterfall — minus the dead expression DSL — as native helpers
composed from the orthogonal core. This is the sufficiency proof (the shipping
`plugins/interco` is the live, smaller cut of the same shape).

```rust
// ---- plugin-local recipes (NOT core) -------------------------------------
const FLOOR: i64 = 1_000;   // $10 absolute floor, native minor units

// "net within $10 OR 1 bp of the smallest leg" — one inline acceptance closure.
fn clean(g: &GroupView<Row>) -> bool {
    g.net().abs() <= (g.min_leg() / 10_000).max(FLOOR)
}

fn safe_agg(tag: &str, key: impl Fn(&Item<Row>)->u64 + 'static) -> Box<dyn Strategy<Row>> {
    labeled(tag, agg_net(key, clean))     // agg_net gates its own net via `clean`
}

fn safe_flow(tag: &str, win: i64, max_size: usize, max_side: usize,
             cost: impl Fn(&Row,&Row)->Option<f64> + 'static) -> Box<dyn Strategy<Row>> {
    let spec = FlowSpec::new()
        .window(win).penalty(1000.0)
        .block_key(|r: &Row| r.day)
        .match_keys(|r| r.tokens.clone())
        .cost(cost);
    accept_if(
        move |g| g.size() <= max_size && g.min_side() <= max_side && clean(g),
        coalesce(tag, labeled(tag, flow(spec.clone()))),
    )
}

fn transactional(prefix: &str) -> Box<dyn Strategy<Row>> {
    seq(vec![
        labeled(&fmt(prefix,"SIGNAL"),
            signal_group(|i: &Item<Row>| i.data.tokens.clone(), clean, 256)),
        windowed(|i: &Item<Row>| i.data.day, 10,
            safe_agg(&fmt(prefix,"OBJSUB"), |i| i.data.objsub_match)),
        windowed(|i: &Item<Row>| i.data.day, 10,
            safe_agg(&fmt(prefix,"UNIT"), |i| i.data.unit)),
        labeled(&fmt(prefix,"EXACT"), exact_1to1(|_: &Item<Row>| Some(0))),
        safe_flow(&fmt(prefix,"FLOW"), 15, 100, 2, tiered_cost(0.0)),
        windowed(|i: &Item<Row>| i.data.day, 10,
            safe_flow(&fmt(prefix,"SHORTFLOW"), 10, 100, 5, tiered_cost(0.05))),
    ])
}

// numeraire iteration = `pivot`, exactly as before
fn all_cur() -> Box<dyn Strategy<Row>> {
    seq(vec![
        when(|i: &Item<Row>| i.data.trx_amt != 0,
            partition_by(|i: &Item<Row>| i.data.trx_ccy,
                |_| pivot(|i: &Item<Row>| i.data.trx_amt, transactional("T_TRX")))),
        when(|i: &Item<Row>| i.data.trx_usd != 0,
            pivot(|i: &Item<Row>| i.data.trx_usd, transactional("T_USD"))),
        transactional("T_BSUSD"),
    ])
}

fn strategy() -> Box<dyn Strategy<Row>> {
    let structural = seq(vec![
        safe_agg("S1_GLOBAL_OBJSUB", |i| i.data.objsub_match),
        partition_by(|i: &Item<Row>| i.data.unit, |_| seq(vec![
            when(|i: &Item<Row>| i.data.prior_close, seq(vec![
                safe_agg("S0A_PRIOR_UNIT", |i| i.data.unit),
                partition_by(|i: &Item<Row>| i.data.source_class,
                    |_| safe_agg("S0B_PRIOR_SRC", |i| i.data.unit)),
                partition_by(|i: &Item<Row>| i.data.objsub_match,
                    |_| safe_agg("S0C_PRIOR_OBJ", |i| i.data.unit)),
            ])),
            safe_agg("S3_UNIT", |i| i.data.unit),
            partition_by(|i: &Item<Row>| i.data.source_class,
                |_| safe_agg("S5_UNIT_SRC", |i| i.data.unit)),
            partition_by(|i: &Item<Row>| i.data.objsub_match,
                |_| safe_agg("S7_UNIT_OBJ", |i| i.data.unit)),
        ])),
    ]);
    let core = seq(vec![
        structural,
        // soak the immaterial tail into per-source-class variance buckets.
        when(|i: &Item<Row>| i.amount != 0 && i.amount.abs() <= FLOOR,
            partition_by(|i: &Item<Row>| i.data.source_class, |_| soak("S8_SMALL"))),
        partition_by(|i: &Item<Row>| i.data.unit, |_| all_cur()),
    ]);
    fixed_point(core, 4)
}
```

Everything domain-specific is a closure or a plugin-local `fn`; the core
contributes only orthogonal nodes. No expression IR, no `Model` impl, no
tolerance type, no group-metric atoms.

---

## 8. Surface summary (the whole public API)

```
foundation   Item  Group  GroupView/MemberView  Resolution  Strategy  Allocation  ExtId
             Group::{member_ids,size,abs_net,max_abs,min_abs,min_side}
             GroupView::{net,gross,min_leg,max_leg,original_total,size,min_side,members}

bag combs    seq  partition_by  when  windowed  pivot  fixed_point  restart  identity
leaves       exact_1to1  agg_net  signal_group  cumulative  subset_sum  flow(FlowSpec)
group combs  labeled  accept_if  coalesce  reclaim
soaker       soak
flow         FlowSpec builder  (+ optional flow_util::tiered cost helper)
```

**19 constructor functions** (8 bag combinators + 4 group combinators + 6 leaves
+ 1 soaker) **+ one builder** (`FlowSpec`). The load-bearing simplification is
that acceptance is **one concept**: a `Fn(&GroupView)` closure, with no tolerance
type, no `Scale` enum, and no helper builders — the author writes the inequality
and picks the scale by accessor (`min_leg`/`max_leg`/`original_total`). The
propose/verify pair sits on top: `subset_sum` is a seeded high-recall *proposer*
(the atomic whole-lot clearing `flow` can't express), `restart` runs a seeded
family and keeps the best, and a strict `accept_if` verifier gates what commits —
randomness is always a pure hash of ids + seed, never an RNG. `reclaim` and
`accept_if` together replace `whole_net` (keep a small *break* inside a whole-line
cluster); a net-0 `accept_if` over `flow` plus `soak` is the complementary
*separate break lot* bookkeeping; and an `accept_if` over `gross()` vs
`original_total()` is the materiality drop the old `material` baked in. `flow` is
a strict primitive returning raw arcs; `coalesce` is the sole settlement
authority. Every node obeys one closure idiom and belongs to exactly one family.
```
