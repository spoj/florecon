# Architecture shift: domain plugins + a private SDK

Status: **design / branch `arch/plugin-sdk`** — clamps the plugin interface; sketches the SDK.
Author surface decisions are not final, but the **plugin interface** here is meant to be the
thing we freeze first.

---

## 1. The shift

Today florecon is **one engine + a declarative data-plan**: the host authors a `Plan` (JSON
`PlanNode` tree) and a column schema, ships both across the wasm boundary, and the engine compiles
the plan (`plan_compile.rs`) and matches. The host owns the domain; the wasm is generic.

The new world inverts ownership: **the distributed wasm *is* the domain.** Plan, preprocessing
(raw → match lanes), and any custom matchers are **baked into the artifact**. The host/UI is a
generic shell that knows nothing about the domain — it ships raw rows in and renders a `Report`
out.

Two direct consequences that motivate the whole design:

- **The boundary stops carrying a plan.** `init` no longer takes a `Plan`; the wasm self-describes
  what raw fields it wants and what report it returns.
- **Cross-host determinism becomes free.** One artifact computes the lanes, so the Python host and
  the browser host cannot disagree — there is nothing to keep in sync. (This deletes the entire
  "portable projection DSL + golden vectors" problem we previously worried about.)

There are therefore **two contracts**, and we clamp the outer one first:

| Contract | Audience | Stability | This doc |
| --- | --- | --- | --- |
| **Plugin interface** | generic host/UI ↔ any domain wasm | **freeze now** | §3 |
| **SDK** | us, building the plugin(s) in Rust | private, may churn | §4 |

The SDK can stay **private** and power exactly **one** plugin for now. The plugin interface cannot —
the moment a host loads a wasm, that boundary is load-bearing.

---

## 2. Key realization (why the SDK gets *smaller*, not bigger)

The matching primitives are already generic over the author's own row type and take **Rust
closures**, not data:

```rust
pub fn agg_net<E, FK>(key: FK, tol: impl Into<Tol>) -> Box<dyn Strategy<E>>  where FK: Fn(&E) -> u64;
pub fn signal_group<E, FS>(signals: FS, tol: impl Into<Tol>, cap: usize) -> ...  where FS: Fn(&E) -> Vec<u64>;
pub fn pivot<E, FA>(amount: FA, inner: ...) -> ...  where FA: Fn(&E) -> i64;
pub fn flow<M: Model>(model: M) -> Box<dyn Strategy<M::Tx>>;       // Model is a plain Rust trait
```

`PlanNode`, `Sel`, `CostSpec`/`Cond`, `plan_compile`, the Arrow-schema→`ColumnMap` derivation, and
`plan.py` exist for **one reason**: to express those closures (and the row payload) as *data* so a
non-Rust host can author them. An SDK author writes Rust, so **that entire layer is dead weight for
them.** They call the combinators with real closures, implement `Model` directly, and parse raw
bytes into their own struct however they like.

So "no more algebra, no more plan.rs, be more direct" is not a stylistic preference — it falls out
of the model. The SDK is the **engine + the strategy library + wiring**, minus the data front-end.

```
                 DATA FRONT-END (delete from SDK)            SDK (keep / promote)
   plan.py ─┐
   PlanNode ─┤  serialize closures + payload as data    │   Strategy<E>     (trait, leaves+combinators)
   Sel       ┤  ───────────────────────────────────▶    │   Model           (flow cost, plain trait)
   CostSpec  ┤                                           │   Recon<E>        (stateful engine, algebra-free)
   plan_compile ┘                                        │   Item/Group/Resolution/Tol, Report
   arrow.rs (schema authority)                           │   (combinators take real Rust closures)
```

`Recon<E>` (in `plan.rs` today, line ~390) is already the algebra-free engine:
`Recon::new(strategy, primary)`, generic over `E`, with `upsert/remove/solve/freeze/.../report`.
`Workspace` is just `Recon<PhysicalRow>` + `ColumnMap` + `Plan`. The SDK keeps `Recon`; the
`Workspace`/`Plan` shell is the data front-end.

