"""A Pythonic builder for the serializable Plan.

Selectors (`by` / `key` / `pred` / `order` / `amount`) are authored as `Sel`
integer expressions over a row's integer columns — build them with the helpers
below (`col`, `lit`, `abs_`, `gt`, `is_in`, `iff`, ...). Sel is evaluated by the
engine itself, per row, on every (warm) solve, so it is the only selector form
that is serializable, portable across the Rust/Python/JS hosts, and recomputed
as rows stream in.

It has no host-side expression magic: a Polars/pandas expression or a Python
callable is *not* accepted inside a plan. If you need a derived quantity Sel
cannot express (string/date/float work, joins), compute it host-side as an
integer column and `upsert` it as an ordinary cell, then reference it by name in
the plan — that is data-prep, kept out of plan authoring on purpose.

Mirrors the JS builder (web/core/plan.js): the same plan nodes, the `label`
reason combinator, relative tolerance (`rel_tol`), and the `Sel` helper set.
Note: `plan.col` is a *Sel column reference* ({"col": name}); the top-level
`florecon.col` is the unrelated *schema* column declaration.
"""

# ── Selectors (`Sel`) ───────────────────────────────────────────────────────
# An integer-valued expression over a row's integer columns, used by the
# combinators below. The wire accepts a bare string as a column reference and a
# bare number as a literal, so `"amount"` and `0` work directly; these helpers
# build the operator forms. Booleans are non-zero; comparisons/logic yield 1/0;
# selectors are total (div/mod by zero -> 0, arithmetic wraps).
def col(name: str) -> dict:
    """A Sel column reference, e.g. ``col("amount")``."""
    return {"col": name}

def lit(n: int) -> dict:
    """A Sel integer literal."""
    return {"lit": int(n)}

def _un(op):
    return lambda a: {op: a}

def _bin(op):
    return lambda a, b: {op: [a, b]}

neg = _un("neg")
abs_ = _un("abs")
not_ = _un("not")

add = _bin("add")
sub = _bin("sub")
mul = _bin("mul")
div = _bin("div")
mod_ = _bin("mod")
min_ = _bin("min")
max_ = _bin("max")
eq = _bin("eq")
ne = _bin("ne")
lt = _bin("lt")
le = _bin("le")
gt = _bin("gt")
ge = _bin("ge")
and_ = _bin("and")
or_ = _bin("or")

def is_in(a, members) -> dict:
    """``a in {members}`` -> 1/0."""
    return {"in": [a, list(members)]}

def iff(cond, then, otherwise) -> dict:
    """``cond ? then : otherwise`` (cond is non-zero)."""
    return {"if": [cond, then, otherwise]}

# ── Tolerance (`Tol`) ───────────────────────────────────────────────────────
def rel_tol(bps: int, floor: int = 0) -> dict:
    """Relative netting tolerance: ``bps`` basis points of the bucket's smallest
    non-zero leg, never below ``floor``. Pass to ``agg_net(tol=...)``. A bare int
    tolerance is still absolute slack in the numeraire."""
    return {"bps": int(bps), "floor": int(floor)}

# ── Plan nodes ──────────────────────────────────────────────────────────────
def seq(*steps) -> dict:
    """Cascade: each step runs on the previous step's residual."""
    return {"op": "seq", "steps": list(steps)}

def fixed_point(inner: dict, max: int = 16) -> dict:
    """Repeat ``inner`` on its own residual until it reaches a fixed point (a
    pass that groups nothing more) or ``max`` passes elapse, accumulating every
    group found along the way. State inside ``inner`` (e.g. a warm flow matcher)
    persists across passes; the loop is reentrant-safe because each node treats
    its incoming bag as the authoritative present-set."""
    return {"op": "fixed_point", "inner": inner, "max": int(max)}

def partition(by, inner: dict) -> dict:
    """Fork/join: shard by an integer column, run `inner` per shard."""
    return {"op": "partition", "by": by, "inner": inner}

def windowed(order, width: int, inner: dict) -> dict:
    """Run `inner` within a sliding window over an integer order column."""
    return {"op": "windowed", "order": order, "width": width, "inner": inner}

def pivot(amount, inner: dict) -> dict:
    """Temporarily match inner in another numeraire."""
    return {"op": "pivot", "amount": amount, "inner": inner}

# Group-metric lanes a `filter` `keep` selector reads (Sel column references).
# Unlike every other selector these are *group* metrics, not row columns:
#   SIZE      member count
#   POS, NEG  per-sign member counts
#   MIN_SIDE  min(POS, NEG) -- the "smaller side"
#   MAX_SIDE  max(POS, NEG)
#   NET       signed group net; ABS_NET its magnitude
#   MAX_ABS, MIN_ABS  largest / smallest member magnitude
SIZE, POS, NEG = col("size"), col("pos"), col("neg")
MIN_SIDE, MAX_SIDE = col("min_side"), col("max_side")
NET, ABS_NET = col("net"), col("abs_net")
MAX_ABS, MIN_ABS = col("max_abs"), col("min_abs")

