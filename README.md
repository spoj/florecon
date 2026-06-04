# florecon

florecon reconciles financial ledgers. You describe a matching strategy as a
small plan, run it over your rows, and get back groups plus the signed amount
each row contributes to each group. It ships as one WebAssembly core with thin
Rust, Python, and JavaScript hosts.

Conventions used throughout: money is integer **minor units** (cents). A
`number` column is an integer, a `key` column is a categorical string (hashed
host-side), a `tokens` column is free text (tokenized by the engine). A row id
is a caller-owned integer.

## Recon problems, in Python

Each block below is a complete, runnable program against the Python host.

### Pair equal-and-opposite entries

`exact` pairs each row with an equal-and-opposite one.

```python
from florecon import Workspace, schema, col, NUMBER, plan as P

sch = schema([col("amount", NUMBER)])
data = [(1, [500]), (2, [-500]), (3, [250]), (4, [-250])]

plan = P.exact()

ws = Workspace(sch, plan, primary="amount")
ws.upsert_many(data)
rep = ws.solve()
# two groups, each net 0: rows {1,2} and {3,4}
```

### Net a bucket within a tolerance

`agg_net` accepts a bucket (keyed by a column) whose members net to zero within
a tolerance; `partition` shards the book first, here by company pair.

```python
from florecon import Workspace, schema, col, key, KEY, NUMBER, plan as P

sch = schema([col("pair", KEY), col("account", KEY), col("amount", NUMBER)])
data = [
    (1, [key("HK01", "CN02"), "61500",  10_000]),
    (2, [key("HK01", "CN02"), "61500",  -9_950]),   # nets to 0.50 -> accepted within $1
    (3, [key("HK01", "CN02"), "72000",   5_000]),
    (4, [key("HK01", "CN02"), "72000",  -5_000]),   # nets to 0.00
]

plan = P.partition("pair", P.agg_net("account", tol=100))   # tol = 100 cents = $1.00

ws = Workspace(sch, plan, primary="amount")
ws.upsert_many(data)
rep = ws.solve()
```

### Bridge on a shared reference

`signal` groups rows that share a free-text token and net to zero — e.g. an
invoice number that appears in both a payment memo and an invoice description.

```python
from florecon import Workspace, schema, col, NUMBER, TOKENS, plan as P

sch = schema([col("amount", NUMBER), col("memo", TOKENS)])
data = [
    (1, [ 100, "payment ref INV0042"]),
    (2, [-100, "INV0042 widgets"]),
]

plan = P.signal("memo", tol=0)

ws = Workspace(sch, plan, primary="amount")
ws.upsert_many(data)
rep = ws.solve()   # one net-zero group, bridged by the shared INV0042 token
```

### Let the solver choose among candidates

`flow` is a min-cost-flow matcher: it pairs opposite-sign rows by proximity in
an ordering (here `day`) and a cost model, picking the most plausible
counterparty when several compete.

```python
from florecon import Workspace, schema, col, NUMBER, TOKENS, plan as P
from florecon import strict_assignments

sch = schema([col("amount", NUMBER), col("day", NUMBER), col("ref", TOKENS)])
data = [
    (1, [ 100,  1, ""]),    # an open item
    (2, [-100,  2, ""]),    # a settlement one day later  (closer)
    (3, [-100, 20, ""]),    # another candidate, far out
]

plan = P.flow(order_by="day", tokens="ref", window=30)

ws = Workspace(sch, plan, primary="amount")
ws.upsert_many(data)
rep = ws.solve()
strict_assignments(rep)     # rows 1 & 2 grouped; row 3 left as a residual singleton
```

### Cascade several rules

`seq` runs steps in order; each step only sees what the previous one left over.
Put cheap deterministic rules first and the flow arbiter last, then sweep
immaterial leftovers.

```python
from florecon import Workspace, schema, col, key, KEY, NUMBER, TOKENS, plan as P

sch = schema([col("account", KEY), col("amount", NUMBER),
              col("day", NUMBER), col("memo", TOKENS)])
data = [
    (1, ["61500",  100, 1, "INV1"]),
    (2, ["61500", -100, 2, "INV1"]),
]

plan = P.seq(
    P.agg_net("account", tol=0),                       # net clean buckets
    P.exact(),                                         # pair leftovers
    P.signal("memo", tol=0),                           # bridge on references
    P.flow(order_by="day", tokens="memo", window=30),  # arbitrate the remainder
    P.soak_small("rounding", max_abs=50),              # <= $0.50 -> variance bucket
    P.soak_all("unmatched"),                           # classify whatever is left
)

ws = Workspace(sch, plan, primary="amount")
ws.upsert_many(data)
rep = ws.solve()
```

### Shape the strategy: `partition` and `branch`

The combinators compose into the whole strategy. These snippets highlight just
the plan; bind one to a `Workspace` the same way as above.

`partition` shards the book and runs the inner plan independently per shard.
Nest them to shard on several dimensions at once:

```python
plan = P.partition("pair", P.partition("ccy", P.seq(
    P.agg_net("account", tol=100),
    P.exact(),
    P.flow(order_by="day", tokens="memo", window=30),
)))
```

`branch` routes rows to different sub-plans by a `Sel` predicate — e.g. arbitrate
big-ticket items carefully, but just net and write off the immaterial ones:

```python
plan = P.branch(
    P.ge(P.abs_(P.col("amount")), 100_000),            # >= $1,000 (cents)
    P.flow(order_by="day", tokens="memo", window=7),   # large: arbitrate carefully
    P.seq(                                             # small: net then write off
        P.agg_net("account", tol=0),
        P.soak_all("immaterial"),
    ),
)
```

They nest freely — here a materiality `branch` runs inside every counterparty
shard, and a `fixed_point` repeats a netting pass until nothing more groups:

