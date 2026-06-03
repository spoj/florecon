"""A Pythonic builder for the serializable Plan. 
This module allows Polars/Pandas expressions or simple callables inside the plan, 
which are resolved dynamically into virtual columns by `solve()`.
"""

def seq(*steps) -> dict:
    """Cascade: each step runs on the previous step's residual."""
    return {"op": "seq", "steps": list(steps)}

def partition(by, inner: dict) -> dict:
    """Fork/join: shard by an integer column, run `inner` per shard."""
    return {"op": "partition", "by": by, "inner": inner}

def windowed(order, width: int, inner: dict) -> dict:
    """Run `inner` within a sliding window over an integer order column."""
    return {"op": "windowed", "order": order, "width": width, "inner": inner}

def pivot(amount, inner: dict) -> dict:
    """Temporarily match inner in another numeraire."""
    return {"op": "pivot", "amount": amount, "inner": inner}

def soak_small(origin: str, max_bps: int = None, max_abs: int = None, by = None) -> dict:
    """Consume residual lots that are small versus their original line amount
    and/or an absolute threshold. If `by` is supplied, bucket by that class;
    otherwise emit singleton variance groups."""
    node = {"op": "soak_small", "origin": origin}
    if max_bps is not None:
        node["max_bps"] = int(max_bps)
    if max_abs is not None:
        node["max_abs"] = int(max_abs)
    if by is not None:
        node["by"] = by
    return node

def soak_all(origin: str = "unmatched", by = None) -> dict:
    """Consume all remaining residual lots into singleton or bucketed groups."""
    node = {"op": "soak_all", "origin": origin}
    if by is not None:
        node["by"] = by
    return node

def agg_net(key, tol: int = 0) -> dict:
    """Accept an aggregation bucket (`key`) that nets to zero within `tol`."""
    return {"op": "agg_net", "key": key, "tol": tol}

def exact() -> dict:
    """Pair opposite-sign rows of equal magnitude on current amount."""
    return {"op": "exact"}

def signal(signals, tol: int = 0, cap: int = 256) -> dict:
    """Group rows sharing an out-of-band token signal that net to zero."""
    return {"op": "signal", "signals": signals, "tol": tol, "cap": cap}

def flow(day, tokens, penalty: float = 1000.0, window: int = -1, cost: dict = None) -> dict:
    """The min-cost-flow arbiter over the residual."""
    node = {"op": "flow", "day": day, "tokens": tokens, "penalty": penalty, "window": window}
    if cost is not None:
        node["cost"] = cost
    return node

# --- flow cost model as data ------------------------------------------------
TOKEN_SHARED = "token_shared"
AMOUNT_EQUAL = "amount_equal"

def branch(pred, and_then: dict, or_else: dict) -> dict:
    return {"op": "branch", "pred": pred, "and_then": and_then, "or_else": or_else}

def tier(when, base: float, day_slope: float = 0.0, max_day: int = None) -> dict:
    t = {"when": list(when), "base": base, "day_slope": day_slope}
    if max_day is not None:
        t["max_day"] = max_day
    return t

def cost_spec(*tiers) -> dict:
    """An ordered list of confidence tiers; first satisfied tier wins, no tier
    means the pair is forbidden."""
    return {"tiers": list(tiers)}
