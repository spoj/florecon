# florecon workbench (browser host)

A single static page that drives any **florecon plugin** entirely client-side:
the plugin `.wasm` and your data never leave the browser. It is the JS mirror of
the Python host (`hosts/python`) — same ABI, same verbs, same projections — plus
an interactive review UI.

```
core/florecon.js     host: packed-u64 ABI, describe + dispatch, Workspace verbs
core/projections.js  strict/primary assignments, connected components (no deps)
core/persist.js      durable decisions + serialize/restore + CSV/JSON export
core/tagstore.js     review/attention tag overlay (host-owned, row-id keyed)
ingest.js            CSV -> map to describe().input columns -> Arrow batch
app.js               the workbench UI (data-driven from describe())
index.html / styles.css
```

## What it does

1. **Load a plugin** (`.wasm`) → it self-describes via `describe()`.
2. **Load a CSV** and **map** its columns onto the plugin's declared input
   columns (auto-mapped by name; adjust as needed).
3. **Solve.** Review the result: facet/slice rows, inspect proposed groups, drill
   into the detail table.
4. **Decide:** `pin` / `pin clean` / `merge selection` / `dissolve` / `unpin`,
   and `tag` rows for review.
5. **Persist & export:** save the workspace (pinned decisions + tags + a dataset
   echo, so it reloads with no re-upload), or export `groups.csv` / `results.csv`
   / `result.json`.

The v2 difference from the old plan-DSL app: **there is no plan to author.** The
plugin owns the strategy; the browser only maps data to declared columns and
drives the review loop.

## Run it

It is plain ES modules + an import map (no build step). Serve the folder and open
`index.html`:

```sh
cd hosts/js
python3 -m http.server 8000      # or any static server
# open http://localhost:8000/
```

`apache-arrow` is loaded in the browser via the import map (esm.sh). For an
offline/air-gapped deploy, vendor an `apache-arrow` ESM build locally and point
the import map at it.

A plugin wasm to try: build the bundled interco plugin
(`just build-wasm` at the repo root) and load
`target/wasm32-unknown-unknown/release/interco_plugin.wasm`, or your own
`lf_solver.wasm`.

## Test (head-less)

The whole stack below the DOM is verified under node — it loads the real wasm,
ingests a CSV, solves, pins, round-trips a saved workspace, and asserts the
result matches the Python host:

```sh
npm install          # apache-arrow
npm test             # node core/host.test.mjs
```

The UI layer (`app.js` + DOM) is the only part not covered by the head-less
test; everything it calls is.