```python
plan = P.partition("pair", P.seq(
    P.fixed_point(P.agg_net("account", tol=100)),      # net buckets to convergence
    P.branch(
        P.ge(P.abs_(P.col("amount")), 100_000),
        P.flow(order_by="day", tokens="memo", window=7),
        P.soak_all("immaterial"),
    ),
))
```

### Adjudicate interactively

A `Workspace` is stateful: stream rows in, solve, then sign off what you trust.
Frozen groups survive later solves untouched; rows can be added or removed and
re-solved incrementally.

```python
ws.solve()
ws.freeze_clean(tol=0)            # sign off every clean net-zero match
ws.upsert_many([(5, [...]), (6, [...])])   # tomorrow's rows arrive
ws.solve()                       # warm re-solve; frozen groups are kept as-is
ws.breakup(group_id)             # changed your mind about a live group
ws.solve()
```

## Reading the result

`solve()` returns a report with two lists:

- `groups` — each has `group_id`, `status` (`live` or `frozen`), `net`, `size`,
  `origin` (which rule formed it), and `reason`.
- `allocations` — each is one row's signed contribution to one group:
  `id` (row), `group_id`, `amount`.

A row can contribute to more than one group, so the allocations are the source
of truth and a single row-to-group assignment is a projection you pick:

```python
from florecon import strict_assignments, connected_components

strict_assignments(rep)    # [(row_id, group_id), ...] — errors if a row is split
connected_components(rep)  # settlement clusters: [{"rows": [...], "groups": [...]}, ...]
```

## Getting started

### Python

The wheel is `py3-none-any` — it bundles the wasm and a wasmtime host, so there
is no native extension to compile.

```bash
python -m build --wheel py/                 # -> py/dist/florecon-0.1.0-py3-none-any.whl
pip install py/dist/florecon-*.whl
```

Or run straight from the source tree:

```bash
pip install pyarrow wasmtime
PYTHONPATH=py/src python your_script.py
```

### Rust

The crate is the source of truth (engine + plan API). Build a `Plan` and drive a
`Workspace` (`upsert` / `solve` / `freeze` / `breakup`).

```bash
cargo test
cargo run --release --example interco [path.parquet]   # the cascade on a real ledger
```

### JavaScript / Node

The browser/Node host is a thin wrapper over the wasm: you marshal an Arrow IPC
batch plus JSON commands and call `dispatch`.

```bash
./scripts/build_wasm.sh        # builds the wasm and stages web/core/engine.wasm
node web/smoke.mjs             # headless host check
```

```js
import { Florecon } from "@florecon/core";

const fe = await Florecon.load("./engine.wasm");
fe.dispatch({ op: "init", plan }, arrowBytes);   // rows ride in the Arrow batch
fe.dispatch({ op: "solve" });
```

### Web demo

`web/` is a demo that runs in the browser with an interactive UI.

```bash
python -m http.server 8000     # then open http://localhost:8000/web/index.html
```

## How it works

A plan is a tree of **strategies**. A strategy takes a bag of rows and returns
the groups it formed plus the rows it left over. Every strategy preserves signed
amount: what goes in equals the grouped allocations plus the leftovers. A plan
therefore cannot create or lose money — only decide how it is grouped — so a bad
plan yields a bad *proposal* (mass sitting in unmatched/variance groups), never a
broken ledger.

Leaves form groups:

- `exact` — pair a row with an equal-and-opposite row.
- `agg_net` — accept a bucket (by a key column) that nets to zero within a tolerance.
- `signal` — group rows sharing a free-text token that net to zero.
- `flow` — a min-cost-flow matcher that pairs opposite-sign rows by proximity in
  an ordering and a cost model, choosing among competing candidates; it can split
  a row's amount across counterparties.
- `soak_small` / `soak_all` — sweep leftover rows into variance or write-off buckets.

Combinators arrange leaves:

- `seq` — run steps in order; each sees only the previous step's leftovers (a cascade).
- `partition` — shard by a column and run the inner plan independently per shard
  (e.g. per company pair, per currency).
- `windowed` — restrict matching to a sliding window over an ordering.
- `branch` — route rows to different sub-plans by a predicate.
- `filter` — keep only groups meeting a condition (size, net, ...), dissolving the
  rest back to leftovers.
- `pivot` — run a sub-plan in a different amount column, translating results back.
- `fixed_point` — repeat a sub-plan on its own leftovers until nothing more groups.

The `by` / `key` / `order` / `pred` fields of these nodes accept a column name or
a small integer expression over the row's columns (`col`, `lit`, comparisons,
`and_`/`or_`, `iff`, ...).

A `Workspace` is the same plan made interactive. You `upsert` and `remove` rows,
call `solve` to (re)compute groups, and `freeze` groups you trust or `breakup`
ones you don't. Frozen groups are kept as fixed constraints and survive later
solves; live groups are recomputed every solve, so their ids are re-minted each
time — reference rows by their stable id, not a live group id. Re-solving is
incremental and warm-started. You can also swap the plan itself with `replan` to
retune the rules without reloading rows — frozen decisions are kept.

Money is `i64` minor units, so netting is exact.

## The wire contract

`Plan`, `Cmd`, and `Report` are a versioned JSON contract
(`schema/plan.schema.json`, `additionalProperties: false` throughout). The engine
exports `abi_version`; every host refuses to run against a mismatched binary, so
the bindings cannot silently drift.

```bash
pip install jsonschema
python schema/validate.py        # validate the canonical commands against the schema
```

## Tests

```bash
cargo test                       # Rust lib + doctests
node web/smoke.mjs               # browser-host ABI
PYTHONPATH=py/src python py/smoke_stateful.py   # stateful Python host
```
