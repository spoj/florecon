import pyarrow as pa
from ._host import Florecon

# Optional polars import
try:
    import polars as pl
except ImportError:
    pl = None

def _is_polars_expr(v):
    return pl is not None and type(v).__name__ == "Expr"

def _walk_plan(node, ctx):
    if isinstance(node, dict):
        new_node = {}
        for k, v in node.items():
            if callable(v) or _is_polars_expr(v):
                name = f"__vcol_{len(ctx['exprs'])}"
                ctx['exprs'][name] = v
                new_node[k] = name
            else:
                new_node[k] = _walk_plan(v, ctx)
        return new_node
    elif isinstance(node, list):
        return [_walk_plan(x, ctx) for x in node]
    else:
        return node

def solve(df, plan_def: dict, primary_amount, wasm_path=None) -> dict:
    """Evaluate dynamic expressions, compile to Arrow, and solve via WASM."""
    ctx = {'exprs': {}}
    
    # Process primary amount
    if callable(primary_amount) or _is_polars_expr(primary_amount):
        primary_name = "__vcol_primary"
        ctx['exprs'][primary_name] = primary_amount
    else:
        primary_name = primary_amount

    # Walk plan and extract expressions
    root_plan = _walk_plan(plan_def, ctx)
    
    final_plan = {
        "primary": primary_name,
        "root": root_plan
    }
    
    # If Polars DataFrame or LazyFrame
    if pl is not None and (isinstance(df, pl.DataFrame) or isinstance(df, pl.LazyFrame)):
        exprs = []
        for name, expr in ctx['exprs'].items():
            if _is_polars_expr(expr):
                exprs.append(expr.alias(name))
            else:
                raise ValueError("Polars DataFrames require Polars Exprs, not Callables")
        if exprs:
            df = df.with_columns(exprs)
            
        if isinstance(df, pl.LazyFrame):
            df = df.collect()
        
        # Cast to int64 or utf8
        cast_exprs = []
        for c in df.columns:
            if df.schema[c] in (pl.String, pl.Categorical):
                cast_exprs.append(pl.col(c).cast(pl.String))
            elif df.schema[c] == pl.Boolean:
                cast_exprs.append(pl.col(c).cast(pl.Int64))
            else:
                cast_exprs.append(pl.col(c).cast(pl.Int64))
        df = df.with_columns(cast_exprs)
        
        arrow_table = df.to_arrow()
        
    else:
        # Assume Pandas or Dictionary list for Callables
        import pandas as pd
        if isinstance(df, pd.DataFrame):
            for name, func in ctx['exprs'].items():
                if callable(func):
                    df[name] = df.apply(func, axis=1)
            arrow_table = pa.Table.from_pandas(df)
        else:
            raise TypeError("Unsupported DataFrame type. Use Polars or Pandas.")

    # Get Arrow IPC stream
    sink = pa.BufferOutputStream()
    with pa.ipc.new_stream(sink, arrow_table.schema) as writer:
        writer.write_table(arrow_table)
    arrow_bytes = sink.getvalue().to_pybytes()

    # The Arrow batch schema *is* the engine's column map: int64 columns are
    # integer lanes, utf8 columns are free-text reference lanes the engine
    # tokenizes. No separate map crosses the wire.
    #
    # A stateless batch solve is host-side sugar over the single `dispatch`
    # concept: open a throwaway workspace with `init` (plan + rows), then
    # `solve`, and read the report.
    fe = Florecon(wasm_path)
    env = fe.dispatch({"op": "init", "plan": final_plan}, arrow_bytes)
    if not env.get("ok"):
        raise RuntimeError(env.get("error"))
    env = fe.dispatch({"op": "solve"})
    if not env.get("ok"):
        raise RuntimeError(env.get("error"))
    return env.get("report")
