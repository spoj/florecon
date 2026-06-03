# Implementation Plan: Arrow IPC Native — Tear Down Logical Layer

## Goal
Replace the schema/lowering pipeline (`schema.rs`, `expr.rs`, `lower.rs`, `LoweredRow`/`LoweredCell`) with Arrow IPC native parsing that directly produces `PhysicalRow { ints: Vec<i64>, tokens: Vec<Vec<u64>> }`, using a `ColumnMap` built from the Arrow schema to resolve column-name references at plan-compile time.

## Current State Assessment
The `arrow-ipc` branch already has:
- Arrow dependency in `Cargo.toml` ✓
- `src/arrow.rs` with a `rows_from_ipc()` parser that converts Arrow → old `Row`/`Cell` format → lowering pipeline
- `src/plan.rs` with `CONTRACT_VERSION=8`, `Plan{primary,root}`, `PlanNode::Pivot`, amountless primitives ✓
- `src/plan_compile.rs` compiling the new plan shape ✓
- `src/wasm.rs` with `arrow_ptr`/`arrow_len` parameters ✓
- `src/strategy.rs` with `Item::new()`, `pivot()`, amountless primitives ✓
- Host contract versions already at v8 ✓
- JSON schema already at v8 with pivot/amountless primitives ✓

**What's NOT done**: The old logical layer still exists and all plan execution still routes through it. Specifically:
- `LoweredRow`/`LoweredCell` are still the data carrier
- `Schema`/`Column`/`Kind` still define column layout
- `scalar_ref()`/`bool_ref()` from `expr.rs` still resolve names via `Schema::index()`
- `Row::lower()` still runs the FNV-1a lowering per cell
- `PlanModel::base_amount()` still takes `&LoweredRow`
- All closures reference `LoweredRow::int()`/`LoweredRow::tokens()`
- Tests construct `Row::new(vec![Cell::Num(...), ...])`

## Tasks

### Phase 1: Define new types (lowest blast radius)

1. **Create `PhysicalRow` and `ColumnMap` in `src/row.rs`**
   - File: `src/row.rs`
   - Changes: Replace the existing `LoweredRow`/`LoweredCell` with:
     ```rust
     pub struct PhysicalRow {
         pub ints: Vec<i64>,
         pub tokens: Vec<Vec<u64>>,
     }
     
     pub struct ColumnMap {
         pub int_cols: HashMap<String, usize>,
         pub token_cols: HashMap<String, usize>,
     }
     ```
   - Add helper methods on `PhysicalRow`:
     - `pub fn int(&self, idx: usize) -> i64` — returns `ints[idx]` or 0
     - `pub fn tokens(&self, idx: usize) -> Vec<u64>` — returns clone of `tokens[idx]` or empty
   - Add helper methods on `ColumnMap`:
     - `pub fn int_index(&self, name: &str) -> Result<usize, ApiError>` — looks up `int_cols`
     - `pub fn token_index(&self, name: &str) -> Result<usize, ApiError>` — looks up `token_cols`
   - Acceptance: `cargo check` passes (old `LoweredRow`/`LoweredCell` still exist, just new types added)

2. **Rewrite `src/arrow.rs` to produce `PhysicalRow` directly**
   - File: `src/arrow.rs`
   - Changes:
     - Parse Arrow IPC `StreamReader` from `&[u8]`
     - Column 0 = ID (UInt64 or Int64)
     - Remaining columns: Int64 → push to PhyiscalRow.ints, Utf8/LargeUtf8 → tokenize (split on whitespace, filter alphanumeric ≥6 chars, FNV-1a hash, dedup) → push to PhysicalRow.tokens
     - Build `ColumnMap` from Arrow field names + types
     - Return `(Vec<(ExtId, PhysicalRow)>, ColumnMap)`
     - Move FNV-1a and token-extraction logic from `lower.rs` into this module (or a shared `token.rs` utility)
   - Acceptance: `cargo check --lib` passes (return type may be unused initially)

