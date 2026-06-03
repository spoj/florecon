use crate::error::ApiError;
use crate::lower::cat;
use crate::row::LoweredRow;
use crate::schema::Schema;

/// A scalar value reference used by plan nodes. A bare string remains the common
/// case and resolves to a schema column; structured forms are inline scalar
/// expressions compiled only for the node that uses them (not materialized as
/// derived columns).
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(untagged))]
pub enum ScalarRef {
    Name(String),
    Expr(ScalarExpr),
}

impl From<&str> for ScalarRef {
    fn from(s: &str) -> Self {
        ScalarRef::Name(s.to_string())
    }
}
impl From<String> for ScalarRef {
    fn from(s: String) -> Self {
        ScalarRef::Name(s)
    }
}
impl From<ScalarExpr> for ScalarRef {
    fn from(e: ScalarExpr) -> Self {
        ScalarRef::Expr(e)
    }
}

/// A tiny, typed scalar expression AST. It is intentionally not SQL: no nulls,
/// casts, string functions, or row-creating relational statements.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ScalarExpr {
    Col(String),
    Lit(i64),
    Key(String),
    Abs(Box<ScalarRef>),
    Neg(Box<ScalarRef>),
    Add(Vec<ScalarRef>),
    Sub(Box<ScalarRef>, Box<ScalarRef>),
}

/// A boolean value reference. Predicates compile to closures and are evaluated
/// directly by the node that uses them; they are not materialized as columns.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(untagged))]
pub enum BoolRef {
    Name(String),
    Expr(BoolExpr),
}

