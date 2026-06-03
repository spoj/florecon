# Implementation Plan

## Goal
Remove legacy row-mode/leaf-level amount selection, make every strategy consume `Item.amount`, introduce one plan-level primary numeraire with a `pivot` combinator for temporary alternate numeraires, and migrate all Rust/WASM/Python/web surfaces to the new contract.

## Tasks
1. **Define the new wire/API shape and bump the contract**
   - File: `src/plan.rs`
   - Changes:
     - Bump `CONTRACT_VERSION` from `7` to `8` and update the version comment.
     - Replace the current tagged-union `pub enum Plan` as the top-level wire type with:
       ```rust
       pub struct Plan {
           pub primary: ScalarRef,
           pub root: PlanNode,
       }
       #[serde(tag = "op", rename_all = "snake_case")]
       pub enum PlanNode { ... }
       ```
     - Move existing structural/leaf variants from `Plan` to `PlanNode`.
     - Remove `Plan::Lots` entirely.
     - Add `PlanNode::Pivot { amount: ScalarRef, inner: Box<PlanNode> }`.
     - Remove `amount: ScalarRef` from `AggNet`, `Exact`, `Signal`, and `Flow`:
       - `AggNet { key: ScalarRef, tol: i64 }`
       - `Exact {}`
       - `Signal { signals: String, tol: i64, cap: usize }`
       - `Flow { day: ScalarRef, tokens: String, penalty: f64, window: i64, cost: CostSpec }`
     - Keep `SoakSmall`/`SoakAll` unchanged except documentation should no longer say they require lot mode.
     - Update all intra-file tests and helper constructors to use `Plan { primary: ..., root: ... }` and `PlanNode::*`.
   - Acceptance: `cargo test --features serde plan::tests::plan_json_round_trips` compiles against the new `Plan`/`PlanNode` types and serialized JSON has top-level `primary` and `root` fields.

2. **Update JSON Schema for the v8 contract**
   - File: `schema/plan.schema.json`
   - Changes:
     - Set description and `x-contract-version` to `8`.
     - Redefine `$defs.Plan` as an object with required `primary` (`ScalarRef`) and `root` (`PlanNode`).
     - Add `$defs.PlanNode` as the tagged `oneOf` union currently held by `Plan`.
     - Remove the `lots` node from the union.
     - Add `pivot` node:
       ```json
       { "op": "pivot", "amount": ScalarRef, "inner": PlanNode }
       ```
     - Remove `amount` properties/requirements from `agg_net`, `exact`, `signal`, and `flow` nodes.
     - Keep `SolveRequest.plan` and `Cmd.init.plan` referencing `$defs.Plan`.
   - Acceptance: `python schema/validate.py` validates a regenerated v8 data file and rejects a legacy v7 plan without the migration step.

3. **Remove row mode from `Item` and make amounts mandatory at bag creation**
   - File: `src/strategy.rs`
   - Changes:
     - Remove `Item::row`, `lot: bool`, `effective_amount`, and `stamp_amount`.
     - Keep `Item` amount-bearing only:
       ```rust
       pub struct Item<E> {
           pub id: ExtId,
           pub original: i64,
           pub amount: i64,
           pub data: E,
       }
       impl<E> Item<E> {
           pub fn new(id: ExtId, amount: i64, data: E) -> Self { ... }
       }
       ```
     - `original` is always the original amount in the currently active numeraire; `amount` is the currently available residual amount in that same active numeraire.
   - Acceptance: no code path can create an `Item` without an amount; `grep` for `Item::row`, `lot:`, and `stamp_amount` returns no production matches.

4. **Refactor strategy primitives to read only `Item.amount`**
   - File: `src/strategy.rs`
   - Changes:
     - Change primitive APIs:
       - `agg_net(key, tol)`; sum `item.amount`.
       - `exact_1to1(key)`; group by caller key plus `item.amount.abs()`, pair opposite signs using `item.amount`.
       - `running_zero(order, tol)`; accumulate `item.amount`.
       - `signal_group(signals, tol, cap)`; net `item.amount`.
       - `flow(model)`; network supply must always be `item.amount`.
     - Remove every `amount: FA` field/closure from primitive structs.
     - Update `Flow::flow_amount`, `Flow::flow_sig`, warm/cold verification, and unmatched residual readback so they never call `model.base_amount(&item.data)` for strategy items.
     - Keep `crate::flow::Model::base_amount` unchanged for the lower-level `Matcher` API; it remains necessary for direct matcher users, but the strategy `flow` leaf wraps strategy items with the current `Item.amount`.
   - Acceptance: add a strategy test with payload data containing a misleading amount field and `Item::new(..., amount, data)` proving `agg_net`, `exact_1to1`, `signal_group`, and strategy `flow` use the item amount, not payload-derived amount.

