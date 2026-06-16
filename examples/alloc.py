# florecon — 3. allocate: re-express a coarse cost on a finer basis (the dual)
#
# A Measure is a sparse quantity over named axes, held as a DataFrame of axis
# columns + an integer `v`. `allocate` splits a coarse cost across a driver,
# penny-exact (largest-remainder rounding). The one idea is ANY: a reserved
# coord meaning "unresolved on this axis" — every residual parks on ANY,
# in-cube, so nothing vanishes. Conserves value: `a.total() == coarse.total()`.
#
# Independent of the other cells; depends only on pandas.

import pandas as pd

ANY = "·"     # reserved catch-all coord: "unresolved on this axis"

class Measure:
    def __init__(self, df, axes):
        self.axes = list(axes)
        self.df = (df[self.axes + ["v"]].groupby(self.axes, as_index=False)["v"].sum()
                   if len(df) else pd.DataFrame(columns=self.axes + ["v"]))

    @staticmethod
    def build(axes, cells):
        """cells: list of (coord-dict, value)."""
        rows = [{**coords, "v": v} for coords, v in cells]
        return Measure(pd.DataFrame(rows, columns=list(axes) + ["v"]), axes)

    def total(self):
        return int(self.df["v"].sum())

    def get(self, coords):
        m = pd.Series(True, index=self.df.index)
        for a, c in coords.items():
            m &= self.df[a] == c
        return int(self.df.loc[m, "v"].sum())

    def pending(self):
        """The sub-cube still parked on ANY (unresolved on some axis)."""
        if len(self.df) == 0:
            return self
        mask = (self.df[self.axes] == ANY).any(axis=1)
        return Measure(self.df[mask], self.axes)

    def allocate(self, drv):
        """Split this coarse cost across `drv`, penny-exact; park where no driver."""
        shared = [a for a in self.axes if a in drv.axes]
        new    = [a for a in drv.axes if a not in self.axes]
        conly  = [a for a in self.axes if a not in drv.axes]
        out_axes = drv.axes + conly
        out = []
        for _, c in self.df.iterrows():
            amt = int(c["v"])
            d = drv.df
            for a in shared:
                d = d[d[a] == c[a]]
            wtotal = int(d["v"].sum())
            if wtotal <= 0:                                   # no driver -> park on ANY
                out.append({**{a: c[a] for a in self.axes},
                            **{a: ANY for a in new}, "v": amt})
                continue
            shares = _largest_remainder(amt, list(d["v"].astype(int)),
                                        wtotal, [tuple(r) for r in d[drv.axes].values])
            for (_, dr), q in zip(d.iterrows(), shares):
                if q:
                    out.append({**{a: dr[a] for a in drv.axes},
                                **{a: c[a] for a in conly}, "v": q})
        return Measure(pd.DataFrame(out, columns=out_axes + ["v"]), out_axes)

    def rekey(self, f):
        """Rewrite each cell's coords with f(coord-dict)->coord-dict, then re-sum."""
        rows = [{**f({a: r[a] for a in self.axes}), "v": int(r["v"])}
                for _, r in self.df.iterrows()]
        new_axes = list(rows[0].keys() - {"v"}) if rows else self.axes
        return Measure(pd.DataFrame(rows), new_axes)

def _largest_remainder(amt, weights, wtotal, keys):
    base = [amt * w // wtotal for w in weights]
    rem  = [amt * w %  wtotal for w in weights]
    deficit = amt - sum(base)
    for i in sorted(range(len(weights)), key=lambda i: (-rem[i], keys[i]))[:deficit]:
        base[i] += 1
    return base

# ── demo ──────────────────────────────────────────────────────────────────────
if __name__ == "__main__":
    rent = Measure.build(["geog", "time"], [
        ({"geog": 1, "time": 1}, 1000),
        ({"geog": 2, "time": 1}, 500),     # no driver here
    ])
    rev = Measure.build(["geog", "product", "time"], [
        ({"geog": 1, "product": 10, "time": 1}, 30),
        ({"geog": 1, "product": 11, "time": 1}, 70),
    ])
    a = rent.allocate(rev)
    print(a.df)
    assert a.total() == rent.total()                       # conserves value
    assert a.get({"geog": 1, "product": 10, "time": 1}) == 300
    assert a.pending().total() == 500                      # geog 2 parked on ANY
    print("ok:", a.total(), a.pending().total())
