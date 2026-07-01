use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cost::CostAggregate;

/// Task identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub String);

/// Task-level error.
#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskError {
    #[error("auth error: {0}")]
    AuthError(String),
    #[error("task cancelled: {0}")]
    Cancelled(String),
    #[error("budget exceeded: limit={limit}, spent={spent}")]
    BudgetExceeded { limit: f64, spent: f64 },
    #[error("retries exhausted: max={max}")]
    RetriesExhausted { max: usize },
    #[error("internal: {0}")]
    Internal(String),
}


/// Task completion result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    /// Aggregated cost across all sessions.
    pub cost: CostAggregate,
    /// Number of completed steps (step_history entries with result: Some).
    /// Note: in the workflow engine, this counts completed workflow steps,
    /// not individual LLM turns within a step.
    pub turns: u32,
    /// Final assistant message, if any.
    pub final_message: Option<String>,
}

/// Task-level checkpoint (supplementary to session checkpoints).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Monotonically increasing sequence number.
    pub seq: usize,
    /// When the checkpoint was saved.
    pub saved_at: DateTime<Utc>,
    /// Human-readable description.
    pub description: String,
    /// Arbitrary payload (JSON).
    pub payload: Value,
}


