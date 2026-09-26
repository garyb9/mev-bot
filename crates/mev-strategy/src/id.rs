//! Strategy identifiers (SPEC-0003 §4).

use std::fmt;

use serde::{Deserialize, Serialize};

/// Stable identifier for a strategy, used in metrics, persistence, and config.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StrategyId(String);

impl StrategyId {
    /// Construct an id from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StrategyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for StrategyId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for StrategyId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_as_a_plain_string() {
        let id = StrategyId::new("funding_basis");
        assert_eq!(id.as_str(), "funding_basis");
        assert_eq!(id.to_string(), "funding_basis");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"funding_basis\"");
        assert_eq!(serde_json::from_str::<StrategyId>(&json).unwrap(), id);
    }
}
