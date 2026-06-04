//! `Sel` — a tiny, integer-only expression engine for plan *selectors*.
//!
//! This is the generalization of the old "selector is a column name" rule. A
//! [`Sel`] is a pure, total, deterministic expression over the row's integer
//! lanes, compiled **once** into a monomorphic `Fn(&PhysicalRow) -> i64`
//! closure (no per-row interpreter, no boxing in the hot path beyond the
//! closure tree itself). It is the value that feeds `branch`/`partition`/
//! `windowed`/`agg_net`/`pivot` selectors.
//!
//! Design boundaries (see the conservation discussion in [`crate::plan`]):
//!
//! - **Integer-only.** Every value is an `i64`; a boolean is "non-zero", which
//!   matches the engine's existing `r.int(p) != 0` convention. No floats, no
//!   strings — those would pull the engine toward a dynamic type system the
//!   conserved (integer) amount must never depend on.
//! - **Total.** Division / modulo by zero yield `0`; arithmetic wraps. An
//!   expression can never panic or diverge, so a malformed plan degrades to a
//!   bad *proposal*, never a crash or a broken ledger.
//! - **Selectors only.** A `Sel` decides routing/keys/weights. It never writes
//!   the conserved amount except through a conserving combinator (e.g.
//!   [`crate::plan::PlanNode::Pivot`], whose boundary renormalizes back to the
//!   input residual).
//!
//! Wire form (serde): a bare JSON string is a column (`"amount"` ==
//! `{"col":"amount"}`), a bare integer is a literal (`5` == `{"lit":5}`), and
//! every operator is a one-key object whose value is its operand(s):
//! `{"gt":["amount",0]}`, `{"if":[cond,then,else]}`, `{"in":["account",[4000,5000]]}`.
//! Bare strings keep every pre-existing column-name plan valid unchanged.

use crate::error::ApiError;
use crate::row::{ColumnMap, PhysicalRow};

/// A compiled selector: a pure `i64`-valued function of a row.
pub type Compiled = Box<dyn Fn(&PhysicalRow) -> i64>;

/// Bound on expression nesting depth. Compilation builds a closure tree and
/// evaluation recurses it, so a bound keeps both the compile and the per-row
/// eval stack finite regardless of plan input.
const MAX_DEPTH: usize = 64;

/// An integer-valued selector expression. See the module docs for the wire
/// form and the design boundaries.
#[derive(Clone, Debug, PartialEq)]
pub enum Sel {
    /// Read an integer column by name.
    Col(String),
    /// A constant.
    Lit(i64),
    Neg(Box<Sel>),
    Abs(Box<Sel>),
    Add(Box<Sel>, Box<Sel>),
    Sub(Box<Sel>, Box<Sel>),
    Mul(Box<Sel>, Box<Sel>),
    /// Integer division; division by zero yields `0`.
    Div(Box<Sel>, Box<Sel>),
    /// Integer remainder; modulo by zero yields `0`.
    Mod(Box<Sel>, Box<Sel>),
    Min(Box<Sel>, Box<Sel>),
    Max(Box<Sel>, Box<Sel>),
    /// Comparisons yield `1`/`0`.
    Eq(Box<Sel>, Box<Sel>),
    Ne(Box<Sel>, Box<Sel>),
    Lt(Box<Sel>, Box<Sel>),
    Le(Box<Sel>, Box<Sel>),
    Gt(Box<Sel>, Box<Sel>),
    Ge(Box<Sel>, Box<Sel>),
    /// Logical ops treat any non-zero operand as true and yield `1`/`0`.
    And(Box<Sel>, Box<Sel>),
    Or(Box<Sel>, Box<Sel>),
    Not(Box<Sel>),
    /// `1` if the operand equals any member of the set, else `0`.
    In(Box<Sel>, Vec<i64>),
    /// `then` if `cond` is non-zero, else `else`.
    If(Box<Sel>, Box<Sel>, Box<Sel>),
}

impl From<&str> for Sel {
    fn from(s: &str) -> Sel {
        Sel::Col(s.to_string())
    }
}
impl From<String> for Sel {
    fn from(s: String) -> Sel {
        Sel::Col(s)
    }
}
impl From<i64> for Sel {
    fn from(n: i64) -> Sel {
        Sel::Lit(n)
    }
}

