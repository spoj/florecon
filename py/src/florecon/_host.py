"""The wasmtime host: alloc/write/call/read over WASM linear memory, plus a
stateful Workspace wrapper. State lives in the module; only JSON + Arrow cross."""

import importlib.resources as resources
import json

import pyarrow as pa
import wasmtime

from .data import KEY, NUMBER, TOKENS, cat

# Must equal the engine's abi_version() export (plan::CONTRACT_VERSION).
CONTRACT_VERSION = 16


class ContractMismatch(RuntimeError):
    """The bundled WASM speaks a different wire contract than this host."""


def _load_module(engine: wasmtime.Engine, wasm_path):
    if wasm_path is None:
        data = resources.files("florecon").joinpath("_engine.wasm").read_bytes()
        return wasmtime.Module(engine, data)
    return wasmtime.Module.from_file(engine, str(wasm_path))


class Florecon:
    """Low-level handle around the WASM engine. The wire is a single concept:
    one :meth:`dispatch` entry point driving a persistent workspace via the
    ``Cmd`` protocol. A stateless batch solve is just ``init`` + ``solve`` over a
    workspace the caller discards (see :func:`florecon.api.solve`)."""

    def __init__(self, wasm_path=None):
        engine = wasmtime.Engine()
        self.store = wasmtime.Store(engine)
        self.inst = wasmtime.Instance(self.store, _load_module(engine, wasm_path), [])
        ex = self.inst.exports(self.store)
        self.memory = ex["memory"]
        self._alloc = ex["alloc"]
        self._dealloc = ex["dealloc"]
        self._dispatch = ex["dispatch"]
        self.engine_version = ex["abi_version"](self.store)
        if self.engine_version != CONTRACT_VERSION:
            raise ContractMismatch(
                f"wasm contract v{self.engine_version} != host v{CONTRACT_VERSION}"
            )

    def _call(self, fn, payload: dict, arrow_bytes: bytes = None) -> dict:
        data = json.dumps(payload).encode("utf-8")
        n = len(data)
        ptr = self._alloc(self.store, n)
        self.memory.write(self.store, data, ptr)
        
        arrow_n = len(arrow_bytes) if arrow_bytes else 0
        arrow_ptr = 0
        if arrow_n > 0:
            arrow_ptr = self._alloc(self.store, arrow_n)
            self.memory.write(self.store, arrow_bytes, arrow_ptr)
            
        packed = fn(self.store, ptr, n, arrow_ptr, arrow_n)
        
        self._dealloc(self.store, ptr, n)
        if arrow_n > 0:
            self._dealloc(self.store, arrow_ptr, arrow_n)

        out_ptr = packed & 0xFFFFFFFF
        out_len = (packed >> 32) & 0xFFFFFFFF
        out = self.memory.read(self.store, out_ptr, out_ptr + out_len)
        self._dealloc(self.store, out_ptr, out_len)
        return json.loads(bytes(out))

    def dispatch(self, command: dict, arrow_bytes: bytes = None) -> dict:
        """Drive the persistent workspace with one Cmd dict (the single wire
        entry point). ``init``/``upsert`` carry their rows in ``arrow_bytes``."""
        return self._call(self._dispatch, command, arrow_bytes)


def _ok(env: dict, key: str = "report") -> dict:
    if not env.get("ok"):
        raise RuntimeError(env.get("error", "unknown engine error"))
    return env.get(key, {})


# --- schema -> Arrow ---------------------------------------------------------
# The Arrow batch schema *is* the engine's column map, so the host builds typed
# Arrow from the kinds: NUMBER/KEY become int64 (KEY hashed via `cat`), TOKENS
# stays utf8 text for the engine to tokenize. Column 0 is always the uint64 id.
_ARROW_TYPE = {NUMBER: pa.int64(), KEY: pa.int64(), TOKENS: pa.string()}


def _cols(schema) -> list:
    """Normalize a schema (a ``{"cols": [...]}`` dict, or a bare iterable of
    ``col()`` dicts / ``(name, kind)`` pairs) to a list of ``{name, kind}``."""
    raw = schema["cols"] if isinstance(schema, dict) else list(schema)
    out = []
    for c in raw:
        out.append(c if isinstance(c, dict) else {"name": c[0], "kind": c[1]})
    return out