5. **Implement the `pivot` combinator in the strategy algebra**
   - File: `src/strategy.rs`
   - Changes:
     - Add `pivot(amount_fn, inner)` as a structural combinator.
     - Semantics:
       1. Input items arrive in the caller’s active numeraire.
       2. For each item, compute full alternate original amount with `amount_fn(&item.data)`.
       3. Convert the caller-active residual `item.amount` into an alternate residual proportional to `item.original -> alt_original`.
       4. Run `inner` with `Item.original = alt_original` and `Item.amount = alt_residual`.
       5. Translate all groups and residual items back to the caller-active numeraire before returning.
     - Add a deterministic proportional allocator for conversion back to caller-active amounts. For each id, convert all produced parts for that id (group allocations plus final residual, in stable group/residual order) so their caller-active amounts sum exactly to the input caller-active residual. Use `i128` arithmetic and assign any rounding remainder deterministically to the last non-zero part for the id.
     - Recompute each returned group `net` from translated member amounts.
     - Document that nested pivots are supported because each pivot returns to its caller’s active numeraire.
   - Acceptance:
     - Unit test `pivot_exact_reports_outer_amount`: rows with primary `usd = ±110` and pivot `native = ±100` match inside `pivot("native", exact)` but report allocations/net in primary `±110`/`0`.
     - Unit test `pivot_partial_split_conserves_outer_amount`: a split row translated through pivot has all allocation plus residual amounts summing exactly to the original primary amount despite rounding.

6. **Compile plan-level primary and pivot nodes**
   - File: `src/plan_compile.rs`
   - Changes:
     - Introduce `CompiledPlan { primary: ScalarEval, strategy: Box<dyn Strategy<LoweredRow>> }`.
     - Change public internal compile entry point to compile `Plan` into `CompiledPlan`; add a recursive `compile_node(&PlanNode, &Schema)` for strategy nodes.
     - Compile `Plan.primary` once with `scalar_ref` and use it as the root item amount source.
     - Compile `PlanNode::Pivot` to `strategy::pivot(move |row| amount_eval.eval(row), compile_node(inner))`.
     - Compile `AggNet`, `Exact`, `Signal`, and `Flow` without leaf amount expressions.
     - Update `PlanModel` for flow cost:
       - Remove its `amount: ScalarEval` field.
       - `match_keys_lot(tx, amount)` continues to add `amount.unsigned_abs()` as the exact-amount candidate key.
       - `cost_lot(a, a_amount, b, b_amount)` continues to evaluate `Cond::AmountEqual` from the lot amounts supplied by the strategy leaf.
       - `base_amount` can return `0` or be documented as unused by strategy `flow`; direct `Matcher` users are unaffected.
   - Acceptance: compile errors from old `Plan::...` matches are resolved; an omitted `cost` field still defaults to `CostSpec::default()`.

7. **Update `Session`, `Recon`, and `Workspace` to initialize live items from the primary numeraire**
   - File: `src/plan.rs`
   - Changes:
     - `Session::run_strategy` must use `CompiledPlan.primary` to create `Item::new(id, primary.eval(row), row.clone())` instead of `Item::row`.
     - Change generic `Recon<E>` to own an amount initializer, for example:
       ```rust
       amount: Box<dyn Fn(&E) -> i64>
       pub fn new(strategy: Box<dyn Strategy<E>>, amount: impl Fn(&E) -> i64 + 'static) -> Self
       ```
     - `Recon::upsert` / `push_live_singleton` must initialize fresh singleton allocations with the primary amount, not `0`.
     - Remove `StoredAlloc.lot`; every allocation is now amount-native.
     - Keep `StoredAlloc.original` as the primary original amount for frozen/live residual accounting.
     - In `Recon::solve`, frozen allocations subtract from each row’s primary original to form the live residual `Item::new(id, original - frozen, item.clone())`; there is no whole-row/lot branch.
     - `Workspace::new` must compile the plan once, pass `CompiledPlan.strategy` and a cloned primary evaluator closure into `Recon::new`, and store no strategy-level amount.
     - Manual `group`, `group_allocations`, `remove_allocations`, and `ungroup` continue to operate on `AllocationSpec.amount`, now explicitly primary-numeraire amounts.
   - Acceptance:
     - Before first solve, a workspace report’s unmatched singleton allocations have primary amounts, not zero.
     - Freezing a partial allocation and re-solving leaves the remaining primary residual amount available exactly once.

