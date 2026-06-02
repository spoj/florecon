"""Column-value helpers for building rows that cross into the engine."""


def Int(v: int) -> dict:
    return {"Int": int(v)}


def Tokens(ts) -> dict:
    return {"Tokens": [int(t) for t in ts]}


def row(*values) -> dict:
    """Wrap positional Value dicts into a Row payload."""
    return {"values": list(values)}
