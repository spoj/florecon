// Browser host for the florecon WASM module — a thin alloc/write/call/read
// wrapper over linear memory. The module has no imports; i64 returns arrive as
// BigInt.

export function strictAssignments(report) {
  const byId = new Map();
  for (const a of report.allocations || []) {
    if (!byId.has(a.id)) byId.set(a.id, new Set());
    byId.get(a.id).add(a.group_id);
  }
  const out = [];
  for (const [id, groups] of byId) {
    if (groups.size !== 1) {
      throw new Error(`row ${id} is split across groups ${[...groups].join(",")}`);
    }
    out.push([id, [...groups][0]]);
  }
  out.sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  return out;
}

export function primaryAssignments(report, policy = "largest_abs") {
  const byId = new Map();
  for (const a of report.allocations || []) {
    if (!byId.has(a.id)) byId.set(a.id, []);
    byId.get(a.id).push(a);
  }
  const groups = new Map((report.groups || []).map((g) => [g.group_id, g]));
  const score = (a) => {
    const g = groups.get(a.group_id) || {};
    if (policy === "first_group") return [-a.group_id];
    if (policy === "prefer_clean") return [Math.abs(g.net || 0) === 0 ? 1 : 0, Math.abs(a.amount || 0), -a.group_id];
    return [Math.abs(a.amount || 0), Math.abs(g.net || 0) === 0 ? 1 : 0, -a.group_id];
  };
  const better = (a, b) => {
    const sa = score(a), sb = score(b);
    for (let i = 0; i < Math.max(sa.length, sb.length); i++) {
      const da = sa[i] ?? 0, db = sb[i] ?? 0;
      if (da !== db) return da > db;
    }
    return false;
  };
  const out = [];
  for (const [id, as] of byId) {
    let best = as[0];
    for (const a of as.slice(1)) if (better(a, best)) best = a;
    out.push([id, best.group_id]);
  }
  out.sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  return out;
}

export function connectedComponents(report) {
  const rowToGroups = new Map();
  const groupToRows = new Map();
  for (const a of report.allocations || []) {
    if (!rowToGroups.has(a.id)) rowToGroups.set(a.id, []);
    rowToGroups.get(a.id).push(a.group_id);
    if (!groupToRows.has(a.group_id)) groupToRows.set(a.group_id, []);
    groupToRows.get(a.group_id).push(a.id);
  }
  const seenRows = new Set();
  const seenGroups = new Set();
  const out = [];
  for (const start of rowToGroups.keys()) {
    if (seenRows.has(start)) continue;
    const rows = new Set();
    const groups = new Set();
    const rowStack = [start];
    const groupStack = [];
    while (rowStack.length || groupStack.length) {
      while (rowStack.length) {
        const r = rowStack.pop();
        if (seenRows.has(r)) continue;
        seenRows.add(r); rows.add(r);
        for (const g of rowToGroups.get(r) || []) groupStack.push(g);
      }
      while (groupStack.length) {
        const g = groupStack.pop();
        if (seenGroups.has(g)) continue;
        seenGroups.add(g); groups.add(g);
        for (const r of groupToRows.get(g) || []) rowStack.push(r);
      }
    }
    out.push({ rows: [...rows].sort((a, b) => a - b), groups: [...groups].sort((a, b) => a - b) });
  }
  out.sort((a, b) => (a.rows[0] ?? 0) - (b.rows[0] ?? 0));
  return out;
}

export class Florecon {
  // The wire-contract version this host speaks; must equal the engine's
  // abi_version() export. Bump in lockstep with plan::CONTRACT_VERSION.
  static CONTRACT_VERSION = 10;

  static async load(url) {
    const bytes = await (await fetch(url)).arrayBuffer();
    const { instance } = await WebAssembly.instantiate(bytes, {});
    return new Florecon(instance);
  }

  constructor(instance) {
    this.ex = instance.exports;
    this.mem = this.ex.memory;
    this.enc = new TextEncoder();
    this.dec = new TextDecoder();
    const v = this.ex.abi_version();
    if (v !== Florecon.CONTRACT_VERSION) {
      throw new Error(
        `wasm contract v${v} != host v${Florecon.CONTRACT_VERSION}; ` +
          "rebuild the wasm or update this host",
      );
    }
    this.engineVersion = v;
  }

  _call(fn, payload, arrowBytes = null) {
    const data = this.enc.encode(JSON.stringify(payload));
    const n = data.length;
    const ptr = this.ex.alloc(n);
    new Uint8Array(this.mem.buffer, ptr, n).set(data);

    let arrowN = 0, arrowPtr = 0;
    if (arrowBytes && arrowBytes.length > 0) {
      arrowN = arrowBytes.length;
      arrowPtr = this.ex.alloc(arrowN);
      new Uint8Array(this.mem.buffer, arrowPtr, arrowN).set(arrowBytes);
    }

    const packed = fn(ptr, n, arrowPtr, arrowN);

    this.ex.dealloc(ptr, n);
    if (arrowN > 0) this.ex.dealloc(arrowPtr, arrowN);

    const outPtr = Number(packed & 0xffffffffn);
    const outLen = Number((packed >> 32n) & 0xffffffffn);
    const out = new Uint8Array(this.mem.buffer, outPtr, outLen).slice();
    this.ex.dealloc(outPtr, outLen);
    return JSON.parse(this.dec.decode(out));
  }

  // The single low-level entry point: one command (init/upsert/remove/solve/
  // freeze/...) against the persistent workspace. A stateless batch solve is
  // just `dispatch({op:"init", plan}, arrowBytes)` followed by
  // `dispatch({op:"solve"})`.
  dispatch(cmd, arrowBytes = null) {
    return this._call(this.ex.dispatch, cmd, arrowBytes);
  }
}
