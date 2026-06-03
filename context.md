# Implementation context: amountless primitives + primary amount + pivot

Scope requested: `src/strategy.rs`, `src/plan.rs`, `src/plan_compile.rs`, `src/flow.rs`, `schema/plan.schema.json`, `examples/interco.rs`, `web/ingest.js`, `python/export_web.py`. I also checked adjacent host/version/projection call sites that will break with the wire-shape change.

## Current design (what must be refactored)

### Strategy layer: row/lot mode is encoded on `Item`

- `src/strategy.rs:48-56` currently has:
  - `id`, `original`, `amount`, `data`, plus `lot: bool`.
  - `lot=false` means legacy row mode: leaves ignore `item.amount` and recompute via their amount closures.
  - `lot=true` means allocation mode: leaves use the current residual `item.amount`.
- `Item::row` (`src/strategy.rs:60-68`) creates zero `original`/`amount` with `lot=false`.
- `Item::lot` (`src/strategy.rs:70-78`) initializes amount/original and sets `lot=true`.
- `effective_amount`/`stamp_amount` (`src/strategy.rs:80-96`) are the compatibility shim; most primitives call `stamp_amount` before matching.
- `lots(amount, inner)` (`src/strategy.rs:923-960`) is the explicit adapter from row mode to lot mode. This is the main node to remove/replace.

Desired direction from request: `Item` should always be amountful: `id/original/amount/data` only. Delete `lot`, `row`, `lot` mode semantics, `effective_amount`, and `stamp_amount`; create a single constructor such as `Item::new(id, amount, data)` or direct struct literals.

### Existing primitives all carry amount closures (except soakers)

Current amount-bearing primitives in `src/strategy.rs`:

- `exact_1to1(key, amount)`:
  - struct stores `key` and `amount` (`src/strategy.rs:371-374`).
  - `run` stamps amount for every item (`src/strategy.rs:382-389`).
  - pairs by `item.amount.abs()` and emits allocation amounts from `item.amount` (`src/strategy.rs:397-431`).
- `agg_net(key, amount, tol)`:
  - stamps amount (`src/strategy.rs:469-476`).
  - sums `item.amount` and emits those allocation amounts (`src/strategy.rs:480-497`).
- `running_zero(order, amount, tol)`:
  - stamps amount in timeline walk (`src/strategy.rs:524-583`). Not exposed in `Plan` today but should be made amountless too if primitives are consistently amountless.
- `signal_group(signals, amount, tol, cap)`:
  - stamps amount, then uses `amt` vector from `item.amount` (`src/strategy.rs:590-680`).
- `flow(model)`:
  - already wraps tx in `FlowTx { tx, amount }` and uses `FlowLotModel` to conserve a supplied current amount (`src/strategy.rs:695-729`).
  - but it still has row-mode fallback: `flow_amount` uses `item.amount` if `item.lot`, else `model.base_amount(&item.data)` (`src/strategy.rs:750-755`).
  - cold verification repeats the same fallback (`src/strategy.rs:854-860`).
- `soak_small` / `soak_all` are already amountless and operate on `Item.amount/original` (`src/strategy.rs:982-1040`, `1078-1115`). These are useful patterns for the target primitive shape.

Likely new strategy API:

- `agg_net(key, tol)` uses existing `item.amount`.
- `exact_1to1()` or `exact_1to1(key)` uses existing `item.amount.abs()` as the amount/equality key. The current Rust API has a custom key closure; Plan only needs exact-by-current-amount.
- `signal_group(signals, tol, cap)` uses existing `item.amount`.
- `running_zero(order, tol)` uses existing `item.amount`.
- `flow(model)` always uses `item.amount` for `FlowTx.amount`; no fallback to `Model::base_amount` inside strategy.

### Flow layer supports amount-wrapped txs, but direct `Matcher` still has `base_amount`

