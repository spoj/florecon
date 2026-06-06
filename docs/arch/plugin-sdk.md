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

### 3.5a Static manifest (custom section) — discovery without running the wasm

`describe()` is authoritative but requires compiling/instantiating the module. For a *folder of
many plugins*, the host must triage cheaply. So a conforming wasm **also** carries the same document
as a WebAssembly **custom section** named `florecon.manifest`:

```rust
// emitted by export_plugin! from the SAME DescribeDoc const that backs describe()
#[link_section = "florecon.manifest"]
static MANIFEST: [u8; N] = *b"{ \"abi_version\": 19, \"domain\": {...}, ... }";
```

Because the macro generates the section and `describe()` from one source, **they cannot drift** —
and that makes `describe()` the integrity check on the manifest (§9.3). Custom sections carry no
execution semantics, so a host reads them by scanning the binary: **no wasm runtime required.**

The manifest is the `describe()` doc plus discovery/trust fields:

```jsonc
{
  "abi_version": 19,
  "domain":  { "id": "florecon.intercompany", "version": "1.4.0" },
  "build":   { "git": "3e6de0a", "at": "2026-06-06T...", "digest": "blake3:..." },
  "input":   { "encoding": "arrow_ipc", "fields": [ /* required raw fields */ ] },
  "report":  { "schema_version": 3 },
  "capabilities": ["solve", "freeze", "group", "breakup"],
  "selector": {                       // OPTIONAL applicability rule (§9.2)
    "requires_fields": ["amount_minor", "entity"],
    "tag": "intercompany",
    "priority": 100
  }
}
```

See §9 for how the host uses this.

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
    type Raw;                      // decoded raw row (author's choice of repr)
    type Key: Hash + Eq + 'static; // the NATURAL identity of a row

    /// Split the opaque raw buffer into individual raw rows. Decoding only —
    /// no cross-row logic. Use anything: arrow, serde, polars, hand-rolled.
    fn decode(&self, raw: &[u8]) -> Result<Vec<Self::Raw>, Error>;

    /// Row-local: the stable identity of this row. MUST be deterministic and
    /// unique per logical row. The SDK hashes it to the engine ExtId, so the
    /// author never mints a u64 by hand (§10).
    fn key(&self, raw: &Self::Raw) -> Self::Key;

    /// Row-local: derive the match lanes. Deterministic, no other rows.
    fn project(&self, raw: &Self::Raw) -> Self::Row;

    /// The conserved primary amount (single numeraire, signed, minor units).
    fn primary(row: &Self::Row) -> i64;

    /// The baked matching strategy (built per §4.1).
    fn strategy(&self) -> Box<dyn Strategy<Self::Row>>;

    /// Self-description for the generic host (§3.2) and manifest (§3.5a).
    fn describe(&self) -> DescribeDoc;
}

