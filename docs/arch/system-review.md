# Florecon — system-level architectural review

A characterization of what the system *is*, as built across the `florecon`
kernel and the `lf_solver` plugin. Not a feature list — a description of the
shape, the load-bearing invariants, the boundaries, and the tensions.

---

## 1. One-sentence characterization

> **Florecon is a portable, warm-stateful reconciliation *kernel* with a strict
> host/guest ABI, programmed by closures over a conservation-preserving strategy
> algebra, and delivered as a swappable back-end behind a thin multi-host wire
> contract.**

Equivalently: *"Extism for financial reconciliation"* — a computational core
that any host (Python notebook, browser, JVM/.NET ERP) drives over a finite verb
set, where the domain logic lives in a compiled plugin and the engine guarantees
that nothing is created or lost.

---

## 2. The layer cake (what sits on what)

```
            ┌──────────────────────────────────────────────────────┐
  HOST      │  Python (_host.py) │ JS (core/florecon.js) │ JVM/.NET │   thin, dumb,
  (per      │  persist · tags · projections · ingest · workbench    │   domain-agnostic
  runtime)  └───────────────────────────┬──────────────────────────┘
                                         │  describe() + Arrow IPC + JSON verbs
  ─────────────────────────────────  ABI boundary (sdk/abi.rs, ABI_VERSION=1)
                                         │  packed-u64 over linear memory
            ┌───────────────────────────┴──────────────────────────┐
  PLUGIN    │  lf_solver / interco:  #[derive(Record)] + Config +   │   domain logic,
  (guest)   │  one strategy() closure tree + project()              │   closures
            └───────────────────────────┬──────────────────────────┘
                                         │  Strategy<E> algebra (Item/Group/Tol)
  ─────────────────────────────────  SDK seam (sdk/plugin.rs, sdk/record.rs)
            ┌───────────────────────────┴──────────────────────────┐
  KERNEL    │  Recon (stateful workspace)  ·  strategy combinators  │   generic,
  (florecon │  ·  Report (allocation hypergraph)                    │   domain-free
  core)     ├───────────────────────────────────────────────────────┤
            │  engine.rs — warm incremental network simplex         │   the hot core
            └───────────────────────────────────────────────────────┘
```

Five strata, each with a single job and a clean seam to the next. The line
counts tell the story of where the mass is:

| Stratum | Module | LOC | Character |
|---|---|---|---|
| hot core | `engine.rs` | 1,988 | warm-started min-cost-flow (network simplex, cached basis) |
| algebra | `strategy/mod.rs` + `flow.rs` | 3,380 | the combinator vocabulary + the one stateful leaf |
| workspace | `recon.rs` | 761 | the stateful facade: pin/merge/solve lifecycle |
| ABI | `sdk/*` | ~1,000 | wire contract: describe, Arrow tables, packed-u64, conformance |
| plugin | `lf_solver/src/lib.rs` | 557 | **all the domain knowledge, in one file** |
| hosts | py ~640 + js ~254 | ~900 | thin per-runtime adapters |

The asymmetry is the point: ~7,900 lines of reusable, domain-free machinery
support a 557-line domain plugin and ~900 lines of host glue.

---

## 3. The load-bearing invariants

These are what make it a *system* and not a pile of functions. Each is enforced
**by construction**, not by convention.

1. **Conservation.** `groups ⊎ residual = input`. Every row's allocations sum to
   its original amount; a bad strategy degrades to a worse *grouping*, never to
   lost or invented money. This is the domain's physical law, and it lives in the
   kernel, not the plugin.

2. **One allocation type, end to end.** The `Allocation {id, amount}` /
   `AllocationOut {id, group_id, amount}` hypergraph is the *only* truth that
   crosses the ABI. Row→group is an explicit projection (`strict_/primary_
   assignments`), never core state. Lots that split across a match and a residual
   are first-class, not a special case.

3. **Two orthogonal axes, never crossed.**
   - *Lifecycle*: `proposed | pinned` — the machine's opinion vs the operator's
     decision. Owned by the engine.
   - *Review*: the tag overlay — attention/buckets. Owned by the host, keyed by
     stable row id, invisible to the engine.
   The engine never learns the word "review"; the host never re-implements
   matching. Clean separation of "what nets" from "what a human is thinking."

4. **Closures over data.** Predicates/keys/costs/orders are `Fn`, compiled into
   the plugin — not a serialized expression IR. The strategy is *code*, type-
   checked, expressive, fast. (Cost: it is not inspectable/diffable as data —
   see §7.)

5. **Single source of truth for columns.** `#[derive(Record)]` makes the Rust
   input struct the *only* declaration of the wire schema; `describe()` is
   generated from it. A missing column is a `SchemaError`, never a silent null.

6. **Warm state is preserved, not assumed.** The flow leaf keeps a live simplex
   basis; `partition_by` keeps one warm child per shard. Determinism is guarded
   (debug rebuilds a cold leaf and asserts equal objective).

---

## 4. The hot core: a warm incremental solver

The genuine algorithmic asset is `engine.rs`: a **bounded-variable network
simplex with a cached basis**. Each `solve` dispatches on what changed —
localized **dual repair** for supply/bound/removal, primal pricing (with rolling
block pricing) for cost changes, full rebuild only on `remove_node`.

Measured (40k nodes, 1M arcs): cold 18.26 s → warm supply edit **6.9 ms**
(~2,650×), warm arc removal **5.9 ms** (~3,100×). This is the "each node retains
state, recalc is fast" capability — a real what-if engine for one large network.

