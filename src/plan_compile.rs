use crate::error::ApiError;
use crate::Model;
use crate::plan::{Cond, CostSpec, Plan, PlanNode};
use crate::row::{PhysicalRow, ColumnMap};
use crate::sel::Sel;
use crate::strategy::{
    Group, SoakMode, Strategy, accept_if, agg_net, branch, coalesce, exact_1to1_any, fixed_point,
    flow, labeled, partition_by, pivot, seq, signal_group, snap, soak_all, soak_small, trim,
    windowed,
};
use std::collections::HashMap;

/// The named integer lanes a [`PlanNode::Filter`] `keep` selector reads, in lane
/// order. A group is projected onto these metrics ([`group_metrics`]) and the
/// selector — an ordinary [`Sel`] — is evaluated over that synthetic row, so the
/// whole `Sel` operator set (arithmetic, comparisons, `and`/`or`, `in`, `if`)
/// composes over group shape for free.
const GROUP_METRIC_LANES: [&str; 9] = [
    "size", "pos", "neg", "min_side", "max_side", "net", "abs_net", "max_abs", "min_abs",
];

/// The [`ColumnMap`] naming the group-metric lanes for a filter `keep`
/// selector. Token columns are empty: group metrics are integer-only.
fn group_metric_map() -> ColumnMap {
    ColumnMap {
        int_cols: GROUP_METRIC_LANES
            .iter()
            .enumerate()
            .map(|(i, &name)| (name.to_string(), i))
            .collect(),
        token_cols: HashMap::new(),
    }
}

/// Project a resolved [`Group`] onto the integer metric lanes in
/// [`GROUP_METRIC_LANES`] order, as a synthetic [`PhysicalRow`] a filter `keep`
/// [`Sel`] evaluates against.
fn group_metrics(g: &Group) -> PhysicalRow {
    let pos = g.members.iter().filter(|a| a.amount > 0).count() as i64;
    let neg = g.members.iter().filter(|a| a.amount < 0).count() as i64;
    let mags = || g.members.iter().map(|a| a.amount.abs());
    let max_abs = mags().max().unwrap_or(0);
    let min_abs = mags().min().unwrap_or(0);
    PhysicalRow {
        ints: vec![
            g.members.len() as i64, // size
            pos,
            neg,
            pos.min(neg), // min_side
            pos.max(neg), // max_side
            g.net,
            g.net.abs(), // abs_net
            max_abs,
            min_abs,
        ],
        tokens: Vec::new(),
    }
}

#[derive(Clone)]
struct PlanModel {
    order_by: usize,
    tokens: usize,
    penalty: f64,
    window: i64,
    cost: CostSpec,
}

impl Model for PlanModel {
    type Tx = PhysicalRow;

    fn base_amount(&self, _tx: &PhysicalRow) -> i64 {
        0
    }
    fn penalty(&self, _tx: &PhysicalRow) -> f64 {
        self.penalty
    }
    fn block_key(&self, tx: &PhysicalRow) -> i64 {
        tx.int(self.order_by)
    }
    fn window(&self) -> i64 {
        self.window
    }
    fn match_keys_lot(&self, tx: &PhysicalRow, amount: i64) -> Vec<u64> {
        let mut keys = tx.tokens(self.tokens);
        if amount != 0 {
            keys.push(amount.unsigned_abs());
        }
        keys
    }
    fn cost(&self, a: &PhysicalRow, b: &PhysicalRow) -> Option<f64> {
        self.cost_lot(a, 0, b, 0)
    }
    fn cost_lot(
        &self,
        a: &PhysicalRow,
        a_amount: i64,
        b: &PhysicalRow,
        b_amount: i64,
    ) -> Option<f64> {
        let token_shared = {
            let bt = b.tokens(self.tokens);
            a.tokens(self.tokens).iter().any(|t| bt.contains(t))
        };
        let amount_equal = a_amount != 0 && a_amount.abs() == b_amount.abs();
        let dd = (a.int(self.order_by) - b.int(self.order_by)).abs();
        for tier in &self.cost.tiers {
            // A tier may relax AmountEqual to a tolerance measured against the
            // smaller leg; `None` keeps strict equality.
            let amount_ok = match tier.amount_tol {
                None => amount_equal,
                Some(tol) => {
                    a_amount != 0 && b_amount != 0 && {
                        let scale = a_amount.abs().min(b_amount.abs());
                        (a_amount.abs() - b_amount.abs()).abs() <= tol.slack(scale)
                    }
                }
            };
            let holds = tier.when.iter().all(|c| match c {
                Cond::TokenShared => token_shared,
                Cond::AmountEqual => amount_ok,
            });
            if !holds {
                continue;
            }
            return Some(tier.base + tier.slope * dd as f64);
        }
        None
    }
}

