# florecon

Incremental financial reconciliation via min-cost transportation.

Reconciliation as a **conserving combinator algebra** over a min-cost-flow core.
Cheap deterministic rules (exact match, aggregate netting, reference-token
bridges) cascade ahead of a network-simplex arbiter that resolves the ambiguous
remainder. Every stage preserves mass: an input row lands in exactly one group
or in the residual, always.

## Architecture

Four layers, each a thin lowering of the one above:

| Layer | Module | Role |
|---|---|---|
| Engine | `net` | Network-simplex transportation solver. Stable node/arc handles, warm-started re-solves with incremental potentials, snapshot/restore. Domain-agnostic, no FX, no Arrow. |
| Facade | `recon` | Describe a domain once via the `Model` trait, then `upsert` / `remove` / `solve`. Assigns slot ids, generates candidate arcs, reads back netted groups. |
| Algebra | `strategy` | `Strategy: Bag -> (Groups, residual)`, conserving by construction. Primitives `exact_1to1`, `agg_net`, `signal_group`, `running_zero`, `flow`; combinators `seq`, `partition_by`, `windowed`. |
| API | `api` | Serializable `Plan` (the strategy tree as data), stateful `Session` (owns rows natively), relational `Report`. Conservation enforced at the boundary. |

The `wasm` feature compiles the API into a single C-ABI module (`alloc` /
`dealloc` / `solve` over linear memory, no wasm-bindgen) that any runtime can
drive — the same artifact targets the browser, Databricks, or a PyO3 wheel.

## Design notes

- **Conservation is the correctness property.** It is structural, not a runtime
  check you can forget: the combinators conserve by construction, and the API
  boundary verifies `assigned ⊎ residual == input` before returning. A bad plan
  becomes a bad *proposal* (bounced to residual), never a broken ledger.
- **Numeraire per shard.** `partition_by(currency)` makes the native amount the
  conserved quantity within each shard, so FX never enters the flow.
- **Money is `i64` minor units.** Integral flows, exact net-zero.
- **Predicates and derived columns belong upstream.** The DSL expresses the
  *structure* of reconciliation (group / net / shard / cascade / window); which
  rows and what keys are a data-prep concern, computed before the table is
  handed in.

## Running

The combinator pipeline on a parquet file:

```bash
cargo run --release --example interco [path.parquet]
# 279k rows: 87.7% by count, 85.2% by value, conservation OK, ~0.4s
```

The same pipeline through WASM, driven from Python via wasmtime:

```bash
cargo build --release --target wasm32-unknown-unknown --features wasm --lib
python -m venv .venv && . .venv/bin/activate && pip install wasmtime pyarrow
python python/run_interco.py [path.parquet] [--pair COMPANY ICP] [--max N]
# full file via wasm: 87.7% count, 85.2% value — exact match to native
```

## Workbench (browser + WASM)

An interactive reconciliation UI: cross-filtering slicers, a groups table, a
line-level detail pane, and the interactive verbs (freeze / break up / recalc)
driven by the stateful `Workspace`. All computation runs in the WASM module in
the browser — the data never leaves the client.

```bash
cargo build --release --target wasm32-unknown-unknown --features wasm --lib
. .venv/bin/activate && python python/export_web.py [--pair COMPANY ICP]  # -> web/data.json
python -m http.server 8000        # serve from the repo root
# open http://localhost:8000/web/index.html
node web/smoke.mjs                # headless check of the browser host ABI
```

The browser host (`web/florecon.js`) is the exact analog of the Python host:
allocate, write a JSON command, call `dispatch`, read the JSON report. The
workbench owns only display records and filter state; the engine owns the rows
and the conserved partition.

## Features

- `serde` — serialize snapshots and `Plan`s.
- `wasm` — the C-ABI consumption surface (implies `serde`).

## Tests

```bash
cargo test --features serde     # 37 lib tests
cargo clippy --all-targets --features wasm
```
