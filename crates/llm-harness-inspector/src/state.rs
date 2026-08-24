//! Shared state for Inspector route handlers.

use llm_harness_agent::AgentHarness;
use std::sync::Arc;

/// A constant-time string comparison guard for the inspector token.
#[derive(Clone)]
pub struct AuthToken {
    secret: Arc<String>,
}

impl AuthToken {
    pub fn new(secret: String) -> Self {
        Self {
            secret: Arc::new(secret),
        }
    }

    /// Generate a random token.
    pub fn generate() -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        // Simple random token from system entropy — not crypto-grade, but
        // sufficient for local-only inspector auth.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        let secret = format!("{pid:x}-{nanos:x}");
        Self::new(secret)
    }

    /// Check if a provided token matches, in constant time.
    pub fn matches(&self, provided: &str) -> bool {
        let a = self.secret.as_bytes();
        let b = provided.as_bytes();
        if a.len() != b.len() {
            return false;
        }
        let mut diff = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        diff == 0
    }

    /// Return the secret string (for logging at server startup).
    pub fn as_str(&self) -> &str {
        &self.secret
    }
}

#[derive(Clone)]
pub struct InspectorState {
    pub harness: Arc<AgentHarness>,
    pub token: AuthToken,
}