8. **Strengthen boundary conservation around primary amounts**
   - File: `src/plan.rs`
   - Changes:
     - Keep the existing id coverage `conservation_airlock`.
     - Add an amount conservation check for `Session` reports: for each id, sum all `AllocationOut.amount` and compare to the primary amount used to create the input item.
     - Add the equivalent check in `Workspace::solve`/`report` paths: frozen plus live allocations for each row id must sum to that row’s primary original after every operation that changes groups.
     - If adding a new error is preferred, extend `src/error.rs` with `AmountConservationViolated { id, expected, actual }`; otherwise reuse an existing API error only if its message remains clear.
   - Acceptance: tests fail if a pivot rounding bug drops or creates one minor unit.

9. **Update WASM command surface documentation and version checks**
   - File: `src/wasm.rs`
   - Changes:
     - Update comments for `Cmd::Init`/manual grouping to say amounts are in the plan primary numeraire.
     - No command shape changes are needed beyond the new `Plan` shape carried by `Init` and `SolveRequest`.
   - Acceptance: `abi_version()` returns `8`; browser/Python hosts reject old v7 wasm until updated.

10. **Update TypeScript host types and browser host version**
    - Files: `web/core/index.d.ts`, `web/core/florecon.js`, `web/core/package.json` if package versioning is desired
    - Changes:
      - Set `Florecon.CONTRACT_VERSION = 8`.
      - Add `PlanNode` type and redefine `Plan`:
        ```ts
        export interface Plan { primary: ScalarRef; root: PlanNode; }
        export type PlanNode = ... | { op: "pivot"; amount: ScalarRef; inner: PlanNode } | ...;
        ```
      - Remove `lots` from `PlanNode`.
      - Remove `amount` from `agg_net`, `exact`, `signal`, and `flow` TS variants.
      - Document `AllocationOut.amount` and `AllocationSpec.amount` as primary-numeraire amounts.
    - Acceptance: `npm`/Node smoke scripts type-check conceptually against the new declarations and runtime version check uses 8.

11. **Update Python host version and plan builders**
    - Files: `py/src/florecon/_host.py`, `py/src/florecon/plan.py`, `py/README.md`, `py/src/florecon/__init__.py`
    - Changes:
      - Set Python `CONTRACT_VERSION = 8`.
      - Add a top-level builder, for example `def plan(primary, root): return {"primary": primary, "root": root}`.
      - Add `pivot(amount, inner)` builder.
      - Remove or deprecate `lots` builder. If kept temporarily for migration, make it return `pivot(amount, inner)` and mark it deprecated in docstrings.
      - Change builders:
        - `agg_net(key, tol=0)`
        - `exact()`
        - `signal(signals, tol=0, cap=256)`
        - `flow(day, tokens, penalty=1000.0, window=-1, cost=None)`
      - Update README examples to wrap roots with `P.plan("native", ...)` and remove leaf amount arguments.
    - Acceptance: Python examples build v8 JSON; old helper signatures are either removed with clear errors or documented as deprecated compatibility wrappers.

12. **Migrate browser ingest and data generation to emit v8 plans**
    - Files: `web/ingest.js`, `web/setup.js`, `web/ingest.smoke.mjs`, `web/dom.smoke.mjs`, `python/export_web.py`, `python/run_interco.py`, `python/enrich_web.py` if it materializes plan snippets
    - Changes:
      - Generic CSV ingest should emit:
        ```js
        { primary: "amount", root: { op: "seq", steps: [ ... ] } }
        ```
        and steps without leaf `amount` fields.
      - Interco export/run scripts should emit:
        ```python
        {"primary": "native", "root": {"op": "partition", ...}}
        ```
        with `agg_net`, `exact`, `signal`, and `flow` amount fields removed.
      - If a generated dataset wants native matching but USD reporting, emit `primary: "usd"` and wrap the existing native/currency leg in `pivot("native", ...)`; otherwise keep the current behavior by using `primary: "native"`.
      - Keep `data.netKey` aligned with the primary display column. For generated interco data this remains `"native"` unless intentionally migrating UI nets to USD.
      - Update setup copy from “Amount is the conserved value” to “Primary amount is the report/conservation numeraire”; mention pivot/native matching only if the UI exposes a separate field later.
    - Acceptance: `node web/ingest.smoke.mjs` sees `data.plan.primary === "amount"`, `data.plan.root.op === "partition"` or `"seq"`, and no leaf step contains an `amount` key.

