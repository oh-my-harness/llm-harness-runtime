use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    time::Duration,
};

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use llm_harness_types::{DataBlock, RunContext};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::KnowledgeAccessContext;

/// Stable capabilities advertised by a knowledge source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeCapability {
    Search,
    Read,
    Revisioned,
    SectionRead,
    LineRead,
    ChunkRead,
}

/// Expected update behavior of a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessClass {
    Static,
    Periodic,
    NearRealTime,
    Live,
}

/// Value shape accepted by a declared source filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterValueType {
    String,
    Number,
    Boolean,
    DateTime,
}

/// Operators accepted by a declared source filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOperator {
    Eq,
    In,
    Range,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterFieldDescriptor {
    pub name: String,
    pub value_type: FilterValueType,
    pub operators: BTreeSet<FilterOperator>,
}

/// Stable description used for catalog discovery and request validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeSourceDescriptor {
    pub id: String,
    pub name: String,
    pub description: String,
    pub domains: Vec<String>,
    pub capabilities: BTreeSet<KnowledgeCapability>,
    pub freshness: FreshnessClass,
    pub filter_fields: Vec<FilterFieldDescriptor>,
}

/// Stable opaque reference. It is never an authorization credential.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct KnowledgeRef {
    pub source_id: String,
    pub item_id: String,
    pub revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionRef {
    pub id: String,
    pub title: Option<String>,
}

