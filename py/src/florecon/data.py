"""Helpers for describing a typed schema and building bare rows.

A column's kind decides how its cells lower inside the engine, so the type lives
on the schema (set once), not on every cell. Cells are then bare scalars: a
number for ``number`` columns, a string for ``key``/``tokens`` columns.
"""

NUMBER = "number"  # a genuine integer: money (minor units), an epoch day
KEY = "key"        # a categorical string, lowered to one id
TOKENS = "tokens"  # free text, lowered to a set of reference-signal ids


def col(name: str, kind: str = NUMBER) -> dict:
    return {"name": name, "kind": kind}


def schema(cols, token_drop=()) -> dict:
    """A schema dict from ``(name, kind)`` pairs or ``col()`` dicts."""
    out = []
    for c in cols:
        out.append(c if isinstance(c, dict) else col(c[0], c[1]))
    s = {"cols": out}
    if token_drop:
        s["token_drop"] = list(token_drop)
    return s


def key(*parts, sort: bool = True, sep: str = "|") -> str:
    """Compose a composite key string for a ``key`` column (e.g. a bilateral
    company key). ``sort`` makes it order-independent; the engine then lowers the
    whole string like any other categorical. Composing the key is domain logic
    and lives here, not in the engine."""
    ps = ["" if p is None else str(p) for p in parts]
    if sort:
        ps = sorted(ps)
    return sep.join(ps)
