# The host surface — persistence, export, review, and the workbench

Status: **design + Python implemented, JS/web in progress.** Companion to
`recon-surface.md` (the workspace verbs) and `sdk-surface.md` (the plugin ABI).

This is the layer *above* the engine: everything an operator needs that the
conservation core deliberately does **not** know about — durable decisions,
review tagging, result export, and an interactive workbench. It re-founds the
good parts of the original (master) browser app on the v2 plugin ABI.

## What changed from master, and why

Master's web app was built around the **plan-DSL**: the browser *authored a
plan* (`buildDataset` emitted `{primary, root}`) and shipped it to a generic
engine. v2 deletes the plan-DSL — **the plugin owns the strategy** (closures,
compiled in) and self-describes its raw input columns via `describe()`. So the
host's job inverts:

| Concern | master (plan-DSL) | v2 (plugin) |
|---|---|---|
| Strategy | authored in the browser, shipped as data | compiled into the plugin `.wasm` |
| Columns | browser maps to engine *roles* + builds a plan | browser maps to the plugin's **declared** input columns |
| "What runs" | a `plan` object | the loaded plugin |
| Lifecycle word | `frozen` | `pinned` |
| Group verbs | `freeze_singletons`, `group_allocations` | `pin_singletons`, `merge` |

Everything else master got right is **kept** and carried forward.

## What we keep from master (it was good)

1. **The durable-state model.** Everything *proposed* is a deterministic
   function of `(rows, plugin)` via `solve`, so it is never saved. The only
   durable operator state is:
   - **pinned decisions** — committed groups, expressed *allocation-native* in
     stable `row_id` terms (group ids are ephemeral across solves), and
   - **the tag overlay** — review buckets, already keyed by stable `row_id`.

   A saved workspace = `decisions + tags (+ meta)`. On load: re-`upsert`,
   `solve` to recover proposals, then re-assert the pinned decisions on top.
   Small, robust, and survives plugin tweaks that only move *proposals*.

2. **The review/attention tag axis** — many-to-many, host-owned, keyed by
   `row_id`, **orthogonal** to the engine's `proposed | pinned` lifecycle. The
   engine never learns the word "review". Keying by `row_id` (not group id) makes
   tags survive a re-solve for free.

3. **Allocation-native everything** — exports and decisions are projections over
   the `allocations` hypergraph (`{id, group_id, amount}`), never over ephemeral
   group ids. Row→group is an explicit projection (`primary_assignments`).

4. **Export as plain files** — CSV (groups + row-level) and JSON (the raw
   report), produced from the report alone.

## The pieces

### Persistence (`persist`)
- `decisions(report)` → the pinned groups, allocation-native.
- `serialize(report, tags, meta)` / `parse` → the portable workspace object.
- `apply_decisions(ws, decisions)` → re-assert: multi-leg groups via `merge`
  (exact amounts, so splits survive), accepted singletons via `pin_singletons`.
  Merges first (they pull rows out of proposed groups), then singletons.
- `save_workspace` / `load_workspace` → file round-trip.

The raw rows are **not** embedded in the Python workspace (a notebook re-supplies
them from Spark). The **browser** workspace *does* embed a dataset echo, because
the browser has no other data source — that is the one host-specific difference.

### Export (`persist`)
- `report_frames(report)` → `(groups, allocations)` pyarrow Tables — the bridge
  for writing results back to Spark/Delta.
- `groups_csv`, `results_csv`, `result_json` — file deliverables.

### Review (`TagStore`)
- `ensure_tag / add / remove / clear / tagged / dump / restore`.

### The workbench (`hosts/js`)
A single static page, all client-side wasm: **data never leaves the laptop**
(the compliance win), IT only hosts static files. It loads a plugin `.wasm`,
reads its `describe()`, lets the operator map an uploaded CSV (or Arrow/Parquet)
to the declared columns, then drives the review loop:

- **facets** — slice rows by any declared key/dim or by tag;
- **groups** — the proposed/pinned groups, with net and size;
- **detail** — the rows, selectable;
- **verbs** — `pin` / `merge` (selection) / `dissolve` / `detach` / `unpin`,
  and `tag` / `untag`;
- **save / load** workspace, **export** CSV/JSON.

The browser host (`core/florecon.js`) is the exact mirror of the Python
`Florecon`/`Workspace`: same packed-u64 ABI, same `describe`/`dispatch`, same
verb set. One protocol, two hosts.
