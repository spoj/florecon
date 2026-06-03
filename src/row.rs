/// A typed column value the engine works on — the *lowered*, internal form,
/// produced from a business cell by lowering. `Int` carries money (minor units),
/// dates (days), and partition/bucket keys; `Tokens` carries hashed reference
/// signals. Callers do not construct this directly.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum LoweredCell {
    Int(i64),
    Tokens(Vec<u64>),
}

/// One row's lowered column values, positional against the session schema.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LoweredRow {
    pub values: Vec<LoweredCell>,
}

impl LoweredRow {
    pub(crate) fn int(&self, idx: usize) -> i64 {
        match self.values.get(idx) {
            Some(LoweredCell::Int(i)) => *i,
            _ => 0,
        }
    }

    pub(crate) fn tokens(&self, idx: usize) -> Vec<u64> {
        match self.values.get(idx) {
            Some(LoweredCell::Tokens(t)) => t.clone(),
            _ => Vec::new(),
        }
    }
}