florecon_sdk::export_plugin!(MyPlugin);   // emits abi_version/alloc/dealloc/describe/dispatch
```

`export_plugin!` generates the §3 ABI:

- a `thread_local!` `Recon<Row>` session built from `strategy()` + `primary`;
- `dispatch` decodes the `Cmd`; for `init`/`upsert` it runs `decode` then, per raw row,
  `ext_id = stable_hash(key(raw))` and `upsert(ext_id, project(raw))` — **the SDK owns id minting,
  warm-start, freeze, and Report rendering** (§10);
- `describe` returns the author's `DescribeDoc`;
- `abi_version` returns the interface constant.

The author's surface is `decode`, `key`, `project`, `primary`, `strategy`, `describe` — and only
`key`/`primary` carry invariant weight (§10). Everything that makes the wasm *conform and stay
correct across solves* is the macro + `Recon`.

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

## 9. Plugin discovery & cheap validation (host-side)

Scenario: a company folder holds many `*.wasm` plugins. A generic host must (a) find the conforming
ones, (b) validate them cheaply, and (c) pick the **correct** plugin for a given dataset — ideally
without instantiating dozens of modules.

### 9.1 Three-tier triage (cheapest first)

| Tier | Method | Cost | Runtime |
| --- | --- | --- | --- |
| **0 Discover** | read the `florecon.manifest` custom section from the file bytes | ~µs, no deps | **none** |
| **1 Confirm** | compile + `describe()`; assert it equals the manifest | ~ms | compile only |
| **2 Probe** | run a tiny sample batch through `init`+`solve` | ~10s ms | full instantiate |

Almost all selection happens at **Tier 0**. Tier 1 runs once, on the *chosen* plugin, to defeat a
stale/forged manifest. Tier 2 is only for genuinely ambiguous matches.

### 9.2 Selection: matching a dataset to a plugin

The host indexes every manifest (Tier 0) and resolves a dataset to a plugin by, in order:

1. **Explicit domain id.** If the dataset is tagged with a `domain.id`, pick the manifest that
   advertises it. Most robust; zero ambiguity.
2. **Structural fit.** Otherwise pick manifests whose `input.fields` (and optional
   `selector.requires_fields` / `selector.tag`) are *satisfiable* by the dataset's columns — i.e.
   the plugin can be fed from the data on hand.
3. **Tiebreak** among survivors by `selector.priority`, then highest compatible `domain.version`.

If still ambiguous, fall back to Tier 2 (probe the small candidate set) or surface the choice to the
operator. Incompatible `abi_version` is filtered out at Tier 0 and never instantiated.

### 9.3 Validation / trust ladder

- **Structural** (Tier 0): manifest present, JSON parses, `abi_version` compatible, required fields
  declared. Cheap reject of non-plugins and wrong-version binaries.
- **Integrity** (Tier 0): `build.digest` lets the host detect truncation/corruption; a registry can
  *pin* expected digests so an unknown/changed binary is flagged before it is ever compiled.
- **Provenance** (optional, Tier 0): a detached/embedded **signature** over the module, verified
  against a company key. The wasm sandbox bounds blast radius, but signing gives supply-chain trust
  for a folder of artifacts that may include custom Rust matchers.
- **Authoritative** (Tier 1): after selection, instantiate and assert `describe() == manifest`. This
  is what makes a baked manifest safe to trust at Tier 0 — a lying manifest fails here.

### 9.4 Optional: a cached registry index

A host may pre-scan the folder once into `index.json` (`domain.id → {path, version, digest,
input.fields, mtime}`) so steady-state selection is a map lookup. The Tier-0 scan is cheap enough
that the index is an optimization, not a requirement; invalidate entries by file `mtime`/`digest`.

### 9.5 The cheap scanner (no wasm runtime, ~30 lines)

Custom sections are section id `0`: a LEB128-length name followed by the payload. Scan for the one
named `florecon.manifest` and parse its JSON — no wasmtime, no browser `WebAssembly`:

```python
def read_manifest(path):
    b = open(path, "rb").read()
    if b[:4] != b"\0asm": return None          # not wasm
    p = 8                                          # skip magic + version
    while p < len(b):
        sec_id = b[p]; p += 1
        size, p = _uleb(b, p)
        body, p = b[p:p+size], p + size
        if sec_id == 0:                            # custom section
            nlen, q = _uleb(body, 0)
            name = body[q:q+nlen]
            if name == b"florecon.manifest":
                return json.loads(body[q+nlen:])
    return None
```

(Browser equivalent: `WebAssembly.Module.customSections(await WebAssembly.compile(bytes),
"florecon.manifest")` — compile, no instantiate.)

### 9.6 SDK responsibility

`export_plugin!` owns the manifest↔`describe()` coupling: it serializes the author's `DescribeDoc`
once, returns it from `describe()`, **and** embeds it via `#[link_section = "florecon.manifest"]`,
folding in `build.git`/`build.digest` at compile time. The author writes `describe()` and gets free,
drift-proof, runtime-less discovery.

---

## 10. Author responsibility boundary: identity & the bookkeeping the SDK owns

The worry: "if the wasm holds the domain, every author must get warm-start / stable ids / freeze
right." They don't. That bookkeeping is **`Recon<E>`'s**, inherited by every plugin:

