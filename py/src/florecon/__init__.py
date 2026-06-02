"""florecon — incremental financial reconciliation by min-cost flow.

The engine ships as a single WASM core embedded in this wheel; this package is
a thin wasmtime host plus a Plan builder. One artifact, every OS — there is no
native extension to compile.

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
    print(ws.report())
"""

from ._host import CONTRACT_VERSION, ContractMismatch, Florecon, Workspace
from .data import Int, Tokens, row
from .intern import Interner
from . import plan

__all__ = [
    "Florecon",
    "Workspace",
    "plan",
    "Int",
    "Tokens",
    "row",
    "Interner",
    "CONTRACT_VERSION",
    "ContractMismatch",
]
__version__ = "0.1.0"
