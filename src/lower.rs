//! Lowering — the one place business values become engine integers.
//!
//! The engine is integer-only ([`LoweredCell::Int`]/[`LoweredCell::Tokens`]).
//! Callers — the
//! Python batch host, the browser, native Rust — describe rows with *business*
//! values instead: currency codes, account strings, free-text reference fields.
//! This module lowers those to stable i64s with a pure FNV-1a hash.
//!
//! How a cell lowers is a property of its *column*, not the cell, so the policy
//! lives in the [`Schema`](crate::schema::Schema) as a per-column [`Kind`]:
//!
//! | [`Kind`] | cell  | lowers to        | matching semantics      |
//! |----------|-------|------------------|-------------------------|
//! | `Number` | i64   | [`LoweredCell::Int`] | compare / net / bucket |
//! | `Key`    | str   | `Int(cat(s))`    | whole-string equality   |
//! | `Tokens` | str   | `Tokens(..)`     | reference-token overlap |
//!
//! A cell is therefore a bare scalar ([`Cell`]) — a number or a string — and
//! the schema says how to read it. There is no separate "pair" or "text" value:
//! a composite/bilateral key is just a `Key` column whose string the caller
//! composed (sorted-join is domain logic, not an engine concept).
//!
//! "Pure" is the load-bearing word: the same string lowers to the same id in
//! every process and shard with no shared dictionary, so ids agree across
//! languages and partitions without coordination. The Python host mirrors this
//! hash byte-for-byte, and the `matches_python_host` test pins the two together.
//!
//! ```
//! use florecon::lower::{Kind, Row, TokenCfg};
//! let kinds = [Kind::Key, Kind::Key, Kind::Number, Kind::Tokens];
//! let row = Row::new(vec![
//!     "00288|00492".into(),        // composite key, caller-composed
//!     "USD".into(),                // categorical key
//!     20517i64.into(),             // genuine integer (epoch day)
//!     "INV0001234567 memo".into(), // free text -> tokens
//! ])
//! .lower(&kinds, &TokenCfg::default())
//! .unwrap();
//! assert_eq!(row.values.len(), 4);
//! ```

use crate::error::ApiError;
use crate::row::{LoweredCell, LoweredRow};

const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
/// Mask categorical/pair ids non-negative so they read cleanly as i64 partition
/// keys (token ids keep the full 64-bit hash).
const I63: u64 = 0x7FFF_FFFF_FFFF_FFFF;

/// FNV-1a over the UTF-8 bytes of `s`. The lowering hash; stable across
/// languages (mirrored byte-for-byte by the Python host).
pub fn fnv1a(s: &str) -> u64 {
    let mut h = FNV_OFFSET;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Lower a categorical string to a non-negative engine id.
pub fn cat(s: &str) -> i64 {
    (fnv1a(s) & I63) as i64
}

/// Token-extraction policy for [`tokens`]. Defaults mirror the host: keep
/// alphanumeric tokens of length `minlen..=maxlen` that carry at least one digit
/// (pure-alpha words are dropped) and are not in `drop` (compared upper-cased).
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TokenCfg {
    pub minlen: usize,
    pub maxlen: usize,
    /// Stopwords to discard, matched against the upper-cased token.
    pub drop: Vec<String>,
}

impl Default for TokenCfg {
    fn default() -> Self {
        TokenCfg {
            minlen: 6,
            maxlen: 40,
            drop: Vec::new(),
        }
    }
}

/// Extract reference tokens from free-text `fields` and hash each to a u64
/// signal id, order-preserving and de-duplicated. Within each field, whitespace
/// splits candidates; each candidate is reduced to its alphanumeric characters,
/// upper-cased, then kept only if it is in the length band, carries a digit, and
/// is not a stopword.
pub fn tokens(fields: &[String], cfg: &TokenCfg) -> Vec<u64> {
    let mut out: Vec<u64> = Vec::new();
    for field in fields {
        for raw in field.split_whitespace() {
            let t: String = raw
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_uppercase();
            let len = t.chars().count();
            if len < cfg.minlen || len > cfg.maxlen {
                continue;
            }
            if t.chars().all(|c| c.is_alphabetic()) {
                continue;
            }
            if cfg.drop.iter().any(|d| d == &t) {
                continue;
            }
            let h = fnv1a(&t);
            if !out.contains(&h) {
                out.push(h);
            }
        }
    }
    out
}

/// How a column's cells lower to engine values. A column-level property (the
/// whole column is one kind), declared once in the [`Schema`](crate::schema::Schema).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Kind {
    /// A genuine number (money in minor units, an epoch day): the i64 as-is.
    Number,
    /// A categorical string, lowered to one stable id by [`cat`]. A numeric cell
    /// is taken as an already-lowered key and passes through.
    Key,
    /// A free-text field, lowered to a set of reference-signal ids by [`tokens`].
    Tokens,
}

