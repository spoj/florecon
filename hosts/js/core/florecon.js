// Browser host for a florecon plugin wasm — the exact mirror of the Python host
// (_host.py): the packed-u64 ABI over linear memory, `describe` + `dispatch`,
// and the interactive Workspace verbs. The module has no imports; i64 returns
// arrive as BigInt. One wire protocol, two hosts.
import { tableToIPC, tableFromArrays, makeVector, vectorFromArray, Int64, Float64, Utf8 } from "apache-arrow";
export {
  strictAssignments,
  primaryAssignments,
  connectedComponents,
} from "./projections.js";

export const ABI_VERSION = 1;

const TD = new TextDecoder();
const TE = new TextEncoder();

export class PluginError extends Error {
  constructor(code, message, id, groupId) {
    super(message);
    this.name = "PluginError";
    this.code = code;
    this.id = id;
    this.groupId = groupId;
  }
}
export class ContractMismatch extends Error {}
export class SchemaError extends Error {}

function ok(env) {
  if (!env.ok) {
    const e = env.error || {};
    throw new PluginError(e.code || "unknown", e.message || "plugin error", e.id, e.group_id);
  }
  return env.report || {};
}

// --- low-level handle around the wasm instance ------------------------------
export class Florecon {
  static async load(source) {
    const bytes =
      source instanceof ArrayBuffer ? source : await (await fetch(source)).arrayBuffer();
    const { instance } = await WebAssembly.instantiate(bytes, {});
    return new Florecon(instance);
  }

  constructor(instance) {
    this.ex = instance.exports;
    this.mem = this.ex.memory;
    this.abiVersion = this.ex.abi_version();
    if (this.abiVersion !== ABI_VERSION)
      throw new ContractMismatch(`plugin ABI v${this.abiVersion} != host v${ABI_VERSION}`);
  }

  _readPacked(packed) {
    const ptr = Number(packed & 0xffffffffn);
    const len = Number((packed >> 32n) & 0xffffffffn);
    const out = new Uint8Array(this.mem.buffer, ptr, len).slice();
    this.ex.dealloc(ptr, len);
    return out;
  }

  describe() {
    return JSON.parse(TD.decode(this._readPacked(this.ex.describe())));
  }

  dispatch(command, arrowBytes) {
    const cmd = TE.encode(JSON.stringify(command));
    const cp = this.ex.alloc(cmd.length);
    new Uint8Array(this.mem.buffer).set(cmd, cp);
    let ap = 0;
    const an = arrowBytes ? arrowBytes.length : 0;
    if (an) {
      ap = this.ex.alloc(an);
      new Uint8Array(this.mem.buffer).set(arrowBytes, ap);
    }
    const packed = this.ex.dispatch(cp, cmd.length, ap, an);
    this.ex.dealloc(cp, cmd.length);
    if (an) this.ex.dealloc(ap, an);
    return JSON.parse(TD.decode(this._readPacked(packed)));
  }
}

// --- interactive session ----------------------------------------------------
export class Workspace {
  static async load(source, { config } = {}) {
    return new Workspace(await Florecon.load(source), { config });
  }

  constructor(fe, { config } = {}) {
    this.fe = fe;
    this.spec = fe.describe();
    this.fields = this.spec.input;
    this.domain = this.spec.domain || {};
    this.amountField = (this.fields.find((f) => f.amount) || {}).name || null;
    const init = { op: "init" };
    if (config) init.config = config;
    this.last = ok(this.fe.dispatch(init, this._ipc(this._emptyTable())));
  }

  // The declared column names, for ingest + validation.
  columns() {
    return this.fields.map((f) => ({ name: f.name, type: f.type, amount: !!f.amount }));
  }

  _emptyTable() {
    // A zero-row Arrow table carrying the declared schema (what `init` ships,
    // matching the Python host's empty batch so `Table::from_ipc` is happy).
    const cols = {};
    for (const f of this.fields) {
      if (f.type === "utf8") cols[f.name] = vectorFromArray([], new Utf8());
      else if (f.type === "i64") cols[f.name] = makeVector({ data: new BigInt64Array(0), type: new Int64() });
      else cols[f.name] = makeVector({ data: new Float64Array(0), type: new Float64() });
    }
    return tableFromArrays(cols);
  }

  _ipc(table) {
    return table ? tableToIPC(table, "stream") : null;
  }

  // Validate a built Arrow table against the declared schema before shipping.
  _validate(table) {
    const present = new Set(table.schema.fields.map((f) => f.name));
    const missing = this.fields.filter((f) => !present.has(f.name)).map((f) => f.name);
    if (missing.length)
      throw new SchemaError(
        `frame is missing columns declared by ${this.domain.id}: ${missing.join(", ")}`
      );
    return table;
  }

  upsert(table) {
    if (!table || table.numRows === 0) return this;
    this.last = this.fe.dispatch({ op: "upsert" }, this._ipc(this._validate(table)));
    ok(this.last);
    return this;
  }

  remove(...ids) {
    this.last = this.fe.dispatch({ op: "remove", ids });
    return ok(this.last), this;
  }

  solve() {
    this.last = this.fe.dispatch({ op: "solve" });
    return ok(this.last);
  }

  report() {
    return ok(this.fe.dispatch({ op: "report" }));
  }

  pin(groupId) {
    return ok((this.last = this.fe.dispatch({ op: "pin", by: "group", group_id: groupId })));
  }
  pinClean(tol = 0) {
    return ok((this.last = this.fe.dispatch({ op: "pin", by: "clean", tol })));
  }
  pinSingletons(ids) {
    return ok((this.last = this.fe.dispatch({ op: "pin", by: "singletons", ids })));
  }
  unpin(groupId) {
    return ok((this.last = this.fe.dispatch({ op: "unpin", group_id: groupId })));
  }

  merge(allocations, label = "manual", reason) {
    const cmd = {
      op: "merge",
      allocations: allocations.map((a) => ({ id: Number(a.id), amount: Number(a.amount) })),
      label,
    };
    if (reason != null) cmd.reason = String(reason);
    return ok((this.last = this.fe.dispatch(cmd)));
  }
  detach(groupId, ids) {
    return ok((this.last = this.fe.dispatch({ op: "detach", group_id: groupId, ids })));
  }
  dissolve(groupId) {
    return ok((this.last = this.fe.dispatch({ op: "dissolve", group_id: groupId })));
  }
}
