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

// Rows and column identity cross the boundary as an Arrow IPC stream batch, not
// as JSON. Column 0 is the row id (UInt64); every other column is either an
// Int64 integer lane (money minor units, epoch day, or a host-hashed
// categorical key) or a Utf8 free-text column the engine tokenizes. The engine
// derives its column map from that schema, so no `schema`/`map` rides the wire.

// Plan selectors are plain column names (strings) resolved against the Arrow
// batch schema; there is no expression language on the wire. A Plan is one
// primary numeraire (the report/conservation column) plus a strategy root.
// Mirrors schema/plan.schema.json.

export type Cond = "token_shared" | "amount_equal";
/** Absolute slack, or relative (bps of a context scale) with an optional floor. */
export type Tol = number | { bps: number; floor?: number };
export interface CostTier {
  when: Cond[];
  base: number;
  slope?: number;
  amount_tol?: Tol | null;
}
export interface CostSpec { tiers: CostTier[]; }

export type PlanNode =
  | { op: "seq"; steps: PlanNode[] }
  | { op: "fixed_point"; inner: PlanNode; max?: number }
  | { op: "partition"; by: string; inner: PlanNode }
  | { op: "branch"; pred: string; and_then: PlanNode; or_else: PlanNode }
  | { op: "windowed"; order: string; width: number; inner: PlanNode }
  | { op: "pivot"; amount: string; inner: PlanNode }
  | { op: "filter"; keep: string | number | object; inner: PlanNode }
  | { op: "coalesce"; origin: string; inner: PlanNode }
  | { op: "trim"; tol: Tol; inner: PlanNode }
  | { op: "snap"; tol: Tol; inner: PlanNode }
  | { op: "soak_small"; max_bps?: number | null; max_abs?: number | null; origin: string; by?: string | null }
  | { op: "soak_all"; origin: string; by?: string | null }
  | { op: "agg_net"; key: string; tol: number }
  | { op: "exact" }
  | { op: "signal"; signals: string; tol: number; cap: number }
  | { op: "flow"; order_by: string; tokens: string; penalty: number; window: number; cost?: CostSpec };

export interface Plan {
  /** The report/conservation numeraire column; every primitive operates on it
   * unless a `pivot` subtree switches numeraire. */
  primary: string;
  root: PlanNode;
}

export type Cmd =
  | { op: "init"; plan: Plan }
  | { op: "upsert" }
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

  /** The single low-level entry point: drive the persistent workspace with one
   * command. `init`/`upsert` carry their rows in `arrowBytes`; column identity
   * is derived from the batch schema. A stateless batch solve is just an `init`
   * command followed by a `solve` command. */
  dispatch(command: Cmd, arrowBytes?: Uint8Array | null): Envelope;
}
