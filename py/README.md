# florecon (Python)

Incremental financial reconciliation by min-cost flow. The engine is a single
WASM core embedded in this wheel and driven via `wasmtime` — there is no native
extension to compile, so one `py3-none-any` wheel runs on every OS.

```python
from florecon import Workspace, plan as P, Int, Tokens

schema = ["unit", "ccy", "day", "objsub", "native", "tokens"]
pln = P.partition("unit", P.partition("ccy", P.seq(
    P.agg_net("objsub", "native", tol=100),
    P.exact("native"),
    P.signal("tokens", "native", tol=100, cap=256),
    P.flow("native", day="day", native="native", tokens="tokens"),
)))

ws = Workspace(schema, pln)
ws.upsert(1, [Int(1), Int(1), Int(1), Int(0), Int(100), Tokens([])])
ws.upsert(2, [Int(1), Int(1), Int(2), Int(0), Int(-100), Tokens([])])
ws.solve()
print(ws.report())          # one clean group
```

The bundled `_engine.wasm` is produced by:

    cargo build --release --target wasm32-unknown-unknown --features wasm --lib
    cp target/wasm32-unknown-unknown/release/florecon.wasm py/src/florecon/_engine.wasm
