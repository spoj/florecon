"""A Pythonic builder for the serializable Plan. Each function returns the JSON
dict the engine interprets; they nest exactly like the strategy combinators.

    plan.partition("unit", plan.partition("ccy", plan.seq(
        plan.agg_net("objsub", "native", tol=100),
        plan.exact("native"),
        plan.signal("tokens", "native", tol=100, cap=256),
        plan.flow("native", day="day", tokens="tokens"),
    )))
"""


def seq(*steps) -> dict:
    """Cascade: each step runs on the previous step's residual."""
    return {"op": "seq", "steps": list(steps)}


def partition(by: str, inner: dict) -> dict:
    """Fork/join: shard by an integer column, run `inner` per shard."""
    return {"op": "partition", "by": by, "inner": inner}


def windowed(order: str, width: int, inner: dict) -> dict:
    """Run `inner` within a sliding window over an integer order column."""
    return {"op": "windowed", "order": order, "width": width, "inner": inner}


def lots(amount: str, inner: dict) -> dict:
    """Enter lot mode: initialize each row's original/current residual amount
    from `amount`, then thread shrinking residuals through `inner`. Put this
    inside `partition(...)` when residuals must stay inside a hard scope."""
    return {"op": "lots", "amount": amount, "inner": inner}


def soak_small(origin: str, max_bps: int = None, max_abs: int = None, by: str = None) -> dict:
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


def soak_all(origin: str = "unmatched", by: str = None) -> dict:
    """Consume all remaining residual lots into singleton or bucketed groups."""
    node = {"op": "soak_all", "origin": origin}
    if by is not None:
        node["by"] = by
    return node


def agg_net(key: str, amount: str, tol: int = 0) -> dict:
    """Accept an aggregation bucket (`key`) that nets to zero within `tol`."""
    return {"op": "agg_net", "key": key, "amount": amount, "tol": tol}


def exact(amount: str) -> dict:
    """Pair opposite-sign rows of equal magnitude on `amount`."""
    return {"op": "exact", "amount": amount}


def signal(signals: str, amount: str, tol: int = 0, cap: int = 256) -> dict:
    """Group rows sharing an out-of-band token signal that net to zero."""
    return {"op": "signal", "signals": signals, "amount": amount, "tol": tol, "cap": cap}


def flow(
    amount,
    day,
    tokens: str,
    penalty: float = 1000.0,
    window: int = -1,
    cost: dict = None,
) -> dict:
    """The min-cost-flow arbiter over the residual. `amount` is the conserved
    numeraire and the exact-amount signal; `cost` defaults to the
    reference-bridge > exact-amount cascade. Pass `cost=cost_spec(...)` to
    override."""
    node = {
        "op": "flow",
        "amount": amount,
        "day": day,
        "tokens": tokens,
        "penalty": penalty,
        "window": window,
    }
    if cost is not None:
        node["cost"] = cost
    return node


# --- flow cost model as data ------------------------------------------------

TOKEN_SHARED = "token_shared"
AMOUNT_EQUAL = "amount_equal"


def col_ref(name: str) -> str:
    return name


def lit(value: int) -> dict:
    return {"lit": int(value)}


def key_lit(value: str) -> dict:
    return {"key": value}


def abs_(expr) -> dict:
    return {"abs": expr}


def neg(expr) -> dict:
    return {"neg": expr}


def add(*terms) -> dict:
    return {"add": list(terms)}


def sub(left, right) -> dict:
    return {"sub": [left, right]}


def eq(left, right) -> dict:
    return {"eq": [left, right]}


def ne(left, right) -> dict:
    return {"ne": [left, right]}


def gt(left, right) -> dict:
    return {"gt": [left, right]}


def ge(left, right) -> dict:
    return {"ge": [left, right]}


def lt(left, right) -> dict:
    return {"lt": [left, right]}


def le(left, right) -> dict:
    return {"le": [left, right]}


def and_(*preds) -> dict:
    return {"and": list(preds)}


def or_(*preds) -> dict:
    return {"or": list(preds)}


def not_(pred) -> dict:
    return {"not": pred}


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
