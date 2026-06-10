# florecon (Python host)

A generic [wasmtime](https://github.com/bytecodealliance/wasmtime-py) host that
drives self-describing **florecon** reconciliation plugins. The host knows
nothing about any domain: it loads a plugin `.wasm`, reads its `describe()`, and
ships the raw columns the plugin declares. The same code runs every florecon
plugin.

Money is integer minor units; nothing is created or lost.

## Authoring a plugin

This package also ships the `florecon` CLI, so you can build a plugin without
cloning the florecon repo:

```bash
florecon new my-recon       # scaffold a Rust plugin (needs a Rust toolchain)
cd my-recon
florecon author             # fast native loop on data/sample.csv
florecon check              # type-check only
florecon ship               # build the production solver.wasm
florecon package            # optional: bundle the wasm into dist/*.whl
```

Then either load `target/wasm32-unknown-unknown/release/solver.wasm` with the
host below or hand off the wheel from `dist/`. See the scaffolded `README.md`
for the full journey.

## Running a plugin

```python
from florecon import Workspace

ws = Workspace("interco_plugin.wasm")     # any florecon plugin wasm

ws.upsert(
    {"row_id": 1, "company": "A", "icp": "B", "objsub": "61500",
     "indicative_usd_amt": 100.0, "trx_currency": "USD", "trx_amt": 100.0,
     "gl_date": 0, "reference": "INV0001"},
    {"row_id": 2, "company": "B", "icp": "A", "objsub": "61500",
     "indicative_usd_amt": -100.0, "trx_currency": "USD", "trx_amt": 100.0,
     "gl_date": 1, "reference": "INV0001"},
)
rep = ws.solve()              # the proposal: groups + per-row allocations
ws.pin_clean(tol=0)           # sign off every clean net-zero match
ws.solve()                    # warm re-solve; pinned groups kept verbatim
```

## Surface

A group lives on a lifecycle axis — `proposed` (the solver's current opinion,
recomputed each `solve`) or `pinned` (your decision, kept verbatim).

```text
ledger      upsert(*rows) · remove(*ids)
machine     solve()
lifecycle   pin(gid) · pin_clean(tol) · pin_singletons(ids) · unpin(gid)
partition   merge(allocs, label, reason) · detach(gid, ids) · dissolve(gid)
read        report()
```

Failures raise `PluginError` carrying a stable `code` (e.g. `"frozen_group"`,
`"conservation_violated"`) plus the `id` / `group_id` it concerns.
`strict_assignments` / `connected_components` project the allocation hypergraph
into per-row assignments or settlement clusters.

The plugin/host ABI is versioned: the host refuses a wasm whose `abi_version`
differs from `florecon.ABI_VERSION`.