def _arrow_schema(cols) -> "pa.Schema":
    fields = [pa.field("id", pa.uint64())]
    for c in cols:
        fields.append(pa.field(c["name"], _ARROW_TYPE[c["kind"]]))
    return pa.schema(fields)


def _lower(kind, value):
    """Lower one bare cell to its Arrow value per the column kind."""
    if value is None:
        return None
    if kind == NUMBER:
        return int(value)
    if kind == KEY:
        return cat(str(value))
    if kind == TOKENS:
        return str(value)
    raise ValueError(f"unknown column kind: {kind!r}")


def _ipc(batch: "pa.RecordBatch") -> bytes:
    sink = pa.BufferOutputStream()
    with pa.ipc.new_stream(sink, batch.schema) as writer:
        writer.write_batch(batch)
    return sink.getvalue().to_pybytes()


def _batch(cols, rows) -> "pa.RecordBatch":
    """Build a RecordBatch from ``rows`` of ``(id, [cell, ...])``. A zero-row
    batch still carries the full schema, which is exactly what a schema-only
    init needs."""
    sch = _arrow_schema(cols)
    ids = [int(rid) for rid, _ in rows]
    arrays = [pa.array(ids, type=pa.uint64())]
    for j, c in enumerate(cols):
        lane = [_lower(c["kind"], values[j]) for _, values in rows]
        arrays.append(pa.array(lane, type=_ARROW_TYPE[c["kind"]]))
    return pa.RecordBatch.from_arrays(arrays, schema=sch)


class Workspace:
    """The interactive reconciliation workspace: edit rows, solve, then freeze
    what you trust and break up what you don't. Mirrors plan::Workspace.

    State lives inside the WASM module across calls, so repeated :meth:`solve`
    calls warm re-solve incrementally; rows stream in via :meth:`upsert`.
    """

    def __init__(self, schema, plan, primary=None, wasm_path=None, _engine: Florecon = None):
        """Open a workspace on ``schema``. ``plan`` is either a full
        ``{"primary": <col>, "root": <node>}`` dict or a bare root node with the
        primary amount column given via ``primary=``.

        The init crossing ships a *schema-only* (zero-row) Arrow batch so the
        engine derives its column map from the schema; rows then stream in via
        :meth:`upsert`.
        """
        self.fe = _engine or Florecon(wasm_path)
        self.cols = _cols(schema)
        if primary is not None:
            plan = {"primary": primary, "root": plan}
        if not (isinstance(plan, dict) and "primary" in plan and "root" in plan):
            raise ValueError(
                "plan must be {'primary': <col>, 'root': <node>} or pass primary="
            )
        self.plan = plan
        self.last = self.fe.dispatch(
            {"op": "init", "plan": plan}, _ipc(_batch(self.cols, []))
        )
        _ok(self.last)

    def upsert(self, id: int, values) -> "Workspace":
        """Insert/replace one row. ``values`` is a bare row: one scalar per
        schema column (int for ``number``, str for ``key``/``tokens``)."""
        return self.upsert_many([(id, values)])

    def upsert_many(self, rows) -> "Workspace":
        """Insert/replace many rows in one crossing. ``rows`` is an iterable of
        ``(id, [cell, ...])``. Far cheaper than per-row :meth:`upsert` framing."""
        rows = list(rows)
        if rows:
            self.last = self.fe.dispatch({"op": "upsert"}, _ipc(_batch(self.cols, rows)))
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

    def group(self, ids, net: int = 0, origin: str = "manual", reason=None) -> dict:
        """Manually assert a frozen group over all live allocation mass for `ids`.
        `net` is used only when no allocation amounts are known yet."""
        cmd = {"op": "group", "ids": list(ids), "net": int(net), "origin": origin}
        if reason is not None:
            cmd["reason"] = str(reason)
        self.last = self.fe.dispatch(cmd)
        return _ok(self.last)

    def group_allocations(self, allocations, origin: str = "manual", reason=None) -> dict:
        """Manually assert a frozen group over exact allocation amounts.
        `allocations` is an iterable of {"id": ..., "amount": ...}."""
        cmd = {"op": "group_allocations", "allocations": list(allocations), "origin": origin}
        if reason is not None:
            cmd["reason"] = str(reason)
        self.last = self.fe.dispatch(cmd)
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
