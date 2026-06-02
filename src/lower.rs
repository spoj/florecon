//! String lowering — the one place business values become engine integers.
//!
//! The engine is integer-only ([`Value::Int`]/[`Value::Tokens`]). Callers — the
//! Python batch host, the browser, native Rust — describe rows with *business*
//! values instead: currency codes, account strings, bilateral company pairs,
//! free-text reference fields. This module lowers those to stable i64s with a
//! pure FNV-1a hash.
//!
//! "Pure" is the load-bearing word: the same string lowers to the same id in
//! every process and shard with no shared dictionary, so ids agree across
//! languages and partitions without coordination. The Python host mirrors this
//! hash byte-for-byte (see `py/src/florecon/intern.py`), and the
//! `matches_python_host` test pins the two together.
//!
//! It is a first-class Rust API, not just a wire detail: build engine rows from
//! strings directly with [`RawValue`]/[`RawRow`], or call [`cat`]/[`pair`]/
//! [`tokens`] piecemeal.
//!
//! ```
//! use florecon::lower::{RawRow, RawValue, TokenCfg};
//! let cfg = TokenCfg::default();
//! let row = RawRow::new(vec![
//!     RawValue::pair("00492", "00288"), // bilateral key, order-independent
//!     RawValue::str("USD"),             // categorical
//!     RawValue::Int(20517),             // genuine integer (epoch day)
//!     RawValue::text(["INV0001234567 memo text"]), // free-text -> tokens
//! ])
//! .lower(&cfg);
//! assert_eq!(row.values.len(), 4);
//! ```

use crate::plan::{Row, Value};

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

/// Lower an unordered pair (e.g. a bilateral `company|counterparty` key) to one
/// id. Order-independent: `pair(a, b) == pair(b, a)`.
pub fn pair(a: &str, b: &str) -> i64 {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    (fnv1a(&format!("{lo}|{hi}")) & I63) as i64
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

/// A business-level input value. `Int`/`Tokens` pass through unchanged (already
/// lowered, or genuinely numeric columns like money and epoch dates); `Str`/
/// `Pair`/`Text` carry strings the engine never sees, lowered by [`Self::lower`].
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum RawValue {
    Int(i64),
    Tokens(Vec<u64>),
    Str(String),
    Pair(String, String),
    Text(Vec<String>),
}

impl RawValue {
    /// Lower to the engine [`Value`]. `cfg` governs `Text` token extraction; the
    /// other variants ignore it.
    pub fn lower(self, cfg: &TokenCfg) -> Value {
        match self {
            RawValue::Int(i) => Value::Int(i),
            RawValue::Tokens(t) => Value::Tokens(t),
            RawValue::Str(s) => Value::Int(cat(&s)),
            RawValue::Pair(a, b) => Value::Int(pair(&a, &b)),
            RawValue::Text(fields) => Value::Tokens(tokens(&fields, cfg)),
        }
    }

    /// `Str` constructor taking anything string-like.
    pub fn str(s: impl Into<String>) -> Self {
        RawValue::Str(s.into())
    }

    /// `Pair` constructor taking anything string-like.
    pub fn pair(a: impl Into<String>, b: impl Into<String>) -> Self {
        RawValue::Pair(a.into(), b.into())
    }

    /// `Text` constructor from an iterator of free-text fields.
    pub fn text<I, S>(fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        RawValue::Text(fields.into_iter().map(Into::into).collect())
    }
}

/// A row of business values, positional against the schema. Lower with
/// [`Self::lower`] to get an engine [`Row`].
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RawRow {
    pub values: Vec<RawValue>,
}

impl RawRow {
    pub fn new(values: Vec<RawValue>) -> Self {
        RawRow { values }
    }

    /// Lower every value to its engine [`Value`], producing the row the engine
    /// consumes.
    pub fn lower(self, cfg: &TokenCfg) -> Row {
        Row {
            values: self.values.into_iter().map(|v| v.lower(cfg)).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pinned against the Python host (py/src/florecon/intern.py) and confirmed
    // against web/data.json row 0. If these drift, ids stop agreeing across the
    // batch path, the browser, and native Rust.
    #[test]
    fn matches_python_host() {
        assert_eq!(cat("USD"), 7056772390745336839);
        assert_eq!(cat("00492"), 7792345195920810492);
        assert_eq!(pair("00492", "00288"), 7686300666667729858);
        assert_eq!(pair("00288", "00492"), 7686300666667729858); // order-independent
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
    fn lower_passthrough_and_strings() {
        let cfg = TokenCfg::default();
        assert_eq!(RawValue::Int(5).lower(&cfg), Value::Int(5));
        assert_eq!(RawValue::Tokens(vec![9]).lower(&cfg), Value::Tokens(vec![9]));
        assert_eq!(RawValue::str("USD").lower(&cfg), Value::Int(cat("USD")));
        assert_eq!(
            RawValue::pair("00492", "00288").lower(&cfg),
            Value::Int(7686300666667729858)
        );
    }
}
