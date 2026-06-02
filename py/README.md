# florecon (Python)

Incremental financial reconciliation by min-cost flow. The engine is a single
WASM core embedded in this wheel and driven via `wasmtime` — there is no native
extension to compile, so one `py3-none-any` wheel runs on every OS.

```python
from florecon import Workspace, plan as P, schema, col, key, KEY, NUMBER, TOKENS

sch = schema([
    col("unit", KEY), col("ccy", KEY), col("day", NUMBER),
    col("objsub", KEY), col("native", NUMBER), col("tokens", TOKENS),
])
pln = P.partition("unit", P.partition("ccy", P.seq(
    P.agg_net("objsub", "native", tol=100),
    P.exact("native"),
    P.signal("tokens", "native", tol=100, cap=256),
    P.flow("native", day="day", native="native", tokens="tokens"),
)))

ws = Workspace(sch, pln)
# bare cells: a string for key/tokens columns, an int for number columns
ws.upsert(1, [key("00492", "00288"), "USD", 1, "61500", 100, "INV1"])
ws.upsert(2, [key("00492", "00288"), "USD", 2, "61500", -100, "INV1"])
ws.solve()
print(ws.report())          # one clean group
```

The bundled `_engine.wasm` is produced by:

    cargo build --release --target wasm32-unknown-unknown --features wasm --lib
    cp target/wasm32-unknown-unknown/release/florecon.wasm py/src/florecon/_engine.wasm
