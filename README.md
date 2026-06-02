# florecon

**Reconciliation by flow. Nothing created, nothing lost.**

`florecon` (flow-recon) reconciles financial ledgers as a **conserving
combinator algebra over a min-cost-flow core**. Cheap deterministic rules
(exact match, aggregate netting, reference-token bridges) cascade ahead of a
network-simplex arbiter that resolves the ambiguous remainder. Every stage
preserves mass: an input row lands in exactly **one group**, always — an
unmatched row is simply a group of one.

The mental model is a **conserved pile you adjudicate, not a search you run**.
The system is always one partition of every row into groups — there is no
"outside" and no separate residual bucket. The machine *proposes* (a confidence
cascade from exact matches down to flow), and you *adjudicate*: **freeze** what
you trust, **break up** what you don't, **recalc** to retry. Frozen groups are a
signed-off reconciliation; the live singletons are your exception queue.

## One core, every skin

The engine is one WASM module driven over linear memory; every distributable is
a thin skin over the same bytes and the same JSON wire contract. The bindings
cannot drift, because all they do is marshal `Plan` / `Cmd` / `Report` JSON to
one binary.

| Distributable | Path | What it is |
|---|---|---|
| Rust crate | `/` (`florecon`) | The engine + algebra + plan API, source of truth. |
| Python wheel | `py/` (`florecon`) | `py3-none-any` wheel bundling the wasm + a wasmtime host + a Plan builder. One artifact, every OS — no native extension to compile. |
| npm package | `web/core/` (`@florecon/core`) | The browser/Node host + TypeScript types + bundled wasm. |
| Workbench | `web/` | A no-framework analytical UI; all compute runs in the wasm in the browser, data never leaves the client. |
| **Contract** | `schema/plan.schema.json` | The versioned JSON Schema for `Plan` / `Cmd` / `Report`. The actual product. |

## Architecture

Four layers, each a thin lowering of the one above:

| Layer | Module | Role |
|---|---|---|
| Engine | `engine` | Network-simplex transportation solver. Stable node/arc handles, warm-started re-solves with incremental potentials, snapshot/restore. Domain-agnostic, no FX, no Arrow. |
| Flow | `flow` | The incremental min-cost-flow matcher: describe a domain once via the `Model` trait, then `upsert` / `remove` / `solve`. Generates candidate arcs (sorted, deterministic), reads back netted groups. The engine behind the `flow` leaf. |
| Algebra | `strategy` | `Strategy: Bag -> (Groups, residual)`, conserving by construction. Primitives `exact_1to1`, `agg_net`, `signal_group`, `running_zero`, `flow`; combinators `seq`, `partition_by`, `windowed`. |
| Plan | `plan` | Serializable `Plan` (the strategy tree as data, pricing included via `CostSpec`), one generic stateful facade `Recon<E>` (with `Workspace` its `Row` specialization), relational `Report`. Conservation enforced at the boundary. |

The `wasm` feature compiles the `plan` API into a single C-ABI module (`alloc` /
`dealloc` / `solve` / `dispatch` over linear memory, no wasm-bindgen) that any
runtime can drive — the same artifact targets the browser, Databricks, or a
wasmtime wheel.

## The vocabulary is the brand

The same words name the same things in the crate, the wheel, the UI, and the
code: `Plan`, `Workspace`, `Group`, and the verbs `freeze` / `breakup` /
`recalc`. A `Plan` is the strategy as data; a `Workspace` is the conserved pile
you adjudicate; a `Group` is a reconciliation with a `net`. "Residual" survives
only as a human word for the exception queue — the *live singleton groups* — not
as a separate data structure.

## Design notes

These are the load-bearing decisions and the non-obvious ones — the things that
stay true as the code moves. (Implementation walk-throughs are deliberately
*not* kept as prose; they rot. Read the code.)

- **Conservation is the correctness property.** It is structural, not a runtime
  check you can forget: the combinators conserve by construction, and the
  boundary verifies the partition before returning. A bad plan becomes a bad
  *proposal* (a row left as a singleton), never a broken ledger.
- **Everything is a group; `live | frozen` is the only recalc axis.** A
  workspace is one partition of every id into groups — there is no separate
  residual set. `Status` is the sole recalc axis and **only an operator flips
  it**: `live` is the machine's current opinion (re-pooled on every recalc),
  `frozen` is your decision (inviolable). Matched vs unmatched is *arity*, not
  status — a live singleton is an unmatched row, a frozen singleton an accepted
  exception. Live-singleton ids are **ephemeral** (re-minted each solve); only
  frozen group ids are stable, so never reference a live id across a solve.
- **Review/attention is a second axis, never a third status.** Staging and tags
  are a host-owned, many-to-many overlay keyed by the **stable row id**,
  orthogonal to the engine partition and never crossing into the conservation
  engine (so they survive recalc for free). A tagged set is promoted to a match
  or an exception through the existing verbs; the engine never learns the word
  "staging".
