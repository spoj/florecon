use crate::error::ApiError;
use crate::flow::Model;
use crate::plan::{Cond, CostSpec, Plan, PlanNode};
use crate::row::{PhysicalRow, ColumnMap};
use crate::strategy::{
    SoakMode, Strategy, agg_net, branch, exact_1to1_any, flow, partition_by, pivot, seq,
    signal_group, soak_all, soak_small, windowed,
};

#[derive(Clone)]
struct PlanModel {
    day: usize,
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
        tx.int(self.day)
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
        let dd = (a.int(self.day) - b.int(self.day)).abs();
        for tier in &self.cost.tiers {
            let holds = tier.when.iter().all(|c| match c {
                Cond::TokenShared => token_shared,
                Cond::AmountEqual => amount_equal,
            });
            if !holds {
                continue;
            }
            if let Some(md) = tier.max_day {
                if dd > md {
                    continue;
                }
            }
            return Some(tier.base + tier.day_slope * dd as f64);
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
        PlanNode::Partition { by, inner } => {
            let k = map.int_index(by)?;
            compile_node(inner, map)?;
            let inner = (**inner).clone();
            let map_clone = map.clone();
            let factory = move || compile_node(&inner, &map_clone).expect("already validated");
            partition_by(move |r: &PhysicalRow| r.int(k), factory)
        }
        PlanNode::Branch {
            pred,
            and_then,
            or_else,
        } => {
            let p = map.int_index(pred)?;
            branch(
                move |r: &PhysicalRow| r.int(p) != 0,
                compile_node(and_then, map)?,
                compile_node(or_else, map)?,
            )
        }
        PlanNode::Windowed {
            order,
            width,
            inner,
        } => {
            let o = map.int_index(order)?;
            windowed(
                move |r: &PhysicalRow| r.int(o),
                *width,
                compile_node(inner, map)?,
            )
        }
        PlanNode::Pivot { amount, inner } => {
            let a = map.int_index(amount)?;
            pivot(
                move |r: &PhysicalRow| r.int(a),
                compile_node(inner, map)?,
            )
        }
        PlanNode::AggNet { key, tol } => {
            let k = map.int_index(key)?;
            agg_net(move |r: &PhysicalRow| r.int(k) as u64, *tol)
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
            day,
            tokens,
            penalty,
            window,
            cost,
        } => {
            let model = PlanModel {
                day: map.int_index(day)?,
                tokens: map.token_index(tokens)?,
                penalty: *penalty,
                window: *window,
                cost: cost.clone(),
            };
            flow(model)
        }
    })
}
