use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use llm_harness_types::{DataBlock, RunContext, RunId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{KnowledgeAccessContext, KnowledgeRef};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EvidenceProviderId(pub String);

#[derive(Clone)]
pub struct EvidenceReceipt {
    provider_id: EvidenceProviderId,
    run_id: RunId,
    tool_use_id: Option<String>,
    access_fingerprint: String,
    reference_digest: String,
    content_digest: String,
    issued_at: DateTime<Utc>,
    proof: Vec<u8>,
}

#[derive(Clone)]
pub struct KnowledgeEvidence {
    pub reference: KnowledgeRef,
    pub blocks: Vec<DataBlock>,
    pub obtained_at: DateTime<Utc>,
    pub receipt: EvidenceReceipt,
}

#[derive(Clone)]
pub struct KnowledgeEvidenceSet(pub Vec<KnowledgeEvidence>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedKnowledgeEvidence {
    pub provider_id: EvidenceProviderId,
    pub reference: KnowledgeRef,
    pub obtained_at: DateTime<Utc>,
    pub run_id: RunId,
    pub content_digest: String,
}

pub struct EvidenceAuthority {
    secret: Vec<u8>,
    providers: BTreeSet<EvidenceProviderId>,
}

pub struct EvidenceIssuer<'a> {
    authority: &'a EvidenceAuthority,
    provider_id: EvidenceProviderId,
    run: &'a RunContext,
    access: &'a KnowledgeAccessContext,
}

impl EvidenceAuthority {
    pub fn new(
        secret: Vec<u8>,
        providers: impl IntoIterator<Item = EvidenceProviderId>,
    ) -> Result<Self, &'static str> {
        if secret.len() < 32 {
            return Err("evidence authority secret must contain at least 32 bytes");
        }
        Ok(Self {
            secret,
            providers: providers.into_iter().collect(),
        })
    }

    pub fn is_registered(&self, provider_id: &EvidenceProviderId) -> bool {
        self.providers.contains(provider_id)
    }

    pub fn issuer<'a>(
        &'a self,
        provider_id: &EvidenceProviderId,
        run: &'a RunContext,
        access: &'a KnowledgeAccessContext,
    ) -> Result<EvidenceIssuer<'a>, &'static str> {
        if !self.providers.contains(provider_id) {
            return Err("evidence provider is not registered");
        }
        Ok(EvidenceIssuer {
            authority: self,
            provider_id: provider_id.clone(),
            run,
            access,
        })
    }

    pub fn verify(
        &self,
        evidence: &KnowledgeEvidence,
        run: &RunContext,
        access: &KnowledgeAccessContext,
        tool_use_id: Option<&str>,
    ) -> bool {
        let receipt = &evidence.receipt;
        self.providers.contains(&receipt.provider_id)
            && receipt.run_id == run.id()
            && receipt.tool_use_id.as_deref() == tool_use_id
            && receipt.access_fingerprint == fingerprint_access(&self.secret, access)
            && receipt.reference_digest == digest_json(&evidence.reference)
            && receipt.content_digest == digest_json(&evidence.blocks)
            && self.verify_proof(receipt)
    }

    pub fn persisted(
        &self,
        evidence: &KnowledgeEvidence,
        run: &RunContext,
        access: &KnowledgeAccessContext,
        tool_use_id: Option<&str>,
    ) -> Option<PersistedKnowledgeEvidence> {
        self.verify(evidence, run, access, tool_use_id)
            .then(|| PersistedKnowledgeEvidence {
                provider_id: evidence.receipt.provider_id.clone(),
                reference: evidence.reference.clone(),
                obtained_at: evidence.obtained_at,
                run_id: evidence.receipt.run_id,
                content_digest: evidence.receipt.content_digest.clone(),
            })
    }

    fn sign(&self, receipt: &EvidenceReceipt) -> Vec<u8> {
        self.receipt_mac(receipt).finalize().into_bytes().to_vec()
    }

    fn verify_proof(&self, receipt: &EvidenceReceipt) -> bool {
        self.receipt_mac(receipt)
            .verify_slice(&receipt.proof)
            .is_ok()
    }

    fn receipt_mac(&self, receipt: &EvidenceReceipt) -> HmacSha256 {
        let mut mac = HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts any key");
        mac.update(receipt.provider_id.0.as_bytes());
        mac.update(receipt.run_id.to_string().as_bytes());
        mac.update(receipt.tool_use_id.as_deref().unwrap_or("").as_bytes());
        mac.update(receipt.access_fingerprint.as_bytes());
        mac.update(receipt.reference_digest.as_bytes());
        mac.update(receipt.content_digest.as_bytes());
        mac.update(&receipt.issued_at.timestamp_millis().to_be_bytes());
        mac
    }
}

