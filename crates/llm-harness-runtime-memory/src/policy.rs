use std::fmt;

use futures::future::BoxFuture;
use llm_harness_runtime_knowledge::KnowledgeRequestContext;
use tokio_util::sync::CancellationToken;

use crate::{MemoryProvenance, MemoryWrite, MemoryWriteIntent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryPolicyRejection {
    EmptyContent,
    ContentTooLarge,
    SensitiveContent,
    UnsupportedKind,
    InvalidTtl,
    Other,
}

impl MemoryPolicyRejection {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EmptyContent => "empty_content",
            Self::ContentTooLarge => "content_too_large",
            Self::SensitiveContent => "sensitive_content",
            Self::UnsupportedKind => "unsupported_kind",
            Self::InvalidTtl => "invalid_ttl",
            Self::Other => "rejected",
        }
    }
}

#[derive(thiserror::Error, Clone, PartialEq, Eq)]
pub enum MemoryPolicyError {
    #[error("memory write was rejected by policy")]
    Rejected(MemoryPolicyRejection),
    #[error("memory write policy was aborted")]
    Aborted,
    #[error("memory write policy failed")]
    Backend(String),
}

impl MemoryPolicyError {
    pub fn rejection(&self) -> Option<MemoryPolicyRejection> {
        match self {
            Self::Rejected(rejection) => Some(*rejection),
            Self::Aborted | Self::Backend(_) => None,
        }
    }

    /// Return internal detail for an explicitly controlled diagnostic sink.
    pub fn diagnostic(&self) -> Option<&str> {
        match self {
            Self::Backend(detail) => Some(detail),
            Self::Rejected(_) | Self::Aborted => None,
        }
    }
}

impl fmt::Debug for MemoryPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(rejection) => {
                formatter.debug_tuple("Rejected").field(rejection).finish()
            }
            Self::Aborted => formatter.write_str("Aborted"),
            Self::Backend(_) => formatter.write_str("Backend"),
        }
    }
}

/// Trusted policy boundary that turns model intent into a persistable record.
///
/// Implementations own validation, normalization, TTL selection, metadata,
/// secret rejection, and idempotency-key generation.
pub trait MemoryWritePolicy: Send + Sync {
    fn prepare<'a>(
        &'a self,
        ctx: KnowledgeRequestContext<'a>,
        intent: MemoryWriteIntent,
        provenance: MemoryProvenance,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<MemoryWrite, MemoryPolicyError>>;
}