- `src/flow.rs:23-67` defines `Model` with `base_amount`, `cost`, `cost_lot`, `match_keys`, `match_keys_lot`.
- `Matcher::upsert` always reads `model.base_amount(&tx)` and `model.match_keys(&tx)` (`src/flow.rs:170-173`); this can stay for direct `Matcher` users.
- Strategy `flow(model)` currently uses `FlowLotModel` so the underlying `Matcher` sees `FlowTx.amount` as `base_amount` (`src/strategy.rs:706-729`). This is already the right adapter for amountless strategy leaves.
- Readback is allocation-native and partial-flow aware:
  - matched allocation groups at `src/flow.rs:309-351`.
  - unmatched residual amounts at `src/flow.rs:354-367`.

Pitfall: `Flow::flow_sig` currently builds `keys` with `self.model.match_keys(&item.data)` (`src/strategy.rs:768-776`) rather than `match_keys_lot(&item.data, current_amount)`. Because `amount` is also in the signature, amount changes still force an upsert, but after the refactor the signature should reflect the actual keys passed through `FlowLotModel` (`match_keys_lot`) to avoid stale-warm edge cases and to make pivoted/current amounts explicit.

## Plan and boundary call sites

### Current `Plan` wire shape embeds amount in leaf nodes

`src/plan.rs:60-135`:

- `Plan::Lots { amount, inner }` at `src/plan.rs:78-82`.
- Amount-bearing leaves:
  - `AggNet { key, amount, tol }` (`src/plan.rs:83-88`).
  - `Exact { amount }` (`src/plan.rs:89-90`).
  - `Signal { signals, amount, tol, cap }` (`src/plan.rs:91-97`).
  - `Flow { amount, day, tokens, penalty, window, cost }` (`src/plan.rs:120-135`).
- Contract version is currently v7 (`src/plan.rs:27-33`). This refactor is breaking and must bump it.

Current batch boundary:

- `Session::run_strategy` compiles plan, then materializes rows with `Item::row(*id, row.clone())` (`src/plan.rs:273-286`). This is the main place where primary amount should be evaluated and placed into every `Item`.
- `SolveRequest` only has `{ schema, rows, plan }` (`src/plan.rs:360-368`).

Current interactive boundary:

- `Workspace::new(schema, plan)` compiles plan and creates `Recon::new(strategy)` (`src/plan.rs:1000-1012` in full file; wrapper methods at `1040+`).
- `Recon<E>` stores only `strategy`, `items`, `groups`, `next_id` (`src/plan.rs:430-439`). It currently cannot initialize amountful `Item`s without either an amount closure or precomputed item amounts.
- `Recon::solve` builds the bag with either frozen-lot residuals or `Item::row` (`src/plan.rs:542-579`). This must become “subtract frozen primary allocations from primary original; create an amountful residual item if non-zero”.

Likely new wire/setup shape:

- Add a primary amount scalar at the setup/boundary level, e.g. `SolveRequest { schema, rows, amount: ScalarRef, plan }` and WASM `Cmd::Init { schema, amount, plan, rows }`.
- Native API probably needs corresponding breaking changes:
  - `Session::solve(amount, plan)` or `Session` constructed with a primary amount expression.
  - `Workspace::new(schema, amount, plan)`.
  - Generic `Recon<E>` likely needs `Recon::new(amount_fn, strategy)` so it can create amountful live singletons and solve bags.
- Remove `Plan::Lots`; add `Plan::Pivot { amount: ScalarRef, inner: Box<Plan> }` (wire op likely `pivot`).
- Remove `amount` fields from `Plan::AggNet`, `Exact`, `Signal`, `Flow`.

### Plan compiler call sites

`src/plan_compile.rs` currently imports and compiles `lots` plus amountful leaves:

