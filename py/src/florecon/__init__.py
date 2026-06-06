"""florecon — incremental financial reconciliation by min-cost flow.

The host is a thin, generic wasmtime driver. A *plugin* ``.wasm`` owns the
domain (preprocessing, identity, matching) and describes itself; the host ships
the raw columns it asks for and drives the interactive workspace.

    from florecon import Workspace

    ws = Workspace("interco_plugin.wasm")
    ws.upsert(
        {"row_id": 1, "company": "A", "icp": "B", "objsub": "61500",
         "indicative_usd_amt": 100.0, "trx_currency": "USD", "trx_amt": 100.0,
         "gl_date": 0, "reference": "INV0001"},
        {"row_id": 2, "company": "B", "icp": "A", "objsub": "61500",
         "indicative_usd_amt": -100.0, "trx_currency": "USD", "trx_amt": 100.0,
         "gl_date": 1, "reference": "INV0001"},
    )
    print(ws.solve())
"""

from ._host import ABI_VERSION, ContractMismatch, Florecon, Workspace
from .projections import connected_components, strict_assignments

__all__ = [
    "Florecon",
    "Workspace",
    "ABI_VERSION",
    "ContractMismatch",
    "strict_assignments",
    "connected_components",
]
__version__ = "0.1.0"
