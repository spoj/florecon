"""The wasmtime host for a florecon plugin.

The host is generic and dumb: it loads a plugin ``.wasm``, asks it to
``describe()`` itself (which raw columns it needs, which is the numeraire), then
ships a columnar table as Arrow IPC and drives the planless ``Cmd`` protocol.
State lives inside the module; only JSON + Arrow cross the boundary.
"""

import json

import pyarrow as pa
import wasmtime

# Must equal the SDK's florecon::sdk::ABI_VERSION export.
ABI_VERSION = 1


class ContractMismatch(RuntimeError):
    """The plugin wasm speaks a different ABI than this host."""


_PA_TYPE = {"i64": pa.int64(), "f64": pa.float64(), "utf8": pa.string()}


class Florecon:
    """Low-level handle around a plugin wasm: alloc/write/call/read over linear
    memory, plus the ``describe`` and ``dispatch`` exports."""

    def __init__(self, wasm_path):
        engine = wasmtime.Engine()
        self.store = wasmtime.Store(engine)
        module = wasmtime.Module.from_file(engine, str(wasm_path))
        self.inst = wasmtime.Instance(self.store, module, [])
        ex = self.inst.exports(self.store)
        self.memory = ex["memory"]
        self._alloc = ex["alloc"]
        self._dealloc = ex["dealloc"]
        self._dispatch = ex["dispatch"]
        self._describe = ex["describe"]
        self.abi_version = ex["abi_version"](self.store)
        if self.abi_version != ABI_VERSION:
            raise ContractMismatch(f"plugin ABI v{self.abi_version} != host v{ABI_VERSION}")

    def _read_packed(self, packed: int) -> bytes:
        out_ptr = packed & 0xFFFFFFFF
        out_len = (packed >> 32) & 0xFFFFFFFF
        out = self.memory.read(self.store, out_ptr, out_ptr + out_len)
        self._dealloc(self.store, out_ptr, out_len)
        return bytes(out)

    def describe(self) -> dict:
        """The plugin's self-description: ``{abi_version, domain, input, ...}``."""
        return json.loads(self._read_packed(self._describe(self.store)))

    def dispatch(self, command: dict, arrow_bytes: bytes = None) -> dict:
        """Drive the persistent session with one ``Cmd`` dict. ``init``/``upsert``
        carry their rows in ``arrow_bytes``."""
        data = json.dumps(command).encode("utf-8")
        n = len(data)
        ptr = self._alloc(self.store, n)
        self.memory.write(self.store, data, ptr)

        arrow_n = len(arrow_bytes) if arrow_bytes else 0
        arrow_ptr = 0
        if arrow_n > 0:
            arrow_ptr = self._alloc(self.store, arrow_n)
            self.memory.write(self.store, arrow_bytes, arrow_ptr)

        packed = self._dispatch(self.store, ptr, n, arrow_ptr, arrow_n)

        self._dealloc(self.store, ptr, n)
        if arrow_n > 0:
            self._dealloc(self.store, arrow_ptr, arrow_n)
        return json.loads(self._read_packed(packed))


def _ok(env: dict, key: str = "report") -> dict:
    if not env.get("ok"):
        raise RuntimeError(env.get("error", "unknown plugin error"))
    return env.get(key, {})


def _ipc(batch: "pa.RecordBatch") -> bytes:
    sink = pa.BufferOutputStream()
    with pa.ipc.new_stream(sink, batch.schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue().to_pybytes()


class Workspace:
    """An interactive reconciliation session over one plugin.

    The plugin owns the domain (preprocessing, identity, matching). The host
    just ships the raw columns the plugin's ``describe()`` declares, as rows of
    ``{column: value}`` dicts, and drives solve / freeze / breakup. State lives
    in the wasm module, so repeated :meth:`solve` calls warm re-solve.
    """

    def __init__(self, wasm_path=None, _engine: Florecon = None):
        self.fe = _engine or Florecon(wasm_path)
        self.spec = self.fe.describe()
        self.fields = self.spec["input"]
        self.domain = self.spec.get("domain", {})
        self._schema = pa.schema(
            [pa.field(f["name"], _PA_TYPE[f["type"]]) for f in self.fields]
        )
        self.last = self.fe.dispatch({"op": "init"}, _ipc(self._batch([])))
        _ok(self.last)

    def _batch(self, rows) -> "pa.RecordBatch":
        """Build an Arrow batch of the declared columns from ``{col: value}``
        rows. A zero-row batch still carries the full schema."""
        arrays = []
        for f in self.fields:
            ty = _PA_TYPE[f["type"]]
            lane = [r.get(f["name"]) for r in rows]
            arrays.append(pa.array(lane, type=ty))
        return pa.RecordBatch.from_arrays(arrays, schema=self._schema)

    def upsert(self, *rows) -> "Workspace":
        """Insert/replace rows. Each row is a ``{column: value}`` dict using the
        plugin's declared raw column names."""
        rows = list(rows)
        if rows:
            self.last = self.fe.dispatch({"op": "upsert"}, _ipc(self._batch(rows)))
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

    def group(self, ids, net: int = 0, origin: str = "manual", reason=None) -> dict:
        cmd = {"op": "group", "ids": list(ids), "net": int(net), "origin": origin}
        if reason is not None:
            cmd["reason"] = str(reason)
        self.last = self.fe.dispatch(cmd)
        return _ok(self.last)

    def group_allocations(self, allocations, origin: str = "manual", reason=None) -> dict:
        cmd = {"op": "group_allocations", "allocations": list(allocations), "origin": origin}
        if reason is not None:
            cmd["reason"] = str(reason)
        self.last = self.fe.dispatch(cmd)
        return _ok(self.last)

    def remove_allocations(self, group_id: int, ids) -> dict:
        self.last = self.fe.dispatch(
            {"op": "remove_allocations", "group_id": group_id, "ids": list(ids)}
        )
        return _ok(self.last)

    def ungroup(self, ids) -> dict:
        self.last = self.fe.dispatch({"op": "ungroup", "ids": list(ids)})
        return _ok(self.last)

    def report(self) -> dict:
        return _ok(self.fe.dispatch({"op": "report"}))