---

## 3. The plugin interface (FREEZE THIS)

The contract between a **generic host** and a **domain wasm**. A conforming wasm exports exactly
this; a host speaks exactly this and nothing domain-specific.

### 3.1 Wasm exports (the ABI)

```
abi_version() -> u32                         // plugin-interface version gate
alloc(len: u32) -> u32                        // guest-owned scratch buffer
dealloc(ptr: u32, len: u32)
describe() -> u64                             // (len<<32)|ptr to a JSON DescribeDoc
dispatch(cmd_ptr, cmd_len, raw_ptr, raw_len) -> u64   // (len<<32)|ptr to a JSON Envelope
```

`dispatch` is unchanged from today's shape (JSON command + a second raw byte buffer + packed
ptr/len return). `describe` is **new** and is what makes the host generic. `init` **loses** its
`plan` field.

### 3.2 `describe()` — self-description (the generic-host enabler)

```jsonc
{
  "abi_version": 19,
  "domain": { "id": "florecon.intercompany", "version": "1.4.0" },
  "input": {                       // what raw fields the host must put in the raw buffer
    "encoding": "arrow_ipc",       // or "json_rows"; host treats it opaquely
    "fields": [                    // names/types are the DOMAIN's raw inputs, not match lanes
      { "name": "amount_minor", "type": "i64" },
      { "name": "fx_micros",    "type": "i64" },
      { "name": "entity",       "type": "str" },
      { "name": "invoice",      "type": "str" }
    ]
  },
  "report":   { "schema_version": 3 },        // the Report shape the host will render
  "capabilities": ["solve", "freeze", "group", "breakup"]   // which commands are supported
}
```

The host reads `describe()`, builds raw batches from `input.fields`, and renders `Report`. It never
sees a plan, a strategy, or a match-lane schema. The domain's *raw* schema is the only domain detail
that crosses, and it is self-advertised.

### 3.3 Commands (the opaque RPC) — no plan anywhere

```
init                              // open the session; raw buffer may seed initial rows
upsert                            // raw buffer = a batch of rows to add/replace
remove        { ids }
solve                             // run the baked strategy; returns a Report
report                            // current Report, no recompute
freeze        { group_id }
unfreeze      { group_id }
freeze_clean  { tol }
freeze_singletons { ids }
breakup       { group_id }
group         { ids, net?, origin?, reason? }
group_allocations { allocations, origin?, reason? }
remove_allocations { group_id, ids }
ungroup       { ids }
```

This is today's `Cmd` set **minus** `Init { plan }` and `Replan { plan }` (a baked plugin has no
runtime plan to swap). Everything else — the freeze/group human-decision surface and the `Report` —
is already domain-agnostic and carries over verbatim.

### 3.4 Response envelope (unchanged)

```jsonc
{ "ok": true,  "report": { /* groups, allocations, components */ } }
{ "ok": false, "error": "..." }
```

### 3.5 Versioning / conformance

- `abi_version()` gates the **interface**; every host refuses a mismatched binary (as today).
- `domain.version` (semver, in `describe`) gates the **domain build** for caching/audit; a `Report`
  can be stamped with `(abi_version, domain.id, domain.version)` for provenance.
- A **conformance harness** drives any candidate wasm through the command set and validates the
  `Report`/`describe` shapes, so a generic UI can trust arbitrary artifacts.

> **Clamp rule:** changes to §3.1–§3.4 are breaking and bump `abi_version`. The SDK (below) may
> churn freely *as long as the wasm it emits still satisfies §3*.

---

## 4. The SDK (private; powers one plugin)

Goal: an author writes a domain plugin with **two tiers of effort** and gets a §3-conforming wasm.
No `Plan`, no `PlanNode`, no `Sel`, no `CostSpec` data, no Arrow-schema authority — **direct Rust**.