- `PlanModel` has `amount: ScalarEval` and `base_amount` evaluates it (`src/plan_compile.rs:12-27`). In the new shape, `PlanModel` should not own the primary amount; flow should receive current `Item.amount` via `FlowTx`, while `PlanModel` keeps only day/tokens/penalty/window/cost.
- `Plan::Lots` compiles to `lots(...)` (`src/plan_compile.rs:127-130`) — remove.
- `Plan::AggNet` compiles `key` + `amount` closures (`src/plan_compile.rs:131-138`) — remove amount.
- `Plan::Exact` compiles amount both as equality key and value (`src/plan_compile.rs:139-148`) — remove amount, exact uses current item amount.
- `Plan::Signal` compiles `signals` + amount (`src/plan_compile.rs:150-162`) — remove amount.
- `Plan::Flow` compiles amount into `PlanModel` (`src/plan_compile.rs:205-221`) — remove amount.
- Partition compile pre-validates inner by compiling once, then clones schema/plan into a per-shard factory (`src/plan_compile.rs:95-101`). If `pivot` has validation, this pattern should still be followed.

## Pivot combinator: likely semantics and risks

Requested behavior: “temporarily matches in another amount and converts allocations/residuals back.” This suggests pivot should be a strategy combinator, not a leaf, with shape roughly:

```rust
pivot(amount: Fn(&E) -> i64, inner: Box<dyn Strategy<E>>) -> Box<dyn Strategy<E>>
```

and plan node:

```json
{ "op": "pivot", "amount": "native", "inner": { ... amountless strategy ... } }
```

Recommended semantics:

1. Outer bag is in primary amount domain. `Item.original` and `Item.amount` are primary.
2. Pivot builds an inner bag in alternate amount domain:
   - full alt amount = `amount(&item.data)`.
   - if the outer item is not partially consumed, inner `amount=original=full_alt`.
   - if the outer item is already partially consumed, inner current alt should be proportional to the remaining primary amount (e.g. `full_alt * outer.amount / outer.original`) or use a defined fallback if `outer.original == 0`.
3. Run `inner` in alt domain.
4. Convert every returned group allocation and residual back to primary domain before returning outward.
5. Returned `Group.net`, `Allocation.amount`, residual `Item.amount`, and residual `Item.original` must be primary-domain values. Report stays primary amount.

Hardest part: exact integer conversion and conservation.

- Use deterministic proportional conversion and reconcile rounding by deriving residual primary as `outer_current_primary - sum(converted primary allocations for that id)`, not by independently converting each residual. This keeps per-id primary conservation exact.
- If one row appears in multiple groups from the pivoted inner run, allocation conversion needs deterministic apportionment (largest remainder or final-allocation correction) so the per-id converted allocations plus residual equal the outer primary amount exactly.
- If `outer.original == 0` or `alt_original == 0`, proportional conversion is ambiguous. Decide explicitly (leave residual unpivoted, treat as zero, or error). Do not silently divide by zero; add tests.
- Sign consistency matters. In normal currency lanes primary and pivot amounts should have the same sign. If signs differ, the flow source/sink orientation can invert under pivot; either normalize/sign-check or document it.
- Tolerances inside pivot are in pivot amount units; group `net` after conversion is in primary units and may not be zero even if the pivoted group netted exactly. That is likely desired for native-vs-USD reporting, but tests/UI should expect it.
- Origins: either preserve inner origins (`flow`, `exact_1to1`) or prefix (`pivot:flow`). Existing UI filters by origin; preserving origins is least disruptive but hides the amount domain. Make the choice deliberately.

## Report/workspace consequences

The report is already allocation-native:

- `AllocationOut { id, group_id, amount }` is the incidence edge (`src/report.rs:33-43`).
- Split rows are expected; strict projection refuses them (`src/report.rs:85-105`).
- `report_from_resolution` materializes every residual item as an unmatched group and records allocation amounts from `Item.amount` (`src/plan.rs:298-354`). This should continue, but now every residual amount should be primary.

Workspace-specific pitfalls:

