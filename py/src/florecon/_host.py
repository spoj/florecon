"""The wasmtime host: alloc/write/call/read over WASM linear memory, plus a
stateful Workspace wrapper. State lives in the module; only JSON crosses."""

import importlib.resources as resources
import json

import wasmtime

# Must equal the engine's abi_version() export (plan::CONTRACT_VERSION).
CONTRACT_VERSION = 8


class ContractMismatch(RuntimeError):
    """The bundled WASM speaks a different wire contract than this host."""


def _load_module(engine: wasmtime.Engine, wasm_path):
    if wasm_path is None:
        data = resources.files("florecon").joinpath("_engine.wasm").read_bytes()
        return wasmtime.Module(engine, data)
    return wasmtime.Module.from_file(engine, str(wasm_path))


class Florecon:
    """Low-level handle: stateless ``solve`` and stateful ``dispatch``."""

    def __init__(self, wasm_path=None):
        engine = wasmtime.Engine()
        self.store = wasmtime.Store(engine)
        self.inst = wasmtime.Instance(self.store, _load_module(engine, wasm_path), [])
        ex = self.inst.exports(self.store)
        self.memory = ex["memory"]
        self._alloc = ex["alloc"]
        self._dealloc = ex["dealloc"]
        self._solve = ex["solve"]
        self._dispatch = ex["dispatch"]
        self.engine_version = ex["abi_version"](self.store)
        if self.engine_version != CONTRACT_VERSION:
            raise ContractMismatch(
                f"wasm contract v{self.engine_version} != host v{CONTRACT_VERSION}"
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
        """Stateless batch solve of a SolveRequest dict."""
        return self._call(self._solve, request)

    def dispatch(self, command: dict) -> dict:
        """Drive the stateful workspace with one Cmd dict."""
        return self._call(self._dispatch, command)


def _ok(env: dict, key: str = "report") -> dict:
    if not env.get("ok"):
        raise RuntimeError(env.get("error", "unknown engine error"))
    return env.get(key, {})


class Workspace:
    """The interactive reconciliation workspace: edit rows, solve, then freeze
    what you trust and break up what you don't. Mirrors plan::Workspace."""

    def __init__(self, schema, pln, wasm_path=None, _engine: Florecon = None):
        self.fe = _engine or Florecon(wasm_path)
        # schema is a full dict ({"cols": [{name, kind}, ...], "token_drop": ...});
        # a bare list of columns is tolerated.
        self.schema = schema if isinstance(schema, dict) else {"cols": list(schema)}
        self.plan = pln
        self.last = self.fe.dispatch(
            {"op": "init", "schema": self.schema, "plan": pln, "rows": []}
        )
        _ok(self.last)

    def upsert(self, id: int, values) -> "Workspace":
        # values is a bare row: one scalar per column (int for number columns,
        # str for key/tokens columns).
        self.last = self.fe.dispatch({"op": "upsert", "rows": [[id, list(values)]]})
        _ok(self.last)
        return self

    def remove(self, *ids: int) -> "Workspace":
        self.last = self.fe.dispatch({"op": "remove", "ids": list(ids)})
        _ok(self.last)
        return self

    def solve(self) -> dict:
        self.last = self.fe.dispatch({"op": "solve"})
        return _ok(self.last)

    def freeze(self, group_id: int) -> dict:
        self.last = self.fe.dispatch({"op": "freeze", "group_id": group_id})
        return _ok(self.last)

    def freeze_singletons(self, ids) -> dict:
        """Freeze the live singleton groups holding `ids` (accepted unmatched
        exceptions) in one crossing — the "freeze N unmatched" path."""
        self.last = self.fe.dispatch({"op": "freeze_singletons", "ids": list(ids)})
        return _ok(self.last)

    def freeze_clean(self, tol: int) -> dict:
        self.last = self.fe.dispatch({"op": "freeze_clean", "tol": tol})
        return _ok(self.last)

    def unfreeze(self, group_id: int) -> dict:
        self.last = self.fe.dispatch({"op": "unfreeze", "group_id": group_id})
        return _ok(self.last)

    def breakup(self, group_id: int) -> dict:
        self.last = self.fe.dispatch({"op": "breakup", "group_id": group_id})
        return _ok(self.last)

    def group(self, ids, net: int = 0, origin: str = "manual") -> dict:
        """Manually assert a frozen group over all live allocation mass for `ids`.
        `net` is used only when no allocation amounts are known yet."""
        self.last = self.fe.dispatch(
            {"op": "group", "ids": list(ids), "net": int(net), "origin": origin}
        )
        return _ok(self.last)

    def group_allocations(self, allocations, origin: str = "manual") -> dict:
        """Manually assert a frozen group over exact allocation amounts.
        `allocations` is an iterable of {"id": ..., "amount": ...}."""
        self.last = self.fe.dispatch(
            {"op": "group_allocations", "allocations": list(allocations), "origin": origin}
        )
        return _ok(self.last)

    def remove_allocations(self, group_id: int, ids) -> dict:
        """Remove specific row allocations from one live group."""
        self.last = self.fe.dispatch(
            {"op": "remove_allocations", "group_id": group_id, "ids": list(ids)}
        )
        return _ok(self.last)

    def ungroup(self, ids) -> dict:
        """Send `ids` back to live singleton allocation groups, removing them
        from their live allocation groups."""
        self.last = self.fe.dispatch({"op": "ungroup", "ids": list(ids)})
        return _ok(self.last)

    def report(self) -> dict:
        return _ok(self.fe.dispatch({"op": "report"}))
