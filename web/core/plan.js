// Plan builder — a tiny, pure helper for authoring florecon plans (the strategy
// tree as data) and `Sel` selector expressions, without hand-writing JSON.
//
// Nothing here touches the engine; these functions just return the plain
// objects the wire expects, so `dispatch({op:"init", plan: plan(...)})` works.
//
// ── Selectors (`Sel`) ──────────────────────────────────────────────────────
// A selector is an integer-valued expression over a row's integer columns,
// used by `branch`/`partition`/`windowed`/`aggNet`/`pivot`. The wire accepts a
// bare string as a column reference and a bare number as a literal, so you can
// pass "amount" or 0 directly; the helpers below build the operator forms.
//
//   gt("amount", 0)                      // amount > 0           -> 1/0
//   and(gt("amount", 0), eq("account", 4000))
//   isIn("account", [4000, 5000])        // account in {…}       -> 1/0
//   iff(gt("amount", 0), "debit_col", "credit_col")
//   abs("native")                        // |native|
//
// Booleans are "non-zero"; comparisons/logic yield 1/0. Arithmetic wraps and
// division/modulo by zero yield 0 (selectors are total — they never throw).

export const col = (name) => ({ col: name });
export const lit = (n) => ({ lit: n });

const un = (op) => (a) => ({ [op]: a });
const bin = (op) => (a, b) => ({ [op]: [a, b] });

export const neg = un("neg");
export const abs = un("abs");
export const not = un("not");

export const add = bin("add");
export const sub = bin("sub");
export const mul = bin("mul");
export const div = bin("div");
export const mod = bin("mod");
export const min = bin("min");
export const max = bin("max");

export const eq = bin("eq");
export const ne = bin("ne");
export const lt = bin("lt");
export const le = bin("le");
export const gt = bin("gt");
export const ge = bin("ge");
export const and = bin("and");
export const or = bin("or");

export const isIn = (a, set) => ({ in: [a, set] });
export const iff = (cond, then, otherwise) => ({ if: [cond, then, otherwise] });

// -- Tolerance (`Tol`) -------------------------------------------------------
// A netting tolerance is either absolute (a bare number, the active numeraire's
// slack) or relative (`bps` basis points of the bucket's smallest non-zero
// leg, never below `floor`). The wire accepts a bare number as Abs, so the
// default `tol = 0` still works; `relTol` builds the proportional form.
//
//   aggNet("copair", relTol(10, 1))   // within 0.1% of the smaller side, min 1
export const relTol = (bps, floor = 0) => ({ bps, floor });

// ── Plan nodes ─────────────────────────────────────────────────────────────
export const seq = (...steps) => ({ op: "seq", steps });
// Stamp an author tag onto every group `inner` produces (the report `reason`),
// naming a stage without changing what forms the group.
export const label = (tag, inner) => ({ op: "label", tag, inner });
export const fixedPoint = (inner, max = 16) => ({ op: "fixed_point", inner, max });
export const partition = (by, inner) => ({ op: "partition", by, inner });
export const branch = (pred, andThen, orElse) => ({ op: "branch", pred, and_then: andThen, or_else: orElse });
export const windowed = (order, width, inner) => ({ op: "windowed", order, width, inner });
export const pivot = (amount, inner) => ({ op: "pivot", amount, inner });

// Group-metric lanes a `filter`/`acceptIf` `keep` selector reads. Unlike every
// other selector these name *group* metrics, not row columns:
//   SIZE      member count
//   POS, NEG  per-sign member counts
//   MIN_SIDE  min(POS, NEG) — the "smaller side"
//   MAX_SIDE  max(POS, NEG)
//   NET       signed group net;  ABS_NET its magnitude
//   MAX_ABS, MIN_ABS  largest / smallest member magnitude
export const SIZE = col("size"), POS = col("pos"), NEG = col("neg");
export const MIN_SIDE = col("min_side"), MAX_SIDE = col("max_side");
export const NET = col("net"), ABS_NET = col("abs_net");
export const MAX_ABS = col("max_abs"), MIN_ABS = col("min_abs");

// Gate `inner`'s output: keep only the groups for which the `keep` selector (a
// Sel over the group metrics above, non-zero = keep) holds; dissolve the rest
// back into the residual for a later stage.
//   filter(and(le(SIZE, 12), gt(MIN_SIDE, 2)), flow("day", "tokens"))
export const filter = (keep, inner) => ({ op: "filter", keep, inner });
// `acceptIf` reads more naturally for a keep-if-true predicate.
export const acceptIf = filter;

// Collapse `inner`'s allocation-hyperedge groups into connected-component
// clusters: groups sharing any member id merge into one coarse group (each
// row's allocations summed to one clean edge), uniformly stamped with `origin`.
// `minLink` gives up weak ties: a bridging allocation (a row shared by 2+
// groups) below this magnitude is cut and leaked back to the residual, so a
// cluster splits along an immaterial overlap. 0 (default) disables leaking.
// `absorb` walks the dual graph into the residual: a residual lot whose id
// already lives in a cluster (a partial row's tail) is folded into it.
//   coalesce("settlement", flow("day", "tokens"), { minLink: 100, absorb: true })
export const coalesce = (origin, inner, { minLink = 0, absorb = false } = {}) =>
  ({ op: "coalesce", origin, inner,
     ...(minLink ? { min_link: minLink } : {}),
     ...(absorb ? { absorb: true } : {}) });

// `tol` is absolute (a number) or relative (`relTol(bps, floor)`).
export const aggNet = (key, tol = 0) => ({ op: "agg_net", key, tol });
export const exact = () => ({ op: "exact" });
export const signal = (signals, { tol = 0, cap = 256 } = {}) => ({ op: "signal", signals, tol, cap });
export const soakSmall = (origin, { maxBps = null, maxAbs = null, by = null } = {}) =>
  ({ op: "soak_small", origin, max_bps: maxBps, max_abs: maxAbs, by });
export const soakAll = (origin, by = null) => ({ op: "soak_all", origin, by });
export const flow = (day, tokens, { penalty = 1000, window = -1, cost = null } = {}) =>
  ({ op: "flow", day, tokens, penalty, window, ...(cost ? { cost } : {}) });

// Flow cost model as ordered confidence tiers (first matching tier wins).
//   cost(tier(["token_shared","amount_equal"], 1.5), tier(["token_shared"], 2.0))
// `amountBps` relaxes this tier's `amount_equal` to a relative tolerance.
export const tier = (when, base, { daySlope = 0, maxDay = null, amountBps = null } = {}) =>
  ({ when, base, day_slope: daySlope, max_day: maxDay, ...(amountBps != null ? { amount_bps: amountBps } : {}) });
export const cost = (...tiers) => ({ tiers });

// A full plan: the conserved numeraire column + the root node.
export const plan = (primary, root) => ({ primary, root });
