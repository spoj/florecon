# florecon

florecon reconciles financial ledgers as a **conserving strategy algebra** over a
network-simplex core. You compose a matching strategy from small combinators, run
it over a bag of rows, and get back groups plus the signed amount each row
contributes to each group. Nothing is created and nothing is lost: what goes in
equals the grouped allocations plus the leftovers, *by construction* — so a bad
strategy yields a bad proposal, never a broken ledger.

It is a Rust crate. A domain is packaged as one self-describing WebAssembly
plugin via the authoring SDK; hosts stay dumb columnar-table carriers.

Money is `i64` **minor units** (cents), so netting is exact.

## The algebra

A `Strategy` maps a bag of `Item`s to `(groups, residual)`. **Leaves** form
groups; **combinators** arrange leaves. Every node conserves signed amount.

Leaves:

- `exact_1to1` — pair a row with an equal-and-opposite row sharing a key.
- `agg_net` — accept a bucket (by a key) that nets to zero within a `Tol`.
- `signal_group` — group rows sharing a free-text token that net to zero.
- `flow` — a min-cost-flow arbiter (the [`engine`]): pairs opposite-sign rows by
  proximity in an ordering and a cost model, splitting a row across
  counterparties when needed. The domain is described by a `FlowSpec` of
  closures (penalty, window, block key, match keys, pair cost).
- `soak_all` / `soak_small` / `soak_if` — sweep leftovers into variance or
  write-off buckets.

Combinators:

- `seq` — run steps in order; each sees only the previous step's leftovers.
- `partition_by` / `partition_by_with` — shard by a key and run a sub-strategy
  per shard (e.g. per company pair, per currency).
- `when` / `identity` — route rows through a guarded sub-strategy, else pass on.
- `windowed` — restrict matching to a sliding window over an ordering.
- `pivot` — run a sub-strategy in a different amount lane, translating back.
- `fixed_point` — repeat a sub-strategy on its own leftovers until it converges.

Predicates, keys, costs and orders are **plain Rust closures**, not a serialized
expression language.

```rust
use florecon::strategy::*;

// Per company-pair, per currency: net clean buckets, pair exacts, bridge on
// references, then let the flow engine arbitrate the remainder.
let strategy = partition_by(|t: &Tx| t.pair, move || {
    partition_by(|t: &Tx| t.ccy, move || {
        seq(vec![
            agg_net(|t: &Tx| t.account, Tol::Abs(100)),
            exact_1to1(|_: &Tx| Some(0)),
            signal_group(|t: &Tx| t.tokens.clone(), Tol::Abs(0), 256),
            flow(FlowSpec::new()
                .window(30)
                .penalty(1000.0)
                .block_key(|t: &Tx| t.day)
                .cost(|a: &Tx, b: &Tx| Some(1.0 + (a.day - b.day).abs() as f64))),
        ])
    })
});
```

## The workspace

[`Recon`] is the stateful facade: stream rows in, solve, sign off what you trust.
A group lives on two orthogonal axes — a **lifecycle** (`Proposed`, owned by the
solver and recomputed each solve, vs `Pinned`, a human decision kept verbatim)
and a provenance label. The operations decompose into four orthogonal families:

```text
ledger      upsert(id, item) · remove(&ids)
machine     solve()
lifecycle   pin(id) · pin_where(pred) · unpin(id)
partition   merge(allocs, label, reason) · detach(id, ids) · dissolve(id)
```

`solve` recomputes the proposed pool from `items − pinned mass`; pinned groups
keep stable ids. `merge` asserts a pinned group over exact allocation amounts
(atomic). `pin_where(pred)` is the one bulk-pin primitive — "pin every clean
match", "pin these singletons", and more are just predicates. Conservation is
verified at the solve boundary.

## The result

`report()` returns an allocation **hypergraph**:

- `groups` — `group_id`, `status` (`live`/`frozen`), `net`, `size`, `origin`,
  `reason`.
- `allocations` — one row's signed contribution to one group: `id`, `group_id`,
  `amount`.

A row may contribute to several groups, so the allocations are the source of
truth and a single row-to-group assignment is a projection you choose:
`Report::strict_assignments` (refuses split rows) or `connected_components`
(settlement clusters).

## Authoring a plugin

Implement [`sdk::Plugin`] — declare the raw input columns, the stable row id, the
projection to a typed row, the conserved numeraire, and the strategy — then
`export_plugin!` emits a self-describing wasm module. The declared schema is
**enforced** at ingest: a missing or mistyped column fails loudly, and a
`RowView` access to an undeclared column panics rather than silently reading
zero. The conformance kit (`sdk::conformance::assert_conformance`) mechanically
proves identity/derivation/warm-start integrity from one Arrow batch.

Authoring is meant to be LLM-assisted Rust: the closures *are* the strategy
language (full expressivity, and the compiler is your correctness oracle), so
there is no separate DSL. You never clone this repo — the author CLI ships with
the host, so from your own data project:

```bash
uv add florecon-host        # (or: pip install florecon-host)
florecon new my-recon       # scaffold a plugin (crates.io dep, domain = my-recon)
cd my-recon
```

The journey then has two phases:

1. **Author (Rust only, fast).** Iterate the strategy natively against a CSV
   sample — no wasm, no Python. The expensive solver lives in the `florecon`
   dependency (built once at `-O3`, then cached), so your strategy recompiles in
   under a second and runs at near-release speed:

   ```bash
   florecon author          # build + run the strategy once on data/sample.csv
   florecon check           # type-check only (fastest feedback)
   ```

   Edit the four marked spots in `solver/src/lib.rs`, re-run, read the report and
   the conservation line. Repeat.

2. **Ship + consume (wasm + Python).** When the strategy fits, build the
   production wasm and run it where the data already lives:

   ```bash
   florecon ship            # the perf-tuned solver.wasm
   cd app && uv run python run.py
   ```

   If you want a distributable artifact instead of loading the wasm from
   `target/`, `florecon package` builds the same release wasm, embeds it in a
   wheel, and writes `dist/*.whl`.

`florecon author` / `ship` / `package` / `check` are thin, cross-platform
`cargo` wrappers (they need a Rust toolchain). The worked starter — the exact
thing `florecon new` scaffolds — is
[`examples/starter-plugin`](examples/starter-plugin) (its README walks the full
path); `plugins/interco` is the larger real example (intercompany
reconciliation).

## Developing florecon itself

```bash
cargo test --workspace --features sdk      # lib + plugin + doctests
cargo clippy --workspace --all-targets --features sdk -- -D warnings
just build-wasm                            # interco plugin -> wasm, staged into hosts/python/
just smoke-py                              # drive that wasm through the generic Python host
just golden-check                          # the cross-language wire contract (Rust + Python)
just starter                               # build + run the author starter (kept honest by CI)
python scripts/sync-template.py            # refresh the host-bundled copy of that starter
```

The author CLI (`florecon._cli`) lives in `hosts/python`; the starter it ships
is bundled from `examples/starter-plugin` by `scripts/sync-template.py` (CI runs
it with `--check`). The `engine::Snapshot` (behind the `serde` feature) persists
a warm basis.

## Design docs

- `docs/arch/strategy-surface.md` — the algebra.
- `docs/arch/recon-surface.md` — the workspace surface.
- `docs/arch/sdk-surface.md` — the plugin SDK and wire.
- `docs/arch/plugin-sdk.md` — the original architecture shift (historical).

[`Recon`]: https://docs.rs/florecon
[`engine`]: https://docs.rs/florecon
[`sdk::Plugin`]: https://docs.rs/florecon
