"""Thin wasmtime host for the florecon WASM consumption surface.

State lives in WASM linear memory; we cross the boundary only with a JSON
SolveRequest and read back a JSON envelope. See src/wasm.rs for the ABI.
"""

import json
import wasmtime

# The wire-contract version this host speaks; must equal the engine's
# abi_version() export. Bump in lockstep with plan::CONTRACT_VERSION.
CONTRACT_VERSION = 2


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


# --- interning at the boundary ---------------------------------------------
# Categorical columns and token signals lower to integers here, in the host,
# not in analyst code. Hand legible strings; get engine values plus a reverse
# dictionary so reports decode back to strings (no hand-built display map).

_FNV_OFFSET = 0xCBF29CE484222325
_FNV_PRIME = 0x100000001B3
_MASK64 = 0xFFFFFFFFFFFFFFFF
_I63 = 0x7FFFFFFFFFFFFFFF


def _fnv1a(s: str) -> int:
    h = _FNV_OFFSET
    for b in s.encode("utf-8"):
        h ^= b
        h = (h * _FNV_PRIME) & _MASK64
    return h


class Interner:
    """Deterministic string->i64 interning with a kept reverse dictionary."""

    def __init__(self):
        self._by_id = {}

    def code(self, s: str) -> int:
        s = s or ""
        h = _fnv1a(s) & _I63
        self._by_id[h] = s
        return h

    def cat(self, s: str) -> dict:
        return {"Int": self.code(s)}

    def pair(self, a: str, b: str) -> dict:
        lo, hi = sorted((a or "", b or ""))
        h = _fnv1a(f"{lo}|{hi}") & _I63
        self._by_id[h] = f"{lo}|{hi}"
        return {"Int": h}

    def tokens(self, texts, *, minlen: int = 6, maxlen: int = 40, drop=()) -> dict:
        drop = set(drop)
        out = []
        for field in texts:
            if not field:
                continue
            for raw in str(field).split():
                t = "".join(c for c in raw if c.isalnum()).upper()
                if len(t) < minlen or len(t) > maxlen or t in drop or t.isalpha():
                    continue
                h = _fnv1a(t)
                if h not in out:
                    out.append(h)
        return {"Tokens": out}

    def label(self, code: int):
        return self._by_id.get(code)
