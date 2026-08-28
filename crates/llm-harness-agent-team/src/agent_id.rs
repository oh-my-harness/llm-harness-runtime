use std::fmt;

use serde::{Deserialize, Serialize};

/// Unique identifier of a team agent (its registry/inbox key).
///
/// Construct with [`AgentId::new`] from a name (e.g. `"researcher-1"`)
/// or [`AgentId::generate`] for an anonymous UUIDv7 id. `"operator"` is
/// the conventional human-endpoint id; reserved ids ("timer", "self",
/// "operator", "reminder:*") cannot be used as member ids.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(String);

impl AgentId {
    /// Create an id from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Generate a fresh anonymous id (UUIDv7).
    pub fn generate() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for AgentId {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for AgentId {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_id_new_and_display() {
        let id = AgentId::new("alpha");
        assert_eq!(id.as_str(), "alpha");
        assert_eq!(id.to_string(), "alpha");
    }

    #[test]
    fn agent_id_equality() {
        let a = AgentId::new("alpha");
        let b = AgentId::from("alpha");
        let c = AgentId::new("beta");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn agent_id_generate_is_unique() {
        let a = AgentId::generate();
        let b = AgentId::generate();
        assert_ne!(a, b, "generated AgentIds must be unique");
        assert!(!a.as_str().is_empty());
        assert!(!b.as_str().is_empty());
    }

    #[test]
    fn agent_id_serde_roundtrip() {
        let id = AgentId::new("alpha");
        let json = serde_json::to_string(&id).expect("serialize");
        let back: AgentId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }
}