pub(crate) struct CompiledPlan {
    pub primary: usize,
    pub strategy: Box<dyn Strategy<PhysicalRow>>,
}

pub(crate) fn compile(plan: &Plan, map: &ColumnMap) -> Result<CompiledPlan, ApiError> {
    Ok(CompiledPlan {
        primary: map.int_index(&plan.primary)?,
        strategy: compile_node(&plan.root, map)?,
    })
}

fn compile_node(
    plan: &PlanNode,
    map: &ColumnMap,
) -> Result<Box<dyn Strategy<PhysicalRow>>, ApiError> {
    Ok(match plan {
        PlanNode::Seq { steps } => {
            let mut compiled = Vec::with_capacity(steps.len());
            for s in steps {
                compiled.push(compile_node(s, map)?);
            }
            seq(compiled)
        }
        PlanNode::Label { tag, inner } => labeled(tag.clone(), compile_node(inner, map)?),
        PlanNode::FixedPoint { inner, max } => {
            fixed_point(compile_node(inner, map)?, *max)
        }
        PlanNode::Partition { by, inner } => {
            let key = by.compile(map)?;
            compile_node(inner, map)?;
            let inner = (**inner).clone();
            let map_clone = map.clone();
            let factory = move || compile_node(&inner, &map_clone).expect("already validated");
            partition_by(move |r: &PhysicalRow| key(r), factory)
        }
        PlanNode::Branch {
            pred,
            and_then,
            or_else,
        } => {
            let p = pred.compile(map)?;
            branch(
                move |r: &PhysicalRow| p(r) != 0,
                compile_node(and_then, map)?,
                compile_node(or_else, map)?,
            )
        }
        PlanNode::Windowed {
            order,
            width,
            inner,
        } => {
            let o = order.compile(map)?;
            windowed(
                move |r: &PhysicalRow| o(r),
                *width,
                compile_node(inner, map)?,
            )
        }
        PlanNode::Pivot { amount, inner } => {
            let a = amount.compile(map)?;
            pivot(
                move |r: &PhysicalRow| a(r),
                compile_node(inner, map)?,
            )
        }
        PlanNode::Filter { keep, inner } => {
            // `keep` reads group metrics, not row columns, so it compiles
            // against the fixed metric map and is evaluated over each group's
            // projected metric row. Non-zero keeps the group.
            let metric_map = group_metric_map();
            let pred: Sel = keep.clone();
            let f = pred.compile(&metric_map)?;
            accept_if(
                move |g: &Group| f(&group_metrics(g)) != 0,
                compile_node(inner, map)?,
            )
        }
        PlanNode::Coalesce { origin, inner } => {
            coalesce(origin.clone(), compile_node(inner, map)?)
        }
        PlanNode::Trim { tol, inner } => trim(*tol, compile_node(inner, map)?),
        PlanNode::Snap { tol, inner } => snap(*tol, compile_node(inner, map)?),
        PlanNode::AggNet { key, tol } => {
            let k = key.compile(map)?;
            agg_net(move |r: &PhysicalRow| k(r) as u64, *tol)
        }
        PlanNode::Exact {} => exact_1to1_any(),
        PlanNode::Signal { signals, tol, cap } => {
            let s = map.token_index(signals)?;
            signal_group(move |r: &PhysicalRow| r.tokens(s), *tol, *cap)
        }
        PlanNode::SoakSmall {
            max_bps,
            max_abs,
            origin,
            by,
        } => {
            if let Some(by) = by {
                let k = map.int_index(by)?;
                soak_small(
                    *max_bps,
                    *max_abs,
                    SoakMode::Bucket,
                    origin.clone(),
                    move |i: &crate::strategy::Item<PhysicalRow>| i.data.int(k),
                )
            } else {
                soak_small(
                    *max_bps,
                    *max_abs,
                    SoakMode::Singleton,
                    origin.clone(),
                    |_i: &crate::strategy::Item<PhysicalRow>| 0i64,
                )
            }
        }
        PlanNode::SoakAll { origin, by } => {
            if let Some(by) = by {
                let k = map.int_index(by)?;
                soak_all(
                    SoakMode::Bucket,
                    origin.clone(),
                    move |i: &crate::strategy::Item<PhysicalRow>| i.data.int(k),
                )
            } else {
                soak_all(
                    SoakMode::Singleton,
                    origin.clone(),
                    |_i: &crate::strategy::Item<PhysicalRow>| 0i64,
                )
            }
        }
        PlanNode::Flow {
            order_by,
            tokens,
            penalty,
            window,
            cost,
        } => {
            let model = PlanModel {
                order_by: map.int_index(order_by)?,
                tokens: map.token_index(tokens)?,
                penalty: *penalty,
                window: *window,
                cost: cost.clone(),
            };
            flow(model)
        }
    })
}
