"""Validate florecon wire JSON against the contract (schema/plan.schema.json).

    python schema/validate.py                 # validate the built-in canonical commands
    python schema/validate.py command.json …  # validate one or more Cmd documents

A document with an `op` is validated as a Cmd as-is; a bare `{"plan": …}` is
validated as the `init` command it abbreviates. Requires `jsonschema`.
"""

import json
import sys
from pathlib import Path

import jsonschema

ROOT = Path(__file__).resolve().parent.parent
SCHEMA = json.load(open(ROOT / "schema" / "plan.schema.json"))

# Canonical commands that together touch the Plan tree and the stateful verbs,
# so a no-arg run is a self-contained smoke of the schema itself.
SAMPLES = [
    {"op": "init", "plan": {"primary": "native", "root": {"op": "seq", "steps": [
        {"op": "exact"},
        {"op": "fixed_point", "inner": {"op": "agg_net", "key": "objsub", "tol": 0}},
        {"op": "flow", "order_by": "day", "tokens": "tokens", "penalty": 1000.0, "window": -1},
    ]}}},
    {"op": "solve"},
    {"op": "freeze_clean", "tol": 0},
    {"op": "report"},
]


def check(cmd: dict, label: str) -> None:
    cmd = cmd if "op" in cmd else {"op": "init", "plan": cmd["plan"]}
    jsonschema.validate(cmd, SCHEMA)
    print(f"{label}: valid {cmd['op']} command (contract v{SCHEMA['x-contract-version']})")


def main() -> None:
    if len(sys.argv) > 1:
        for a in sys.argv[1:]:
            check(json.load(open(a)), a)
    else:
        for cmd in SAMPLES:
            check(cmd, f"<built-in {cmd['op']}>")


if __name__ == "__main__":
    main()
