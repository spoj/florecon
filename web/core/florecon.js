// Browser host for the florecon WASM module — mirror of python/florecon.py.
// The module has no imports; i64 returns arrive as BigInt.

export class Florecon {
  // The wire-contract version this host speaks; must equal the engine's
  // abi_version() export. Bump in lockstep with plan::CONTRACT_VERSION.
  static CONTRACT_VERSION = 2;

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

  _call(fn, payload) {
    const data = this.enc.encode(JSON.stringify(payload));
    const n = data.length;
    const ptr = this.ex.alloc(n);
    // Re-read buffer after alloc: growth may have detached the old one.
    new Uint8Array(this.mem.buffer, ptr, n).set(data);
    const packed = fn(ptr, n); // BigInt: (len << 32) | ptr
    this.ex.dealloc(ptr, n);
    const outPtr = Number(packed & 0xffffffffn);
    const outLen = Number((packed >> 32n) & 0xffffffffn);
    const out = new Uint8Array(this.mem.buffer, outPtr, outLen).slice();
    this.ex.dealloc(outPtr, outLen);
    return JSON.parse(this.dec.decode(out));
  }

  // Stateful interactive command (init/upsert/remove/solve/freeze/...).
  dispatch(cmd) {
    return this._call(this.ex.dispatch, cmd);
  }

  // Stateless batch solve.
  solve(req) {
    return this._call(this.ex.solve, req);
  }
}
