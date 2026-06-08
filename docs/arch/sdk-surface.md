# The plugin SDK surface — redesign

> **Update — authoring surface v2 (IMPLEMENTED).** The "three stringly
> contracts" below are now collapsed by `#[derive(Record)]`: one struct is the
> input schema (`describe()`), the typed projection, and the identity. The
> `Plugin` trait is `type Input: Record` + `type Row` + `type Config` +
> `domain()` / `new(config)` / `project(&Input)` / `primary()` / `strategy()`;
> `describe()` and identity are *provided*, and an author never writes a stringly
> column name or touches `RowView`. The host accepts a **dataframe only**
> (polars / pandas / pyarrow), validated and cast against `describe()` — no dict
> rows. `Config` is delivered in the `init` command, so tolerances/windows tune
> at runtime without rebuilding the wasm. See the `Plugin`/`Record` rustdoc and
> `examples/starter-plugin/` for the current shape; the sections below are the original
> rationale.

Status: proposal. Companion to `recon-surface.md` (the workspace) and
`strategy-surface.md` (the algebra). This rethinks `src/sdk/*` so the authoring
and host surfaces are **robust** (the loader catches what the conformance kit
can't), **clean** (one enforced schema, no decorative parallel docs), **
orthogonal** (the wire command set mirrors the workspace ops), and **expressive**
(typed errors the FE can act on).

## 1. The central problem: three stringly contracts that agree only by discipline

A plugin's columns are described in **three independent places** today, with
nothing tying them together:

1. `Plugin::describe()` — a hand-written `DescribeDoc` advertising columns + a
   `primary` flag, read by **nothing** at runtime (decorative).
2. The Arrow stream — whatever the host ships, validated against **nothing**.
3. `RowView::i64("amount")` lookups inside `project`/`id` — stringly, with
   **silent zero-fill** on absent/null/typo.

A typo (`r.i64("amont")`) silently yields `0` — a conserved-amount corruption
that **passes the conformance kit**, because the kit reuses the same projection
and is therefore self-consistent. `describe()` can drift arbitrarily from what
`project` actually reads.

### What the interco plugin teaches us about "primary"

`describe()` marks `indicative_usd_amt` (USD) as `primary`, while
`Plugin::primary(&Row)` conserves `snative` (native currency). These are
**intentionally different**: the host shows USD; conservation runs per
single-currency shard so FX never enters. So the numeraire is a *derived*
quantity, not a raw column — "bind the numeraire to the primary column" would be
wrong. The real defect is the **overloaded word "primary"** for two distinct
concepts:

- the **display amount** — which raw column the host shows as the headline
  figure (a UI hint);
- the **conserved numeraire** — `Plugin::primary(&Row)`, author-derived, the
  quantity `Recon` conserves.

The redesign separates them by name: `Field::amount()` marks the display column;
`Plugin::primary` stays the conserved numeraire, documented as distinct.

## 2. The schema becomes the enforced spine

Keep `Plugin::describe() -> DescribeDoc` and the `DescribeDoc` type (renaming to
`Schema` would collide with `arrow::datatypes::Schema` in plugin tests), but make
it **load-bearing** instead of decorative:

- **`Table::from_ipc(bytes, &DescribeDoc)`** validates the shipped Arrow against
  the declared columns: every declared column must be present with a compatible
  type, or ingestion fails loudly (`missing declared column "x"` /
  `column "x": declared i64, shipped Utf8`). It then **decodes only declared
  columns** — wide tables stop materializing unused data.
- **`RowView` accessors panic on a contract breach**: reading an *undeclared*
  column (`undeclared column "amont"`) or the *wrong type* (`column "ref" is
  text; read with str()`). A declared-but-null cell still yields the type's zero
  (legitimate). This converts the silent-zero typo class into an immediate,
  located failure that conformance now catches.

Net effect: `describe`, the Arrow stream, and projection are forced into
agreement at load/run time — a column `project` reads but doesn't declare panics;
a column declared but not shipped errors. No auto-generation, no drift.

Type compatibility (declared ⇐ shipped): `i64 ⇐ {Int64, Int32, Date32, Date64}`,
`f64 ⇐ {Float64, Float32, Int64, Int32}`, `utf8 ⇐ {Utf8, LargeUtf8}`.

## 3. The wire command set mirrors the workspace

The ABI `Cmd` enum carries the *same* overlaps the workspace had —
`Freeze`/`FreezeClean`/`FreezeSingletons`, `Group`/`GroupAllocations`,
`Breakup`/`RemoveAllocations`/`Ungroup` (13 commands). It collapses onto the
orthogonal `Recon` surface from `recon-surface.md`:

```jsonc
{ "op": "init" }                          // reset + ingest the batch
{ "op": "upsert" }                        // ingest more rows
{ "op": "remove", "ids": [..] }
{ "op": "solve" }
{ "op": "pin", "by": "group", "group_id": 7 }
{ "op": "pin", "by": "clean", "tol": 100 }
{ "op": "pin", "by": "singletons", "ids": [..] }
{ "op": "unpin", "group_id": 7 }
{ "op": "merge", "allocations": [{id,amount}..], "label"?, "reason"? }
{ "op": "detach", "group_id": 7, "ids": [..] }
{ "op": "dissolve", "group_id": 7 }
{ "op": "report" }
```

`pin`'s `by` selector is the wire image of `Recon::pin_where(pred)`:
`group` → `pin(id)`; `clean` → `pin_where(|g| g.is_match() && g.clean(tol))`;
`singletons` → `pin_where(|g| g.is_singleton() && g.contains_any(ids))`. 13 → 10
commands, each orthogonal.

## 4. Typed errors across the boundary

Every dispatch arm currently flattens the rich `ApiError` (which knows *which
id*, *which group*) to an opaque `String`. The envelope's `error` becomes a
typed body the FE can branch on, unifying the workspace errors with the ABI-only
ones (bad command, no session, ingest failure, duplicate id):

```jsonc
{ "ok": false,
  "error": { "code": "frozen_member", "id": 42,
             "message": "row 42 is in a frozen group; unfreeze it first" } }
```

`ApiError` gains `#[derive(Serialize)]` (under the `serde`/`sdk` feature) with a
`code` tag; the SDK wraps it together with `BadCommand`, `NoSession`, `Ingest`,
and `DuplicateId` in one serializable `DispatchError`. `message` is always
present (the `Display` text); structured fields ride alongside.

## 5. Self-description stays honest

- `Field::primary()` → **`Field::amount()`** (display headline; at most one).
- `Plugin::primary(&Row)` keeps its name and meaning (conserved numeraire),
  with a doc note that it is distinct from the display `amount` column.
- Conformance asserts **at most one `amount` field** declared, alongside the
  existing identity/derive properties.

## 6. Smaller cleanups (carried from review)

- Conformance panic/doc text says **"fix Plugin::key"** though the method is
  `id` — corrected to `id` throughout.
- `Plugin::primary` is a free fn while `id`/`project` take `&self`; left as-is
  (primary must stay a pure row→numeraire extractor), but documented.

## 7. Result

| Concern | Before | After |
|---|---|---|
| Column contract | 3 unchecked stringly sources | 1 enforced schema (load + accessor checks) |
| Typo'd column | silent `0`, passes conformance | panic, located, caught by conformance |
| "primary" | one word, two meanings | `Field::amount` (display) vs `primary` (numeraire) |
| Wire commands | 13, overlapping | 10, orthogonal (mirror `Recon`) |
| Errors | stringified | typed `{code, fields, message}` |
| Table decode | every shipped column, cloned | only declared columns |

## 8. Migration order

1. `recon-surface.md` lands first (the workspace ops the wire mirrors).
2. `error.rs`: `ApiError: Serialize` + a `DispatchError` wrapper.
3. `sdk/describe.rs`: `Field::amount()`; `DescribeDoc::amount_field`,
   type-compat helper.
4. `sdk/table.rs`: `from_ipc(bytes, &DescribeDoc)` validation + declared-only
   decode; `RowView` undeclared/wrong-type panics.
5. `sdk/conformance.rs`: new `from_ipc` signature; single-`amount` assertion;
   `key`→`id` text.
6. `sdk/abi.rs`: collapsed `Cmd` + `Select`; typed-error envelope.
7. `plugins/interco`, `lib.rs` docs/re-exports; `cargo test` across the
   workspace.

## 9. The cross-language contract: golden wire vectors

The old `schema/plan.schema.json` was a hand-maintained JSON Schema that drifted
(it rotted to contract v18, describing a deleted plan-as-data wire). Its heir is
**generated, not authored**: `plugins/interco/tests/golden.rs` drives the real
`sdk::abi::dispatch` over a canonical command script and pins each
`(cmd, arrow) -> envelope` triple under `golden/` (`UPDATE_GOLDEN=1` to
regenerate). The Rust test self-verifies against the committed fixtures; the
Python host replays the *identical* commands and Arrow payloads against the
**wasm** (`hosts/python/golden_replay.py`).

Because the expected envelopes are produced by the same serde the wasm emits, a
renamed field, a changed `status` string, or a moved error `code` breaks one of
the two replays — the contract cannot silently rot, and every new host language
inherits a ready-made conformance suite. Reports are compared after a stable
normalization (groups by id, allocations by `(group, id)`) so internal ordering
is not part of the contract. `just golden-check` runs both halves.
