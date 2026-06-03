use crate::error::ApiError;
use crate::lower::{Kind, TokenCfg};

/// One schema column: a name (referenced by the plan) and a [`Kind`] (how its
/// cells lower).
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Column {
    pub name: String,
    pub kind: Kind,
}

/// Column layout shared by every row in a session: the ordered, typed columns
/// plus the text-token policy. Lowering reads the per-column [`Kind`]s; the plan
/// references columns by name.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Schema {
    cols: Vec<Column>,
    /// Stopwords for token lowering, matched upper-cased. Empty by default;
    /// carried here so the lowering policy travels with the schema it applies to.
    #[cfg_attr(feature = "serde", serde(default))]
    token_drop: Vec<String>,
}

impl Schema {
    /// Schema from `(name, kind)` pairs.
    pub fn typed<I, S>(cols: I) -> Self
    where
        I: IntoIterator<Item = (S, Kind)>,
        S: Into<String>,
    {
        Schema {
            cols: cols
                .into_iter()
                .map(|(name, kind)| Column {
                    name: name.into(),
                    kind,
                })
                .collect(),
            token_drop: Vec::new(),
        }
    }

    /// The per-column lowering kinds, positional against rows.
    pub fn kinds(&self) -> Vec<Kind> {
        self.cols.iter().map(|c| c.kind).collect()
    }

    /// The token-extraction policy for this schema.
    pub fn token_cfg(&self) -> TokenCfg {
        TokenCfg {
            drop: self.token_drop.clone(),
            ..TokenCfg::default()
        }
    }

    pub fn len(&self) -> usize {
        self.cols.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cols.is_empty()
    }

    pub(crate) fn index(&self, name: &str) -> Result<usize, ApiError> {
        self.cols
            .iter()
            .position(|c| c.name == name)
            .ok_or_else(|| ApiError::UnknownColumn(name.to_string()))
    }
}
