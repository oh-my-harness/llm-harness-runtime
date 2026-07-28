use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use llm_harness_runtime_knowledge::{KnowledgeAccessContext, KnowledgeRequestContext};
use sha2::Sha256;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::{
    MemoryMutationOrigin, MemoryPolicyError, MemoryPolicyRejection, MemoryProvenance, MemoryWrite,
    MemoryWriteIntent, MemoryWritePolicy,
};

type HmacSha256 = Hmac<Sha256>;

pub trait MemoryContentGuard: Send + Sync {
    fn check(&self, content: &str) -> Result<(), MemoryPolicyRejection>;
}

/// Conservative guard for common credentials and private-key material.
#[derive(Debug, Default)]
pub struct DefaultMemoryContentGuard;

impl MemoryContentGuard for DefaultMemoryContentGuard {
    fn check(&self, content: &str) -> Result<(), MemoryPolicyRejection> {
        let lowercase = content.to_ascii_lowercase();
        let fixed_markers = [
            "-----begin private key-----",
            "-----begin rsa private key-----",
            "-----begin ec private key-----",
            "authorization: bearer ",
        ];
        if fixed_markers
            .iter()
            .any(|marker| lowercase.contains(marker))
            || content
                .split(|character: char| character.is_whitespace() || character == '"')
                .any(looks_like_secret_token)
            || lowercase.lines().any(has_sensitive_assignment)
        {
            Err(MemoryPolicyRejection::SensitiveContent)
        } else {
            Ok(())
        }
    }
}

fn looks_like_secret_token(value: &str) -> bool {
    let value = value.trim_matches(|character: char| {
        matches!(character, ',' | ';' | '\'' | ')' | '(' | ']' | '[')
    });
    (value.starts_with("sk-") && value.len() >= 20)
        || (value.starts_with("AKIA")
            && value.len() == 20
            && value
                .chars()
                .all(|character| character.is_ascii_alphanumeric()))
}

fn has_sensitive_assignment(line: &str) -> bool {
    const LABELS: [&str; 7] = [
        "api_key",
        "apikey",
        "access_token",
        "refresh_token",
        "client_secret",
        "password",
        "passwd",
    ];
    let Some((label, value)) = line.split_once('=').or_else(|| line.split_once(':')) else {
        return false;
    };
    let normalized_label = label
        .trim()
        .trim_matches(|character: char| matches!(character, '"' | '\'' | '-' | '*'));
    let value = value.trim().trim_matches(['"', '\'']);
    LABELS.contains(&normalized_label)
        && !value.is_empty()
        && !matches!(
            value,
            "***" | "none" | "unknown" | "redacted" | "<redacted>"
        )
}

#[derive(Clone)]
pub struct SecureMemoryWritePolicyConfig {
    pub max_content_bytes: usize,
    pub allowed_kinds: Option<BTreeSet<String>>,
    pub default_ttl: Option<Duration>,
    pub max_ttl: Duration,
    pub metadata: BTreeMap<String, serde_json::Value>,
}

impl Default for SecureMemoryWritePolicyConfig {
    fn default() -> Self {
        Self {
            max_content_bytes: 16 * 1024,
            allowed_kinds: None,
            default_ttl: None,
            max_ttl: Duration::from_secs(365 * 24 * 60 * 60),
            metadata: BTreeMap::new(),
        }
    }
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum SecureMemoryWritePolicyBuildError {
    #[error("memory policy secret must contain at least 32 bytes")]
    SecretTooShort,
    #[error("memory policy max content bytes must be greater than zero")]
    ZeroContentLimit,
    #[error("memory policy max TTL must be greater than zero")]
    ZeroMaxTtl,
    #[error("memory policy default TTL exceeds max TTL")]
    DefaultTtlExceedsMaximum,
    #[error("memory policy allowed kinds must not contain empty values")]
    EmptyAllowedKind,
}

pub struct SecureMemoryWritePolicy {
    secret: Zeroizing<Vec<u8>>,
    config: SecureMemoryWritePolicyConfig,
    guard: Arc<dyn MemoryContentGuard>,
}

impl SecureMemoryWritePolicy {
    pub fn new(
        secret: Vec<u8>,
        config: SecureMemoryWritePolicyConfig,
    ) -> Result<Self, SecureMemoryWritePolicyBuildError> {
        Self::with_guard(secret, config, Arc::new(DefaultMemoryContentGuard))
    }

