"""Thin wasmtime host for the florecon WASM consumption surface.

State lives in WASM linear memory; we cross the boundary only with a JSON
SolveRequest and read back a JSON envelope. See src/wasm.rs for the ABI.
"""

import json
import wasmtime


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

    def solve(self, request: dict) -> dict:
        data = json.dumps(request).encode("utf-8")
        n = len(data)
        ptr = self._alloc(self.store, n)
        self.memory.write(self.store, data, ptr)
        packed = self._solve(self.store, ptr, n)
        self._dealloc(self.store, ptr, n)
        # packed = (len << 32) | ptr  (wasm32 pointers are 32-bit)
        out_ptr = packed & 0xFFFFFFFF
        out_len = (packed >> 32) & 0xFFFFFFFF
        out = self.memory.read(self.store, out_ptr, out_ptr + out_len)
        self._dealloc(self.store, out_ptr, out_len)
        return json.loads(bytes(out))
