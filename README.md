# florecon

**Reconciliation as partitioning.** Given a bag of entries (each a stable id
plus an opaque payload), parse it into a list of groups. That is the whole
problem.

```rust
use florecon::strategy::*;

#[derive(Clone)]
struct Tx { amount: i64, day: i64, account: u64 }

let strategy = seq(vec![
    // clean opposite-and-equal pairs sharing an account
    exact_1to1(|e: &Entry<Tx>| Some(e.account), |e| e.amount),
    // buckets that net to within 5 (the tolerance is just an inline closure)
    agg_net(|e: &Entry<Tx>| e.account, |g| g.net(|e| e.amount).abs() <= 5),
    // the min-cost-flow arbiter on what's left, emitting whole-row clusters
    flow(FlowSpec::new()
        .amount(|t: &Tx| t.amount)
        .penalty(1000.0)
        .window(7)
        .block_key(|t: &Tx| t.day)
        .cost(|a: &Tx, b: &Tx| Some(1.0 + (a.day - b.day).abs() as f64))),
]);
```

## The model

- **`Entry<E> { id, data }`** — a stable `Id` and an opaque payload. Derefs to
  the payload, so `|e| e.amount` reaches a field while `e.id` names identity.
- **`Group { members: Vec<Id>, origin, reason }`** — a set of *whole* entries.
- **`Strategy<E>: bag -> (groups, residual)`** — a pure function. The output is a
  partition: every input id lands in exactly one group or in residual.

There is **no privileged numeraire**. Every number a leaf needs — an amount to
net, a value to search, a flow supply — is a closure over the payload. So:

- multi-currency is just different closures (`agg_net(key, |g| g.net(|e| e.usd) == 0)`);
- there is no `pivot`, no `original`, no fractional allocation;
- a match is judged by an **acceptance closure** over a `GroupView`
  (`|g| g.net(|e| e.amount).abs() <= 5 * g.min_leg(|e| e.amount) / 10_000`) —
  no tolerance type, the author writes the inequality and picks the lane.

Conservation is **identity**, not arithmetic: nothing lost, nothing invented.

## The surface

```
combinators  seq  partition_by  when  windowed  fixed_point  accept_if  labeled  identity  soak
leaves       exact_1to1  agg_net  signal_group  cumulative  subset_sum  flow(FlowSpec)
```

`flow` keeps using min-cost flow internally (the [`netsimplex`](netsimplex/)
crate — a standalone network-simplex transportation solver) to *discover* the
optimal matching, but reads it back as **whole-row connected-component
clusters** — a row is never split. The break, if any, is just the cluster's net,
which a downstream `accept_if` can gate.

## Workspace

- **`florecon`** — the strategy algebra (this crate).
- **`netsimplex`** — the domain-agnostic min-cost transportation solver behind
  `flow`.