    pub fn with_guard(
        secret: Vec<u8>,
        mut config: SecureMemoryWritePolicyConfig,
        guard: Arc<dyn MemoryContentGuard>,
    ) -> Result<Self, SecureMemoryWritePolicyBuildError> {
        if secret.len() < 32 {
            return Err(SecureMemoryWritePolicyBuildError::SecretTooShort);
        }
        if config.max_content_bytes == 0 {
            return Err(SecureMemoryWritePolicyBuildError::ZeroContentLimit);
        }
        if config.max_ttl.is_zero() {
            return Err(SecureMemoryWritePolicyBuildError::ZeroMaxTtl);
        }
        if config
            .default_ttl
            .is_some_and(|default_ttl| default_ttl > config.max_ttl)
        {
            return Err(SecureMemoryWritePolicyBuildError::DefaultTtlExceedsMaximum);
        }
        if config
            .allowed_kinds
            .as_ref()
            .is_some_and(|kinds| kinds.iter().any(|kind| kind.trim().is_empty()))
        {
            return Err(SecureMemoryWritePolicyBuildError::EmptyAllowedKind);
        }
        config.allowed_kinds = config.allowed_kinds.map(|kinds| {
            kinds
                .into_iter()
                .map(|kind| kind.trim().to_string())
                .collect()
        });
        Ok(Self {
            secret: Zeroizing::new(secret),
            config,
            guard,
        })
    }