def filter(keep, inner: dict) -> dict:
    """Gate ``inner``'s output: keep only the groups for which the ``keep``
    selector evaluates non-zero, dissolving every rejected group back into the
    residual for a later stage. ``keep`` is a Sel over *group metrics* (``SIZE``,
    ``MIN_SIDE``, ``ABS_NET``, ...), e.g.::

        filter(and_(le(SIZE, 12), gt(MIN_SIDE, 2)), flow(...))
    """
    return {"op": "filter", "keep": keep, "inner": inner}

# `accept_if` reads more naturally for a keep-if-true predicate.
accept_if = filter

def coalesce(origin: str, inner: dict) -> dict:
    """Collapse ``inner``'s allocation-hyperedge groups into connected-component
    clusters: groups sharing any member id merge into one coarse group (each
    row's allocations summed to a single clean edge), uniformly stamped with
    ``origin``. Turns the matcher's interlocking partial-allocation view into the
    settlement-cluster view a human actions against.

    Pure group->group transform: the residual is never touched. Compose with
    ``trim`` / ``snap`` to move material to/from the residual.
    """
    return {"op": "coalesce", "origin": origin, "inner": inner}

def trim(tol, inner: dict) -> dict:
    """Cut every group edge within ``tol`` (of its row's ``original``) to the
    residual — matched -> residual, one-directional. ``tol`` is an absolute int
    or ``rel_tol(bps, floor)``. Trimming a bridging edge splits a cluster, so
    ``trim`` before ``coalesce`` yields smaller islands and more residual."""
    return {"op": "trim", "tol": tol, "inner": inner}

def snap(tol, inner: dict) -> dict:
    """Fold every sub-``tol`` edge onto its row's dominant edge instead of the
    floor. The residual edge is eligible both ways, so a small tail absorbs into
    its group while a small match leaks to residual — whichever side is the
    minority. The dominant edge never folds into itself, so a clean match is
    never silently un-matched. ``tol`` is an absolute int or ``rel_tol(bps,
    floor)``."""
    return {"op": "snap", "tol": tol, "inner": inner}

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

def agg_net(key, tol=0) -> dict:
    """Accept an aggregation bucket (`key`) that nets to zero within `tol`.
    `tol` is an absolute int (numeraire slack) or ``rel_tol(bps, floor)``."""
    return {"op": "agg_net", "key": key, "tol": tol}

def exact() -> dict:
    """Pair opposite-sign rows of equal magnitude on current amount."""
    return {"op": "exact"}

def signal(signals, tol=0, cap: int = 256) -> dict:
    """Group rows sharing an out-of-band token signal that net to zero within
    ``tol``. ``tol`` is an absolute int (numeraire slack) or ``rel_tol(bps,
    floor)`` (relative to the bucket's smallest non-zero leg)."""
    return {"op": "signal", "signals": signals, "tol": tol, "cap": cap}

def flow(order_by, tokens, penalty: float = 1000.0, window: int = -1, cost: dict = None) -> dict:
    """The min-cost-flow arbiter over the residual. ``order_by`` is a 1-D
    ordering expression; ``window`` is the proximity radius in those units (the
    trust bound for weak candidates). Flow is domain-agnostic — it knows an
    ordering and a window, not "days"."""
    node = {"op": "flow", "order_by": order_by, "tokens": tokens, "penalty": penalty, "window": window}
    if cost is not None:
        node["cost"] = cost
    return node

# --- flow cost model as data ------------------------------------------------
TOKEN_SHARED = "token_shared"
AMOUNT_EQUAL = "amount_equal"

def branch(pred, and_then: dict, or_else: dict) -> dict:
    """Route rows where ``pred`` is non-zero to ``and_then``, the rest to
    ``or_else``. ``pred`` is a Sel (e.g. ``ge(abs_(col("amount")), 100000)``)."""
    return {"op": "branch", "pred": pred, "and_then": and_then, "or_else": or_else}

def label(tag: str, inner: dict) -> dict:
    """Stamp an author tag onto every group ``inner`` produces (the report
    ``reason``), naming a stage without changing what forms the group."""
    return {"op": "label", "tag": tag, "inner": inner}

def tier(when, base: float, slope: float = 0.0, amount_tol=None) -> dict:
    """One confidence tier: cost is ``base + slope * |Δorder_by|``. ``amount_tol``
    relaxes ``AMOUNT_EQUAL`` against the smaller leg (absolute int or
    ``rel_tol(bps, floor)``); ``None`` keeps strict equality."""
    t = {"when": list(when), "base": base, "slope": slope}
    if amount_tol is not None:
        t["amount_tol"] = amount_tol
    return t

def cost_spec(*tiers) -> dict:
    """An ordered list of confidence tiers; first satisfied tier wins, no tier
    means the pair is forbidden."""
    return {"tiers": list(tiers)}