13. **Update workbench assumptions about the conserved amount**
    - File: `web/app.js`
    - Changes:
      - Continue using `data.netKey` for manual group net display, but default to `data.plan.primary` when it is a string before falling back to legacy `"native"`.
      - Update comments around `primaryAssignments` to avoid confusing “primary group” projection with the new plan `primary` numeraire.
      - Ensure manual match/group net calculations use the primary display key (`state.netKey`) and therefore agree with `AllocationSpec.amount`.
    - Acceptance: `node web/smoke.mjs` still passes and manual grouping sends primary amounts to `group_allocations`.

14. **Update Rust examples and crate documentation**
    - Files: `examples/interco.rs`, `examples/bench.rs` if affected, `src/lib.rs`, `README.md`
    - Changes:
      - Replace `Item::lot` with `Item::new`.
      - Update strategy combinator calls to the new no-amount primitive APIs.
      - README design notes:
        - Replace “Numeraire per shard” with “Plan primary numeraire; use `pivot` inside shards for alternate matching numeraires”.
        - Remove “Lot capability is explicit and scoped” via `lots`; explain all rows are amount-bearing items from plan entry.
        - Add `pivot(amount, inner)` semantics and warning that report allocations are in the caller/root active numeraire after pivot translation.
      - Update Python and plan JSON examples to v8.
    - Acceptance: `cargo run --release --example interco -- --max ...` (or equivalent available invocation) compiles after API changes.

15. **Add a migration utility for existing v7 plan/data JSON**
    - New File: `scripts/migrate_plan_v7_to_v8.py`
    - Changes:
      - Accept either a bare legacy plan or a dataset/SolveRequest containing `plan`.
      - Determine the top-level `primary` from the first legacy leaf `amount` encountered unless `--primary` is supplied.
      - Recursively migrate nodes:
        - Drop `lots(amount, inner)` and replace with `pivot(amount, migrated_inner)` when `amount` differs from the current active amount; otherwise inline `inner`.
        - Remove matching `amount` from `agg_net`, `exact`, `signal`, and `flow`.
        - If a leaf amount differs from the current active amount, wrap that migrated leaf in `pivot(leaf_amount, leaf_without_amount)`.
        - Preserve `partition`, `branch`, `windowed`, `seq`, `soak_small`, `soak_all`, and `cost` structures.
      - Emit warnings when a legacy `seq` uses multiple leaf amounts without an explicit `lots`/future `pivot`, because the old row-mode semantics may not be equivalent for split residuals.
    - Acceptance: migrating the current `python/export_web.py`/`python/run_interco.py` legacy plan produces the same intended v8 structure with `primary: "native"` and no `pivot` needed.

16. **Update and add Rust tests for plan/session/workspace behavior**
    - Files: `src/strategy.rs`, `src/plan.rs`, `src/plan_compile.rs` if tests are split later
    - Changes:
      - Update all existing tests to construct `Plan { primary, root }` and `PlanNode` variants.
      - Add tests:
        - `primary_initializes_unmatched_singletons`: unmatched allocations equal primary amount before/after solve.
        - `leaf_amount_fields_removed_from_json`: v8 serialized plan has no leaf `amount` fields.
        - `pivot_exact_reports_primary`: native match under pivot reports primary allocations/net.
        - `pivot_rounding_amount_conservation`: split allocations under pivot conserve every input id’s primary amount exactly.
        - `workspace_freeze_partial_under_pivot`: freeze a pivot-produced partial group, re-solve, and verify remaining primary residual is correct.
        - `legacy_v7_plan_rejected_or_migrated`: depending on whether runtime compatibility is implemented, assert raw v7 JSON fails clearly or migration output succeeds.
      - Update `flow_cost_defaults_when_omitted` for the new `Flow` shape.
    - Acceptance: `cargo test --features serde` passes.

17. **Update JS/Python smoke tests and schema validation fixtures**
    - Files: `web/smoke.mjs`, `web/ingest.smoke.mjs`, `web/dom.smoke.mjs`, `py` tests if present, `schema/validate.py` only if it hardcodes schema details
    - Changes:
      - Update expected `CONTRACT_VERSION` to 8.
      - Update any inline plan JSON to top-level `{ primary, root }`.
      - Assert generated steps do not include leaf `amount` properties.
      - Add a schema validation smoke for a `pivot` plan.
    - Acceptance: `node web/smoke.mjs`, `node web/ingest.smoke.mjs`, and `python schema/validate.py <regenerated-data>` pass after rebuilding wasm.