    fn prepare_write(
        &self,
        ctx: KnowledgeRequestContext<'_>,
        intent: MemoryWriteIntent,
        provenance: MemoryProvenance,
        abort: &CancellationToken,
    ) -> Result<MemoryWrite, MemoryPolicyError> {
        if abort.is_cancelled() {
            return Err(MemoryPolicyError::Aborted);
        }
        let content = intent.content.trim().replace("\r\n", "\n");
        if content.is_empty() {
            return Err(MemoryPolicyError::Rejected(
                MemoryPolicyRejection::EmptyContent,
            ));
        }
        if content.len() > self.config.max_content_bytes {
            return Err(MemoryPolicyError::Rejected(
                MemoryPolicyRejection::ContentTooLarge,
            ));
        }
        self.guard
            .check(&content)
            .map_err(MemoryPolicyError::Rejected)?;
        let kind = intent.kind.map(|kind| kind.trim().to_string());
        if kind.as_ref().is_some_and(String::is_empty) {
            return Err(MemoryPolicyError::Rejected(
                MemoryPolicyRejection::UnsupportedKind,
            ));
        }
        if let Some(allowed) = &self.config.allowed_kinds
            && kind.as_ref().is_some_and(|value| !allowed.contains(value))
        {
            return Err(MemoryPolicyError::Rejected(
                MemoryPolicyRejection::UnsupportedKind,
            ));
        }
        let ttl = intent.requested_ttl.or(self.config.default_ttl);
        if ttl.is_some_and(|ttl| ttl.is_zero() || ttl > self.config.max_ttl) {
            return Err(MemoryPolicyError::Rejected(
                MemoryPolicyRejection::InvalidTtl,
            ));
        }
        let expires_at = ttl
            .map(chrono::Duration::from_std)
            .transpose()
            .map_err(|_| MemoryPolicyError::Rejected(MemoryPolicyRejection::InvalidTtl))?
            .and_then(|ttl| provenance.recorded_at.checked_add_signed(ttl));
        if ttl.is_some() && expires_at.is_none() {
            return Err(MemoryPolicyError::Rejected(
                MemoryPolicyRejection::InvalidTtl,
            ));
        }
        let idempotency_key = idempotency_key(
            &self.secret,
            ctx.access,
            &provenance,
            &content,
            kind.as_deref(),
            ttl,
        );
        Ok(MemoryWrite {
            content,
            kind,
            metadata: self.config.metadata.clone(),
            provenance,
            idempotency_key,
            expires_at,
        })
    }
}

impl MemoryWritePolicy for SecureMemoryWritePolicy {
    fn prepare<'a>(
        &'a self,
        ctx: KnowledgeRequestContext<'a>,
        intent: MemoryWriteIntent,
        provenance: MemoryProvenance,
        abort: CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<MemoryWrite, MemoryPolicyError>> {
        Box::pin(async move { self.prepare_write(ctx, intent, provenance, &abort) })
    }
}

fn idempotency_key(
    secret: &[u8],
    access: &KnowledgeAccessContext,
    provenance: &MemoryProvenance,
    content: &str,
    kind: Option<&str>,
    ttl: Option<Duration>,
) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key");
    update(&mut mac, "memory-write-v1");
    update_access(&mut mac, access);
    update(&mut mac, &provenance.run_id.to_string());
    update(&mut mac, provenance.session_id.as_deref().unwrap_or(""));
    match &provenance.origin {
        MemoryMutationOrigin::ExplicitTool { tool_use_id } => {
            update(&mut mac, "explicit_tool");
            update(&mut mac, tool_use_id);
        }
        MemoryMutationOrigin::Application { operation_id } => {
            update(&mut mac, "application");
            update(&mut mac, operation_id);
        }
        MemoryMutationOrigin::Capture {
            policy_id,
            candidate_id,
        } => {
            update(&mut mac, "capture");
            update(&mut mac, policy_id);
            update(&mut mac, candidate_id);
        }
    }
    update(&mut mac, content);
    update(&mut mac, kind.unwrap_or(""));
    update(
        &mut mac,
        &ttl.map(|value| value.as_nanos()).unwrap_or(0).to_string(),
    );
    format!(
        "mem_{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}

fn update_access(mac: &mut HmacSha256, access: &KnowledgeAccessContext) {
    update(mac, &access.scope.namespace);
    update(mac, access.scope.tenant.as_deref().unwrap_or(""));
    update(mac, access.scope.project.as_deref().unwrap_or(""));
    for (key, value) in &access.scope.attributes {
        update(mac, key);
        update(mac, value);
    }
    update(mac, &access.principal.subject);
    update(mac, &access.principal.principal_type);
    for (key, value) in &access.principal.attributes {
        update(mac, key);
        update(mac, value);
    }
    update(mac, access.authorization_version.as_deref().unwrap_or(""));
    if let Some(delegation) = &access.delegation {
        update(mac, &delegation.id);
        mac.update(delegation.proof.as_bytes());
    }
}

fn update(mac: &mut HmacSha256, value: &str) {
    mac.update(&(value.len() as u64).to_be_bytes());
    mac.update(value.as_bytes());
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use llm_harness_runtime_knowledge::{KnowledgeScope, PrincipalRef};
    use llm_harness_types::{RunContext, RunRequest};

    use super::*;

    fn access(subject: &str) -> KnowledgeAccessContext {
        KnowledgeAccessContext::new(
            KnowledgeScope::new("memory"),
            PrincipalRef::new(subject, "user"),
        )
    }

    fn provenance(run: &RunContext) -> MemoryProvenance {
        MemoryProvenance {
            run_id: run.id(),
            session_id: Some("session-1".into()),
            origin: MemoryMutationOrigin::ExplicitTool {
                tool_use_id: "tool-1".into(),
            },
            recorded_at: Utc::now(),
        }
    }

    #[test]
    fn default_guard_rejects_common_secrets() {
        let guard = DefaultMemoryContentGuard;
        for content in [
            "password = correct-horse-battery-staple",
            "password: 123456",
            "api_key: sk-12345678901234567890",
            "-----BEGIN PRIVATE KEY-----\nabc",
            "Authorization: Bearer abcdefghijklmnop",
            "AKIA1234567890ABCDEF",
        ] {
            assert_eq!(
                guard.check(content),
                Err(MemoryPolicyRejection::SensitiveContent)
            );
        }
        guard
            .check("Prefers examples about API design")
            .expect("ordinary preference should pass");
    }

    #[tokio::test]
    async fn idempotency_is_stable_and_bound_to_access() {
        let policy = SecureMemoryWritePolicy::new(vec![7; 32], Default::default()).unwrap();
        let run = RunContext::new(RunRequest::from_text("remember"));
        let allowed = access("user-1");
        let other = access("user-2");
        let intent = MemoryWriteIntent {
            content: "  Prefers concise explanations  ".into(),
            kind: Some("preference".into()),
            requested_ttl: None,
        };
        let first = policy
            .prepare(
                KnowledgeRequestContext {
                    run: &run,
                    access: &allowed,
                },
                intent.clone(),
                provenance(&run),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let repeated = policy
            .prepare(
                KnowledgeRequestContext {
                    run: &run,
                    access: &allowed,
                },
                intent.clone(),
                provenance(&run),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let other_user = policy
            .prepare(
                KnowledgeRequestContext {
                    run: &run,
                    access: &other,
                },
                intent,
                provenance(&run),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(first.idempotency_key, repeated.idempotency_key);
        assert_ne!(first.idempotency_key, other_user.idempotency_key);
        assert_eq!(first.content, "Prefers concise explanations");
    }

    #[tokio::test]
    async fn ttl_above_policy_maximum_is_rejected() {
        let policy = SecureMemoryWritePolicy::new(vec![7; 32], Default::default()).unwrap();
        let run = RunContext::new(RunRequest::from_text("remember"));
        let access = access("user-1");
        let error = match policy
            .prepare(
                KnowledgeRequestContext {
                    run: &run,
                    access: &access,
                },
                MemoryWriteIntent {
                    content: "Prefers concise explanations".into(),
                    kind: None,
                    requested_ttl: Some(Duration::from_secs(366 * 24 * 60 * 60)),
                },
                provenance(&run),
                CancellationToken::new(),
            )
            .await
        {
            Ok(_) => panic!("TTL above maximum must be rejected"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            MemoryPolicyError::Rejected(MemoryPolicyRejection::InvalidTtl)
        );
    }
}
