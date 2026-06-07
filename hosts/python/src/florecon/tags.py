"""Host-side review/attention overlay.

A *tag* is a many-to-many review axis (e.g. "needs-review", "escalate",
"FX-difference") that an operator drapes over rows. It is **orthogonal** to the
engine's lifecycle axis (``proposed`` | ``pinned``): the conservation engine
never learns the word "review". Tags are owned entirely by the host and keyed by
the **stable row id** (``ExtId``), never by group id — proposed group ids are
re-minted every solve, so keying by row id makes tags survive a re-solve for
free.

This mirrors the same orthogonality the engine enforces for groups, one layer
out: status is the machine's business, review is the operator's, and the two
never contaminate each other.
"""

# A small, stable chip palette; tags get a colour by creation order.
_PALETTE = [
    "#217346", "#6d3fd1", "#b7791f", "#0f8d80",
    "#c2410c", "#2563eb", "#be185d", "#4d7c0f",
]


class TagStore:
    """An in-memory overlay of ``row_id -> {tag_id}`` plus tag metadata.

    Nothing is persisted implicitly; serialize with :meth:`dump` (and embed it in
    a saved workspace) for durability. A row may carry several tags or none.
    """

    def __init__(self):
        self.tags: dict[int, set[str]] = {}
        self.meta: dict[str, dict] = {}

    def ensure_tag(self, label: str, kind: str = "bucket") -> str | None:
        """Create (or look up) a tag by human label. Idempotent on the slugged
        id, so reusing a bucket name reuses the same tag and colour."""
        name = (label or "").strip()
        if not name:
            return None
        tid = "tag:" + name.lower()
        if tid not in self.meta:
            self.meta[tid] = {
                "label": name,
                "color": _PALETTE[len(self.meta) % len(_PALETTE)],
                "kind": kind,
            }
        return tid

    def tags_of(self, row_id: int) -> set[str]:
        return self.tags.get(int(row_id), set())

    def label(self, tid: str) -> str:
        return self.meta.get(tid, {}).get("label", tid)

    def color(self, tid: str) -> str:
        return self.meta.get(tid, {}).get("color", "#6b7280")

    def add(self, row_id: int, tid: str) -> None:
        self.tags.setdefault(int(row_id), set()).add(tid)

    def remove(self, row_id: int, tid: str) -> None:
        s = self.tags.get(int(row_id))
        if not s:
            return
        s.discard(tid)
        if not s:
            del self.tags[int(row_id)]

    def clear(self, row_id: int) -> None:
        """Drop every tag on a row."""
        self.tags.pop(int(row_id), None)

    def tagged(self, tid: str) -> list[int]:
        """Every row id currently carrying ``tid`` (sorted)."""
        return sorted(r for r, s in self.tags.items() if tid in s)

    def dump(self) -> dict:
        """Serialize the whole overlay to a plain JSON-able object."""
        return {
            "tags": {str(r): sorted(s) for r, s in self.tags.items() if s},
            "meta": dict(self.meta),
        }

    def restore(self, obj: dict | None) -> "TagStore":
        """Replace the overlay from a :meth:`dump`. Row id keys are coerced back
        to ints. Returns self."""
        self.tags = {}
        self.meta = {}
        if not obj:
            return self
        for tid, m in (obj.get("meta") or {}).items():
            self.meta[tid] = m
        for r, arr in (obj.get("tags") or {}).items():
            try:
                key = int(r)
            except (TypeError, ValueError):
                key = r
            self.tags[key] = set(arr)
        return self
