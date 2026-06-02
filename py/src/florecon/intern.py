"""Interning at the boundary.

Categorical columns and token signals cross into the engine as integers, but
that lowering is the host's job, not the analyst's. Hand a column its
business-legible strings; the interner returns the engine `Value` and keeps the
reverse dictionary, so reports decode back to strings without a separately
hand-maintained display map.

    it = Interner()
    rows.append(row(it.pair(co, icp), it.cat(ccy), Int(day),
                    it.cat(objsub), Int(native), it.tokens([ref, memo])))
    ...
    it.label(code)          # recover the original string for an interned id
"""

from .data import Int, Tokens, row  # noqa: F401  (re-exported for callers)

_FNV_OFFSET = 0xCBF29CE484222325
_FNV_PRIME = 0x100000001B3
_MASK64 = 0xFFFFFFFFFFFFFFFF
_I63 = 0x7FFFFFFFFFFFFFFF  # keep interned categories non-negative


def _fnv1a(s: str) -> int:
    h = _FNV_OFFSET
    for b in s.encode("utf-8"):
        h ^= b
        h = (h * _FNV_PRIME) & _MASK64
    return h


class Interner:
    """Deterministic string→i64 interning with a kept reverse dictionary.

    The hash is pure (fnv1a), so ids are stable across processes and shards
    without coordination; the reverse map exists only to decode reports for
    display. Categorical ids are masked non-negative so they read cleanly as
    partition keys.
    """

    def __init__(self):
        self._by_id: dict[int, str] = {}

    # --- categorical columns ------------------------------------------------
    def code(self, s: str) -> int:
        """Intern a string, returning the raw i64 id (no Value wrapper)."""
        s = s or ""
        h = _fnv1a(s) & _I63
        self._by_id[h] = s
        return h

    def cat(self, s: str) -> dict:
        """Intern a categorical string to an engine `Int` value."""
        return Int(self.code(s))

    def pair(self, a: str, b: str) -> dict:
        """Intern an unordered pair (e.g. a bilateral company key) to `Int`."""
        lo, hi = sorted((a or "", b or ""))
        s = f"{lo}|{hi}"
        h = _fnv1a(s) & _I63
        self._by_id[h] = s
        return Int(h)

    # --- token signals ------------------------------------------------------
    def tokens(self, texts, *, minlen: int = 6, maxlen: int = 40, drop=()) -> dict:
        """Extract alnum tokens from free-text fields and intern them to a
        `Tokens` value. Pure-alpha and out-of-band tokens are dropped."""
        drop = set(drop)
        out: list[int] = []
        for field in texts:
            if not field:
                continue
            for raw in str(field).split():
                t = "".join(c for c in raw if c.isalnum()).upper()
                if len(t) < minlen or len(t) > maxlen or t in drop or t.isalpha():
                    continue
                h = _fnv1a(t)
                if h not in out:
                    out.append(h)
        return Tokens(out)

    # --- reverse (the display dictionary) -----------------------------------
    def label(self, code: int):
        """Recover the original string for an interned id, or None."""
        return self._by_id.get(code)
