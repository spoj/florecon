"""The wasmtime host for a florecon plugin.

The host is generic and dumb: it loads a plugin ``.wasm``, asks it to
``describe()`` itself (which raw columns it needs, which one is the headline
amount), then ships a columnar table as Arrow IPC and drives the planless
``Cmd`` protocol. State lives inside the module; only JSON + Arrow cross the
boundary. The host knows *nothing* about any domain — the same code runs every
florecon plugin.
"""

import json

import pyarrow as pa
import wasmtime

# Must equal the SDK's florecon::sdk::ABI_VERSION export.
ABI_VERSION = 1


class ContractMismatch(RuntimeError):
    """The plugin wasm speaks a different ABI than this host."""


class PluginError(RuntimeError):
    """A typed failure returned by the plugin's dispatch envelope.

    Carries the stable ``code`` (e.g. ``"unknown_group"``, ``"frozen_group"``,
    ``"conservation_violated"``) plus the row ``id`` / ``group_id`` it concerns
    when the plugin knows them.
    """

    def __init__(self, code: str, message: str, id=None, group_id=None):
        super().__init__(f"{code}: {message}")
        self.code = code
        self.id = id
        self.group_id = group_id


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
        carry their rows in ``arrow_bytes``. Returns the raw envelope."""
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


def _ok(env: dict) -> dict:
    """Unwrap a dispatch envelope to its report, or raise the typed error."""
    if not env.get("ok"):
        e = env.get("error") or {}
        raise PluginError(
            e.get("code", "unknown"),
            e.get("message", "unknown plugin error"),
            id=e.get("id"),
            group_id=e.get("group_id"),
        )
    return env.get("report", {})


def _ipc(batch: "pa.RecordBatch") -> bytes:
    sink = pa.BufferOutputStream()
    with pa.ipc.new_stream(sink, batch.schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue().to_pybytes()


class Workspace:
    """An interactive reconciliation session over one plugin.

    The plugin owns the domain (preprocessing, identity, matching). The host
    just ships the raw columns the plugin's ``describe()`` declares, as rows of
    ``{column: value}`` dicts, and drives the lifecycle (``pin``/``unpin``) and
    partition (``merge``/``detach``/``dissolve``) verbs. State lives in the wasm
    module, so repeated :meth:`solve` calls warm re-solve.

    A group lives on a lifecycle axis: ``proposed`` groups are the solver's
    current opinion (recomputed each :meth:`solve`); ``pinned`` groups are your
    decisions, kept verbatim across solves.
    """

    def __init__(self, wasm_path=None, _engine: Florecon = None):
        self.fe = _engine or Florecon(wasm_path)
        self.spec = self.fe.describe()
        self.fields = self.spec["input"]
        self.domain = self.spec.get("domain", {})
        #: Name of the column the plugin flags as the headline display amount.
        self.amount_field = next(
            (f["name"] for f in self.fields if f.get("amount")), None
        )
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

    # --- ledger --------------------------------------------------------------

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

    # --- machine -------------------------------------------------------------

    def solve(self) -> dict:
        self.last = self.fe.dispatch({"op": "solve"})
        return _ok(self.last)

    # --- lifecycle -----------------------------------------------------------

    def pin(self, group_id: int) -> dict:
        """Pin one proposed group — keep it verbatim across later solves."""
        self.last = self.fe.dispatch({"op": "pin", "by": "group", "group_id": group_id})
        return _ok(self.last)

    def pin_clean(self, tol: int = 0) -> dict:
        """Pin every proposed match that nets to zero within ``tol``."""
        self.last = self.fe.dispatch({"op": "pin", "by": "clean", "tol": int(tol)})
        return _ok(self.last)

    def pin_singletons(self, ids) -> dict:
        """Pin the named lone lots (proposed singletons) as accepted-as-is."""
        self.last = self.fe.dispatch({"op": "pin", "by": "singletons", "ids": list(ids)})
        return _ok(self.last)

    def unpin(self, group_id: int) -> dict:
        """Release a pinned group back to the solver's control."""
        self.last = self.fe.dispatch({"op": "unpin", "group_id": group_id})
        return _ok(self.last)

    # --- partition -----------------------------------------------------------

    def merge(self, allocations, label: str = "manual", reason=None) -> dict:
        """Assert a pinned group over exact allocations ``[{"id", "amount"}, ...]``."""
        cmd = {
            "op": "merge",
            "allocations": [
                {"id": int(a["id"]), "amount": int(a["amount"])} for a in allocations
            ],
            "label": label,
        }
        if reason is not None:
            cmd["reason"] = str(reason)
        self.last = self.fe.dispatch(cmd)
        return _ok(self.last)

    def detach(self, group_id: int, ids) -> dict:
        """Pull rows out of a proposed group into lone singletons."""
        self.last = self.fe.dispatch(
            {"op": "detach", "group_id": group_id, "ids": list(ids)}
        )
        return _ok(self.last)

    def dissolve(self, group_id: int) -> dict:
        """Break a proposed group back into singletons."""
        self.last = self.fe.dispatch({"op": "dissolve", "group_id": group_id})
        return _ok(self.last)

    # --- read ----------------------------------------------------------------

    def report(self) -> dict:
        return _ok(self.fe.dispatch({"op": "report"}))