### 4.1 Low end — the matching core we already have

The author works in their **own row type** `E` and composes strategies directly:

- Implement `Strategy<E>` for a fully custom matcher, **or**
- Build one from the provided combinators with real closures, **and/or**
- Implement `Model` for a custom `flow` cost.

```rust
struct Row { amount: i64, account: u64, day: i64, memo: Vec<u64>, usd: i64 }

fn strategy() -> Box<dyn Strategy<Row>> {
    seq(vec![
        agg_net(|r: &Row| r.account, Tol::Rel { bps: 10, floor: 0 }),
        exact_1to1_any(),
        signal_group(|r: &Row| r.memo.clone(), Tol::Abs(0), 256),
        pivot(|r: &Row| r.usd, flow(MyCost { window: 30 })),   // custom Model, direct
        soak_small("rounding", /* ... */),
        soak_all("unmatched", /* ... */),
    ])
}
```

No data layer is involved: `agg_net`'s key is a closure, `flow`'s cost is a trait impl, the residual
classifiers are direct calls. This tier is **already implemented** — it is `strategy.rs` + `flow.rs`
+ `Recon<E>`. The SDK just *exposes* it without the `Plan` wrapper.

### 4.2 High end — wiring niceties so the author conforms to §3

The author should not hand-write the ABI, the buffer dance, session state, freeze plumbing, or
`Report` rendering. They implement one trait and call one macro:

```rust
pub trait Plugin {
    type Row: Clone + 'static;

    /// The conserved primary amount (signed, minor units).
    fn primary(row: &Self::Row) -> i64;

    /// Parse the host's opaque raw buffer into rows. Use anything: arrow, serde,
    /// polars, hand-rolled. This is the "preprocessing baked into the wasm".
    fn derive(&self, raw: &[u8]) -> Result<Vec<(ExtId, Self::Row)>, Error>;

    /// The baked matching strategy (built per §4.1).
    fn strategy(&self) -> Box<dyn Strategy<Self::Row>>;

    /// Self-description for the generic host (§3.2).
    fn describe(&self) -> DescribeDoc;
}

florecon_sdk::export_plugin!(MyPlugin);   // emits abi_version/alloc/dealloc/describe/dispatch
```

`export_plugin!` generates the §3 ABI:

- a `thread_local!` `Recon<Row>` session built from `strategy()` + `primary`;
- `dispatch` decodes the `Cmd`, runs `derive` on the raw buffer for `init`/`upsert`, forwards
  freeze/group/solve to `Recon`, and serializes the `Report` envelope;
- `describe` returns the author's `DescribeDoc`;
- `abi_version` returns the interface constant.

The author's surface is therefore **exactly four functions**: `primary`, `derive`, `strategy`,
`describe`. Everything that makes the wasm *conform* is the macro's job — that is the "high-end
wiring nicety."

### 4.3 What the SDK is made of (file-level)

| SDK piece | Source today | Action |
| --- | --- | --- |
| `Strategy<E>` trait + combinators (closure-based) | `strategy.rs` | **keep**, this is the low-end core |
| `Model` + `flow` | `flow.rs` | **keep**, direct cost trait |
| `Recon<E>` engine + freeze/group/report | `plan.rs` (Recon half) | **extract** to `sdk::engine`, drop `Workspace`/`Plan` coupling |
| `Item/Group/Resolution/Tol`, `Report`/`GroupOut`/... | `strategy.rs`, `report.rs` | **keep** |
| `export_plugin!` + ABI harness + `DescribeDoc` | new, generalize `wasm.rs` | **new** (drops `Init{plan}`, adds `describe`, generic over `Plugin::Row`) |

### 4.4 What leaves the SDK (the "algebra")

These are the **data front-end**; they are not part of the SDK an author touches:

