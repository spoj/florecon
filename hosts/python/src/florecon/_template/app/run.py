#!/usr/bin/env python
"""Phase 2 — run the built plugin wasm on real data, in Python.

The host is generic: it reads the plugin's `describe()` and validates your
DataFrame against it. Your only job is to supply the raw columns the plugin's
`Line` record declares. The wasm is loaded straight from the cargo target dir —
no copy step (that keeps the justfile portable).

    cd app && uv sync && uv run python run.py
"""
from pathlib import Path

import polars as pl

from florecon import Workspace

# The release wasm produced by `florecon ship` (run it first). Cargo writes to
# the workspace-root target dir, so the path is target/, not solver/target/.
WASM = (
    Path(__file__).resolve().parent.parent
    / "target/wasm32-unknown-unknown/release/solver.wasm"
)


def main() -> None:
    # The raw columns the plugin's `Line` record declares.
    df = pl.DataFrame(
        {
            "id": [1, 2, 3, 4],
            "group": ["A", "A", "B", "B"],
            "amount": [100.0, -100.0, 50.0, -50.0],
        }
    )

    # Tune at runtime without rebuilding: Workspace(str(WASM), config={"tol": 100})
    ws = Workspace(str(WASM))
    ws.upsert(df)
    report = ws.solve()

    print(f"{len(report['groups'])} groups, {len(report['allocations'])} allocations")
    for g in report["groups"]:
        print(g)


if __name__ == "__main__":
    main()
