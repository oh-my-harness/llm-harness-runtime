use std::fmt;

use futures::future::BoxFuture;
use llm_harness_knowledge::{KnowledgeRef, KnowledgeRequestContext};
use tokio_util::sync::CancellationToken;

use crate::{MemoryMutationOrigin, MemoryWrite};

/// The exact normalized mutation intent presented to a trusted application gate.
///
/// This type intentionally implements neither `Debug` nor serialization so
/// memory content and exact references are not accidentally written by generic
/// diagnostics or persistence code.
#[derive(Clone, PartialEq, Eq)]
pub enum MemoryMutation {
    Write { write: MemoryWrite },
    Delete { reference: KnowledgeRef },
}

/// One run-bound mutation authorization request.
///
/// The application gate may bind its approval request to `ctx.run.id()`, a
/// trusted session extension, `origin`, and the complete normalized mutation.
/// It owns request IDs, expiry, single-use semantics, disconnect handling, and
/// replay protection.
#[derive(Clone, PartialEq, Eq)]
pub struct MemoryMutationRequest {
    pub mutation: MemoryMutation,
    pub origin: MemoryMutationOrigin,
}

#[derive(thiserror::Error, Clone, PartialEq, Eq)]
pub enum MemoryMutationGateError {
    #[error("memory mutation was not approved")]
    Denied,
    #[error("memory mutation approval is unavailable")]
    Unavailable,
    #[error("memory mutation approval was aborted")]
    Aborted,
    #[error("memory mutation approval failed")]
    Backend(String),
}

impl MemoryMutationGateError {
    /// Return internal detail for an explicitly controlled diagnostic sink.
    pub fn diagnostic(&self) -> Option<&str> {
        match self {
            Self::Backend(detail) => Some(detail),
            Self::Denied | Self::Unavailable | Self::Aborted => None,
        }
    }
}

impl fmt::Debug for MemoryMutationGateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied => formatter.write_str("Denied"),
            Self::Unavailable => formatter.write_str("Unavailable"),
            Self::Aborted => formatter.write_str("Aborted"),
            Self::Backend(_) => formatter.write_str("Backend"),
        }
    }
}

/// Mandatory trusted boundary for every memory mutation.
///
/// Runtime access control still authorizes the principal and source, and the
/// write policy still validates and normalizes content. This gate is invoked
/// afterwards with the exact mutation that would reach the store. Applications
/// implement interactive approval, pre-authorized workflow operations, or
/// another explicit trusted policy here. There is deliberately no permissive
/// default implementation.
pub trait MemoryMutationGate: Send + Sync {
    fn authorize<'a>(
        &'a self,
        ctx: KnowledgeRequestContext<'a>,
        request: MemoryMutationRequest,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), MemoryMutationGateError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_detail_requires_an_explicit_diagnostic_sink() {
        let error = MemoryMutationGateError::Backend("approval service token leaked".into());

        assert_eq!(format!("{error:?}"), "Backend");
        assert_eq!(error.to_string(), "memory mutation approval failed");
        assert_eq!(error.diagnostic(), Some("approval service token leaked"));
    }
}
