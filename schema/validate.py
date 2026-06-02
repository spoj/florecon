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
    # A workbench bundle (web/data.json) carries extra UI fields alongside the
    # request; project to the SolveRequest subset the contract defines.
    req = {k: doc[k] for k in ("schema", "rows", "plan") if k in doc}
    jsonschema.validate(req, SCHEMA)
    v = SCHEMA["x-contract-version"]
    print(f"{path}: valid SolveRequest (contract v{v}, {len(req.get('rows', []))} rows)")


def main() -> None:
    args = sys.argv[1:] or [str(ROOT / "web" / "data.json")]
    for a in args:
        validate(Path(a))


if __name__ == "__main__":
    main()