| Concern | Owner | Author touches it? |
| --- | --- | --- |
| Warm-start flow basis, present-set delta | `Recon` + `flow`/`Matcher` | no |
| Monotonic group-id minting (`next_id`, never reused) | `Recon` | no |
| Freeze / unfreeze / breakup / group id stability | `Recon` | no |
| Incremental upsert / remove | `Recon` | no |
| Conservation (incl. pivot airlock) | engine | no |
| Report rendering, envelope, ABI | `export_plugin!` | no |
| **Stable row identity from raw data** | **author (`key`)** | **yes** |
| **Coherent primary numeraire** | **author (`primary`)** | **yes** |
| Row-local lane derivation | author (`project`) | yes (but signature-constrained) |

So the genuinely tricky part collapses to **one thing: identity.** Warm-start and frozen-decision
persistence both key off a *stable* `ExtId`. If `key` is non-unique, unstable across batches, or
non-deterministic, you get silent churn (warm-start thrashes) or detached freezes — with no compile
error. Everything else the author writes is ordinary, local, and hard to get "invariant-wrong".

### 10.1 Make identity a typed obligation, not a freeform u64

The author never mints an `ExtId`. They **name** the natural key (`type Key` + `fn key`), and the
SDK hashes it to the `ExtId` with the same stable FNV-1a the engine already uses for categories. This
turns "how do I produce a stable u64" (easy to botch) into "which field(s) identify a row" (a domain
question the author can actually answer).

### 10.2 Forbid the cross-row footgun by signature

`key` and `project` take **one** `&Self::Raw`, never the batch. Cross-row derivations (rank,
dedupe, running balance) are therefore *unrepresentable* in the per-row hooks — the type system keeps
derivation row-local, which is exactly what the warm/incremental model requires. (Cross-row features
go upstream or become strategies.)

### 10.3 Strict mode: catch identity bugs at runtime, loudly

The harness can run cheap invariant checks (toggleable; off in prod):

- **Collision**: two raw rows in a batch hash to the same `ExtId` but differ in content → duplicate
  key, raise instead of silently overwriting.
- **Determinism**: re-run `project`/`key` on a sample and assert identical output (catches float /
  locale / hashmap-order nondeterminism).
- **Conservation**: the engine already returns `ConservationViolated`; the harness surfaces it as a
  clear per-row diagnostic pointing back at `primary`/`project`.

### 10.4 A generic conformance kit (the real guarantee)

Because the SDK controls the harness, it can mechanically test the properties that *only* break when
identity/derive is wrong — without the author writing any of these tests:

- **Idempotent upsert**: upsert the same batch twice → Report unchanged (stable keys).
- **Order independence**: shuffle the batch → Report unchanged (no positional identity).
- **Warm == cold**: incremental upserts vs one cold load → identical Report (warm-start integrity).
- **Freeze survives churn**: freeze a group, upsert unrelated rows, re-solve → the frozen group is
  intact (id stability across solves).

The engine already proves the last two for itself (`warm_start_matches_cold`,
`dual_warm_matches_cold`); the kit re-offers the same harness to plugin authors as a drop-in test.
A plugin that passes the kit has, by construction, gotten identity right.

### 10.5 Net answer

Not tricky to get the *bookkeeping* right — the author doesn't implement it. The one sharp edge is
**stable identity**, and we de-risk it three ways: (1) typed `key` so the SDK owns id minting, (2)
row-local hook signatures so cross-row mistakes can't compile, (3) a conformance kit + strict mode
that catch the remaining "unstable/non-unique key" failure modes mechanically.

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
- **Manifest signing.** Decide whether `build.digest` alone suffices (corruption detection) or we
  want detached signatures + a company key for supply-chain trust over a plugin folder (§9.3).
- **Who mints identity — host or plugin?** Default to the plugin (`fn key`, §10.1) so identity is a
  domain decision baked with the rest. Allow a host-supplied id column as the trivial `key` for
  data that already carries a stable id.

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
