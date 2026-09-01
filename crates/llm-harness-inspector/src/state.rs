//! Shared state for Inspector route handlers.

use llm_harness_agent::AgentHarness;
use parking_lot::RwLock;
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

    /// Check a provided token matches in constant time.
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

    /// Return the secret string (for the startup log / URL fragment).
    pub fn as_str(&self) -> &str {
        &self.secret
    }
}

/// Registry of inspectable harnesses, keyed by agent display name.
///
/// One agent is the *default* — used when a request carries no `?agent=`
/// query. This keeps every existing single-agent caller and the original
/// UI working unchanged.
pub struct HarnessRegistry {
    agents: RwLock<std::collections::HashMap<String, Arc<AgentHarness>>>,
    default_key: String,
}

impl HarnessRegistry {
    /// New registry with `default_key` as the implicit agent.
    pub fn new(default_key: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            agents: RwLock::new(std::collections::HashMap::new()),
            default_key: default_key.into(),
        })
    }

    /// Register (or replace) the harness for `key`.
    pub fn register(&self, key: impl Into<String>, harness: Arc<AgentHarness>) {
        self.agents.write().insert(key.into(), harness);
    }

    /// Remove a harness by key (team deleted at runtime).
    pub fn remove(&self, key: &str) {
        self.agents.write().remove(key);
    }

    /// Resolve a harness: explicit key, or the default when `None`.
    /// Unknown key → None.
    pub fn get(&self, key: Option<&str>) -> Option<Arc<AgentHarness>> {
        let agents = self.agents.read();
        match key {
            Some(k) => agents.get(k).cloned(),
            None => agents.get(&self.default_key).cloned(),
        }
    }

    /// All registered agent keys, sorted for stable UI ordering.
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.agents.read().keys().cloned().collect();
        keys.sort();
        keys
    }

    pub fn default_key(&self) -> &str {
        &self.default_key
    }
}

/// External-event channel: lets code outside the harness tree (bench
/// operator loops, CI probes) publish conversation lines into the UI as a
/// virtual agent. Lines are buffered so late subscribers (agent switching)
/// still see the full history.
#[derive(Clone)]
pub struct SharedEvents {
    sender: tokio::sync::broadcast::Sender<serde_json::Value>,
    buffer: Arc<parking_lot::Mutex<Vec<serde_json::Value>>>,
}

impl SharedEvents {
    pub fn new() -> Arc<Self> {
        let (sender, _) = tokio::sync::broadcast::channel(1024);
        Arc::new(Self {
            sender,
            buffer: Arc::new(parking_lot::Mutex::new(Vec::new())),
        })
    }

    /// Publish one line for virtual agent `agent`.
    pub fn publish(&self, agent: &str, kind: &str, text: &str) {
        let line = serde_json::json!({
            "agent": agent,
            "kind": kind, // "user" | "assistant" | "system"
            "text": text,
        });
        self.buffer.lock().push(line.clone());
        let _ = self.sender.send(line);
    }

    /// Full buffer (late-subscriber replay).
    pub fn replay(&self) -> Vec<serde_json::Value> {
        self.buffer.lock().clone()
    }

    /// Distinct agent names that have published at least one line.
    pub fn virtual_agents(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .buffer
            .lock()
            .iter()
            .filter_map(|l| l.get("agent").and_then(|a| a.as_str()).map(String::from))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<serde_json::Value> {
        self.sender.subscribe()
    }
}

impl Default for SharedEvents {
    fn default() -> Self {
        // Self::new returns Arc; a plain default mirrors it field-wise.
        let (sender, _) = tokio::sync::broadcast::channel(1024);
        Self {
            sender,
            buffer: Arc::new(parking_lot::Mutex::new(Vec::new())),
        }
    }
}

/// Shared inspector state: the harness registry plus the auth token.
#[derive(Clone)]
pub struct InspectorState {
    pub registry: Arc<HarnessRegistry>,
    pub token: AuthToken,
    /// External conversation lines (virtual agents like "operator").
    pub shared: Option<Arc<SharedEvents>>,
}

impl InspectorState {
    /// Resolve the harness an agent-keyed request targets. `None` = the
    /// default agent.
    pub fn resolve(&self, key: Option<&str>) -> Result<Arc<AgentHarness>, String> {
        self.registry.get(key).ok_or_else(|| {
            format!(
                "unknown agent {:?} (known: {:?})",
                key.unwrap_or(self.registry.default_key()),
                self.registry.keys(),
            )
        })
    }
}
