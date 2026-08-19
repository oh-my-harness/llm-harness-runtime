use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;

use crate::{KnowledgeError, KnowledgeRef};

/// Trusted application-defined partition for knowledge data.
#[derive(Clone, PartialEq, Eq)]
pub struct KnowledgeScope {
    pub namespace: String,
    pub tenant: Option<String>,
    pub project: Option<String>,
    pub attributes: BTreeMap<String, String>,
}

impl KnowledgeScope {
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            tenant: None,
            project: None,
            attributes: BTreeMap::new(),
        }
    }
}

/// Authenticated subject supplied by the application, never by the model.
#[derive(Clone, PartialEq, Eq)]
pub struct PrincipalRef {
    pub subject: String,
    pub principal_type: String,
    pub attributes: BTreeMap<String, String>,
}

impl PrincipalRef {
    pub fn new(subject: impl Into<String>, principal_type: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            principal_type: principal_type.into(),
            attributes: BTreeMap::new(),
        }
    }
}

/// Operation controlled by the common knowledge authorization boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KnowledgeAction {
    Discover,
    Search,
    Read,
    Write,
    Delete,
}

/// A structural resource selector used to narrow delegated access.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum KnowledgeResourcePattern {
    Source { source_id: String },
    ItemPrefix { source_id: String, prefix: String },
    Domain { domain: String },
}

/// Maximum authority a child run may receive.
#[derive(Clone, PartialEq, Eq)]
pub struct DelegationConstraints {
    pub actions: BTreeSet<KnowledgeAction>,
    pub source_ids: BTreeSet<String>,
    pub domains: BTreeSet<String>,
    pub resource_patterns: Vec<KnowledgeResourcePattern>,
}

/// Opaque proof created and verified by trusted runtime infrastructure.
#[derive(Clone, PartialEq, Eq)]
pub struct DelegationProof(Vec<u8>);

impl DelegationProof {
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A time-bounded, verifiable grant attached to a delegated run.
#[derive(Clone, PartialEq, Eq)]
pub struct DelegationGrant {
    pub id: String,
    pub parent_access_fingerprint: String,
    pub constraints: DelegationConstraints,
    pub expires_at: DateTime<Utc>,
    pub depth: u32,
    pub proof: DelegationProof,
}

/// Trusted knowledge identity and scope for one run.
///
/// This type intentionally implements neither `Debug` nor serialization so
/// generic logging and Session persistence cannot expose principal attributes
/// or delegation proofs.
#[derive(Clone, PartialEq, Eq)]
pub struct KnowledgeAccessContext {
    pub scope: KnowledgeScope,
    pub principal: PrincipalRef,
    pub authorization_version: Option<String>,
    pub delegation: Option<DelegationGrant>,
}

impl KnowledgeAccessContext {
    pub fn new(scope: KnowledgeScope, principal: PrincipalRef) -> Self {
        Self {
            scope,
            principal,
            authorization_version: None,
            delegation: None,
        }
    }
}

/// Resource passed to an authorizer without exposing backend query syntax.
#[derive(Debug, Clone, Copy)]
pub enum KnowledgeResourceRef<'a> {
    Source {
        source_id: &'a str,
        domains: &'a [String],
    },
    Item(&'a KnowledgeRef),
}

/// Authorizer response. A denial is deliberately reason-free at this boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationDecision {
    Allow,
    Deny,
}

/// Application-defined source, item, and action authorization.
pub trait KnowledgeAuthorizer: Send + Sync {
    fn authorize<'a>(
        &'a self,
        access: &'a KnowledgeAccessContext,
        action: KnowledgeAction,
        resource: KnowledgeResourceRef<'a>,
    ) -> BoxFuture<'a, Result<AuthorizationDecision, KnowledgeError>>;
}

/// The sole common authorization boundary used by registries and memory.
pub struct KnowledgeAccessControl {
    authorizer: Arc<dyn KnowledgeAuthorizer>,
}

impl Default for KnowledgeAccessControl {
    fn default() -> Self {
        Self::new(Arc::new(DenyAllAuthorizer))
    }
}

impl KnowledgeAccessControl {
    pub fn new(authorizer: Arc<dyn KnowledgeAuthorizer>) -> Self {
        Self { authorizer }
    }

    /// Authorize an operation, mapping all explicit denials to fail-closed.
    pub async fn authorize(
        &self,
        access: &KnowledgeAccessContext,
        action: KnowledgeAction,
        resource: KnowledgeResourceRef<'_>,
    ) -> Result<(), KnowledgeError> {
        match self.authorizer.authorize(access, action, resource).await? {
            AuthorizationDecision::Allow => Ok(()),
            AuthorizationDecision::Deny => Err(KnowledgeError::Unauthorized),
        }
    }
}

/// Explicit opt-in authorizer for trusted single-user or test deployments.
#[derive(Debug, Default)]
pub struct AllowAllAuthorizer;

impl KnowledgeAuthorizer for AllowAllAuthorizer {
    fn authorize<'a>(
        &'a self,
        _access: &'a KnowledgeAccessContext,
        _action: KnowledgeAction,
        _resource: KnowledgeResourceRef<'a>,
    ) -> BoxFuture<'a, Result<AuthorizationDecision, KnowledgeError>> {
        Box::pin(async { Ok(AuthorizationDecision::Allow) })
    }
}

/// Safe default policy for applications that have not configured access.
#[derive(Debug, Default)]
pub struct DenyAllAuthorizer;

impl KnowledgeAuthorizer for DenyAllAuthorizer {
    fn authorize<'a>(
        &'a self,
        _access: &'a KnowledgeAccessContext,
        _action: KnowledgeAction,
        _resource: KnowledgeResourceRef<'a>,
    ) -> BoxFuture<'a, Result<AuthorizationDecision, KnowledgeError>> {
        Box::pin(async { Ok(AuthorizationDecision::Deny) })
    }
}

#[cfg(test)]
mod tests {
    use llm_harness_types::{RunContext, RunRequest};

    use super::*;
    use crate::KnowledgeRequestContext;

    fn access() -> KnowledgeAccessContext {
        KnowledgeAccessContext::new(
            KnowledgeScope::new("docs"),
            PrincipalRef::new("user-1", "user"),
        )
    }

    #[tokio::test]
    async fn allow_all_must_be_selected_explicitly() {
        let control = KnowledgeAccessControl::new(Arc::new(AllowAllAuthorizer));
        control
            .authorize(
                &access(),
                KnowledgeAction::Search,
                KnowledgeResourceRef::Source {
                    source_id: "docs",
                    domains: &[],
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn default_access_control_fails_closed() {
        let control = KnowledgeAccessControl::default();
        let error = control
            .authorize(
                &access(),
                KnowledgeAction::Read,
                KnowledgeResourceRef::Source {
                    source_id: "docs",
                    domains: &[],
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), crate::KnowledgeErrorCode::Unauthorized);
    }

    #[test]
    fn missing_run_access_context_fails_closed() {
        let run = RunContext::new(RunRequest::from_text("question"));
        let error = match KnowledgeRequestContext::from_run(&run) {
            Ok(_) => panic!("missing access context must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error.code(), crate::KnowledgeErrorCode::Unauthorized);
    }

    #[test]
    fn access_context_is_read_from_typed_run_extension() {
        let run = RunContext::new(RunRequest::from_text("question").with_extension(access()));
        let context = KnowledgeRequestContext::from_run(&run).unwrap();
        assert_eq!(context.access.principal.subject, "user-1");
    }
}
