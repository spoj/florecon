# Data-prep keys: keep the solver scalar, fast, and conserving

The solver has exactly two contracts: **it conserves** (every input id's allocations
sum to its original amount) and **it is fast** (linear-ish, integer-only, no
callbacks into JS — the wasm module is instantiated with *no imports*). Anything
that can be precomputed as a column therefore belongs in **data prep** (the JS
ingest step), not in the solver.

The governing rule:

> **Keying is data prep. Netting is the solver.**
> If a concept can be expressed as "rows are comparable when `f(row)` is equal",
> compute `f` as a derived Int64 lane in JS and let the solver `partition`/`agg_net`
> on that one scalar. Reserve solver primitives for the things that genuinely need
> the *bag* at solve time: netting to zero, tolerance, ordering windows, the flow
> arbiter.

This keeps the solver's key type a single `i64`/`u64` (no composite keys, no
relational joins, no symmetric-key engine) — which is what makes it small and fast.

## The seam: `columns` + `derive`

`buildDataset({ header, rows, columns, plan, derive })` takes a typed `columns`
spec (each `{ ci, name, kind }`, kind one of `amount|number|date|key|text|
display`) and a `plan`. The `derive` array adds extra Int64 lanes, each
materialized **once** into the Arrow batch, that a `Sel` selector can reference
by name from any plan node:

```js
buildDataset({
  header, rows,
  columns: [
    { ci: 0, name: "company",  kind: "key" },
    { ci: 1, name: "icp",      kind: "key" },
    { ci: 4, name: "amount",   kind: "amount" },
    { ci: 5, name: "ref",      kind: "text" },
  ],
  derive: [
    { name: "copair", value: (row, display) => /* -> i64 */ },
    { name: "bucket", value: (row) => Math.floor(Number(row[4])) },
  ],
  // reference derived + typed lanes by name from the plan:
  plan: { primary: "amount", root: /* ... uses "copair", "bucket" ... */ },
});
```

Cost: `rows × lanes` integers in the batch — opt-in and bounded. The lanes are
ordinary engine columns; they can never perturb conservation, because the solver
only ever uses them as **selector/key inputs**, never as the conserved amount.
(On the setup screen the same thing is done by hand: add a `key` column and the
plan editor partitions on it.)

## Recipe 1 — reciprocity as a canonical sorted-pair key

Intercompany reciprocity (`a.company == b.icp && b.company == a.icp`) *looks*
relational, but it factors into two unary functions:

- **canonical co-pair key** = `sorted(company, icp)` — a single hashed lane;
- **orientation bit** = `company < icp ? 0 : 1` — a single lane.

A reciprocal pair is then "same co-pair key, opposite orientation". So:

```js
derive: [
  { name: "copair", value: (row) => {
      const a = row[CO], b = row[ICP];
      const k = a <= b ? `${a}|${b}` : `${b}|${a}`;   // canonical, order-free
      let h = 0; for (const c of k) h = (h * 31 + c.charCodeAt(0)) | 0;
      return h >>> 0;                                  // opaque equality key
  }},
  { name: "orient", value: (row) => (row[CO] <= row[ICP] ? 0 : 1) },
],
```

Plan side — just partition/net on the scalar:

```js
import { plan, seq, label, partition, aggNet, relTol, col } from "./core/plan.js";

plan("amount", seq(
  partition(col("copair"),
    label("intercompany net", aggNet(col("copair"), relTol(10, 1)))),
));
```

`copair` is a **hash**, so it is meaningful under equality only (never `<`/`>`).
That is exactly what `partition` and `agg_net` need.

### The one residual subtlety

A sorted-pair key buckets reciprocal *and* same-orientation rows together, so two
same-orientation rows that happen to sum to zero (a reversal, not an intercompany
match) can also net. Bucketing on the key cannot prevent this (putting `orient`
*in* the key would split genuine reciprocal pairs into different buckets). Two
options, in order of preference:

1. **Accept it** and filter post-hoc — these accidental nets are rare on real data.
2. If they show up, add the *one* genuinely solver-side guard: a bucket-acceptance
   rule "must contain ≥2 distinct values of the `orient` lane before it may net".
   (Not implemented yet; the `orient` lane above is what it would read.)

## Recipe 2 — composite bucket keys

Any tuple key (`company + icp + objsub + currency`, an amount bucket, a trace key
parsed from a memo) is the same move: fold the tuple to one Int64 in JS, partition
on it.

```js
derive: [
  { name: "gkey4", value: (row) => fnv(`${row[CO]}\u0001${row[ICP]}\u0001${row[OBJ]}\u0001${row[CCY]}`) },
  { name: "amt_bucket", value: (row) => Math.trunc(Number(row[AMT])) },  // 1-unit buckets
],
```

```js
partition(col("gkey4"), aggNet(col("gkey4"), relTol(10, 1)))
```

## Recipe 3 — shared-reference exact match

A 1:1 "exact" match keyed on a shared reference (`invoice_no`/`reference`/`doc_no`)
is `agg_net` with a 2-row bucket: derive the normalized reference key, partition on
it, net with tolerance.

```js
derive: [{ name: "refkey", value: (row) => fnv(normalize(row[REF] || row[INV] || row[DOC])) }],
// plan:
partition(col("refkey"), label("S3a exact", aggNet(col("refkey"), relTol(10, 1))))
```

(`exact` the primitive stays strict equal-magnitude pairing — it buckets by exact
magnitude, so it does **not** take a tolerance. Use `agg_net` with `relTol` when you
need slack.)

## What stays in the solver (and why)

| Concern | Where | Why |
|---|---|---|
| Canonical / composite / symmetric keys | **data prep** (`derive`) | precomputable; keeps solver key a scalar |
| Reciprocity, shared-ref, trace, amount buckets | **data prep** (`derive`) | all are equality keys |
| Netting to zero | solver (`agg_net`, `signal`, `running_zero`) | needs the bag |
| **Relative tolerance** | solver (`Tol::Rel { bps, floor }`) | `bps` of the bucket's smallest leg — a bag property |
| Ordering windows | solver (`windowed`, `running_zero`) | needs the sorted bag |
| Numeraire change | solver (`pivot`) | conserving projection |
| Global arbitration | solver (`flow`) | min-cost over the residual |
| Stage naming / "why matched" | solver (`label` → `reason`) | metadata channel on the group |

## Tolerance and labels (the two solver additions)

- **`relTol(bps, floor)`** on `aggNet` (and `tier(..., { amountBps })` on flow cost
  tiers): tolerance proportional to the bucket's smallest non-zero leg, never below
  `floor`. Stays integer-exact (`scale * bps / 10_000`), so conservation is untouched.
- **`label(tag, inner)`**: stamps `tag` onto every group `inner` produces, surfaced
  as `group.reason` in the report (distinct from the machine `origin`). Residual
  singletons are never labeled. This replaces ad-hoc encodings of "why did these
  match" — author the tag once per stage.
