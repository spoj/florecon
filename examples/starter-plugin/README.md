# starter-plugin

A florecon plugin, start to finish. The matching strategy is Rust compiled to
WebAssembly; a generic host ships raw rows to it and reports the proposed groups.
The plugin never modifies accounting values — **conservation of the numeraire is
guaranteed by construction**, so a bad strategy yields a bad *proposal*, never a
broken ledger.

```
solver/        the plugin -> solver.wasm   (schema + projection + matching)
harness/       native author loop: CSV -> Recon -> report + conservation
data/          a representative sample to iterate against
app/           Phase 2: run the built wasm on real data, in Python
```

## Prerequisites

- A **Rust toolchain** (the plugin is Rust). For the ship build also:
  `rustup target add wasm32-unknown-unknown`.
- [`just`](https://github.com/casey/just) for the task runner. On Windows,
  **Git Bash** (recipes route to it).
- For Phase 2 only: [`uv`](https://docs.astral.sh/uv/) (Python).

## Phase 1 — author the strategy (Rust only, fast)

Fast native loop on the sample — no wasm, no Python. The expensive solver lives
in the `florecon` dependency (built at `-O3` once, then cached); your strategy
recompiles in seconds.

```bash
just author                 # build + run once on data/sample.csv
just author data/mine.csv   # …or your own sample
just check                  # type-check only (fastest feedback)
```

Edit the **four numbered spots** in `solver/src/lib.rs`, re-run `just author`,
read the report and the conservation line. Repeat. The compiler (and the
conservation check) is your correctness oracle.

1. **`Line`** — the raw columns the host ships (`#[derive(Record)]`: one struct
   is the input schema, the typed projection, and the identity).
2. **`Config`** — runtime tunables, delivered at `init` as JSON.
3. **`project`** — derive your typed match `Row` from a `Line`.
4. **`strategy`** — the matching cascade: `agg_net` (net by key), `exact_1to1`
   (clean pairs), `signal_group` (token buckets), `flow` (N:M), composed with
   `partition_by` / `when` / `seq` / `fixed_point`.

> Iterate on a representative *sample*, not your full dataset — it tells you
> whether the strategy fits without waiting on millions of rows.

## Phase 2 — ship and run on real data (Python)

When the strategy fits, build the production wasm and run it on real data where
it already lives:

```bash
just ship                   # -> solver/target/wasm32-unknown-unknown/release/solver.wasm
cd app && uv sync && uv run python run.py
```

`run.py` loads the wasm straight from the target dir and feeds it a polars
DataFrame. Tune at runtime without rebuilding: `Workspace(str(WASM),
config={"tol": 100})`.

> Native (harness) and wasm (host) run the same strategy, but native ≠ wasm
> performance. Do a final perf check on the real wasm before relying on it at
> scale.

## Using this as your own plugin

Copy this folder out of the florecon repo and change one line in `Cargo.toml`:

```toml
[workspace.dependencies]
florecon = { version = "0.1", features = ["sdk"] }   # was: path = "../.."
```

Then rename the `solver` crate and the `example.starter` domain to taste.