- **Warm-start must equal cold.** The flow simplex dominates solve time, so its
  matcher is kept alive per shard and re-solved incrementally across recalc.
  Warm and cold solves must agree: guaranteed by deterministic arc ordering and
  a deterministic *scrambled* node order (a monotone id order is a pathological
  network-simplex pivot sequence), and cross-checked on the objective in debug.
- **Strategy nodes may hold state.** `Strategy::run` takes `&mut self`;
  statefulness is an opt-in *capability*, not a mandate (only the flow leaf uses
  it). Any node that holds state owes the warm-equals-cold guarantee above.
- **Numeraire per shard.** `partition_by(currency)` makes the native amount the
  conserved quantity within each shard, so FX never enters the flow.
- **Money is `i64` minor units.** Integral flows, exact net-zero.
- **Cost is data.** The flow arbiter prices candidate pairs with a serializable
  `CostSpec` (ordered confidence tiers; first satisfied tier wins, no tier means
  forbidden). The default reproduces the reference-bridge > exact-amount
  cascade; the whole strategy, pricing included, is one JSON `Plan`.
- **Predicates and derived columns belong upstream.** The DSL expresses the
  *structure* of reconciliation (group / net / shard / cascade / window); which
  rows and what keys are a data-prep concern, computed before the table is
  handed in.
- **Versioned wire contract.** `Plan` / `Cmd` / `Report` are a published schema
  (`schema/plan.schema.json`). The engine exports `abi_version`; every host
  refuses to run against a mismatched binary, so the bindings cannot silently
  drift.

## Running

### Rust — the combinator pipeline on a parquet file

```bash
cargo run --release --example interco [path.parquet]
# 279k rows: 87.7% by count, 85.2% by value, conservation OK, ~0.4s
```

### Build the WASM core

```bash
./scripts/build_wasm.sh
# builds target/.../florecon.wasm and stages it into the wheel and npm package
```

### Python — the wheel

```bash
python -m build --wheel py/        # -> py/dist/florecon-0.1.0-py3-none-any.whl
pip install py/dist/florecon-0.1.0-py3-none-any.whl
```

```python
from florecon import Workspace, plan as P, schema, col, key, KEY, NUMBER, TOKENS

sch = schema([
    col("unit", KEY), col("ccy", KEY), col("day", NUMBER),
    col("objsub", KEY), col("native", NUMBER), col("tokens", TOKENS),
])
pln = P.partition("unit", P.partition("ccy", P.seq(
    P.agg_net("objsub", "native", tol=100),
    P.exact("native"),
    P.signal("tokens", "native", tol=100, cap=256),
    P.flow("native", day="day", native="native", tokens="tokens"),
)))

ws = Workspace(sch, pln)
# bare cells: a string for key/tokens columns, an int for number columns; the
# engine lowers strings to ids itself (you ship business values, not hashes).
ws.upsert(1, [key("00492", "00288"), "USD", 1, "61500", 100, "INV1"])
ws.upsert(2, [key("00492", "00288"), "USD", 2, "61500", -100, "INV1"])
ws.solve()                      # one clean group; ws.freeze(0) signs it off
```

The full interco pipeline through the wasm (exact match to native):

```bash
python python/run_interco.py [path.parquet] [--pair COMPANY ICP] [--max N]
```

### Workbench — browser + WASM

An interactive reconciliation UI: cross-filtering slicers, a groups table, a
line-level detail pane, and the verbs (freeze / break up / recalc) driven by the
stateful `Workspace`. Slicers and detail columns are rendered from a portable
`fields` descriptor in `data.json`, so a differently-shaped book ports by
shipping a different descriptor — no UI code changes. The groups table is itself
a slicer (multi-select to union groups into the detail pane); with nothing
selected the detail pane is a faithful, unfiltered timeline of every line in
view, so nothing is ever "matched away" and hidden while groupings are still
volatile. All computation runs in the wasm in the browser — the data never
leaves the client.

```bash
. .venv/bin/activate && python python/export_web.py [--pair COMPANY ICP]  # -> web/data.json
python python/enrich_web.py        # add display fields to the committed sample (no source parquet)
python -m http.server 8000        # serve from the repo root
# open http://localhost:8000/web/index.html
node web/smoke.mjs                # headless check of the browser host ABI
```

The browser host (`@florecon/core`) is the exact analog of the Python host:
allocate, write a JSON command, call `dispatch`, read the JSON report. The
workbench owns only display records and filter state; the engine owns the rows
and the conserved partition.

## The contract

```bash
pip install jsonschema
python schema/validate.py web/data.json   # validate a SolveRequest against the contract
```

`schema/plan.schema.json` is the single source of truth for what crosses the
boundary. `additionalProperties: false` throughout, so typos and stray fields
are rejected, not silently ignored.

## Features

- `serde` — serialize snapshots and `Plan`s.
- `wasm` — the C-ABI consumption surface (implies `serde`).

## Tests

```bash
cargo test --features serde       # lib tests + doctest
cargo clippy --all-targets --features wasm
node web/smoke.mjs                # browser-host ABI on real data
```
