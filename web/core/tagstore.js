// Host-side tag overlay: a review/attention axis orthogonal to the engine's
// recalc status (live|frozen), keyed by stable row id (ExtId). In-memory only.
//
// Tags are a many-to-many *review/attention* axis that is orthogonal to the
// engine's live|frozen partition. They are owned entirely by the host: the
// conservation engine never learns the word "staging". Tags are keyed by the
// stable row id (ExtId), never by group id — live-singleton group ids are
// ephemeral and re-minted every solve, so keying by row id makes tags survive
// recalc for free.
//
// Shape:
//   tags: Map<ExtId, Set<TagId>>          a row can carry several tags, or none
//   meta: Map<TagId, {label,color,kind}>  kind: "bucket" | "flag"
//
// State lives only for the life of the page: nothing is written to
// localStorage. Reloading the workbench starts from a clean, predictable slate.

const EMPTY = new Set();

// Stable-ish chip palette; tags get a colour by allocation order.
const PALETTE = [
  "#217346", "#6d3fd1", "#b7791f", "#0f8d80",
  "#c2410c", "#2563eb", "#be185d", "#4d7c0f",
];

export class TagStore {
  // `nsKey` is the dataset hash (host-derived, see datasetHash in app.js). It is
  // retained only for diagnostics; tags are never persisted across reloads.
  constructor(nsKey) {
    this.key = "florecon:tags:" + nsKey;
    this.tags = new Map(); // ExtId -> Set<TagId>
    this.meta = new Map(); // TagId -> {label,color,kind}
  }

  // Create (or look up) a tag by human label. Idempotent on the slugged id so
  // re-using the same bucket name re-uses the same tag.
  ensureTag(label, kind = "bucket") {
    const name = (label || "").trim();
    if (!name) return null;
    const tid = "tag:" + name.toLowerCase();
    if (!this.meta.has(tid))
      this.meta.set(tid, { label: name, color: PALETTE[this.meta.size % PALETTE.length], kind });
    return tid;
  }

  tagsOf(id) { return this.tags.get(id) || EMPTY; }
  label(tid) { return this.meta.get(tid)?.label ?? tid; }
  color(tid) { return this.meta.get(tid)?.color ?? "#6b7280"; }

  add(id, tid) {
    let s = this.tags.get(id);
    if (!s) { s = new Set(); this.tags.set(id, s); }
    s.add(tid);
  }

  remove(id, tid) {
    const s = this.tags.get(id);
    if (!s) return;
    s.delete(tid);
    if (!s.size) this.tags.delete(id);
  }

  // Drop every tag on a row (used by untag + the commit verbs).
  clear(id) {
    this.tags.delete(id);
  }

  // Serialize the whole overlay to a plain object (for workspace save).
  dump() {
    const tags = {};
    for (const [id, set] of this.tags) if (set.size) tags[id] = [...set];
    const meta = {};
    for (const [tid, m] of this.meta) meta[tid] = m;
    return { tags, meta };
  }

  // Replace the overlay from a dumped object (for workspace load). Row ids are
  // numeric ExtIds; coerce string keys back to numbers where possible.
  restore(o) {
    this.tags = new Map();
    this.meta = new Map();
    if (!o) return;
    for (const [tid, m] of Object.entries(o.meta || {})) this.meta.set(tid, m);
    for (const [id, arr] of Object.entries(o.tags || {})) {
      const n = Number(id);
      this.tags.set(Number.isNaN(n) ? id : n, new Set(arr));
    }
  }
}