impl EvidenceIssuer<'_> {
    pub fn issue(
        &self,
        tool_use_id: Option<&str>,
        reference: KnowledgeRef,
        blocks: Vec<DataBlock>,
        obtained_at: DateTime<Utc>,
    ) -> Result<KnowledgeEvidence, &'static str> {
        let access_fingerprint = fingerprint_access(&self.authority.secret, self.access);
        let reference_digest = digest_json(&reference);
        let content_digest = digest_json(&blocks);
        let issued_at = Utc::now();
        let mut receipt = EvidenceReceipt {
            provider_id: self.provider_id.clone(),
            run_id: self.run.id(),
            tool_use_id: tool_use_id.map(str::to_string),
            access_fingerprint,
            reference_digest,
            content_digest,
            issued_at,
            proof: vec![],
        };
        receipt.proof = self.authority.sign(&receipt);
        Ok(KnowledgeEvidence {
            reference,
            blocks,
            obtained_at,
            receipt,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CitationRecord {
    pub handle: String,
    pub provider_id: EvidenceProviderId,
    pub reference: KnowledgeRef,
    pub obtained_at: DateTime<Utc>,
    pub content_digest: String,
}

#[derive(Default)]
pub struct KnowledgeRunState {
    inner: Mutex<KnowledgeRunStateInner>,
}

#[derive(Default)]
struct KnowledgeRunStateInner {
    next: u64,
    citations: BTreeMap<String, CitationRecord>,
}

impl KnowledgeRunState {
    pub(crate) fn register(
        &self,
        run_id: RunId,
        evidence: &PersistedKnowledgeEvidence,
    ) -> CitationRecord {
        let mut inner = self.inner.lock().unwrap_or_else(|value| value.into_inner());
        inner.next += 1;
        let namespace_digest = Sha256::digest(run_id.to_string().as_bytes());
        let namespace = URL_SAFE_NO_PAD
            .encode(namespace_digest)
            .chars()
            .take(10)
            .collect::<String>();
        let handle = format!("[K:{namespace}:{}]", inner.next);
        let record = CitationRecord {
            handle: handle.clone(),
            provider_id: evidence.provider_id.clone(),
            reference: evidence.reference.clone(),
            obtained_at: evidence.obtained_at,
            content_digest: evidence.content_digest.clone(),
        };
        inner.citations.insert(handle, record.clone());
        record
    }

    pub fn resolve(&self, handle: &str) -> Option<CitationRecord> {
        self.inner
            .lock()
            .unwrap_or_else(|value| value.into_inner())
            .citations
            .get(handle)
            .cloned()
    }

    pub fn has_citations(&self) -> bool {
        !self
            .inner
            .lock()
            .unwrap_or_else(|value| value.into_inner())
            .citations
            .is_empty()
    }
}

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum CitationValidationError {
    #[error("citation handle is unknown in this run: {0}")]
    UnknownHandle(String),
}

pub struct CitationValidator;

impl CitationValidator {
    pub fn validate(
        run: &RunContext,
        handles: &[String],
    ) -> Result<Vec<CitationRecord>, CitationValidationError> {
        let Some(state) = run.local().get::<KnowledgeRunState>() else {
            return handles
                .first()
                .map(|handle| Err(CitationValidationError::UnknownHandle(handle.clone())))
                .unwrap_or_else(|| Ok(vec![]));
        };
        handles
            .iter()
            .map(|handle| {
                state
                    .resolve(handle)
                    .ok_or_else(|| CitationValidationError::UnknownHandle(handle.clone()))
            })
            .collect()
    }
}

fn digest_json(value: &impl Serialize) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(
        serde_json::to_vec(value).expect("evidence DTO serialization cannot fail"),
    ))
}

