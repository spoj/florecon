"""Helpers for describing a typed schema and lowering its cells to Arrow.

A column's ``kind`` is a *host-side* construction hint: it tells this host how
to build the Arrow column the engine receives, not a schema the engine stores.
The engine derives column identity purely from the Arrow batch schema (column
names + Arrow types), so the kinds here steer how a cell becomes an Arrow value:

- ``NUMBER`` -> ``int64`` passthrough (money in minor units, an epoch day, ...).
- ``KEY``    -> ``int64`` via :func:`cat` (FNV-1a hash of the category string).
- ``TOKENS`` -> ``utf8`` raw text; the engine tokenizes and hashes it itself.

Cells are therefore bare scalars: a number for ``number`` columns, a string for
``key``/``tokens`` columns.
"""

NUMBER = "number"  # a genuine integer: money (minor units), an epoch day
KEY = "key"        # a categorical string, hashed host-side to one int64 id
TOKENS = "tokens"  # free text, sent raw for the engine to tokenize


def col(name: str, kind: str = NUMBER) -> dict:
    return {"name": name, "kind": kind}


def schema(cols) -> dict:
    """A schema dict from ``(name, kind)`` pairs or ``col()`` dicts."""
    out = []
    for c in cols:
        out.append(c if isinstance(c, dict) else col(c[0], c[1]))
    return {"cols": out}


def key(*parts, sort: bool = True, sep: str = "|") -> str:
    """Compose a composite key string for a ``key`` column (e.g. a bilateral
    company key). ``sort`` makes it order-independent; the column is then hashed
    like any other categorical. Composing the key is domain logic and lives
    here, not in the engine."""
    ps = ["" if p is None else str(p) for p in parts]
    if sort:
        ps = sorted(ps)
    return sep.join(ps)


# --- FNV-1a, matching Rust ``token::fnv1a`` / ``token::cat`` -----------------
# A ``KEY`` category must hash to the *same* int64 in every host so the same
# entity collides across books. This is the 64-bit FNV-1a the engine uses, with
# the resulting u64 reinterpreted as a two's-complement i64 (Rust ``as i64``).
_FNV_OFFSET = 0xCBF29CE484222325
_FNV_PRIME = 0x100000001B3
_U64 = (1 << 64) - 1


def fnv1a(data: bytes) -> int:
    """64-bit FNV-1a over raw bytes, returned as an unsigned int in [0, 2^64)."""
    h = _FNV_OFFSET
    for b in data:
        h ^= b
        h = (h * _FNV_PRIME) & _U64
    return h


def cat(s: str) -> int:
    """Hash a categorical string to a signed int64, matching Rust ``token::cat``
    (``fnv1a(bytes) as i64``). Stable across datasets, so the same category maps
    to the same id in every book."""
    h = fnv1a(s.encode("utf-8"))
    return h - (1 << 64) if h >= (1 << 63) else h