/// Source-neutral content location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContentSelector {
    Document,
    Sections {
        sections: Vec<SectionRef>,
    },
    /// 1-indexed logical line range, inclusive at both ends.
    LineRange {
        start: u32,
        end: u32,
    },
    Chunks {
        ids: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeHit {
    pub reference: KnowledgeRef,
    pub title: Option<String>,
    pub snippet: String,
    pub suggested_selectors: Vec<ContentSelector>,
    pub uri: Option<String>,
    pub score: Option<f32>,
    pub updated_at: Option<DateTime<Utc>>,
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// Backend-neutral, allowlisted filter expression.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operator", rename_all = "snake_case")]
pub enum FilterExpr {
    Eq {
        field: String,
        value: serde_json::Value,
    },
    In {
        field: String,
        values: Vec<serde_json::Value>,
    },
    Range {
        field: String,
        gte: Option<serde_json::Value>,
        lte: Option<serde_json::Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceFilter {
    pub source_id: String,
    pub expression: FilterExpr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceSearchRequest {
    pub query: String,
    pub filters: Vec<FilterExpr>,
    pub limit: usize,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceSearchPage {
    pub hits: Vec<KnowledgeHit>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeSearchRequest {
    pub query: String,
    pub domains: Vec<String>,
    pub source_ids: Vec<String>,
    pub filters: Vec<SourceFilter>,
    pub limit: usize,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FederatedKnowledgePage {
    pub hits: Vec<KnowledgeHit>,
    pub next_cursor: Option<String>,
    pub source_failures: Vec<SourceFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeReadRequest {
    pub reference: KnowledgeRef,
    pub selector: ContentSelector,
    pub max_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeContent {
    pub reference: KnowledgeRef,
    pub selector: ContentSelector,
    pub title: Option<String>,
    pub blocks: Vec<DataBlock>,
    pub uri: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
    pub obtained_at: DateTime<Utc>,
    pub truncated: bool,
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// Stable error category safe for aggregation and public error mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeErrorCode {
    Unauthorized,
    NotFound,
    StaleReference,
    InvalidCursor,
    InvalidFilter,
    UnsupportedCapability,
    RateLimited,
    Timeout,
    Aborted,
    Backend,
}

impl KnowledgeErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::NotFound => "not_found",
            Self::StaleReference => "stale_reference",
            Self::InvalidCursor => "invalid_cursor",
            Self::InvalidFilter => "invalid_filter",
            Self::UnsupportedCapability => "unsupported_capability",
            Self::RateLimited => "rate_limited",
            Self::Timeout => "timeout",
            Self::Aborted => "aborted",
            Self::Backend => "backend",
        }
    }
}

/// Sanitized source failure included in a partially successful federated page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFailure {
    pub source_id: String,
    pub code: KnowledgeErrorCode,
    pub retryable: bool,
}

/// Internal knowledge protocol error.
///
/// Backend diagnostics are retained for controlled diagnostics, while
/// `Display`, `code`, and `SourceFailure` remain sanitized.
#[derive(thiserror::Error, Clone, PartialEq)]
pub enum KnowledgeError {
    #[error("knowledge access is unauthorized")]
    Unauthorized,
    #[error("knowledge item was not found")]
    NotFound,
    #[error("the requested knowledge revision is unavailable")]
    StaleReference { latest: Option<KnowledgeRef> },
    #[error("knowledge cursor is invalid")]
    InvalidCursor,
    #[error("knowledge filter is invalid")]
    InvalidFilter(String),
    #[error("knowledge capability is unsupported")]
    UnsupportedCapability(String),
    #[error("knowledge source is rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("knowledge operation timed out")]
    Timeout,
    #[error("knowledge operation was aborted")]
    Aborted,
    #[error("knowledge backend failed")]
    Backend(String),
}

impl KnowledgeError {
    pub fn code(&self) -> KnowledgeErrorCode {
        match self {
            Self::Unauthorized => KnowledgeErrorCode::Unauthorized,
            Self::NotFound => KnowledgeErrorCode::NotFound,
            Self::StaleReference { .. } => KnowledgeErrorCode::StaleReference,
            Self::InvalidCursor => KnowledgeErrorCode::InvalidCursor,
            Self::InvalidFilter(_) => KnowledgeErrorCode::InvalidFilter,
            Self::UnsupportedCapability(_) => KnowledgeErrorCode::UnsupportedCapability,
            Self::RateLimited { .. } => KnowledgeErrorCode::RateLimited,
            Self::Timeout => KnowledgeErrorCode::Timeout,
            Self::Aborted => KnowledgeErrorCode::Aborted,
            Self::Backend(_) => KnowledgeErrorCode::Backend,
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. } | Self::Timeout | Self::Backend(_)
        )
    }

    pub fn source_failure(&self, source_id: impl Into<String>) -> SourceFailure {
        SourceFailure {
            source_id: source_id.into(),
            code: self.code(),
            retryable: self.retryable(),
        }
    }

    /// Return internal detail for an explicitly controlled diagnostic sink.
    pub fn diagnostic(&self) -> Option<&str> {
        match self {
            Self::InvalidFilter(detail)
            | Self::UnsupportedCapability(detail)
            | Self::Backend(detail) => Some(detail),
            _ => None,
        }
    }
}

impl fmt::Debug for KnowledgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KnowledgeError")
            .field("code", &self.code())
            .field("retryable", &self.retryable())
            .finish()
    }
}

/// Per-operation context joining immutable run state with trusted access data.
#[derive(Clone, Copy)]
pub struct KnowledgeRequestContext<'a> {
    pub run: &'a RunContext,
    pub access: &'a KnowledgeAccessContext,
}

impl<'a> KnowledgeRequestContext<'a> {
    /// Build a request context from the run's typed access extension.
    pub fn from_run(run: &'a RunContext) -> Result<Self, KnowledgeError> {
        let access = run
            .extension::<KnowledgeAccessContext>()
            .ok_or(KnowledgeError::Unauthorized)?;
        Ok(Self { run, access })
    }
}

/// Read-only, backend-neutral knowledge source contract.
pub trait KnowledgeSource: Send + Sync {
    fn descriptor(&self) -> &KnowledgeSourceDescriptor;

    fn search<'a>(
        &'a self,
        ctx: KnowledgeRequestContext<'a>,
        request: SourceSearchRequest,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<SourceSearchPage, KnowledgeError>>;

    fn read<'a>(
        &'a self,
        ctx: KnowledgeRequestContext<'a>,
        request: KnowledgeReadRequest,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<KnowledgeContent, KnowledgeError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_diagnostics_are_not_displayed_or_aggregated() {
        let error = KnowledgeError::Backend(
            "postgres://secret@host/db: permission denied for tenant_42".into(),
        );
        assert_eq!(error.to_string(), "knowledge backend failed");
        assert!(!format!("{error:?}").contains("secret"));
        assert!(error.diagnostic().unwrap().contains("secret"));

        let failure = error.source_failure("records");
        let json = serde_json::to_string(&failure).unwrap();
        assert!(!json.contains("secret"));
        assert_eq!(failure.code, KnowledgeErrorCode::Backend);
        assert!(failure.retryable);
    }

    #[test]
    fn knowledge_reference_round_trips_without_becoming_a_credential() {
        let reference = KnowledgeRef {
            source_id: "docs".into(),
            item_id: "opaque-1".into(),
            revision: Some("rev-3".into()),
        };
        let encoded = serde_json::to_string(&reference).unwrap();
        let decoded: KnowledgeRef = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, reference);
    }
}
