// Host-side review/attention overlay — the JS mirror of the Python `TagStore`
// (tags.py). A many-to-many review axis keyed by the stable row id, orthogonal
// to the engine's proposed|pinned lifecycle. The engine never learns "review".
const PALETTE = [
  "#217346", "#6d3fd1", "#b7791f", "#0f8d80",
  "#c2410c", "#2563eb", "#be185d", "#4d7c0f",
];

export class TagStore {
  constructor() {
    this.tags = new Map(); // rowId -> Set<tagId>
    this.meta = new Map(); // tagId -> {label, color, kind}
  }

  ensureTag(label, kind = "bucket") {
    const name = String(label ?? "").trim();
    if (!name) return null;
    const tid = "tag:" + name.toLowerCase();
    if (!this.meta.has(tid))
      this.meta.set(tid, { label: name, color: PALETTE[this.meta.size % PALETTE.length], kind });
    return tid;
  }

  tagsOf(rowId) {
    return this.tags.get(Number(rowId)) || new Set();
  }
  label(tid) {
    return (this.meta.get(tid) || {}).label || tid;
  }
  color(tid) {
    return (this.meta.get(tid) || {}).color || "#6b7280";
  }

  add(rowId, tid) {
    const r = Number(rowId);
    if (!this.tags.has(r)) this.tags.set(r, new Set());
    this.tags.get(r).add(tid);
  }
  remove(rowId, tid) {
    const s = this.tags.get(Number(rowId));
    if (!s) return;
    s.delete(tid);
    if (!s.size) this.tags.delete(Number(rowId));
  }
  clear(rowId) {
    this.tags.delete(Number(rowId));
  }
  tagged(tid) {
    const out = [];
    for (const [r, s] of this.tags) if (s.has(tid)) out.push(r);
    return out.sort((a, b) => a - b);
  }

  dump() {
    const tags = {};
    for (const [r, s] of this.tags) if (s.size) tags[r] = [...s].sort();
    return { tags, meta: Object.fromEntries(this.meta) };
  }
  restore(obj) {
    this.tags = new Map();
    this.meta = new Map();
    if (!obj) return this;
    for (const [tid, m] of Object.entries(obj.meta || {})) this.meta.set(tid, m);
    for (const [r, arr] of Object.entries(obj.tags || {})) this.tags.set(Number(r), new Set(arr));
    return this;
  }
}
