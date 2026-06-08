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
package/       Phase 3: the plugin as an installable wheel (wasm bundled)
.github/       CI: build the wasm + wheel on every push, attach to v* releases
```

## Prerequisites

- A **Rust toolchain** (the plugin is Rust): https://rustup.rs. The wasm target
  installs itself (pinned in `rust-toolchain.toml`) -- no `rustup target add`.
- The `florecon` CLI -- it ships with the host: `uv add florecon-host` (or
  `pip install florecon-host`). Run it as `florecon ...` (or `uv run florecon
  ...` from inside a uv project).
- For Phase 2/3 only: a Python env with `florecon-host` (+ `polars` to run the
  sample), and `uv` to build the wheel.

## Phase 1 — author the strategy (Rust only, fast)

Fast native loop on the sample — no wasm, no Python. The expensive solver lives
in the `florecon` dependency (built at `-O3` once, then cached); your strategy
recompiles in seconds.

```bash
florecon author                 # build + run once on data/sample.csv
florecon author data/mine.csv   # …or your own sample
florecon check                  # type-check only (fastest feedback)
```

Edit the **four numbered spots** in `solver/src/lib.rs`, re-run `florecon
author`, read the report and the conservation line. Repeat. The compiler (and
the conservation check) is your correctness oracle.

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
florecon ship               # -> target/wasm32-unknown-unknown/release/solver.wasm
cd app && uv sync && uv run python run.py
```

`run.py` loads the wasm straight from the target dir and feeds it a polars
DataFrame. Tune at runtime without rebuilding: `Workspace(str(WASM),
config={"tol": 100})`.

> Native (harness) and wasm (host) run the same strategy, but native ≠ wasm
> performance. Do a final perf check on the real wasm before relying on it at
> scale.

## Phase 3 — package & distribute (a wheel your data team installs)

When you want others to *use* the plugin without touching Rust or wasm, build a
wheel that carries the compiled strategy inside it:

```bash
florecon package            # builds the wasm, embeds it, -> dist/*.whl
```

Because the plugin is wasm, the result is **one universal wheel**
(`…-py3-none-any.whl`): build it once on any machine and it runs everywhere
`florecon-host` runs — no per-OS/arch build matrix. The data team then:

```python
pip install ./dist/starter-0.1.0-py3-none-any.whl   # (rename to your project)
import starter, polars as pl
ws = starter.workspace(config={"tol": 100})          # the bundled wasm
ws.upsert(df); report = ws.solve()
```

### CI does this for you

`.github/workflows/build.yml` runs `florecon package` on **every push** (the
wheel is downloadable from the Actions run) and **attaches the wheel + the raw
`solver.wasm` to the GitHub Release** when you push a `v*` tag. It runs the same
`florecon package` you run locally, then smoke-tests the freshly built wasm — so
the artifact that ships is known-good. No PyPI, no tokens, no secrets: it just
builds the file.

```bash
git tag v0.1.0 && git push --tags     # -> wheel + wasm on the Release page
```

## Don't copy this by hand

This folder is what `florecon new <name>` scaffolds for you — with the `florecon`
dependency already pointing at crates.io and the domain named after your
project. Start a fresh plugin with:

```bash
florecon new my-recon
cd my-recon
florecon author
```

(In the florecon repo this seed uses a `path` dependency so CI tests it against
the working tree; the scaffolded copy pins the published crate.)
