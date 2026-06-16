# florecon — 1. the monadic core (State + Writer)
#
# A Strategy is a pure function  bag -> (groups, residual)  — the same arrow as
# a parser combinator `str -> (out, str)`. The bag is a pandas DataFrame whose
# *index is the id*; payload lives in the columns. `groups` is a list of dicts
# {members, origin, reason}; the residual is the un-grouped sub-frame. The
# framework conserves identity: every id lands in exactly one group or residual.
#
# Excel / notebook: paste this cell first. The recon cell reuses these names
# from the shared namespace; as a file it `from core import *` instead.

import pandas as pd

# ── helper every leaf funnels through: split a bag into groups + residual ──────
def split(bag, matched, origin):
    """matched: list of id-lists. residual = bag minus matched (set complement)."""
    used = {i for ids in matched for i in ids}
    groups = [{"members": list(ids), "origin": origin, "reason": []} for ids in matched]
    residual = bag.drop(index=[i for i in used if i in bag.index])
    return groups, residual

# ── acceptance-closure conveniences (a GroupView over the member sub-frame) ───
def net(m, col):       return int(m[col].sum())            # signed residual
def gross(m, col):     return int(m[col].abs().sum())      # matched volume
def min_side(m, col):  return int(min((m[col] > 0).sum(), (m[col] < 0).sum()))

# ── combinators ───────────────────────────────────────────────────────────────
def identity():
    return lambda bag: ([], bag)

def seq(*steps):
    def run(bag):
        groups, residual = [], bag
        for s in steps:
            g, residual = s(residual)
            groups += g
        return groups, residual
    return run

def explain(note, inner):
    def run(bag):
        g, r = inner(bag)
        for grp in g:
            grp["reason"].append(note)   # innermost first, appended outward
        return g, r
    return run

def when(pred, inner):                    # pred: row -> bool
    def run(bag):
        if len(bag) == 0:
            return [], bag
        mask = bag.apply(pred, axis=1)
        g, r = inner(bag[mask])
        return g, pd.concat([r, bag[~mask]]) if (~mask).any() else r
    return run

def partition_by(key, factory):           # key: row -> hashable; factory: k -> strategy
    def run(bag):
        if len(bag) == 0:
            return [], bag
        groups, residual = [], []
        for k, idx in bag.groupby(bag.apply(key, axis=1)).groups.items():
            g, r = factory(k)(bag.loc[idx])
            groups += g
            residual.append(r)
        return groups, pd.concat(residual) if residual else bag.iloc[0:0]
    return run

def accept_if(pred, inner):               # pred: member-frame -> bool
    def run(bag):
        g, r = inner(bag)
        kept, rejected = [], []
        for grp in g:
            (kept if pred(bag.loc[grp["members"]]) else rejected).append(grp)
        extra = [i for grp in rejected for i in grp["members"]]
        return kept, (pd.concat([r, bag.loc[extra]]) if extra else r)
    return run

def soak(origin):                         # consume everything into one group
    def run(bag):
        if len(bag) == 0:
            return [], bag
        return [{"members": list(bag.index), "origin": origin, "reason": []}], bag.iloc[0:0]
    return run

def fixed_point(inner, max_passes=10):    # iterate on own residual to a fixpoint
    def run(bag):
        groups, residual, prev = [], bag, None
        for _ in range(max_passes):
            if len(residual) == 0:
                break
            g, residual = inner(residual)
            groups += g
            fp = tuple(sorted(residual.index))
            if fp == prev:
                break
            prev = fp
        return groups, residual
    return run
