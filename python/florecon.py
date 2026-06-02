"""Thin wasmtime host for the florecon WASM consumption surface.

State lives in WASM linear memory; we cross the boundary only with a JSON
SolveRequest and read back a JSON envelope. See src/wasm.rs for the ABI.
"""

import json
import wasmtime

# The wire-contract version this host speaks; must equal the engine's
# abi_version() export. Bump in lockstep with plan::CONTRACT_VERSION.
CONTRACT_VERSION = 1


class ContractMismatch(RuntimeError):
    pass


class Florecon:
    def __init__(self, wasm_path: str):
        engine = wasmtime.Engine()
        self.store = wasmtime.Store(engine)
        module = wasmtime.Module.from_file(engine, wasm_path)
        self.inst = wasmtime.Instance(self.store, module, [])
        ex = self.inst.exports(self.store)
        self.memory = ex["memory"]
        self._alloc = ex["alloc"]
        self._dealloc = ex["dealloc"]
        self._solve = ex["solve"]
        self._dispatch = ex["dispatch"]
        self.engine_version = ex["abi_version"](self.store)
        if self.engine_version != CONTRACT_VERSION:
            raise ContractMismatch(
                f"wasm contract v{self.engine_version} != host v{CONTRACT_VERSION}; "
                "rebuild the wasm or update this host"
            )

    def _call(self, fn, payload: dict) -> dict:
        data = json.dumps(payload).encode("utf-8")
        n = len(data)
        ptr = self._alloc(self.store, n)
        self.memory.write(self.store, data, ptr)
        packed = fn(self.store, ptr, n)
        self._dealloc(self.store, ptr, n)
        out_ptr = packed & 0xFFFFFFFF
        out_len = (packed >> 32) & 0xFFFFFFFF
        out = self.memory.read(self.store, out_ptr, out_ptr + out_len)
        self._dealloc(self.store, out_ptr, out_len)
        return json.loads(bytes(out))

    def solve(self, request: dict) -> dict:
        """Stateless batch solve."""
        return self._call(self._solve, request)

    def dispatch(self, command: dict) -> dict:
        """Stateful interactive command (init/upsert/remove/solve/freeze/...)."""
        return self._call(self._dispatch, command)
