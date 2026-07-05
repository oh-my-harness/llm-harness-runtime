use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use llm_harness_types::EntryId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;

/// Type of audit event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuditEventType {
    ToolCall,
    Auth,
    HumanApproval,
    BudgetExceeded,
    StateTransition,
}

/// Audit decision applied to an event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuditDecision {
    Allow,
    Deny { reason: String },
    Modify { before: Value, after: Value },
}

/// Cost delta attributed to a single audit event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostDelta {
    /// Input tokens consumed.
    pub input_tokens: u64,
    /// Output tokens generated.
    pub output_tokens: u64,
    /// Estimated cost in USD.
    pub estimated_cost: f64,
}

/// A single audit log entry with hash chain integrity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Event timestamp.
    pub timestamp: DateTime<Utc>,
    /// Links back to OTel trace.
    pub trace_id: String,
    /// Session that produced this event.
    pub session_id: EntryId,
    /// Task identifier (if applicable).
    pub task_id: Option<String>,
    /// Who/what triggered this event: `"user"`, `"agent"`, or `"system"`.
    pub principal: String,
    /// Event type.
    pub event_type: AuditEventType,
    /// Event-specific details.
    pub payload: Value,
    /// Decision applied (if applicable).
    pub decision: Option<AuditDecision>,
    /// Cost attributed to this event (if applicable).
    pub cost_delta: Option<CostDelta>,
    /// SHA-256 of the previous entry (for hash chain integrity).
    pub prev_hash: Option<String>,
    /// SHA-256 of this entry: `SHA-256(serialize(content) + ":" + prev_hash)`.
    pub hash: String,
}

/// Audit sink — backend-agnostic audit log persistence.
pub trait AuditSink: Send + Sync {
    /// Record a single audit entry.
    fn record(&self, entry: AuditEntry) -> BoxFuture<'_, Result<()>>;

    /// Flush any buffered entries.
    fn flush(&self) -> BoxFuture<'_, Result<()>>;
}
