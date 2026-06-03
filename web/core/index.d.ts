// Type surface for @florecon/core. The wire shapes follow schema/plan.schema.json.

export type Status = "live" | "frozen";

export interface Group {
  group_id: number;
  origin: string;
  net: number;
  size: number;
  /** The single recalc-status axis: live (machine opinion) or frozen (operator
   * decision). Matched vs unmatched is arity (`size`), not status. */
  status: Status;
}

export interface AllocationOut {
  id: number;
  group_id: number;
  amount: number;
}

export interface AllocationSpec {
  id: number;
  amount: number;
}

export interface Report {
  groups: Group[];
  /** Allocation-native output: the same id may appear more than once when a row
   * is partly matched and partly left as a residual/variance lot. */
  allocations: AllocationOut[];
}

export interface Component {
  rows: number[];
  groups: number[];
}

export function strictAssignments(report: Report): [number, number][];
export function primaryAssignments(report: Report, policy?: "largest_abs" | "prefer_clean" | "first_group"): [number, number][];
export function connectedComponents(report: Report): Component[];

export interface Envelope {
  ok: boolean;
  error?: string | null;
  report?: Report | null;
}

/** A bare input cell: a number or a string. How it lowers is decided by its
 * column's `kind` in the schema, not by the cell. A `number` column takes the
 * integer as-is; a `key` column lowers a string to one id (a numeric cell is an
 * already-numeric key); a `tokens` column lowers free text to a signal set. */
export type Cell = number | string;
/** A bare positional row: one Cell per schema column. */
export type Row = Cell[];
export type IdRow = [number, Row];

/** How a column's cells lower. */
export type Kind = "number" | "key" | "tokens";
export interface Column {
  name: string;
  kind: Kind;
}
export interface Schema {
  cols: Column[];
  /** Stopwords for `tokens` lowering, matched upper-cased; optional. */
  token_drop?: string[];
}

export type ScalarRef = string | ScalarExpr;
export type ScalarExpr =
  | { col: string }
  | { lit: number }
  | { key: string }
  | { abs: ScalarRef }
  | { neg: ScalarRef }
  | { add: ScalarRef[] }
  | { sub: [ScalarRef, ScalarRef] };

export type BoolRef = string | BoolExpr;
export type BoolExpr =
  | { bool: boolean }
  | { not: BoolRef }
  | { and: BoolRef[] }
  | { or: BoolRef[] }
  | { eq: [ScalarRef, ScalarRef] }
  | { ne: [ScalarRef, ScalarRef] }
  | { gt: [ScalarRef, ScalarRef] }
  | { ge: [ScalarRef, ScalarRef] }
  | { lt: [ScalarRef, ScalarRef] }
  | { le: [ScalarRef, ScalarRef] };

export type Cond = "token_shared" | "amount_equal";
export interface CostTier {
  when: Cond[];
  base: number;
  day_slope?: number;
  max_day?: number | null;
}
export interface CostSpec { tiers: CostTier[]; }

export type Plan =
  | { op: "seq"; steps: Plan[] }
  | { op: "partition"; by: ScalarRef; inner: Plan }
  | { op: "branch"; pred: BoolRef; and_then: Plan; or_else: Plan }
  | { op: "windowed"; order: ScalarRef; width: number; inner: Plan }
  | { op: "lots"; amount: ScalarRef; inner: Plan }
  | { op: "soak_small"; max_bps?: number | null; max_abs?: number | null; origin: string; by?: ScalarRef | null }
  | { op: "soak_all"; origin: string; by?: ScalarRef | null }
  | { op: "agg_net"; key: ScalarRef; amount: ScalarRef; tol: number }
  | { op: "exact"; amount: ScalarRef }
  | { op: "signal"; signals: string; amount: ScalarRef; tol: number; cap: number }
  | { op: "flow"; amount: ScalarRef; day: ScalarRef; tokens: string; penalty: number; window: number; cost?: CostSpec };

export interface SolveRequest {
  schema: Schema;
  rows: IdRow[];
  plan: Plan;
}

export type Cmd =
  | { op: "init"; schema: Schema; plan: Plan; rows?: IdRow[] }
  | { op: "upsert"; rows: IdRow[] }
  | { op: "remove"; ids: number[] }
  | { op: "solve" }
  | { op: "freeze"; group_id: number }
  | { op: "freeze_clean"; tol: number }
  | { op: "freeze_singletons"; ids: number[] }
  | { op: "unfreeze"; group_id: number }
  | { op: "breakup"; group_id: number }
  | { op: "group"; ids: number[]; net?: number; origin?: string }
  | { op: "group_allocations"; allocations: AllocationSpec[]; origin?: string }
  | { op: "remove_allocations"; group_id: number; ids: number[] }
  | { op: "ungroup"; ids: number[] }
  | { op: "report" };

export class Florecon {
  /** The wire-contract version this host speaks. */
  static CONTRACT_VERSION: number;
  /** The contract version reported by the loaded engine. */
  engineVersion: number;

  /** Fetch + instantiate the WASM module at `url`. */
  static load(url: string): Promise<Florecon>;
  constructor(instance: WebAssembly.Instance);

  /** Stateless batch solve. */
  solve(request: SolveRequest): Envelope;
  /** Drive the stateful workspace with one command. */
  dispatch(command: Cmd): Envelope;
}
