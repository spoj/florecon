"""A Pythonic builder for the serializable Plan. Each function returns the JSON
dict the engine interprets; they nest exactly like the strategy combinators.

    plan.partition("unit", plan.partition("ccy", plan.seq(
        plan.agg_net("objsub", "native", tol=100),
        plan.exact("native"),
        plan.signal("tokens", "native", tol=100, cap=256),
        plan.flow("native", day="day", native="native", tokens="tokens"),
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
    amount: str,
    day: str,
    native: str,
    tokens: str,
    penalty: float = 1000.0,
    window: int = -1,
    cost: dict = None,
) -> dict:
    """The min-cost-flow arbiter over the residual. `cost` defaults to the
    reference-bridge > exact-amount cascade; override with `cost_spec`."""
    node = {
        "op": "flow",
        "amount": amount,
        "day": day,
        "native": native,
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


def tier(when, base: float, day_slope: float = 0.0, max_day: int = None) -> dict:
    t = {"when": list(when), "base": base, "day_slope": day_slope}
    if max_day is not None:
        t["max_day"] = max_day
    return t


def cost_spec(*tiers) -> dict:
    """An ordered list of confidence tiers; first satisfied tier wins, no tier
    means the pair is forbidden."""
    return {"tiers": list(tiers)}