impl Sel {
    /// Compile this selector against `map` into a monomorphic row closure.
    /// Fails on an unknown column or an over-deep expression.
    pub fn compile(&self, map: &ColumnMap) -> Result<Compiled, ApiError> {
        compile(self, map, 0)
    }
}

fn compile(s: &Sel, map: &ColumnMap, depth: usize) -> Result<Compiled, ApiError> {
    if depth > MAX_DEPTH {
        return Err(ApiError::BadExpr("selector nested too deep".into()));
    }
    let bin = |a: &Sel, b: &Sel, map: &ColumnMap| -> Result<(Compiled, Compiled), ApiError> {
        Ok((compile(a, map, depth + 1)?, compile(b, map, depth + 1)?))
    };
    Ok(match s {
        Sel::Col(name) => {
            let i = map.int_index(name)?;
            Box::new(move |r| r.int(i))
        }
        Sel::Lit(n) => {
            let n = *n;
            Box::new(move |_| n)
        }
        Sel::Neg(a) => {
            let a = compile(a, map, depth + 1)?;
            Box::new(move |r| a(r).wrapping_neg())
        }
        Sel::Abs(a) => {
            let a = compile(a, map, depth + 1)?;
            Box::new(move |r| a(r).wrapping_abs())
        }
        Sel::Add(a, b) => {
            let (a, b) = bin(a, b, map)?;
            Box::new(move |r| a(r).wrapping_add(b(r)))
        }
        Sel::Sub(a, b) => {
            let (a, b) = bin(a, b, map)?;
            Box::new(move |r| a(r).wrapping_sub(b(r)))
        }
        Sel::Mul(a, b) => {
            let (a, b) = bin(a, b, map)?;
            Box::new(move |r| a(r).wrapping_mul(b(r)))
        }
        Sel::Div(a, b) => {
            let (a, b) = bin(a, b, map)?;
            Box::new(move |r| {
                let d = b(r);
                if d == 0 { 0 } else { a(r).wrapping_div(d) }
            })
        }
        Sel::Mod(a, b) => {
            let (a, b) = bin(a, b, map)?;
            Box::new(move |r| {
                let d = b(r);
                if d == 0 { 0 } else { a(r).wrapping_rem(d) }
            })
        }
        Sel::Min(a, b) => {
            let (a, b) = bin(a, b, map)?;
            Box::new(move |r| a(r).min(b(r)))
        }
        Sel::Max(a, b) => {
            let (a, b) = bin(a, b, map)?;
            Box::new(move |r| a(r).max(b(r)))
        }
        Sel::Eq(a, b) => {
            let (a, b) = bin(a, b, map)?;
            Box::new(move |r| (a(r) == b(r)) as i64)
        }
        Sel::Ne(a, b) => {
            let (a, b) = bin(a, b, map)?;
            Box::new(move |r| (a(r) != b(r)) as i64)
        }
        Sel::Lt(a, b) => {
            let (a, b) = bin(a, b, map)?;
            Box::new(move |r| (a(r) < b(r)) as i64)
        }
        Sel::Le(a, b) => {
            let (a, b) = bin(a, b, map)?;
            Box::new(move |r| (a(r) <= b(r)) as i64)
        }
        Sel::Gt(a, b) => {
            let (a, b) = bin(a, b, map)?;
            Box::new(move |r| (a(r) > b(r)) as i64)
        }
        Sel::Ge(a, b) => {
            let (a, b) = bin(a, b, map)?;
            Box::new(move |r| (a(r) >= b(r)) as i64)
        }
        Sel::And(a, b) => {
            let (a, b) = bin(a, b, map)?;
            Box::new(move |r| (a(r) != 0 && b(r) != 0) as i64)
        }
        Sel::Or(a, b) => {
            let (a, b) = bin(a, b, map)?;
            Box::new(move |r| (a(r) != 0 || b(r) != 0) as i64)
        }
        Sel::Not(a) => {
            let a = compile(a, map, depth + 1)?;
            Box::new(move |r| (a(r) == 0) as i64)
        }
        Sel::In(a, set) => {
            let a = compile(a, map, depth + 1)?;
            let set = set.clone();
            Box::new(move |r| set.contains(&a(r)) as i64)
        }
        Sel::If(c, t, e) => {
            let c = compile(c, map, depth + 1)?;
            let t = compile(t, map, depth + 1)?;
            let e = compile(e, map, depth + 1)?;
            Box::new(move |r| if c(r) != 0 { t(r) } else { e(r) })
        }
    })
}

