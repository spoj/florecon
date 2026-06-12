# florecon

**Two dual conservation laws over accounting data.** The same ledger is read two
ways, and florecon is the algebra for each:

- **reconcile** ([`strategy`](src/strategy.rs)) — parse a bag of entries into a
  *partition* of groups. Conserves **identity**: every input lands in exactly
  one group or in residual, nothing lost, nothing invented.
- **allocate** ([`alloc`](src/alloc.rs)) — re-express a coarse cost on a finer
  basis. Conserves **value**: a cost split across a driver is exact to the
  penny, residuals parked in-cube rather than vanishing.

They are duals — partition by identity vs. couple by value — sharing one stance:
**no privileged numeraire**. Every number is a closure over the caller's own
payload, so multi-currency is just different closures.

## Reconcile

Given a bag of entries (each a stable id plus an opaque payload), parse it into a
list of groups.

```rust
use florecon::strategy::*;

#[derive(Clone)]
struct Tx { amount: i64, day: i64, account: u64 }

let strategy = seq(vec![
    // clean opposite-and-equal pairs sharing an account
    exact_1to1(|e: &Entry<Tx>| Some(e.account), |e| e.amount),
    // buckets that net to within 5 (the tolerance is just an inline closure)
    agg_net(|e: &Entry<Tx>| Some(e.account), |g| g.net(|e| e.amount).abs() <= 5),
    // the min-cost-flow arbiter on what's left, emitting whole-row clusters
    flow(FlowSpec::new()
        .amount(|t: &Tx| t.amount)
        .penalty(1000.0)
        .window(7)
        .block_key(|t: &Tx| t.day)
        .cost(|a: &Tx, b: &Tx| Some(1.0 + (a.day - b.day).abs() as f64))),
]);
```

### The model

- **`Entry<E> { id, data }`** — a stable `Id` and an opaque payload. Derefs to
  the payload, so `|e| e.amount` reaches a field while `e.id` names identity.
- **`Group { members: Vec<Id>, origin, reason }`** — a set of *whole* entries.
- **`Strategy<E>: bag -> (groups, residual)`** — a pure function. The output is a
  partition: every input id lands in exactly one group or in residual.

A match is judged by an **acceptance closure** over a `GroupView`
(`|g| g.net(|e| e.amount).abs() <= 5 * g.min_leg(|e| e.amount) / 10_000`) — no
tolerance type, the author writes the inequality and picks the lane. Conservation
is **identity**, not arithmetic.

```
combinators  seq  partition_by  when  windowed  fixed_point  restart  accept_if  explain  identity  soak
leaves       exact_1to1  agg_net  signal_group  cumulative  subset_sum  flow(FlowSpec)
```

`flow` uses min-cost flow internally (the [`netsimplex`](netsimplex/) crate) to
*discover* the optimal matching, but reads it back as **whole-row
connected-component clusters** — a row is never split. The break, if any, is the
cluster's net, which a downstream `accept_if` can gate.

## Allocate

A `Measure` is a sparse quantity over named axes. Re-express a coarse cost on a
finer basis, exact to the penny.

```rust
use florecon::alloc::*;

let rent = Measure::build(&["geog", "time"], &[
    (&[("geog", 1), ("time", 1)], 1000),
    (&[("geog", 2), ("time", 1)], 500),   // no driver here
]);
let rev = Measure::build(&["geog", "product", "time"], &[
    (&[("geog", 1), ("product", 10), ("time", 1)], 30),
    (&[("geog", 1), ("product", 11), ("time", 1)], 70),
]);

let a = rent.allocate(&rev);
assert_eq!(a.total(), rent.total());           // conserves value
assert_eq!(a.pending().total(), 500);          // geog 2 parked on ANY, in-cube
```

### The model

- **reshape** — `rekey` rewrites each cell's key and re-sums (subsumes
  marginalize / roll-up / relabel).
- **couple** — `combine` aligns shared axes and broadcasts the rest, the scalar
  op a closure (`|a, b| a + b`).
- **down-project** — `allocate` splits a coarse cost across a driver, exact to
  the penny.
- **select / route** — `select` / `partition` / `slice` query a cube; `group_by`
  routes a per-group rule and recombines.

The one idea is **`ANY`**: a reserved coord meaning "unresolved on this axis".
Every residual — a missing driver, an undriven dimension, sub-threshold dust — is
mass parked on `ANY`, *in-cube*. Two inverse moves shuttle mass along the grain:
`rake` refines `ANY` → real detail, `vacuum` surrenders detail → `ANY`. There is
no type-level conserved/unconserved distinction — conservation is a property you
assert (`a.total()`), not one the compiler enforces. The cyclic (reciprocal)
case reaches for `fixed_point`, as recon does.

## Workspace

- **`florecon`** — the reconcile + allocate algebras (this crate).
- **`netsimplex`** — the domain-agnostic min-cost transportation solver behind
  `flow`.
</content>
</invoke>
