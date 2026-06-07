#!/usr/bin/env python
"""__CRATE__ runtime: ship a dataframe to the plugin wasm and print the result.

The host is generic — it reads the plugin's describe() and validates your frame
against it. Your only job here is to produce the raw columns the plugin declares.
"""
from pathlib import Path

import polars as pl

from florecon import Workspace

WASM = Path(__file__).resolve().parent / "solver/target/wasm32-unknown-unknown/release/__LIB__.wasm"


def main() -> None:
    # The raw columns the plugin's `Line` record declares.
    df = pl.DataFrame(
        {
            "id": [1, 2, 3, 4],
            "group": ["A", "A", "B", "B"],
            "amount": [100.0, -100.0, 50.0, -50.0],
        }
    )

    ws = Workspace(str(WASM))            # tune at runtime: Workspace(str(WASM), config={"tol": 0})
    ws.upsert(df)                        # dataframe in (polars / pandas / pyarrow)
    report = ws.solve()

    print(f"{len(report['groups'])} groups, {len(report['allocations'])} allocations")
    for g in report["groups"]:
        print(g)


if __name__ == "__main__":
    main()
