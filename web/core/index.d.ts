// Type surface for @florecon/core. The wire shapes follow schema/plan.schema.json.

export interface Report {
  assignments: [number, number][];
  groups: {
    group_id: number;
    origin: string;
    net: number;
    size: number;
    /** The single recalc-status axis: live (machine opinion) or frozen (operator
     * decision). Matched vs unmatched is arity (`size`), not status. */
    status: "live" | "frozen";
  }[];
}

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

/** A serializable Plan node (tagged union on `op`). See the JSON Schema. */
export type Plan = Record<string, unknown>;

export interface SolveRequest {
  schema: Schema;
  rows: IdRow[];
  plan: Plan;
}

/** Any interactive command: init/upsert/remove/solve/freeze/.../report. */
export type Cmd = Record<string, unknown>;

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
