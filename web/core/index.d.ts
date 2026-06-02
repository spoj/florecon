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

/** A column value: an integer or a list of pre-hashed reference tokens. */
export type Value = { Int: number } | { Tokens: number[] };
export interface Row {
  values: Value[];
}
export type IdRow = [number, Row];

/** A serializable Plan node (tagged union on `op`). See the JSON Schema. */
export type Plan = Record<string, unknown>;

export interface SolveRequest {
  schema: { cols: string[] };
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
