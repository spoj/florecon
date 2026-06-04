"""florecon — incremental financial reconciliation by min-cost flow.

The engine ships as a single WASM core embedded in this wheel; this package is
a thin wasmtime host plus a Plan builder. One artifact, every OS — there is no
native extension to compile.

    from florecon import Workspace, plan as P, schema, col, key, KEY, NUMBER, TOKENS

    sch = schema([
        col("unit", KEY), col("ccy", KEY), col("day", NUMBER),
        col("objsub", KEY), col("native", NUMBER), col("tokens", TOKENS),
    ])
    root = P.partition("unit", P.partition("ccy", P.seq(
        P.agg_net("objsub", tol=100),
        P.exact(),
        P.signal("tokens", tol=100, cap=256),
        P.flow("day", "tokens"),
    )))

    # `native` is the primary numeraire the report nets on.
    ws = Workspace(sch, root, primary="native")
    # bare cells: a string for key/tokens columns, an int for number columns
    ws.upsert(1, [key("00492", "00288"), "USD", 0, "61500", 100, "INV0001"])
    ws.upsert(2, [key("00492", "00288"), "USD", 1, "61500", -100, "INV0001"])
    ws.solve()
    print(ws.report())
    # rows stream in incrementally; solve() again to warm re-solve
    ws.upsert(3, [key("00492", "00288"), "USD", 2, "61500", 50, "INV0002"])
    ws.solve()
"""

from ._host import CONTRACT_VERSION, ContractMismatch, Florecon, Workspace
from .data import KEY, NUMBER, TOKENS, col, key, schema
from . import plan
from .projections import connected_components, strict_assignments

__all__ = [
    "Florecon",
    "Workspace",
    "plan",
    "schema",
    "col",
    "key",
    "KEY",
    "NUMBER",
    "TOKENS",
    "CONTRACT_VERSION",
    "ContractMismatch",
    "strict_assignments",
    "connected_components",
]
__version__ = "0.1.0"