- `PlanNode`, `Plan`, `Cond`, `CostTier`, `CostSpec`  (`plan.rs`)
- `plan_compile.rs`  (PlanNode → Strategy compiler, group-metric lanes)
- `sel.rs`  (Sel-as-data expression evaluator)
- `arrow.rs` as the **schema authority** (an author may still *use* arrow inside `derive`, but the
  boundary no longer derives a `ColumnMap` from an Arrow schema)
- `plan.py`  (the Python plan DSL)

They do not have to be deleted from the repo immediately — see §5.

---

## 5. The existing florecon becomes "the first plugin"

We do not lose today's declarative capability; we **re-seat** it. The current data-plan engine
(`Plan` + `plan_compile` + `Sel` + `arrow` schema + `plan.py`) is exactly *a plugin built on the
SDK*: its `Row` is `PhysicalRow`, its `derive` is "parse the Arrow batch by the schema", and its
`strategy()` is "compile the embedded `PlanNode`". The twist in the new world is that the plan is
**baked at build time** rather than shipped at `init`.

So the migration is non-destructive:

1. Extract `Recon` + `Strategy`/`Model`/`Report` into the SDK surface (`sdk::engine`,
   `sdk::strategy`, `sdk::model`).
2. Generalize `wasm.rs` into the `export_plugin!` harness (drop `Init{plan}`, add `describe`).
3. Build the **first plugin** with the SDK. Initially it can even keep the data-plan internals
   (PlanNode/Sel/arrow) *inside that plugin crate* if we want to preserve declarative authoring —
   they just stop being the boundary.
4. Later, decide whether the data-plan front-end stays (as a "generic, host-authored" plugin) or is
   retired in favor of native-Rust plugins only.

This keeps `main` working while the branch proves the seam.

---

## 6. Open questions / decisions

- **Raw encoding in `describe.input.encoding`.** Keep Arrow IPC (zero-copy, columnar, already
  wired) as the default; allow `json_rows` for tiny/simple plugins. Host stays agnostic either way.
- **Where does `derive` run — per upsert or per solve?** Per-upsert (raw→`Row` at insert time) keeps
  the warm/incremental model clean and forbids cross-row derivations by construction. Recommended.
- **Macro vs trait-object registration.** `export_plugin!` (compile-time, one plugin per wasm) is
  simplest and matches "one artifact = one domain". No dynamic registry needed.
- **Do combinators stay as free functions or move behind a `strat::` module?** Cosmetic; keep free
  functions for directness.
- **Report schema versioning** independent of `abi_version` (so the human-decision surface can
  evolve without rev'ing the whole ABI).

## 7. Non-goals (for this branch)

- No Model-B composition (host wiring two wasms, cross-wasm callbacks). Single self-contained
  artifact only. WIT/component-model is explicitly deferred; the §3 hand ABI is sufficient and keeps
  the browser story and the Arrow fast-path intact.
- No public SDK. One private consumer; we are free to churn `sdk::*` until the plugin interface and
  one real plugin have settled.
- No cross-row preprocessing in `derive`. Stateful/aggregate features are done upstream or modeled
  as strategies.

---

## 8. TL;DR

1. **Freeze the plugin interface (§3):** `abi_version` + `describe` + the planless `Cmd` set +
   `Report`. That is the only load-bearing contract.
2. **The SDK shrinks by deleting the algebra:** authors write closures and `Model` impls directly
   against `Strategy<E>`/`Recon<E>`; `PlanNode`/`Sel`/`plan_compile`/`arrow`-schema/`plan.py` are the
   data front-end and leave the SDK.
3. **Two tiers:** low-end = the existing `Strategy`/`Model` core; high-end = a `Plugin` trait +
   `export_plugin!` that emits a §3-conforming wasm from four functions
   (`primary`, `derive`, `strategy`, `describe`).
4. **Non-destructive:** today's data-plan engine becomes the first SDK-built plugin.
