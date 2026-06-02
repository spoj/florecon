"""Validate a florecon SolveRequest / Plan JSON against the wire contract.

    python schema/validate.py [request.json ...]

With no argument, validates web/data.json (as a SolveRequest). Requires
`jsonschema` (pip install jsonschema).
"""

import json
import sys
from pathlib import Path

import jsonschema

ROOT = Path(__file__).resolve().parent.parent
SCHEMA = json.load(open(ROOT / "schema" / "plan.schema.json"))


def validate(path: Path) -> None:
    doc = json.load(open(path))
    jsonschema.validate(doc, SCHEMA)
    v = SCHEMA["x-contract-version"]
    print(f"{path}: valid SolveRequest (contract v{v})")


def main() -> None:
    args = sys.argv[1:] or [str(ROOT / "web" / "data.json")]
    for a in args:
        validate(Path(a))


if __name__ == "__main__":
    main()