**Crucial system-level caveat:** warm-start's payoff is *workload-shaped*. On
lf_solver's real matrix (131k rows, thousands of tiny per-pair shards) a warm
re-solve is only ~1.2× faster than cold, because per-solve cost is dominated by
work the basis never touches: re-sharding all rows, recomputing the *stateless*
leaves (`agg_net`/`exact`/`signal` hold no basis), and Arrow-in/JSON-out
marshalling. **Warm-start is a single-large-network, localized-edit capability,
not a batch-throughput one.** The architecture supports it; the current batch
delivery path doesn't exploit it. (An interactive single-pair re-solve path would.)

---

## 5. The boundary: a wire contract, with wasm as one back-end

The product is **not the wasm** — it is the wire contract:

- `describe()` → the plugin's schema + domain identity (JSON),
- **Arrow IPC** for bulk row ingress (a *boundary* format, columnar, zero-copy-ish),
- **JSON command verbs** for the finite interactive vocabulary
  (`init/upsert/remove/solve/pin/unpin/merge/detach/dissolve/report`),
- **packed-u64 over linear memory** as the transport (ptr|len in one i64).

wasm is the *current* guest, chosen for its sandbox (data never leaves the host;
an untrusted plugin can't segfault the host). But the same contract admits a
native cdylib (drop Arrow for an all-Rust linked `solve(&plugin, rows)`), or an
out-of-process sidecar. The hosts are interchangeable precisely because they
speak only the contract, never the algebra. Python and JS hosts are line-for-line
mirrors — same verbs, same packed-u64 decode, same projections — which is the
test that the boundary is real.

---

## 6. What the system is *good* at

- **Correctness you can trust without reading the strategy.** Conservation is
  structural; the worst a bug does is mis-group, and that's visible in `net`.
- **Write-once host, write-many plugins.** A new domain is a 557-line plugin +
  zero host changes; `describe()` drives the generic host. The template + `just
  new-plugin` makes this a 30-second scaffold.
- **Portability as a first-class property.** One kernel runs in a notebook, a
  browser (no install, no data egress — the compliance win), or an embedded ERP
  runtime, behind one contract.
- **A real what-if core** for single large networks (the warm engine).
- **Durable operator state that's tiny and robust.** A saved workspace is
  `pinned decisions + tags` (allocation-native, row-id-keyed); everything else is
  re-derived by solve. Survives plugin tweaks that only move proposals.

---

## 7. The central tension: closures vs. data

The redesign's deepest trade, and the thing to keep watching:

- master made strategy a **serializable plan** (`plan.json`) — inspectable,
  diffable, versionable, runtime-tunable, but a stringly DSL.
- v2 makes strategy **closures** — type-safe, expressive, fast, but opaque: you
  can't show a business user *why* two rows didn't match without running it, and
  you can't diff "last quarter's logic" vs "this quarter's."

Feature ② (runtime `Config`) is *strategy-as-data creeping back* — the tunables
that genuinely need to live outside the compile. This is the right instinct: keep
**logic** as closures (for authors), expose **tunables + explanations** as a thin
data layer (for operators). If finance users ever need to see/tune matching or
get "why not?" explanations, a small plan-as-data projection returns — not the
whole DSL.

---

## 8. Risks / open edges (honest)

1. **Batch path leaves warm-start on the table** (§4). The marquee algorithmic
   asset is dormant in the only production workload. Either build the interactive
   single-pair path that exploits it, or accept that for batch it's correctness
   insurance, not speed.
2. **Marshalling tax.** Arrow-in/JSON-out of 131k rows + 129k allocations every
   solve is a fixed cost. Fine for batch; it caps interactivity. Levers: keep the
   Arrow blob opaque (done), an all-Rust linked path (no IPC), or a `solve(frame)`
   facade.
3. **Explainability gap** (§7). Closures can't answer "why didn't these match?"
   to a non-engineer. Not yet needed; will be if this faces business users.
4. **wasm32 4 GiB ceiling.** ~1–2M rows per indivisible block; intercompany is
   naturally pair-sharded so it won't bind, but it's a real wall for any
   single-giant-network domain. (wasm64 raises it but *increases* footprint and
   is blocked by deps; sharding is the better lever.)
5. **Product surface amputated on the branch.** master's `web/` app, portable
   persistence, and tag overlay were deleted; they're being re-grafted onto v2
   (Python layer done + tested; JS host + workbench in progress). Until that
   lands, the "browser tool, no install, no data egress" play exists only as a
   kernel, not a product.
6. **Two solve semantics to keep honest.** Stateless batch solve vs warm stateful
   workspace must agree; the determinism guard enforces equal objective but
   degenerate grouping can differ — a documented, tested, but live concern.

---

## 9. Verdict

A **clean, layered, invariant-driven kernel** with an unusually disciplined
host/guest boundary and a genuine warm-incremental solver at its core. The
engineering is sound where it counts: conservation by construction, one
allocation type, closures-over-plan, a generated wire schema, a tested two-host
mirror.

Its two frontiers are both about *delivery*, not *correctness*: (a) exploiting
the warm engine for interactivity instead of recomputing batches, and (b)
re-growing the product/UI surface (browser workbench, persistence, tags,
explanations) on top of the cleaner ABI. The kernel is the hard part and it's
done; what remains is turning a correct computational core into a tool a finance
user touches.
