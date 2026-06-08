"""starter -- a florecon reconciliation plugin, packaged as a wheel.

The data team installs the wheel and never touches Rust or wasm:

    pip install starter            # (or: uv add starter)
    import starter, polars as pl
    ws = starter.workspace(config={"tol": 100})
    ws.upsert(df)
    report = ws.solve()

The compiled strategy (``solver.wasm``) travels *inside* this package as data,
so there is nothing to build or locate at the call site. Because the plugin is
wasm, this is one universal wheel -- the same file runs on every OS and arch
that ``florecon-host`` supports.
"""

from __future__ import annotations

from importlib.resources import files

from florecon import Workspace

__all__ = ["WASM", "workspace"]

# The bundled plugin wasm, placed here by `florecon package`.
WASM = str(files(__package__) / "solver.wasm")


def workspace(config: dict | None = None) -> Workspace:
    """Open a :class:`florecon.Workspace` on the bundled solver wasm.

    ``config`` is the plugin's runtime JSON (e.g. ``{"tol": 100}``), forwarded
    to the strategy's ``init`` -- tune without rebuilding.
    """
    return Workspace(WASM, config=config or {})