18. **Regenerate web/WASM distribution artifacts after code changes**
    - Files/Commands: `scripts/build_wasm.sh`, generated `py/src/florecon/_engine.wasm`, `web/core/florecon.wasm`, generated `web/data.json` if committed/used locally
    - Changes:
      - Rebuild wasm so Python/npm hosts see `abi_version() == 8`.
      - Regenerate demo data with `python python/export_web.py ...` or migrate existing `web/data.json` using the new script.
    - Acceptance: browser and Python hosts no longer throw contract mismatch; demo loads and solves.

## Files to Modify
- `src/plan.rs` - new `Plan`/`PlanNode` API, contract version, session/workspace/recon amount initialization, tests.
- `src/plan_compile.rs` - compile top-level primary, compile `PlanNode`, add `pivot`, remove leaf amount compilation.
- `src/strategy.rs` - remove row mode and `lots`, refactor primitives to `Item.amount`, add `pivot`, update tests.
- `src/error.rs` - add amount-conservation error if not reusing an existing error.
- `src/wasm.rs` - comments/version-dependent docs for v8 plan semantics.
- `src/lib.rs` - API examples and docs for new strategy constructors.
- `src/report.rs` - documentation that allocation amounts are primary-numeraire amounts at the report boundary.
- `schema/plan.schema.json` - v8 schema, `PlanNode`, `pivot`, no leaf amount fields.
- `web/core/florecon.js` - contract version 8.
- `web/core/index.d.ts` - v8 Plan/PlanNode TypeScript types.
- `web/core/package.json` - optional package version bump for breaking API.
- `web/ingest.js` - emit v8 plans from uploaded CSVs.
- `web/setup.js` - UI copy for primary amount.
- `web/app.js` - default `netKey` from `plan.primary`, update comments/manual net assumptions.
- `web/*.smoke.mjs` - update inline plans and assertions.
- `python/export_web.py` - emit v8 interco demo plans.
- `python/run_interco.py` - emit v8 batch solve plan.
- `python/enrich_web.py` - update if it writes or assumes old plan shape.
- `py/src/florecon/_host.py` - contract version 8.
- `py/src/florecon/plan.py` - new builders and deprecated/removed old signatures.
- `py/src/florecon/projections.py` - docs only if mentioning primary groups/amounts.
- `py/src/florecon/__init__.py`, `py/README.md` - examples/docs.
- `README.md` - design notes and examples.
- `examples/interco.rs` - new item/primitive APIs.

## New Files
- `scripts/migrate_plan_v7_to_v8.py` - migration utility for legacy plans/datasets, especially existing generated web data.

## Dependencies
- Task 1 must happen before schema/types/tests can be updated.
- Tasks 3 and 4 must happen before Task 5, because `pivot` depends on amount-only `Item` semantics.
- Task 6 depends on Tasks 1, 4, and 5.
- Task 7 depends on Task 6 because `Session`/`Workspace` need compiled primary evaluation.
- Task 8 depends on Task 7 and the pivot rounding implementation.
- Host/schema/docs migration tasks (10-14, 17-18) depend on the v8 wire shape from Tasks 1-2.
- Rebuilding wasm/distribution artifacts (Task 18) must be last after Rust tests pass.

## Risks
- `/home/spoj/florecon/context.md` was not present during planning, so any intent documented only there needs confirmation before implementation.
- `pivot` rounding is the main correctness risk. It must conserve primary/caller-active amounts per id exactly, including split flow allocations and residuals.
- Partial frozen groups under `pivot` require careful proportional conversion of remaining primary residual into pivot residual; otherwise re-solves can double-count or strand amount.
- Removing row mode is a broad breaking change for direct Rust users of `Recon<E>` and strategy primitives. The migration story should be explicit in README/release notes.
- `PlanModel::base_amount` becomes unused by strategy `flow` but required by the lower `flow::Model` trait. Avoid changing the lower-level trait unless intentionally making a larger breaking change.
- Existing generated `web/data.json` or downstream user data with v7 plans will fail contract/schema validation until regenerated or migrated.
- The term “primary” is already used in `primaryAssignments` for a UI projection. Update docs/comments to avoid confusion between primary numeraire and primary group projection.