/// A bare input cell: a number or a string. How it lowers is decided by the
/// column's [`Kind`], not by the cell itself, so there is no per-cell tag.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(untagged))]
pub enum Cell {
    Num(i64),
    Str(String),
}

impl From<i64> for Cell {
    fn from(i: i64) -> Self {
        Cell::Num(i)
    }
}
impl From<&str> for Cell {
    fn from(s: &str) -> Self {
        Cell::Str(s.to_string())
    }
}
impl From<String> for Cell {
    fn from(s: String) -> Self {
        Cell::Str(s)
    }
}

impl Cell {
    /// Lower against the column's [`Kind`]. `col` is the column index, reported
    /// on a kind/scalar mismatch.
    pub fn lower(self, kind: Kind, col: usize, cfg: &TokenCfg) -> Result<LoweredCell, ApiError> {
        match (kind, self) {
            (Kind::Number, Cell::Num(i)) => Ok(LoweredCell::Int(i)),
            // A numeric cell in a key column is an already-numeric key.
            (Kind::Key, Cell::Num(i)) => Ok(LoweredCell::Int(i)),
            (Kind::Key, Cell::Str(s)) => Ok(LoweredCell::Int(cat(&s))),
            (Kind::Tokens, Cell::Str(s)) => Ok(LoweredCell::Tokens(tokens(&[s], cfg))),
            (Kind::Tokens, Cell::Num(_)) => Ok(LoweredCell::Tokens(Vec::new())),
            (Kind::Number, Cell::Str(_)) => Err(ApiError::BadCell {
                col,
                want: "number",
            }),
        }
    }
}

/// A row of bare cells, positional against the schema. Lower with
/// [`Self::lower`] (driven by the schema's per-column [`Kind`]s) to get an
/// engine [`LoweredRow`]. Serializes as a bare array, e.g. `["USD", 20517, "INV1"]`.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct Row(pub Vec<Cell>);

impl Row {
    pub fn new(cells: Vec<Cell>) -> Self {
        Row(cells)
    }

    /// Lower every cell against the matching column [`Kind`], producing the row
    /// the engine consumes. Errors on arity or kind/scalar mismatch.
    pub fn lower(self, kinds: &[Kind], cfg: &TokenCfg) -> Result<LoweredRow, ApiError> {
        if self.0.len() != kinds.len() {
            return Err(ApiError::SchemaArity {
                expected: kinds.len(),
                got: self.0.len(),
            });
        }
        let mut values = Vec::with_capacity(self.0.len());
        for (col, (cell, &kind)) in self.0.into_iter().zip(kinds).enumerate() {
            values.push(cell.lower(kind, col, cfg)?);
        }
        Ok(LoweredRow { values })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pinned against the Python host and confirmed against web/data.json row 0.
    // If these drift, ids stop agreeing across the batch path, the browser, and
    // native Rust.
    #[test]
    fn matches_python_host() {
        assert_eq!(cat("USD"), 7056772390745336839);
        assert_eq!(cat("00492"), 7792345195920810492);
        // a composite key is just cat() of a caller-composed (sorted) string
        assert_eq!(cat("00288|00492"), 7686300666667729858);
        assert_eq!(
            tokens(&["INV0001234567 hello THE 12".into()], &TokenCfg::default()),
            vec![6280867139549122728]
        );
    }

    #[test]
    fn token_rules() {
        let cfg = TokenCfg::default();
        // pure-alpha dropped, short dropped, duplicates collapsed, order kept
        assert_eq!(
            tokens(&["AB12345 word AB12345 X9".into()], &cfg),
            vec![fnv1a("AB12345")]
        );
        // drop list (upper-cased match)
        let cfg2 = TokenCfg {
            drop: vec!["OFFSETENTRY".into()],
            ..TokenCfg::default()
        };
        assert!(tokens(&["offsetentry INV900001".into()], &cfg2).contains(&fnv1a("INV900001")));
        assert!(!tokens(&["offsetentry INV900001".into()], &cfg2).contains(&fnv1a("OFFSETENTRY")));
    }

    #[test]
    fn lowers_by_kind() {
        let cfg = TokenCfg::default();
        let kinds = [Kind::Number, Kind::Key, Kind::Key, Kind::Tokens];
        let row = Row::new(vec![
            5i64.into(),
            "USD".into(),
            492i64.into(), // numeric key passes through
            "INV0001234567 x".into(),
        ])
        .lower(&kinds, &cfg)
        .unwrap();
        assert_eq!(
            row.values,
            vec![
                LoweredCell::Int(5),
                LoweredCell::Int(cat("USD")),
                LoweredCell::Int(492),
                LoweredCell::Tokens(vec![6280867139549122728]),
            ]
        );
        // a string in a number column fails loud
        assert!(
            Row::new(vec!["oops".into()])
                .lower(&[Kind::Number], &cfg)
                .is_err()
        );
        // arity mismatch fails loud
        assert!(
            Row::new(vec![5i64.into()])
                .lower(&[Kind::Number, Kind::Key], &cfg)
                .is_err()
        );
    }
}