- `StoredAlloc` currently stores `id/amount/original/lot` (`src/plan.rs:385-391`). After removing row/lot mode, `lot` should go. Every stored allocation should have real primary `amount` and `original`.
- `push_live_singleton` currently creates amount=0/original=0/lot=false (`src/plan.rs:461-475`). With a primary amount at setup, new live singletons should probably carry the row’s primary amount immediately; otherwise manual grouping before first solve still has no allocation amounts.
- `Recon::solve` has two frozen paths: `frozen_lots` and `frozen_whole_rows` (`src/plan.rs:542-575`). This should collapse to one allocation-native path: sum frozen primary amounts per id, residual = primary original - frozen sum, create an amountful item if residual != 0.
- `group()` fallback currently inserts zero-amount row-mode allocations if no live amount was pulled (`src/plan.rs:711-720`) and preserves caller-supplied net only when allocation net is zero (`src/plan.rs:723-733`). With primary amounts known at setup, this fallback should be rare or removed; verify manual grouping before solve.
- `group_allocations`, `take_live_amount`, `remove_allocations`, `ungroup` already operate in allocation amounts (`src/plan.rs:738-882`, `906-972`) and mostly fit the target model once `lot` is removed.
- Conservation currently checks only distinct row-id coverage (`src/plan.rs:206-210`, `347-348`, `613-618`), not per-id amount sums. The pivot refactor makes per-id amount conservation more important; consider adding an internal/debug validation that allocations + residuals equal primary original per id.

## Schema and host version call sites

Breaking wire change means bump all of these together:

- Rust contract: `src/plan.rs:27-33` (`CONTRACT_VERSION = 7`).
- JSON schema: `schema/plan.schema.json:5-6` description and `x-contract-version`.
- Browser host: `web/core/florecon.js:91-115` (`Florecon.CONTRACT_VERSION = 7`, checks `abi_version`).
- Python host: `py/src/florecon/_host.py:9-10` (`CONTRACT_VERSION = 7`, checks `abi_version`).

`schema/plan.schema.json` currently defines:

- `lots` node with `amount` and `inner` (`schema/plan.schema.json:231-250`) — remove/replace with `pivot`.
- amount fields in `agg_net`, `exact`, `signal`, `flow` (`schema/plan.schema.json:305-410`) — remove.
- `SolveRequest` required fields are only `schema`, `rows`, `plan` (`schema/plan.schema.json:414-437`) — add primary amount field.
- `Cmd::init` in schema required fields are `op`, `schema`, `plan`, `rows` (`schema/plan.schema.json:438-465`) — add primary amount field.

Also update TypeScript declarations in `web/core/index.d.ts` (not requested, but host package consumers will otherwise see stale types).

## Example and data exporters

### `examples/interco.rs`

Current example is already single-numeraire per currency shard:

- `Tx.snative` is native signed amount (`examples/interco.rs:22-29`).
- `Interco::base_amount` returns `tx.snative` (`examples/interco.rs:36-40`).
- `match_keys` adds an amount key from `tx.snative.abs()` (`examples/interco.rs:50-55`).
- Rows are pushed with `Item::lot(id, snative, Tx { ... })` (`examples/interco.rs:185-201`).
- Pipeline uses amount closures in `agg_net`, `exact_1to1`, `signal_group`, then `flow` (`examples/interco.rs:208-232`).

Refactor implications:

- Replace `Item::lot(id, snative, tx)` with the new always-amountful constructor/struct.
- Replace amountful primitives with amountless variants.
- Since the example’s primary amount is native and it partitions by currency, it likely does not need `pivot`.
- `Interco::cost` still uses `tx.snative`; if pivot/partial amount support matters for example flow, override `cost_lot`/`match_keys_lot` or change cost to rely on passed amounts. For this example, amountless strategy flow will pass current amounts through `FlowLotModel`; direct `cost` still sees full row amounts unless `cost_lot` is overridden.

### `web/ingest.js`

Current generic upload builder:

- Mapping exposes one required `amount` role (`web/ingest.js:72-80`). Good source for primary amount.
- Engine schema emits an `amount` number column (`web/ingest.js:91-106`).
- Plan inserts amount into every leaf:
  - agg_net `{ key: "gkey", amount: "amount", tol }` (`web/ingest.js:110-112`).
  - exact `{ amount: "amount" }` (`web/ingest.js:112`).
  - signal `{ signals: "tokens", amount: "amount", ... }` (`web/ingest.js:113-114`).
  - flow `{ amount: "amount", day: "date", ... }` (`web/ingest.js:115-119`).
- Return object includes `netKey: "amount"` (`web/ingest.js:155+` in full file), and display mirrors `d.native = d.amount` (`web/ingest.js:123-144`).

Refactor implications:

- Return primary amount at data/setup level, e.g. `amount: "amount"` beside `schema`/`plan`.
- Remove leaf amount fields from generated plan.
- Update `web/app.js` init dispatch (`startApp`) to include the new primary amount field when sending `{ op: "init", schema, plan, rows }`.
- `web/app.js` manual grouping uses `state.netKey` for host-computed net (`web/app.js:654-658`, `729`); keep `netKey` aligned with primary amount display key.
- Update `web/ingest.smoke.mjs` expectations for plan shape after rebuild.

### `python/export_web.py`

Current exporter:

- `plan()` embeds `amount: "native"` in each leaf (`python/export_web.py:31-40`).
- Schema includes `native` number column (`python/export_web.py:89-92`).
- Rows put native cents at the `native` position (`python/export_web.py:115-121`).
- Display includes both `native` and `usd` (`python/export_web.py:135-140`).
- Output currently writes `{ pair, schema, plan, fields, rows, display }` (`python/export_web.py:153-154`).

Refactor implications:

- Output should include the primary setup amount, likely `"amount": "native"` (or whatever name is chosen).
- `plan()` should remove all leaf amount fields.
- Consider adding `netKey: "native"` explicitly for the browser; `web/app.js` defaults to `native`, but explicit is safer.
- Similar stale plan builders exist in `python/run_interco.py`, `py/src/florecon/plan.py`, README/py README examples; not in requested list but likely need updates for tests/docs.

## Existing tests to preserve/adapt

Important current tests in `src/plan.rs`:

- Full pipeline and primitive plan tests start around `src/plan.rs:1120+`; all use amount-bearing plan leaves.
- Warm-vs-cold flow equivalence tests at `src/plan.rs:1671-1815` exercise stateful `Workspace` vs cold `Session` and should be kept after changing setup amount.
- Partial-flow/lot tests:
  - `report_preserves_partial_flow_remainder` uses `Plan::Lots` around `Plan::Flow` and expects +100/-60 to produce flow allocation `(1,60),(2,-60)` and unmatched `(1,40)` (`src/plan.rs:1872-1920`). This should become a baseline test without `Lots` because all Items are amountful.
  - `soak_small_classifies_by_residual_vs_original` uses `Plan::Lots` + `Flow` + `SoakSmall` + `SoakAll` (`src/plan.rs:1922-1966`). This should become a baseline test without `Lots`.
  - `workspace_group_allocations_takes_exact_live_amounts` and `workspace_remove_allocations_targets_one_group` currently wrap `Plan::Exact` in `Plan::Lots` (`src/plan.rs:1968+`); after refactor they should work without `Lots`.

Add new pivot tests:

- Full allocation through pivot: primary USD amounts, pivot native amounts, exact/signal/flow matches in native, returned allocations equal full primary amounts and residual empty.
- Partial allocation through pivot: e.g. primary +100, pivot +120 matched against pivot -60; returned allocation for positive row should be +50 primary and residual +50 primary (or whatever deterministic rounding policy defines).
- Rounding conservation: choose amounts where proportional conversion is fractional; assert per-id converted allocations + residual exactly equal outer primary amount.
- Pivot followed by `soak_small`: residual classification compares primary residual vs primary original after conversion back.
- Warm workspace with pivoted flow: compare warm vs cold or at least debug objective guard under repeated upsert/remove.

