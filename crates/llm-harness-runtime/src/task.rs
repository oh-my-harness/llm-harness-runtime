use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use llm_harness_types::EntryId;
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
    /// Total turns executed.
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

/// Task trace — summary data passed to `TaskVerifier`.
#[derive(Debug, Clone)]
pub struct TaskTrace {
    /// Task identifier.
    pub task_id: TaskId,
    /// All session IDs that belonged to this task.
    pub session_ids: Vec<EntryId>,
    /// All checkpoints recorded during this task.
    pub checkpoints: Vec<Checkpoint>,
    /// Final state snapshot (JSON).
    pub final_state: Value,
}

/// Verdict from a task verifier.
#[derive(Debug, Clone)]
pub enum TaskVerdict {
    Pass,
    Fail { reason: String },
    Indeterminate { reason: String },
}

/// Task completion verifier (business layer implements).
pub trait TaskVerifier: Send + Sync {
    /// Verify task completion and return a verdict.
    fn verify<'a>(&'a self, trace: &'a TaskTrace) -> BoxFuture<'a, TaskVerdict>;
}

/// Retry policy.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of retries before marking the task as failed.
    pub max_retries: usize,
    /// Delay between retries in milliseconds.
    pub backoff_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            backoff_ms: 1000,
        }
    }
}

