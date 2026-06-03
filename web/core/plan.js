export function seq(...steps) {
    return { op: "seq", steps };
}

export function partition(by, inner) {
    return { op: "partition", by, inner };
}

export function windowed(order, width, inner) {
    return { op: "windowed", order, width, inner };
}

export function pivot(amount, inner) {
    return { op: "pivot", amount, inner };
}

export function soak_small(origin, max_bps = null, max_abs = null, by = null) {
    const node = { op: "soak_small", origin };
    if (max_bps !== null) node.max_bps = max_bps;
    if (max_abs !== null) node.max_abs = max_abs;
    if (by !== null) node.by = by;
    return node;
}

export function soak_all(origin = "unmatched", by = null) {
    const node = { op: "soak_all", origin };
    if (by !== null) node.by = by;
    return node;
}

export function agg_net(key, tol = 0) {
    return { op: "agg_net", key, tol };
}

export function exact() {
    return { op: "exact" };
}

export function signal(signals, tol = 0, cap = 256) {
    return { op: "signal", signals, tol, cap };
}

export function flow(day, tokens, penalty = 1000.0, window = -1, cost = null) {
    const node = { op: "flow", day, tokens, penalty, window };
    if (cost !== null) node.cost = cost;
    return node;
}

export function branch(pred, and_then, or_else) {
    return { op: "branch", pred, and_then, or_else };
}
