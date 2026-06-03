from pathlib import Path
import re

t = Path('web/core/florecon.js').read_text()

call_body = '''  _call(fn, payload, arrowBytes = null) {
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
  }'''

t = re.sub(r'\s+_call\(fn, payload\) \{[\s\S]+?return JSON\.parse\(this\.dec\.decode\(out\)\);\n\s+\}', call_body, t)

t = t.replace('dispatch(cmd) {', 'dispatch(cmd, arrowBytes = null) {')
t = t.replace('return this._call(this.ex.dispatch, cmd);', 'return this._call(this.ex.dispatch, cmd, arrowBytes);')

t = t.replace('solve(req) {', 'solve(req, arrowBytes = null) {')
t = t.replace('return this._call(this.ex.solve, req);', 'return this._call(this.ex.solve, req, arrowBytes);')

Path('web/core/florecon.js').write_text(t)
