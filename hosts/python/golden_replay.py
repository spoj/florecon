"""Replay the golden wire vectors against the plugin wasm through the host.

The vectors in ``golden/`` are authored by the Rust side (the source of truth:
``cargo test -p interco-plugin --test golden``). This script feeds the identical
commands and Arrow fixtures to the *wasm* via the generic host and asserts the
returned envelopes match byte-for-structure. It is the cross-language half of
the contract: if the host marshalling or the wasm drifts from Rust, this fails.

    just build-wasm
    PYTHONPATH=hosts/python/src .venv/bin/python hosts/python/golden_replay.py
"""

import json
import pathlib
import sys

from florecon import Florecon

ROOT = pathlib.Path(__file__).resolve().parents[2]
GOLDEN = ROOT / "golden"
WASM = ROOT / "target/wasm32-unknown-unknown/release/interco_plugin.wasm"


def normalize(env: dict) -> dict:
    """Mirror the Rust normalizer: order groups by id, allocations by (group, id)."""
    rep = env.get("report")
    if isinstance(rep, dict):
        rep["groups"] = sorted(rep.get("groups", []), key=lambda g: g["group_id"])
        rep["allocations"] = sorted(
            rep.get("allocations", []), key=lambda a: (a["group_id"], a["id"])
        )
    return env


def fail(msg: str):
    print(f"FAIL: {msg}")
    sys.exit(1)


if not WASM.exists():
    fail(f"plugin wasm not found at {WASM}\n  build it with: just build-wasm")
if not (GOLDEN / "vectors.json").exists():
    fail("golden/vectors.json missing — generate with "
         "`UPDATE_GOLDEN=1 cargo test -p interco-plugin --test golden`")

manifest = json.loads((GOLDEN / "vectors.json").read_text())

fe = Florecon(str(WASM))

# describe() must match what the vectors were generated against.
got = fe.describe()
if got != manifest["describe"]:
    fail(f"describe() drift\n  want={manifest['describe']}\n  got={got}")

for i, step in enumerate(manifest["steps"]):
    arrow_bytes = None
    if step["arrow"]:
        arrow_bytes = (GOLDEN / step["arrow"]).read_bytes()
    got = normalize(fe.dispatch(step["cmd"], arrow_bytes))
    want = step["expect"]
    if got != want:
        fail(
            f"step {i} '{step['name']}' mismatch\n"
            f"  cmd  = {json.dumps(step['cmd'])}\n"
            f"  want = {json.dumps(want, sort_keys=True)}\n"
            f"  got  = {json.dumps(got, sort_keys=True)}"
        )

print(f"GOLDEN REPLAY OK — {len(manifest['steps'])} vectors, abi v{manifest['abi_version']}")
