# florecon — 2. reconcile: leaves + a runner
#
# Leaves are strategies that *match*: they compute id-clusters and call `split`.
# Every number is a closure over a row — no privileged numeraire, so multi-
# currency is just different closures. Run a strategy with `label(...)` to get
# the bag back with group / origin / reason columns (Excel-friendly).
#
# Excel / notebook: paste *after* the core cell (shared namespace).

try:                       # reuse the core cell's names if present (Excel)
    seq
except NameError:          # else import as a module (file usage)
    from core import *
import pandas as pd

# ── leaves ────────────────────────────────────────────────────────────────────
def exact_1to1(key, amount):
    """Pair equal-and-opposite entries (on `amount`) sharing a `key`."""
    def run(bag):
        matched = []
        for k, idx in bag.groupby(bag.apply(key, axis=1)).groups.items():
            if k is None:
                continue
            pos, neg = {}, {}
            for i in sorted(idx):
                a = amount(bag.loc[i])
                if a > 0:   pos.setdefault(a, []).append(i)
                elif a < 0: neg.setdefault(-a, []).append(i)
            for mag, p in pos.items():
                n = neg.get(mag, [])
                for j in range(min(len(p), len(n))):
                    matched.append([p[j], n[j]])
        return split(bag, matched, "exact_1to1")
    return run

def agg_net(key, accept):
    """Bucket by `key`; keep each bucket the `accept(member_frame)` closure passes."""
    def run(bag):
        matched = []
        for k, idx in bag.groupby(bag.apply(key, axis=1)).groups.items():
            if k is not None and accept(bag.loc[idx]):
                matched.append(list(idx))
        return split(bag, matched, "agg_net")
    return run

def subset_sum(amount, band=0, max_group=4):
    """Both-direction whole-lot clearing: the largest-magnitude lot anchors and
    draws a subset of opposite-sign lots summing within `band`. `band` prunes
    the search — gate precisely with a downstream `accept_if`."""
    def _subset(pool, target, picks):
        acc = []
        def dfs(start, picks, t):
            if abs(t) <= band and acc:
                return True
            if picks == 0 or t < -band:
                return False
            for i in range(start, len(pool)):
                pid, v = pool[i]
                if v > t + band:
                    continue
                acc.append(pid)
                if dfs(i + 1, picks - 1, t - v):
                    return True
                acc.pop()
            return False
        return acc[:] if dfs(0, picks, target) else None

    def run(bag):
        am = {i: amount(bag.loc[i]) for i in bag.index}
        used, matched = set(), []
        anchors = sorted((i for i in bag.index if am[i] != 0),
                         key=lambda i: (-abs(am[i]), i))
        for a in anchors:
            if a in used:
                continue
            want_pos = am[a] < 0
            pool = sorted(((i, abs(am[i])) for i in bag.index
                           if i not in used and i != a and am[i] != 0
                           and (am[i] > 0) == want_pos),
                          key=lambda t: (-t[1], t[0]))
            sub = _subset(pool, abs(am[a]), max_group - 1)
            if sub is not None:
                used.add(a); used.update(sub)
                matched.append([a] + sub)
        return split(bag, matched, "subset_sum")
    return run

# ── runner ────────────────────────────────────────────────────────────────────
def label(strategy, bag):
    """Run `strategy` and return `bag` with group / origin / reason columns."""
    groups, residual = strategy(bag)
    out = bag.copy()
    out["group"], out["origin"], out["reason"] = pd.NA, "residual", ""
    for gi, g in enumerate(groups):
        for m in g["members"]:
            out.loc[m, ["group", "origin", "reason"]] = (gi, g["origin"], " / ".join(g["reason"]))
    return out

# ── demo ──────────────────────────────────────────────────────────────────────
if __name__ == "__main__":
    bag = pd.DataFrame(
        {"amount": [100, -100, 30, 70, -100, 5], "account": [1, 1, 2, 2, 2, 1]},
        index=[1, 2, 3, 4, 5, 6],          # index = id
    )
    strategy = seq(
        exact_1to1(lambda r: r.account, lambda r: r.amount),          # clean pairs
        agg_net(lambda r: r.account, lambda m: abs(net(m, "amount")) <= 5),  # net-to-~0 buckets
        accept_if(lambda m: abs(net(m, "amount")) == 0,               # gate the search
                  subset_sum(lambda r: r.amount, band=0, max_group=4)),
    )
    print(label(strategy, bag))
