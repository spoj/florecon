use crate::error::ApiError;
use crate::expr::{BoolEval, ScalarEval, bool_ref, scalar_ref};
use crate::flow::Model;
use crate::plan::{Cond, CostSpec, Plan, PlanNode};
use crate::row::LoweredRow;
use crate::schema::Schema;
use crate::strategy::{
    SoakMode, Strategy, agg_net, branch, exact_1to1_any, flow, partition_by, pivot, seq,
    signal_group, soak_all, soak_small, windowed,
};

#[derive(Clone)]
struct PlanModel {
    day: ScalarEval,
    tokens: usize,
    penalty: f64,
    window: i64,
    cost: CostSpec,
}

impl Model for PlanModel {
    type Tx = LoweredRow;

    // Strategy flow wraps rows in FlowTx and supplies the current Item.amount as
    // the matcher's base amount. This fallback exists only to satisfy the lower
    // flow::Model trait for direct Matcher-style calls and is not used by plan
    // execution.
    fn base_amount(&self, _tx: &LoweredRow) -> i64 {
        0
    }
    fn penalty(&self, _tx: &LoweredRow) -> f64 {
        self.penalty
    }
    fn block_key(&self, tx: &LoweredRow) -> i64 {
        self.day.eval(tx)
    }
    fn window(&self) -> i64 {
        self.window
    }
    fn match_keys_lot(&self, tx: &LoweredRow, amount: i64) -> Vec<u64> {
        let mut keys = tx.tokens(self.tokens);
        if amount != 0 {
            keys.push(amount.unsigned_abs());
        }
        keys
    }
    fn cost(&self, a: &LoweredRow, b: &LoweredRow) -> Option<f64> {
        self.cost_lot(a, 0, b, 0)
    }
    fn cost_lot(
        &self,
        a: &LoweredRow,
        a_amount: i64,
        b: &LoweredRow,
        b_amount: i64,
    ) -> Option<f64> {
        let token_shared = {
            let bt = b.tokens(self.tokens);
            a.tokens(self.tokens).iter().any(|t| bt.contains(t))
        };
        let amount_equal = a_amount != 0 && a_amount.abs() == b_amount.abs();
        let dd = (self.day.eval(a) - self.day.eval(b)).abs();
        for tier in &self.cost.tiers {
            let holds = tier.when.iter().all(|c| match c {
                Cond::TokenShared => token_shared,
                Cond::AmountEqual => amount_equal,
            });
            if !holds {
                continue;
            }
            if let Some(md) = tier.max_day
                && dd > md
            {
                continue;
            }
            return Some(tier.base + tier.day_slope * dd as f64);
        }
        None
    }
}

pub(crate) struct CompiledPlan {
    pub primary: ScalarEval,
    pub strategy: Box<dyn Strategy<LoweredRow>>,
}

/// Compile the serializable plan into a primary amount evaluator plus the
/// closure-based strategy algebra. The strategy assumes all Items are already
/// initialized in that primary numeraire.
pub(crate) fn compile(plan: &Plan, schema: &Schema) -> Result<CompiledPlan, ApiError> {
    Ok(CompiledPlan {
        primary: scalar_ref(&plan.primary, schema)?,
        strategy: compile_node(&plan.root, schema)?,
    })
}

fn compile_node(
    plan: &PlanNode,
    schema: &Schema,
) -> Result<Box<dyn Strategy<LoweredRow>>, ApiError> {
    Ok(match plan {
        PlanNode::Seq { steps } => {
            let mut compiled = Vec::with_capacity(steps.len());
            for s in steps {
                compiled.push(compile_node(s, schema)?);
            }
            seq(compiled)
        }
        PlanNode::Partition { by, inner } => {
            let k = scalar_ref(by, schema)?;
            compile_node(inner, schema)?;
            let inner = (**inner).clone();
            let schema = schema.clone();
            let factory =
                move || compile_node(&inner, &schema).expect("inner plan already validated");
            partition_by(move |r: &LoweredRow| k.eval(r), factory)
        }
        PlanNode::Branch {
            pred,
            and_then,
            or_else,
        } => {
            let p: BoolEval = bool_ref(pred, schema)?;
            branch(
                move |r: &LoweredRow| p.eval(r),
                compile_node(and_then, schema)?,
                compile_node(or_else, schema)?,
            )
        }
        PlanNode::Windowed {
            order,
            width,
            inner,
        } => {
            let o = scalar_ref(order, schema)?;
            windowed(
                move |r: &LoweredRow| o.eval(r),
                *width,
                compile_node(inner, schema)?,
            )
        }
        PlanNode::Pivot { amount, inner } => {
            let a = scalar_ref(amount, schema)?;
            pivot(
                move |r: &LoweredRow| a.eval(r),
                compile_node(inner, schema)?,
            )
        }
        PlanNode::AggNet { key, tol } => {
            let k = scalar_ref(key, schema)?;
            agg_net(move |r: &LoweredRow| k.eval(r) as u64, *tol)
        }
        PlanNode::Exact {} => exact_1to1_any(),
        PlanNode::Signal { signals, tol, cap } => {
            let s = schema.index(signals)?;
            signal_group(move |r: &LoweredRow| r.tokens(s), *tol, *cap)
        }
        PlanNode::SoakSmall {
            max_bps,
            max_abs,
            origin,
            by,
        } => {
            if let Some(by) = by {
                let k = scalar_ref(by, schema)?;
                soak_small(
                    *max_bps,
                    *max_abs,
                    SoakMode::Bucket,
                    origin.clone(),
                    move |i: &crate::strategy::Item<LoweredRow>| k.eval(&i.data),
                )
            } else {
                soak_small(
                    *max_bps,
                    *max_abs,
                    SoakMode::Singleton,
                    origin.clone(),
                    |_i: &crate::strategy::Item<LoweredRow>| 0i64,
                )
            }
        }
        PlanNode::SoakAll { origin, by } => {
            if let Some(by) = by {
                let k = scalar_ref(by, schema)?;
                soak_all(
                    SoakMode::Bucket,
                    origin.clone(),
                    move |i: &crate::strategy::Item<LoweredRow>| k.eval(&i.data),
                )
            } else {
                soak_all(
                    SoakMode::Singleton,
                    origin.clone(),
                    |_i: &crate::strategy::Item<LoweredRow>| 0i64,
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
                day: scalar_ref(day, schema)?,
                tokens: schema.index(tokens)?,
                penalty: *penalty,
                window: *window,
                cost: cost.clone(),
            };
            flow(model)
        }
    })
}
