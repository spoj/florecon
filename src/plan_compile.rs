use crate::error::ApiError;
use crate::expr::{BoolEval, ScalarEval, bool_ref, scalar_ref};
use crate::flow::Model;
use crate::plan::{Cond, CostSpec, Plan};
use crate::row::LoweredRow;
use crate::schema::Schema;
use crate::strategy::{
    Strategy, agg_net, branch, exact_1to1, flow, partition_by, seq, signal_group, windowed,
};

#[derive(Clone)]
struct PlanModel {
    amount: ScalarEval,
    day: ScalarEval,
    tokens: usize,
    penalty: f64,
    window: i64,
    cost: CostSpec,
}

impl Model for PlanModel {
    type Tx = LoweredRow;

    fn base_amount(&self, tx: &LoweredRow) -> i64 {
        self.amount.eval(tx)
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
    fn match_keys(&self, tx: &LoweredRow) -> Vec<u64> {
        let mut keys = tx.tokens(self.tokens);
        let n = self.amount.eval(tx);
        if n != 0 {
            keys.push(n.unsigned_abs());
        }
        keys
    }
    fn cost(&self, a: &LoweredRow, b: &LoweredRow) -> Option<f64> {
        let token_shared = {
            let bt = b.tokens(self.tokens);
            a.tokens(self.tokens).iter().any(|t| bt.contains(t))
        };
        let amount_equal = {
            let (na, nb) = (self.amount.eval(a), self.amount.eval(b));
            na != 0 && na.abs() == nb.abs()
        };
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

/// Compile the serializable plan tree into the closure-based strategy algebra.
pub(crate) fn compile(
    plan: &Plan,
    schema: &Schema,
) -> Result<Box<dyn Strategy<LoweredRow>>, ApiError> {
    Ok(match plan {
        Plan::Seq { steps } => {
            let mut compiled = Vec::with_capacity(steps.len());
            for s in steps {
                compiled.push(compile(s, schema)?);
            }
            seq(compiled)
        }
        Plan::Partition { by, inner } => {
            let k = scalar_ref(by, schema)?;
            compile(inner, schema)?;
            let inner = (**inner).clone();
            let schema = schema.clone();
            let factory = move || compile(&inner, &schema).expect("inner plan already validated");
            partition_by(move |r: &LoweredRow| k.eval(r), factory)
        }
        Plan::Branch {
            pred,
            and_then,
            or_else,
        } => {
            let p: BoolEval = bool_ref(pred, schema)?;
            branch(
                move |r: &LoweredRow| p.eval(r),
                compile(and_then, schema)?,
                compile(or_else, schema)?,
            )
        }
        Plan::Windowed {
            order,
            width,
            inner,
        } => {
            let o = scalar_ref(order, schema)?;
            windowed(
                move |r: &LoweredRow| o.eval(r),
                *width,
                compile(inner, schema)?,
            )
        }
        Plan::AggNet { key, amount, tol } => {
            let (k, a) = (scalar_ref(key, schema)?, scalar_ref(amount, schema)?);
            agg_net(
                move |r: &LoweredRow| k.eval(r) as u64,
                move |r: &LoweredRow| a.eval(r),
                *tol,
            )
        }
        Plan::Exact { amount } => {
            let a = scalar_ref(amount, schema)?;
            let ak = a.clone();
            exact_1to1(
                move |r: &LoweredRow| {
                    let v = ak.eval(r);
                    if v != 0 { Some(v.unsigned_abs()) } else { None }
                },
                move |r: &LoweredRow| a.eval(r),
            )
        }
        Plan::Signal {
            signals,
            amount,
            tol,
            cap,
        } => {
            let (s, a) = (schema.index(signals)?, scalar_ref(amount, schema)?);
            signal_group(
                move |r: &LoweredRow| r.tokens(s),
                move |r: &LoweredRow| a.eval(r),
                *tol,
                *cap,
            )
        }
        Plan::Flow {
            amount,
            day,
            tokens,
            penalty,
            window,
            cost,
        } => {
            let model = PlanModel {
                amount: scalar_ref(amount, schema)?,
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
