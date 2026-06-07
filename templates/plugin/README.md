# __CRATE__

A florecon plugin: the matching strategy is Rust compiled to WebAssembly; a
generic Python host ships raw rows to it and reports the proposed groups. The
plugin never modifies accounting values — conservation of the numeraire is
guaranteed by construction.

```
solver/        Rust florecon plugin -> __LIB__.wasm   (schema + projection + matching)
run.py         runtime: build a dataframe, drive the wasm, print the report
```

## Build & run

```bash
rustup target add wasm32-unknown-unknown    # once
uv add polars florecon                       # the generic host + a dataframe lib
just run                                     # build the wasm, then run.py
```

For a tight authoring loop, run `just dev` in one terminal (rebuilds the wasm on
every Rust change) and re-run `uv run python run.py` as you go.

## Fill in the four spots in `solver/src/lib.rs`

1. **`Line`** — the raw columns the host ships (`#[derive(Record)]`: one struct
   is the input schema, the typed projection, and the identity).
2. **`Config`** — runtime tunables, delivered at `init` (tune without rebuilding).
3. **`project`** — derive your typed match `Row` from a `Line`.
4. **`strategy`** — the matching cascade. Reach for `agg_net` (net by key),
   `exact_1to1` (clean pairs), `signal_group` (token buckets), `flow` (N:M),
   composed with `partition_by` / `when` / `seq` / `fixed_point`.

Tune at runtime without rebuilding:

```python
Workspace(str(WASM), config={"tol": 100})
```
