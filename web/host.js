// Generic browser host helpers for a florecon plugin.
//
// The host is dumb: it reads the plugin's describe() to learn which raw columns
// to ship, builds a columnar Arrow batch of exactly those columns, and drives
// the planless Cmd protocol. The plugin owns the domain (preprocessing,
// identity, matching). Pure and DOM-free, so it runs under node (see smoke.mjs).

import { Table, Utf8, Int64, Float64, vectorFromArray, tableToIPC } from "apache-arrow";

const _PA = { i64: "i64", f64: "f64", utf8: "utf8" };

// Build an Arrow IPC stream of the plugin's declared columns from rows of
// { column: value }. Returns null for an empty batch (init needs no rows).
// Uses explicit vector types so strings stay plain Utf8 (not a dictionary),
// which is what the plugin's decoder expects.
export function buildBatch(fields, rows) {
  if (!rows.length) return null;
  const cols = {};
  for (const f of fields) {
    const name = f.name;
    if (_PA[f.type] === "i64")
      cols[name] = vectorFromArray(
        BigInt64Array.from(rows.map((r) => BigInt(Math.trunc(Number(r[name] ?? 0))))),
        new Int64(),
      );
    else if (_PA[f.type] === "f64")
      cols[name] = vectorFromArray(Float64Array.from(rows.map((r) => Number(r[name] ?? 0))), new Float64());
    else cols[name] = vectorFromArray(rows.map((r) => (r[name] == null ? "" : String(r[name]))), new Utf8());
  }
  return tableToIPC(new Table(cols), "stream");
}

// An interactive reconciliation session over one plugin. Mirrors the Python
// Workspace: edit rows, solve, freeze what you trust, break up what you don't.
export class Session {
  constructor(fe) {
    this.fe = fe;
    this.spec = fe.describe();
    this.fields = this.spec.input;
    this.domain = this.spec.domain || {};
    this.last = this._ok(fe.dispatch({ op: "init" }));
  }

  _ok(env) {
    if (!env.ok) throw new Error(env.error || "unknown plugin error");
    return env.report || {};
  }

  upsert(...rows) {
    const batch = buildBatch(this.fields, rows);
    if (batch) this.last = this._ok(this.fe.dispatch({ op: "upsert" }, batch));
    return this;
  }
  remove(...ids) { return (this.last = this._ok(this.fe.dispatch({ op: "remove", ids }))); }
  solve() { return (this.last = this._ok(this.fe.dispatch({ op: "solve" }))); }
  freeze(groupId) { return (this.last = this._ok(this.fe.dispatch({ op: "freeze", group_id: groupId }))); }
  freezeClean(tol) { return (this.last = this._ok(this.fe.dispatch({ op: "freeze_clean", tol }))); }
  unfreeze(groupId) { return (this.last = this._ok(this.fe.dispatch({ op: "unfreeze", group_id: groupId }))); }
  breakup(groupId) { return (this.last = this._ok(this.fe.dispatch({ op: "breakup", group_id: groupId }))); }
  group(ids, { net = 0, origin = "manual", reason } = {}) {
    const cmd = { op: "group", ids, net, origin };
    if (reason != null) cmd.reason = String(reason);
    return (this.last = this._ok(this.fe.dispatch(cmd)));
  }
  report() { return this._ok(this.fe.dispatch({ op: "report" })); }
}
