//! Shared state for Inspector route handlers.

use llm_harness_agent::AgentHarness;
use std::sync::Arc;

#[derive(Clone)]
pub struct InspectorState {
    pub harness: Arc<AgentHarness>,
}