// ---------------------------------------------------------------------------
// serde: bare string -> Col, bare integer -> Lit, one-key object -> operator.
// ---------------------------------------------------------------------------
#[cfg(feature = "serde")]
mod wire {
    use super::Sel;
    use serde::de::{self, MapAccess, Visitor};
    use serde::ser::SerializeMap;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::fmt;

    impl Serialize for Sel {
        fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
            fn one<S: Serializer, T: Serialize>(ser: S, k: &str, v: &T) -> Result<S::Ok, S::Error> {
                let mut m = ser.serialize_map(Some(1))?;
                m.serialize_entry(k, v)?;
                m.end()
            }
            match self {
                Sel::Col(s) => ser.serialize_str(s),
                Sel::Lit(n) => ser.serialize_i64(*n),
                Sel::Neg(a) => one(ser, "neg", a),
                Sel::Abs(a) => one(ser, "abs", a),
                Sel::Not(a) => one(ser, "not", a),
                Sel::Add(a, b) => one(ser, "add", &(a, b)),
                Sel::Sub(a, b) => one(ser, "sub", &(a, b)),
                Sel::Mul(a, b) => one(ser, "mul", &(a, b)),
                Sel::Div(a, b) => one(ser, "div", &(a, b)),
                Sel::Mod(a, b) => one(ser, "mod", &(a, b)),
                Sel::Min(a, b) => one(ser, "min", &(a, b)),
                Sel::Max(a, b) => one(ser, "max", &(a, b)),
                Sel::Eq(a, b) => one(ser, "eq", &(a, b)),
                Sel::Ne(a, b) => one(ser, "ne", &(a, b)),
                Sel::Lt(a, b) => one(ser, "lt", &(a, b)),
                Sel::Le(a, b) => one(ser, "le", &(a, b)),
                Sel::Gt(a, b) => one(ser, "gt", &(a, b)),
                Sel::Ge(a, b) => one(ser, "ge", &(a, b)),
                Sel::And(a, b) => one(ser, "and", &(a, b)),
                Sel::Or(a, b) => one(ser, "or", &(a, b)),
                Sel::In(a, set) => one(ser, "in", &(a, set)),
                Sel::If(c, t, e) => one(ser, "if", &(c, t, e)),
            }
        }
    }

    impl<'de> Deserialize<'de> for Sel {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Sel, D::Error> {
            de.deserialize_any(SelVisitor)
        }
    }

    struct SelVisitor;
    impl<'de> Visitor<'de> for SelVisitor {
        type Value = Sel;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a column name, an integer literal, or a one-key operator object")
        }
        fn visit_str<E: de::Error>(self, s: &str) -> Result<Sel, E> {
            Ok(Sel::Col(s.to_string()))
        }
        fn visit_i64<E: de::Error>(self, n: i64) -> Result<Sel, E> {
            Ok(Sel::Lit(n))
        }
        fn visit_u64<E: de::Error>(self, n: u64) -> Result<Sel, E> {
            Ok(Sel::Lit(n as i64))
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Sel, M::Error> {
            let key: String = match map.next_key()? {
                Some(k) => k,
                None => return Err(de::Error::custom("empty selector object")),
            };
            type B = Box<Sel>;
            let v = match key.as_str() {
                "col" => Sel::Col(map.next_value()?),
                "lit" => Sel::Lit(map.next_value()?),
                "neg" => Sel::Neg(map.next_value()?),
                "abs" => Sel::Abs(map.next_value()?),
                "not" => Sel::Not(map.next_value()?),
                "add" => { let (a, b): (B, B) = map.next_value()?; Sel::Add(a, b) }
                "sub" => { let (a, b): (B, B) = map.next_value()?; Sel::Sub(a, b) }
                "mul" => { let (a, b): (B, B) = map.next_value()?; Sel::Mul(a, b) }
                "div" => { let (a, b): (B, B) = map.next_value()?; Sel::Div(a, b) }
                "mod" => { let (a, b): (B, B) = map.next_value()?; Sel::Mod(a, b) }
                "min" => { let (a, b): (B, B) = map.next_value()?; Sel::Min(a, b) }
                "max" => { let (a, b): (B, B) = map.next_value()?; Sel::Max(a, b) }
                "eq" => { let (a, b): (B, B) = map.next_value()?; Sel::Eq(a, b) }
                "ne" => { let (a, b): (B, B) = map.next_value()?; Sel::Ne(a, b) }
                "lt" => { let (a, b): (B, B) = map.next_value()?; Sel::Lt(a, b) }
                "le" => { let (a, b): (B, B) = map.next_value()?; Sel::Le(a, b) }
                "gt" => { let (a, b): (B, B) = map.next_value()?; Sel::Gt(a, b) }
                "ge" => { let (a, b): (B, B) = map.next_value()?; Sel::Ge(a, b) }
                "and" => { let (a, b): (B, B) = map.next_value()?; Sel::And(a, b) }
                "or" => { let (a, b): (B, B) = map.next_value()?; Sel::Or(a, b) }
                "in" => { let (a, set): (B, Vec<i64>) = map.next_value()?; Sel::In(a, set) }
                "if" => { let (c, t, e): (B, B, B) = map.next_value()?; Sel::If(c, t, e) }
                other => return Err(de::Error::custom(format!("unknown selector op: {other}"))),
            };
            if map.next_key::<String>()?.is_some() {
                return Err(de::Error::custom("selector object must have exactly one key"));
            }
            Ok(v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn map() -> ColumnMap {
        let mut int_cols = HashMap::new();
        int_cols.insert("amount".into(), 0);
        int_cols.insert("account".into(), 1);
        ColumnMap { int_cols, token_cols: HashMap::new() }
    }
    fn row(amount: i64, account: i64) -> PhysicalRow {
        PhysicalRow { ints: vec![amount, account], tokens: vec![] }
    }

    #[test]
    fn col_and_arithmetic() {
        let s = Sel::Add(Box::new(Sel::Col("amount".into())), Box::new(Sel::Lit(5)));
        let f = s.compile(&map()).unwrap();
        assert_eq!(f(&row(10, 4000)), 15);
    }

    #[test]
    fn comparisons_and_logic_are_zero_one() {
        // (amount > 0) && (account == 4000)
        let s = Sel::And(
            Box::new(Sel::Gt(Box::new("amount".into()), Box::new(0i64.into()))),
            Box::new(Sel::Eq(Box::new("account".into()), Box::new(4000i64.into()))),
        );
        let f = s.compile(&map()).unwrap();
        assert_eq!(f(&row(10, 4000)), 1);
        assert_eq!(f(&row(-10, 4000)), 0);
        assert_eq!(f(&row(10, 5000)), 0);
    }

    #[test]
    fn div_and_mod_by_zero_are_total() {
        let d = Sel::Div(Box::new(1i64.into()), Box::new(0i64.into())).compile(&map()).unwrap();
        let m = Sel::Mod(Box::new(1i64.into()), Box::new(0i64.into())).compile(&map()).unwrap();
        assert_eq!(d(&row(0, 0)), 0);
        assert_eq!(m(&row(0, 0)), 0);
    }

    #[test]
    fn unknown_column_fails_at_compile() {
        assert!(Sel::Col("nope".into()).compile(&map()).is_err());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn wire_bare_string_is_a_column() {
        let s: Sel = serde_json::from_str("\"amount\"").unwrap();
        assert_eq!(s, Sel::Col("amount".into()));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn wire_object_forms_parse_and_roundtrip() {
        // {"gt":["amount",0]}  and  {"in":["account",[4000,5000]]}
        let g: Sel = serde_json::from_str(r#"{"gt":["amount",0]}"#).unwrap();
        assert_eq!(
            g,
            Sel::Gt(Box::new(Sel::Col("amount".into())), Box::new(Sel::Lit(0)))
        );
        let i: Sel = serde_json::from_str(r#"{"in":["account",[4000,5000]]}"#).unwrap();
        let f = i.compile(&map()).unwrap();
        assert_eq!(f(&row(0, 4000)), 1);
        assert_eq!(f(&row(0, 1)), 0);
        // round-trip: a parsed Sel re-serializes to an equivalent Sel
        let back: Sel = serde_json::from_str(&serde_json::to_string(&g).unwrap()).unwrap();
        assert_eq!(back, g);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn wire_rejects_two_key_and_unknown_op() {
        assert!(serde_json::from_str::<Sel>(r#"{"gt":["a",0],"lt":["b",1]}"#).is_err());
        assert!(serde_json::from_str::<Sel>(r#"{"bogus":1}"#).is_err());
    }
}
