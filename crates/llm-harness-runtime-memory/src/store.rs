use std::{collections::BTreeMap, time::Duration};

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use llm_harness_runtime_knowledge::{KnowledgeError, KnowledgeRef, KnowledgeRequestContext};
use llm_harness_types::RunId;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Minimal model-facing write intent. Trusted metadata is added by policy.
#[derive(Clone, PartialEq, Eq)]
pub struct MemoryWriteIntent {
    pub content: String,
    pub kind: Option<String>,
    pub requested_ttl: Option<Duration>,
}

/// Trusted origin of a memory mutation.
///
/// These values are supplied by runtime integration code, never deserialized
/// directly from model tool arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MemoryMutationOrigin {
    ExplicitTool {
        tool_use_id: String,
    },
    Application {
        operation_id: String,
    },
    Capture {
        policy_id: String,
        candidate_id: String,
    },
}

/// Stable session identity optionally attached to a run as a typed extension.
#[derive(Clone, PartialEq, Eq)]
pub struct MemorySessionId(String);

impl MemorySessionId {
    pub fn new(value: impl Into<String>) -> Result<Self, MemorySessionIdError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(MemorySessionIdError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
#[error("memory session ID must not be empty")]
pub struct MemorySessionIdError;

/// Server-created provenance persisted with a memory record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryProvenance {
    pub run_id: RunId,
    pub session_id: Option<String>,
    pub origin: MemoryMutationOrigin,
    pub recorded_at: DateTime<Utc>,
}

/// Internal record passed from policy to the write store.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryWrite {
    pub content: String,
    pub kind: Option<String>,
    pub metadata: BTreeMap<String, serde_json::Value>,
    pub provenance: MemoryProvenance,
    pub idempotency_key: String,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryConsistency {
    Immediate,
    Eventual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryStoreDescriptor {
    /// References returned by this store must belong to this read source.
    pub read_source_id: String,
    pub consistency: MemoryConsistency,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MemoryVisibility {
    Visible,
    Pending { checkpoint: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryWriteReceipt {
    pub reference: KnowledgeRef,
    pub visibility: MemoryVisibility,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryDeleteReceipt {
    pub reference: KnowledgeRef,
    pub visibility: MemoryVisibility,
}

/// Write-side memory backend. It intentionally does not inherit
/// `KnowledgeSource`; applications may use separate write and read models.
pub trait MemoryStore: Send + Sync {
    fn descriptor(&self) -> &MemoryStoreDescriptor;

    fn upsert<'a>(
        &'a self,
        ctx: KnowledgeRequestContext<'a>,
        write: MemoryWrite,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<MemoryWriteReceipt, KnowledgeError>>;

    fn delete<'a>(
        &'a self,
        ctx: KnowledgeRequestContext<'a>,
        reference: KnowledgeRef,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<MemoryDeleteReceipt, KnowledgeError>>;
}
