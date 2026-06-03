use std::collections::HashMap;
use crate::error::ApiError;

#[derive(Clone, Debug, PartialEq)]
pub struct PhysicalRow {
    pub ints: Vec<i64>,
    pub tokens: Vec<Vec<u64>>,
}

impl PhysicalRow {
    pub fn int(&self, idx: usize) -> i64 {
        self.ints.get(idx).copied().unwrap_or(0)
    }

    pub fn tokens(&self, idx: usize) -> Vec<u64> {
        self.tokens.get(idx).cloned().unwrap_or_default()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ColumnMap {
    pub int_cols: HashMap<String, usize>,
    pub token_cols: HashMap<String, usize>,
}

impl ColumnMap {
    pub fn int_index(&self, name: &str) -> Result<usize, ApiError> {
        self.int_cols.get(name).copied().ok_or_else(|| ApiError::UnknownColumn(name.to_string()))
    }

    pub fn token_index(&self, name: &str) -> Result<usize, ApiError> {
        self.token_cols.get(name).copied().ok_or_else(|| ApiError::UnknownColumn(name.to_string()))
    }
}