3. **Create `src/token.rs` for shared token-extraction utilities**
   - File: `src/token.rs` (new)
   - Move `fnv1a()`, `cat()`, `tokens()` from `lower.rs` here
   - Keep `TokenCfg` struct (minlen, maxlen, drop) — used by arrow.rs at parse time
   - Acceptance: `cargo check` passes

### Phase 2: Update plan/compile layer to use new types

4. **Update `src/plan_compile.rs` to work with `ColumnMap` and `PhysicalRow`**
   - File: `src/plan_compile.rs`
   - Changes:
     - Change `compile(plan, schema)` → `compile(plan, column_map: &ColumnMap)`
     - Replace `scalar_ref(name, schema)` → `column_map.int_index(name)`
     - Replace `bool_ref(name, schema)` → `column_map.int_index(name)` (bools are just ints ≠ 0)
     - Replace `schema.index(tokens_name)` → `column_map.token_index(tokens_name)` for Signal/Flow token columns
     - Change all closures from `|r: &LoweredRow| r.int(k)` → `|r: &PhysicalRow| r.int(k)`
     - Change `PlanModel` impl: `type Tx = PhysicalRow`, use `r.int()` / `r.tokens()` in all methods
     - Remove `base_amount` from `PlanModel` (return 0, unused)
     - Keep `CompiledPlan` struct but change `primary: usize` (it's just an index)
   - Acceptance: `cargo check --lib` compiles (may need Phase 3 changes to compile fully)

5. **Update `src/plan.rs` to use new types, remove old type refs**
   - File: `src/plan.rs`
   - Changes:
     - Add `pub use crate::row::{PhysicalRow, ColumnMap};` instead of `LoweredRow`, `LoweredCell`
     - Remove `pub use crate::schema::{Column, Schema};`
     - Remove `pub use crate::expr::{BoolRef, ScalarRef};`
     - Remove `use crate::lower::Row;`
     - Change `Session` to store `BTreeMap<ExtId, PhysicalRow>` instead of `LoweredRow`
     - Change `Session::upsert` to accept `PhysicalRow` directly (no lowering)
     - Change `Session::from_rows` to accept `(ExtId, PhysicalRow)` pairs
     - Change `Session::run_strategy` to use `PhysicalRow` in bag materialization
     - Change `Workspace` inner: `Recon<PhysicalRow>` instead of `Recon<LoweredRow>`
     - Change `Workspace::new` to accept `ColumnMap` instead of `Schema`, compile plan against it
     - Change `Workspace::upsert` to accept `PhysicalRow` directly
     - Remove `schema()` accessor from `Workspace` (or return `&ColumnMap`)
     - Update `SolveRequest` to carry `ColumnMap` instead of `Schema`
     - Update test helpers: replace `schema()`, `row()`, `row_with_class()` with `column_map()`, `phys_row()` factories
   - Acceptance: `cargo check --lib` compiles

6. **Remove `ScalarRef` and `BoolRef` type aliases from `src/expr.rs` (prepare for deletion)**
   - File: `src/plan.rs`
   - Changes: Replace all `ScalarRef` → `String`, `BoolRef` → `String` in `PlanNode` fields
   - These are already `String` under the hood; just remove the alias usage
   - Acceptance: plan.rs compiles without referencing `crate::expr`

### Phase 3: Update boundary and cleanup

7. **Update `src/wasm.rs` to use `PhysicalRow` and `ColumnMap`**
   - File: `src/wasm.rs`
   - Changes:
     - `rows_from_ipc` returns `(Vec<(ExtId, PhysicalRow)>, ColumnMap)` — update call sites
     - `Cmd::Init` should carry `column_map: ColumnMap` instead of `schema: Schema`
     - `SolveRequest` uses `ColumnMap`
     - Update `apply()` and `run()` to use new types
     - Remove `use crate::lower::Row;` and `use crate::schema::Schema;`
   - Acceptance: `cargo check --features wasm` compiles

8. **Update `src/lib.rs` — remove old module exports, add new ones**
   - File: `src/lib.rs`
   - Changes:
     - Remove `pub mod schema;`
     - Remove `pub mod expr;`
     - Remove `pub mod lower;`
     - Add `pub mod token;`
     - Update re-exports: remove `Cell, Kind, Row, TokenCfg, LoweredCell, LoweredRow, Column, Schema`
     - Add: `pub use row::{PhysicalRow, ColumnMap};`
   - Acceptance: `cargo check --lib` compiles

9. **Delete old files**
   - Files to delete: `src/schema.rs`, `src/expr.rs`, `src/lower.rs`
   - Acceptance: `cargo check --lib --features serde` compiles cleanly

### Phase 4: Fix tests and examples

10. **Update tests in `src/plan.rs`**
    - File: `src/plan.rs` (test module)
    - Changes:
      - Replace `schema()` helper with `column_map()` that returns a `ColumnMap` with known column names
      - Replace `row(usd, day, objsub, native, tokens)` with `phys_row(ints: &[i64], token_strs: &[&str])` that creates `PhysicalRow` directly, hashing token strings inline
      - Remove all `Cell`, `Kind`, `Row`, `Schema` imports from test module
      - Update all test assertions — they should pass with same logic
    - Acceptance: `cargo test --lib` all tests pass

11. **Update tests in `src/lower.rs` (move to `src/token.rs`)**
    - File: `src/token.rs`
    - Move the `matches_python_host`, `token_rules`, `lowers_by_kind` tests (adapted for the new module)
    - Remove lowering-specific tests (arity mismatch, kind mismatch — no longer relevant)
    - Acceptance: `cargo test --lib` token tests pass

12. **Update `examples/interco.rs`**
    - File: `examples/interco.rs`
    - Changes:
      - Already uses `Item::new(id, snative, tx)` ✓
      - Already uses amountless primitives ✓
      - Check that `Model` impl for `Interco` doesn't reference any deleted types
      - No `LoweredRow` dependency — should work as-is since `Strategy<E>` is generic
    - Acceptance: `cargo check --example interco` compiles

### Phase 5: Update hosts and schema

13. **Update `schema/plan.schema.json`**
    - File: `schema/plan.schema.json`
    - Changes:
      - Replace `Schema`/$defs references with `ColumnMap` definition:
        ```json
        "ColumnMap": {
          "type": "object",
          "properties": {
            "int_cols": { "type": "object", "additionalProperties": { "type": "integer" } },
            "token_cols": { "type": "object", "additionalProperties": { "type": "integer" } }
          },
          "required": ["int_cols", "token_cols"]
        }
        ```
      - Remove `Cell`, `Kind`, `Column`, `Row`, `IdRow` $defs (no longer on wire)
      - Update `SolveRequest.required` to use `column_map` instead of `schema`
      - Update `Cmd.init` to use `column_map`
      - Keep `PlanNode` definitions (already correct)
    - Acceptance: JSON schema is valid, `python schema/validate.py` passes

14. **Update Python host (`py/src/florecon/_host.py`)**
    - File: `py/src/florecon/_host.py`
    - Changes:
      - `Workspace.__init__` accepts `column_map` dict instead of `schema` dict
      - `Cmd::Init` sends `column_map` not `schema`
      - `Workspace.upsert` sends Arrow IPC buffer alongside command (or uses rows array with pre-hashed token columns)
      - Remove `KEY`, `NUMBER`, `TOKENS`, `col`, `key` convenience imports if they reference deleted types
    - Acceptance: `python -m compileall py/src` passes

15. **Update `python/export_web.py`**
    - File: `python/export_web.py`
    - Changes:
      - Build `ColumnMap` from column definitions instead of `Schema`
      - Export `column_map` in data.json instead of `schema`
      - Use pyarrow to serialize rows as Arrow IPC buffer alongside JSON
    - Acceptance: `python python/export_web.py data/ledger.parquet --max 1000` succeeds

16. **Update JS host (`web/core/florecon.js`)**
    - File: `web/core/florecon.js`
    - Changes:
      - `_call()` method needs to pass Arrow IPC buffer alongside JSON command
      - Add `_call_with_arrow()` method that passes `arrow_ptr`/`arrow_len` to `solve`/`dispatch`
      - Or extend existing `_call` to accept optional Arrow buffer
    - Acceptance: `node web/smoke.mjs` works

17. **Update `web/ingest.js`**
    - File: `web/ingest.js`
    - Changes:
      - Build `ColumnMap` instead of `schema`
      - Use `apache-arrow` to serialize rows into Arrow IPC format
      - Update plan generation to use new amountless plan shape (already mostly there)
    - Acceptance: `node web/ingest.smoke.mjs` passes

### Phase 6: Build and validate

18. **Build WASM and copy artifacts**
    - Commands:
      ```bash
      cargo build --release --target wasm32-unknown-unknown --features wasm
      cp target/wasm32-unknown-unknown/release/florecon.wasm web/core/engine.wasm
      ```
    - Acceptance: WASM binary exists and is loadable

19. **Run full validation suite**
    - Commands:
      ```bash
      cargo fmt --all
      cargo check --all-targets --features serde
      cargo test --all-targets --features serde
      cargo build --release --target wasm32-unknown-unknown --features wasm
      python schema/validate.py web/data.json
      node web/ingest.smoke.mjs
      node web/smoke.mjs
      node web/dom.smoke.mjs
      ```
    - Acceptance: All pass

## Files to Modify

| File | Changes |
|---|---|
| `src/row.rs` | Replace `LoweredRow`/`LoweredCell` with `PhysicalRow`/`ColumnMap` |
| `src/arrow.rs` | Rewrite to produce `(Vec<(ExtId, PhysicalRow)>, ColumnMap)` from Arrow IPC |
| `src/plan.rs` | Use `PhysicalRow`/`ColumnMap` instead of `LoweredRow`/`Schema`; remove old type refs |
| `src/plan_compile.rs` | Compile against `ColumnMap`; closures on `PhysicalRow` |
| `src/wasm.rs` | Use `ColumnMap`/`PhysicalRow`; update Cmd/SolveRequest shapes |
| `src/lib.rs` | Remove old modules, add `token`, update re-exports |
| `src/strategy.rs` | No changes needed (already generic over `E`) |
| `src/flow.rs` | No changes needed (already generic over `Tx`) |
| `src/error.rs` | Possibly add `UnknownTokenColumn` variant |
| `examples/interco.rs` | Verify compiles; minor adjustments if needed |
| `schema/plan.schema.json` | Replace Schema with ColumnMap; remove Cell/Kind/Row defs |
| `py/src/florecon/_host.py` | Use column_map; send Arrow IPC buffers |
| `py/src/florecon/__init__.py` | Update exports (remove KEY/NUMBER/TOKENS if present) |
| `python/export_web.py` | Build ColumnMap; serialize rows as Arrow IPC |
| `web/core/florecon.js` | Add Arrow IPC buffer passing |
| `web/ingest.js` | Build ColumnMap; serialize rows with apache-arrow |
| `web/app.js` | Update init dispatch to include column_map |
| `web/core/index.d.ts` | Update type declarations |

## New Files

| File | Purpose |
|---|---|
| `src/token.rs` | FNV-1a hash, token extraction, `TokenCfg` — moved from `lower.rs` |

## Files to Delete

| File | Reason |
|---|---|
| `src/schema.rs` | Replaced by `ColumnMap` in `row.rs` |
| `src/expr.rs` | Replaced by direct `String` types and `ColumnMap` lookups |
| `src/lower.rs` | Token extraction moved to `token.rs`; cell lowering eliminated |

## Dependencies

- Task 1 (PhysicalRow) must come first — everything depends on it
- Task 2 (arrow.rs rewrite) depends on Task 1 and Task 3 (token.rs)
- Task 3 (token.rs) can be done in parallel with Task 1
- Tasks 4–6 (plan/compile layer) depend on Tasks 1–3
- Tasks 7 (wasm) depends on Tasks 4–6
- Task 8 (lib.rs) depends on Tasks 7 and 9
- Task 9 (delete old files) can happen after Task 8 confirms no references remain
- Tasks 10–12 (tests/examples) depend on Tasks 4–9
- Tasks 13–17 (hosts/schema) depend on Task 9 (shape finalized)
- Tasks 18–19 (build/validate) depend on all prior tasks

## Risks

1. **Token-extraction configuration**: `TokenCfg` (minlen, maxlen, drop) currently lives on `Schema`. After deletion, it must live somewhere. Proposal: embed in `ColumnMap` as optional fields, or keep it as a runtime parameter passed alongside. **Decision needed**: confirm whether token config is per-column or global.

2. **Arrow IPC token encoding ambiguity**: The task says `tokens: Vec<Vec<u64>>` in `PhysicalRow` — these are pre-hashed. But the Arrow IPC stream might carry raw `Utf8` strings (needing FNV-1a in arrow.rs) or `List<UInt64>` (pre-hashed by host). Both should be supported. The `ColumnMap` should indicate which columns get tokenized at parse time. For MVP, Arrow Utf8 columns with a "tokens" type hint → hash during parsing.

3. **String-key columns (Kind::Key)**: These currently lower strings to single i64 via `cat()`. In Arrow, they'd be `Int64` columns (host pre-hashes) or `Utf8` columns (parsed and hashed to ints by arrow.rs). The `ColumnMap` needs to know which `int_cols` entries correspond to key columns. For MVP, all Int64 columns go to `ints`; the compile layer uses indices by name regardless.

4. **Test migration effort**: Tests in `plan.rs` construct rows with `Cell::Num()` and `Cell::Str()` directly. These must all be rewritten to construct `PhysicalRow` with pre-computed ints and tokens. This is mechanical but tedious — ~20 test functions.

5. **Host Arrow IPC serialization**: Python pyarrow and JS apache-arrow both support `RecordBatchStreamWriter`. The hosts must now serialize data as Arrow IPC streams plus a `ColumnMap` JSON. This is new territory — the smoke tests may fail on first attempt.

6. **JSON wire contract compatibility**: The JSON `SolveRequest` and `Cmd::Init` shapes change (`schema` → `column_map`, `rows` format changes). Existing data.json files or test fixtures may break. The `schema/plan.schema.json` rewrite is critical for validation.

7. **WASM memory growth**: Arrow IPC streams can be large. The current WASM `alloc` uses `Vec<u8>` which works, but large record batches may stress WASM linear memory limits. Validate with realistic data sizes.

8. **`flow::Model` trait**: `PlanModel` implements `Model` with `type Tx = LoweredRow`. After migration it becomes `type Tx = PhysicalRow`. The `base_amount` method returns 0 (unused by strategy flow). The `cost_lot` method uses `a.int(day)` and `a.tokens(tokens)`. These method signatures are generic over Tx, so this should work seamlessly — but verify the `Clone` bound on `PhysicalRow` (it needs `#[derive(Clone)]`, already true for `Vec<i64>` and `Vec<Vec<u64>>`).

## Verification Commands (in order)

```bash
# Phase 1
cargo check --lib

# Phase 2-3
cargo check --lib --features serde

# Phase 4
cargo test --lib --features serde
cargo check --example interco

# Phase 5
cargo check --features wasm
python -m compileall py/src python

# Phase 6
cargo build --release --target wasm32-unknown-unknown --features wasm
cp target/wasm32-unknown-unknown/release/florecon.wasm web/core/engine.wasm
python schema/validate.py web/data.json
node web/ingest.smoke.mjs
node web/smoke.mjs
```