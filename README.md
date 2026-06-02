# florecon

**Reconciliation by flow. Nothing created, nothing lost.**

`florecon` (flow-recon) reconciles financial ledgers as a **conserving
combinator algebra over a min-cost-flow core**. Cheap deterministic rules
(exact match, aggregate netting, reference-token bridges) cascade ahead of a
network-simplex arbiter that resolves the ambiguous remainder. Every stage
preserves mass: an input row lands in exactly one group or in the residual,
always.

The mental model is a **conserved pile you adjudicate, not a search you run**.
The system is always fully partitioned into explained groups plus a residual —
there is no "outside". The machine *proposes* (a confidence cascade from exact
matches down to flow), and you *adjudicate*: **freeze** what you trust,
**break up** what you don't, **recalc** to retry. Frozen groups are a signed-off
reconciliation; the residual is your exception queue.

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
docs: `Plan`, `Workspace`, `Group`, `residual`, and the verbs `freeze` /
`breakup` / `recalc`. A `Plan` is the strategy as data; a `Workspace` is the
conserved pile you adjudicate; a `Group` is a proposed reconciliation with a
`net`; the `residual` is what remains unexplained.

## Design notes

- **Conservation is the correctness property.** It is structural, not a runtime
  check you can forget: the combinators conserve by construction, and the
  boundary verifies `assigned ⊎ residual == input` before returning. A bad plan
  becomes a bad *proposal* (bounced to residual), never a broken ledger.
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
  (`schema/plan.schema.json`, contract v1). The engine exports `abi_version`;
  every host refuses to run against a mismatched binary.

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
from florecon import Workspace, plan as P, Int, Tokens

schema = ["unit", "ccy", "day", "objsub", "native", "tokens"]
pln = P.partition("unit", P.partition("ccy", P.seq(
    P.agg_net("objsub", "native", tol=100),
    P.exact("native"),
    P.signal("tokens", "native", tol=100, cap=256),
    P.flow("native", day="day", native="native", tokens="tokens"),
)))

ws = Workspace(schema, pln)
ws.upsert(1, [Int(1), Int(1), Int(1), Int(0), Int(100), Tokens([])])
ws.upsert(2, [Int(1), Int(1), Int(2), Int(0), Int(-100), Tokens([])])
ws.solve()                      # one clean group; ws.freeze(0) signs it off
```

The full interco pipeline through the wasm (exact match to native):

```bash
python python/run_interco.py [path.parquet] [--pair COMPANY ICP] [--max N]
```

### Workbench — browser + WASM

An interactive reconciliation UI: cross-filtering slicers, a groups table, a
line-level detail pane, and the verbs (freeze / break up / recalc) driven by the
stateful `Workspace`. All computation runs in the wasm in the browser — the data
never leaves the client.

```bash
. .venv/bin/activate && python python/export_web.py [--pair COMPANY ICP]  # -> web/data.json
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
python schema/validate.py web/data.json   # validate a SolveRequest against v1
```

`schema/plan.schema.json` is the single source of truth for what crosses the
boundary. `additionalProperties: false` throughout, so typos and stray fields
are rejected, not silently ignored.

## Features

- `serde` — serialize snapshots and `Plan`s.
- `wasm` — the C-ABI consumption surface (implies `serde`).

## Tests

```bash
cargo test --features serde       # 43 lib tests + doctest
cargo clippy --all-targets --features wasm
node web/smoke.mjs                # browser-host ABI on real data
```