impl From<&str> for BoolRef {
    fn from(s: &str) -> Self {
        BoolRef::Name(s.to_string())
    }
}
impl From<String> for BoolRef {
    fn from(s: String) -> Self {
        BoolRef::Name(s)
    }
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum BoolExpr {
    Bool(bool),
    Not(Box<BoolRef>),
    And(Vec<BoolRef>),
    Or(Vec<BoolRef>),
    Eq(Box<ScalarRef>, Box<ScalarRef>),
    Ne(Box<ScalarRef>, Box<ScalarRef>),
    Gt(Box<ScalarRef>, Box<ScalarRef>),
    Ge(Box<ScalarRef>, Box<ScalarRef>),
    Lt(Box<ScalarRef>, Box<ScalarRef>),
    Le(Box<ScalarRef>, Box<ScalarRef>),
}

#[derive(Clone)]
pub(crate) enum ScalarEval {
    Int(usize),
    Lit(i64),
    Abs(Box<ScalarEval>),
    Neg(Box<ScalarEval>),
    Add(Vec<ScalarEval>),
    Sub(Box<ScalarEval>, Box<ScalarEval>),
}

impl ScalarEval {
    pub(crate) fn eval(&self, row: &LoweredRow) -> i64 {
        match self {
            ScalarEval::Int(i) => row.int(*i),
            ScalarEval::Lit(v) => *v,
            ScalarEval::Abs(x) => x.eval(row).abs(),
            ScalarEval::Neg(x) => -x.eval(row),
            ScalarEval::Add(xs) => xs.iter().map(|x| x.eval(row)).sum(),
            ScalarEval::Sub(a, b) => a.eval(row) - b.eval(row),
        }
    }
}

#[derive(Clone)]
pub(crate) enum BoolEval {
    Scalar(ScalarEval),
    Bool(bool),
    Not(Box<BoolEval>),
    And(Vec<BoolEval>),
    Or(Vec<BoolEval>),
    Eq(ScalarEval, ScalarEval),
    Ne(ScalarEval, ScalarEval),
    Gt(ScalarEval, ScalarEval),
    Ge(ScalarEval, ScalarEval),
    Lt(ScalarEval, ScalarEval),
    Le(ScalarEval, ScalarEval),
}

impl BoolEval {
    pub(crate) fn eval(&self, row: &LoweredRow) -> bool {
        match self {
            BoolEval::Scalar(x) => x.eval(row) != 0,
            BoolEval::Bool(b) => *b,
            BoolEval::Not(x) => !x.eval(row),
            BoolEval::And(xs) => xs.iter().all(|x| x.eval(row)),
            BoolEval::Or(xs) => xs.iter().any(|x| x.eval(row)),
            BoolEval::Eq(a, b) => a.eval(row) == b.eval(row),
            BoolEval::Ne(a, b) => a.eval(row) != b.eval(row),
            BoolEval::Gt(a, b) => a.eval(row) > b.eval(row),
            BoolEval::Ge(a, b) => a.eval(row) >= b.eval(row),
            BoolEval::Lt(a, b) => a.eval(row) < b.eval(row),
            BoolEval::Le(a, b) => a.eval(row) <= b.eval(row),
        }
    }
}

pub(crate) fn scalar_ref(r: &ScalarRef, schema: &Schema) -> Result<ScalarEval, ApiError> {
    match r {
        ScalarRef::Name(name) => Ok(ScalarEval::Int(schema.index(name)?)),
        ScalarRef::Expr(e) => scalar_expr(e, schema),
    }
}

fn scalar_expr(e: &ScalarExpr, schema: &Schema) -> Result<ScalarEval, ApiError> {
    Ok(match e {
        ScalarExpr::Col(name) => ScalarEval::Int(schema.index(name)?),
        ScalarExpr::Lit(v) => ScalarEval::Lit(*v),
        ScalarExpr::Key(s) => ScalarEval::Lit(cat(s)),
        ScalarExpr::Abs(x) => ScalarEval::Abs(Box::new(scalar_ref(x, schema)?)),
        ScalarExpr::Neg(x) => ScalarEval::Neg(Box::new(scalar_ref(x, schema)?)),
        ScalarExpr::Add(xs) => {
            if xs.is_empty() {
                return Err(ApiError::BadExpr("add needs at least one term".into()));
            }
            ScalarEval::Add(
                xs.iter()
                    .map(|x| scalar_ref(x, schema))
                    .collect::<Result<Vec<_>, _>>()?,
            )
        }
        ScalarExpr::Sub(a, b) => ScalarEval::Sub(
            Box::new(scalar_ref(a, schema)?),
            Box::new(scalar_ref(b, schema)?),
        ),
    })
}

pub(crate) fn bool_ref(r: &BoolRef, schema: &Schema) -> Result<BoolEval, ApiError> {
    match r {
        BoolRef::Name(name) => Ok(BoolEval::Scalar(ScalarEval::Int(schema.index(name)?))),
        BoolRef::Expr(e) => bool_expr(e, schema),
    }
}

fn bool_expr(e: &BoolExpr, schema: &Schema) -> Result<BoolEval, ApiError> {
    Ok(match e {
        BoolExpr::Bool(b) => BoolEval::Bool(*b),
        BoolExpr::Not(x) => BoolEval::Not(Box::new(bool_ref(x, schema)?)),
        BoolExpr::And(xs) => BoolEval::And(
            xs.iter()
                .map(|x| bool_ref(x, schema))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        BoolExpr::Or(xs) => BoolEval::Or(
            xs.iter()
                .map(|x| bool_ref(x, schema))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        BoolExpr::Eq(a, b) => BoolEval::Eq(scalar_ref(a, schema)?, scalar_ref(b, schema)?),
        BoolExpr::Ne(a, b) => BoolEval::Ne(scalar_ref(a, schema)?, scalar_ref(b, schema)?),
        BoolExpr::Gt(a, b) => BoolEval::Gt(scalar_ref(a, schema)?, scalar_ref(b, schema)?),
        BoolExpr::Ge(a, b) => BoolEval::Ge(scalar_ref(a, schema)?, scalar_ref(b, schema)?),
        BoolExpr::Lt(a, b) => BoolEval::Lt(scalar_ref(a, schema)?, scalar_ref(b, schema)?),
        BoolExpr::Le(a, b) => BoolEval::Le(scalar_ref(a, schema)?, scalar_ref(b, schema)?),
    })
}
