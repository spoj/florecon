import { tableFromArrays, tableToIPC, makeVector, Uint64, Int64, Utf8 } from "apache-arrow";

export function solve(engine, data_rows, plan_def, primary_amount) {
    const ctx = { exprs: {} };
    
    function walk(node) {
        if (Array.isArray(node)) {
            return node.map(walk);
        } else if (node !== null && typeof node === 'object') {
            const out = {};
            for (const [k, v] of Object.entries(node)) {
                if (typeof v === 'function') {
                    const name = `__vcol_${Object.keys(ctx.exprs).length}`;
                    ctx.exprs[name] = v;
                    out[k] = name;
                } else {
                    out[k] = walk(v);
                }
            }
            return out;
        }
        return node;
    }

    let primary_name;
    if (typeof primary_amount === 'function') {
        primary_name = `__vcol_primary`;
        ctx.exprs[primary_name] = primary_amount;
    } else {
        primary_name = primary_amount;
    }

    const root_plan = walk(plan_def);
    const final_plan = { primary: primary_name, root: root_plan };
    
    // Evaluate expressions and extract existing columns
    // This is a minimal generic extractor.
    const all_cols = new Set();
    // Gather intrinsic columns from first row (assuming uniform)
    if (data_rows.length > 0) {
        for (const k of Object.keys(data_rows[0])) {
            all_cols.add(k);
        }
    }
    for (const name of Object.keys(ctx.exprs)) {
        all_cols.add(name);
    }
    
    const arrowCols = {};
    if (data_rows.length > 0 && "id" in data_rows[0]) {
        arrowCols.id = makeVector({ data: new BigInt64Array(data_rows.length), type: new Uint64() });
    } else {
        arrowCols.id = makeVector({ data: new BigInt64Array(data_rows.length), type: new Uint64() });
        for(let i=0; i<data_rows.length; i++) { arrowCols.id.data[0].values[i] = BigInt(i); }
    }

    // Determine type by sampling (string vs number)
    const types = {};
    for (const c of all_cols) {
        if (c === "id") continue;
        let isString = false;
        // Check first non-null
        if (data_rows.length > 0) {
            let val = c in ctx.exprs ? ctx.exprs[c](data_rows[0]) : data_rows[0][c];
            if (typeof val === 'string') isString = true;
        }
        types[c] = isString ? 'string' : 'number';
        if (isString) {
            arrowCols[c] = new Array(data_rows.length).fill("");
        } else {
            arrowCols[c] = makeVector({ data: new BigInt64Array(data_rows.length), type: new Int64() });
        }
    }

    for (let i = 0; i < data_rows.length; i++) {
        const row = data_rows[i];
        if ("id" in row) arrowCols.id.data[0].values[i] = BigInt(row.id);
        
        for (const c of all_cols) {
            if (c === "id") continue;
            let val = c in ctx.exprs ? ctx.exprs[c](row) : row[c];
            if (types[c] === 'string') {
                arrowCols[c][i] = val || "";
            } else {
                arrowCols[c].data[0].values[i] = BigInt(typeof val === 'boolean' ? (val ? 1 : 0) : (val || 0));
            }
        }
    }

    const int_cols = {};
    const token_cols = {};
    for (const c of all_cols) {
        if (c === "id") continue;
        if (types[c] === 'string') {
            token_cols[c] = Object.keys(token_cols).length;
            arrowCols[c] = makeVector({ data: arrowCols[c], type: new Utf8() });
        } else {
            int_cols[c] = Object.keys(int_cols).length;
        }
    }

    const map = { int_cols, token_cols };
    const arrowBytes = tableToIPC(tableFromArrays(arrowCols), "stream");

    const req = { plan: final_plan, map };
    const result = engine.solve(req, arrowBytes);
    if (!result.ok) throw new Error(result.error);
    return result.report;
}