fn fingerprint_access(secret: &[u8], access: &KnowledgeAccessContext) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key");
    update(&mut mac, &access.scope.namespace);
    update(&mut mac, access.scope.tenant.as_deref().unwrap_or(""));
    update(&mut mac, access.scope.project.as_deref().unwrap_or(""));
    for (key, value) in &access.scope.attributes {
        update(&mut mac, key);
        update(&mut mac, value);
    }
    update(&mut mac, &access.principal.subject);
    update(&mut mac, &access.principal.principal_type);
    for (key, value) in &access.principal.attributes {
        update(&mut mac, key);
        update(&mut mac, value);
    }
    update(
        &mut mac,
        access.authorization_version.as_deref().unwrap_or(""),
    );
    if let Some(grant) = &access.delegation {
        update(&mut mac, &grant.id);
        mac.update(grant.proof.as_bytes());
    }
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn update(mac: &mut HmacSha256, value: &str) {
    mac.update(&(value.len() as u64).to_be_bytes());
    mac.update(value.as_bytes());
}

#[cfg(test)]
mod tests {
    use llm_harness_types::{RunContext, RunRequest};

    use super::*;
    use crate::{KnowledgeScope, PrincipalRef};

    fn access() -> KnowledgeAccessContext {
        KnowledgeAccessContext::new(
            KnowledgeScope::new("docs"),
            PrincipalRef::new("user-1", "user"),
        )
    }

    #[test]
    fn receipts_are_bound_to_run_access_tool_reference_and_content() {
        let provider = EvidenceProviderId("registry".into());
        let authority = EvidenceAuthority::new(vec![9; 32], [provider.clone()]).unwrap();
        let access = access();
        let run = RunContext::new(RunRequest::from_text("question"));
        let other_run = RunContext::new(RunRequest::from_text("other"));
        let evidence = authority
            .issuer(&provider, &run, &access)
            .unwrap()
            .issue(
                Some("call-1"),
                KnowledgeRef {
                    source_id: "docs".into(),
                    item_id: "item".into(),
                    revision: Some("rev-1".into()),
                },
                vec![DataBlock::text("body")],
                Utc::now(),
            )
            .unwrap();

        assert!(authority.verify(&evidence, &run, &access, Some("call-1")));
        assert!(!authority.verify(&evidence, &other_run, &access, Some("call-1")));
        assert!(!authority.verify(&evidence, &run, &access, Some("call-2")));
        let mut tampered = evidence.clone();
        tampered.blocks = vec![DataBlock::text("changed")];
        assert!(!authority.verify(&tampered, &run, &access, Some("call-1")));
    }

    #[test]
    fn citation_handles_are_run_scoped_and_forgery_is_rejected() {
        let run = RunContext::new(RunRequest::from_text("question"));
        let other_run = RunContext::new(RunRequest::from_text("other"));
        let state = run.local().get_or_insert_with(KnowledgeRunState::default);
        let record = state.register(
            run.id(),
            &PersistedKnowledgeEvidence {
                provider_id: EvidenceProviderId("registry".into()),
                reference: KnowledgeRef {
                    source_id: "docs".into(),
                    item_id: "item".into(),
                    revision: Some("rev-1".into()),
                },
                obtained_at: Utc::now(),
                run_id: run.id(),
                content_digest: "digest".into(),
            },
        );
        assert_eq!(
            CitationValidator::validate(&run, std::slice::from_ref(&record.handle)).unwrap()[0],
            record
        );
        assert!(CitationValidator::validate(&run, &["[K:forged:1]".into()]).is_err());
        assert!(CitationValidator::validate(&other_run, &[record.handle]).is_err());
    }
}