## Recommended implementation sequence

1. **Decide and codify the wire shape.** Recommended: add `amount: ScalarRef` to `SolveRequest` and WASM `Cmd::Init`, and to browser/Python data bundles. Bump contract to v8 in Rust/schema/web/Python.
2. **Make `Item` always amountful.** Remove `lot`, `Item::row`, `Item::lot`, `effective_amount`, `stamp_amount`, and `lots`. Add a single constructor that sets `original=amount` and `amount=amount`.
3. **Move primary amount initialization to boundaries.**
   - Batch: compile/evaluate primary amount in `Session::run_strategy`/`solve` and build amountful `Item`s.
   - Workspace: store an amount evaluator/closure in `Recon` or `Workspace`; make upsert/live singleton/solve use real primary amounts.
   - Collapse frozen row vs frozen lot logic into allocation-native frozen amount subtraction.
4. **Make strategy primitives amountless.** Update primitive structs/functions/tests and plan compiler. Flow strategy should always use `item.amount`; update `flow_sig` to use `match_keys_lot` with current amount.
5. **Update `Plan` enum and compiler.** Remove `Lots` and leaf amount fields; add `Pivot { amount, inner }`; remove `PlanModel.amount`.
6. **Implement `pivot` in `strategy.rs` and compile it from `Plan::Pivot`.** Focus on exact primary conservation and deterministic rounding. Keep returned report amounts in primary amount.
7. **Update schema and host builders.** `schema/plan.schema.json`, `web/ingest.js`, `python/export_web.py`, plus related `web/app.js`, `web/core/florecon.js`, `py/src/florecon/_host.py`, `py/src/florecon/plan.py`, examples/docs.
8. **Update examples/tests.** Convert interco example to new `Item` and amountless primitives. Rewrite lot tests as baseline amountful tests. Add pivot conversion tests before touching UI smoke tests.
9. **Rebuild/copy WASM artifacts for browser/Python smoke tests.** The browser smoke loads `web/core/engine.wasm`; it must match the updated contract.

## Validation commands

Core Rust:

```bash
cargo fmt --all
cargo check --all-targets --features serde
cargo test --all-targets --features serde
```

WASM/contract:

```bash
cargo build --release --target wasm32-unknown-unknown --features wasm
# then copy target/wasm32-unknown-unknown/release/florecon.wasm to web/core/engine.wasm (and Python package location if used)
python schema/validate.py web/data.json
node web/ingest.smoke.mjs
node web/smoke.mjs
node web/dom.smoke.mjs
```

Example/performance (requires parquet data):

```bash
cargo run --release --example interco -- data/ledger.parquet
# Optional timing diagnostics:
FLORECON_TIME=1 cargo run --release --example interco -- data/ledger.parquet
```

Python host sanity (after bundled wasm is present and contract bumped):

```bash
python -m compileall py/src python
python python/export_web.py data/ledger.parquet --max 1000
python schema/validate.py web/data.json
```

## High-risk pitfalls checklist

- Do not leave any row-mode zero allocations (`amount=0/original=0`) in live singletons; manual grouping before solve should have real primary amounts.
- Do not let pivot return alternate-amount allocations to `Report`; report amounts must be primary amount.
- Do not independently round pivot allocations and residuals; conserve primary amount per id exactly.
- Do not keep `Plan::Flow.amount` or `PlanModel.amount`; it will reintroduce per-leaf amount mode.
- Do not forget contract version fanout (`plan.rs`, schema, browser host, Python host) or browser/Python will reject the wasm.
- Direct `flow::Matcher` users still rely on `Model::base_amount`; changing strategy flow does not require removing it from `flow.rs` unless you want a larger public API break.
- Update `Flow::flow_sig` to match actual current-amount keys or warm solves can miss key-signature changes.
- If pivot changes candidate costs based on amount equality, ensure `cost_lot`/`match_keys_lot` are used consistently so flow cost and candidate generation see pivot/current amounts, not original row columns.
