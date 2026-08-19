use std::collections::{BTreeMap, BTreeSet};

use futures::future::BoxFuture;
use llm_harness_types::{
    FinalAnswerValidationCtx, FinalAnswerValidationError, FinalAnswerValidator,
};

use crate::{CitationValidator, KnowledgeRunState, KnowledgeSourceDescriptor};

/// Whether evidence from a source must be cited in the final answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KnowledgeCitationRequirement {
    /// A citation is not required, but any citation that appears is validated.
    #[default]
    Optional,
    /// Every trusted evidence item read under this requirement must be cited.
    Required,
}

/// Server-owned citation requirements for sources registered in one plugin.
///
/// Exact source rules take precedence over domain rules. If a source has no
/// exact rule, all matching domain rules are combined fail-closed: any
/// `Required` match makes the source required. The default applies only when
/// neither a source nor a domain rule matches.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KnowledgeCitationPolicy {
    default_requirement: KnowledgeCitationRequirement,
    source_requirements: BTreeMap<String, KnowledgeCitationRequirement>,
    domain_requirements: BTreeMap<String, KnowledgeCitationRequirement>,
}

impl KnowledgeCitationPolicy {
    pub fn builder() -> KnowledgeCitationPolicyBuilder {
        KnowledgeCitationPolicyBuilder::default()
    }

    pub fn requirement_for(
        &self,
        descriptor: &KnowledgeSourceDescriptor,
    ) -> KnowledgeCitationRequirement {
        if let Some(requirement) = self.source_requirements.get(&descriptor.id) {
            return *requirement;
        }

        let mut matched = None;
        for domain in &descriptor.domains {
            let Some(requirement) = self.domain_requirements.get(domain) else {
                continue;
            };
            if *requirement == KnowledgeCitationRequirement::Required {
                return KnowledgeCitationRequirement::Required;
            }
            matched = Some(KnowledgeCitationRequirement::Optional);
        }
        matched.unwrap_or(self.default_requirement)
    }

    pub(crate) fn validate_sources<'a>(
        &self,
        descriptors: impl IntoIterator<Item = &'a KnowledgeSourceDescriptor>,
    ) -> Result<(), KnowledgeCitationPolicyScopeError> {
        let descriptors = descriptors.into_iter().collect::<Vec<_>>();
        let source_ids = descriptors
            .iter()
            .map(|descriptor| descriptor.id.as_str())
            .collect::<BTreeSet<_>>();
        let domains = descriptors
            .iter()
            .flat_map(|descriptor| descriptor.domains.iter().map(String::as_str))
            .collect::<BTreeSet<_>>();

        if let Some(source_id) = self
            .source_requirements
            .keys()
            .find(|source_id| !source_ids.contains(source_id.as_str()))
        {
            return Err(KnowledgeCitationPolicyScopeError::UnknownSource(
                source_id.clone(),
            ));
        }
        if let Some(domain) = self
            .domain_requirements
            .keys()
            .find(|domain| !domains.contains(domain.as_str()))
        {
            return Err(KnowledgeCitationPolicyScopeError::UnknownDomain(
                domain.clone(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KnowledgeCitationPolicyBuilder {
    policy: KnowledgeCitationPolicy,
    error: Option<KnowledgeCitationPolicyBuildError>,
}

impl KnowledgeCitationPolicyBuilder {
    pub fn default_requirement(mut self, requirement: KnowledgeCitationRequirement) -> Self {
        self.policy.default_requirement = requirement;
        self
    }

    pub fn source(
        mut self,
        source_id: impl Into<String>,
        requirement: KnowledgeCitationRequirement,
    ) -> Self {
        let source_id = source_id.into();
        if source_id.trim().is_empty() {
            self.record_error(KnowledgeCitationPolicyBuildError::EmptySourceId);
        } else if self
            .policy
            .source_requirements
            .insert(source_id.clone(), requirement)
            .is_some()
        {
            self.record_error(KnowledgeCitationPolicyBuildError::DuplicateSource(
                source_id,
            ));
        }
        self
    }

    pub fn domain(
        mut self,
        domain: impl Into<String>,
        requirement: KnowledgeCitationRequirement,
    ) -> Self {
        let domain = domain.into();
        if domain.trim().is_empty() {
            self.record_error(KnowledgeCitationPolicyBuildError::EmptyDomain);
        } else if self
            .policy
            .domain_requirements
            .insert(domain.clone(), requirement)
            .is_some()
        {
            self.record_error(KnowledgeCitationPolicyBuildError::DuplicateDomain(domain));
        }
        self
    }

    pub fn build(self) -> Result<KnowledgeCitationPolicy, KnowledgeCitationPolicyBuildError> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.policy),
        }
    }

    fn record_error(&mut self, error: KnowledgeCitationPolicyBuildError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum KnowledgeCitationPolicyBuildError {
    #[error("citation policy source ID must not be empty")]
    EmptySourceId,
    #[error("citation policy domain must not be empty")]
    EmptyDomain,
    #[error("duplicate citation policy source: {0}")]
    DuplicateSource(String),
    #[error("duplicate citation policy domain: {0}")]
    DuplicateDomain(String),
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum KnowledgeCitationPolicyScopeError {
    #[error("citation policy references an unknown source: {0}")]
    UnknownSource(String),
    #[error("citation policy references an unknown domain: {0}")]
    UnknownDomain(String),
}

/// Final-answer validator backed by the run-scoped trusted citation registry.
#[derive(Default)]
pub struct KnowledgeFinalAnswerValidator;

impl KnowledgeFinalAnswerValidator {
    pub fn new() -> Self {
        Self
    }
}

impl FinalAnswerValidator for KnowledgeFinalAnswerValidator {
    fn validate<'a>(
        &'a self,
        ctx: FinalAnswerValidationCtx<'a>,
    ) -> BoxFuture<'a, Result<(), FinalAnswerValidationError>> {
        Box::pin(async move {
            let handles = extract_citation_handles(&ctx.candidate.text_content())?;
            CitationValidator::validate(ctx.run, &handles).map_err(|_| {
                FinalAnswerValidationError::new(
                    "invalid_knowledge_citation",
                    "the final answer contains a citation that was not issued in this run",
                )
            })?;
            let cited = handles.into_iter().collect::<BTreeSet<_>>();
            let required_citation_is_missing = ctx
                .run
                .local()
                .get::<KnowledgeRunState>()
                .is_some_and(|state| state.has_missing_required_citations(&cited));
            if required_citation_is_missing {
                return Err(FinalAnswerValidationError::new(
                    "missing_knowledge_citation",
                    "the final answer must cite all required knowledge evidence read during this run",
                ));
            }
            Ok(())
        })
    }
}

fn extract_citation_handles(text: &str) -> Result<Vec<String>, FinalAnswerValidationError> {
    let mut remaining = text;
    let mut handles = BTreeSet::new();

    while let Some(start) = remaining.find("[K:") {
        let candidate = &remaining[start..];
        let Some(end) = candidate.find(']') else {
            return Err(FinalAnswerValidationError::new(
                "invalid_knowledge_citation",
                "the final answer contains a malformed knowledge citation",
            ));
        };
        handles.insert(candidate[..=end].to_owned());
        remaining = &candidate[end + 1..];
    }

    Ok(handles.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use llm_harness_types::{
        AssistantMessage, AssistantMessageKind, ContentBlock, RunContext, RunRequest, StopReason,
    };

    use super::*;
    use crate::{
        EvidenceProviderId, FreshnessClass, KnowledgeRef, KnowledgeSourceDescriptor,
        PersistedKnowledgeEvidence,
    };

    fn candidate(text: &str) -> AssistantMessage {
        AssistantMessage {
            kind: AssistantMessageKind::FinalAnswer,
            message_id: "message".into(),
            turn_id: "turn".into(),
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: Some(StopReason::EndTurn),
            timestamp: Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        }
    }

    fn descriptor(id: &str, domains: &[&str]) -> KnowledgeSourceDescriptor {
        KnowledgeSourceDescriptor {
            id: id.into(),
            name: id.into(),
            description: id.into(),
            domains: domains.iter().map(|domain| (*domain).into()).collect(),
            capabilities: BTreeSet::new(),
            freshness: FreshnessClass::Static,
            filter_fields: vec![],
        }
    }

    fn register_citation(
        run: &RunContext,
        source_id: &str,
        item_id: &str,
        requirement: KnowledgeCitationRequirement,
    ) -> String {
        run.local()
            .get_or_insert_with(KnowledgeRunState::default)
            .register(
                run.id(),
                &PersistedKnowledgeEvidence {
                    provider_id: EvidenceProviderId("registry".into()),
                    reference: KnowledgeRef {
                        source_id: source_id.into(),
                        item_id: item_id.into(),
                        revision: Some("rev-1".into()),
                    },
                    obtained_at: Utc::now(),
                    run_id: run.id(),
                    content_digest: "digest".into(),
                },
                requirement,
            )
            .handle
    }

    async fn validate(
        validator: &KnowledgeFinalAnswerValidator,
        run: &RunContext,
        answer: &AssistantMessage,
    ) -> Result<(), FinalAnswerValidationError> {
        validator
            .validate(FinalAnswerValidationCtx {
                run,
                candidate: answer,
                turn_index: 0,
            })
            .await
    }

    #[tokio::test]
    async fn validates_only_handles_issued_in_the_current_run() {
        let run = RunContext::new(RunRequest::from_text("question"));
        let other_run = RunContext::new(RunRequest::from_text("other"));
        let handle = register_citation(
            &run,
            "memory",
            "preference",
            KnowledgeCitationRequirement::Optional,
        );
        let validator = KnowledgeFinalAnswerValidator::new();

        assert!(
            validate(&validator, &run, &candidate(&format!("supported {handle}")))
                .await
                .is_ok()
        );
        assert!(
            validate(
                &validator,
                &other_run,
                &candidate(&format!("copied {handle}"))
            )
            .await
            .is_err()
        );
        assert!(
            validate(&validator, &run, &candidate("forged [K:not-issued:1]"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn required_evidence_must_be_cited() {
        let run = RunContext::new(RunRequest::from_text("question"));
        register_citation(
            &run,
            "course",
            "lesson",
            KnowledgeCitationRequirement::Required,
        );
        let validator = KnowledgeFinalAnswerValidator::new();

        let error = validate(&validator, &run, &candidate("unsupported answer"))
            .await
            .unwrap_err();
        assert_eq!(error.code, "missing_knowledge_citation");
    }

    #[tokio::test]
    async fn optional_evidence_does_not_require_a_visible_citation() {
        let run = RunContext::new(RunRequest::from_text("question"));
        register_citation(
            &run,
            "memory",
            "preference",
            KnowledgeCitationRequirement::Optional,
        );
        let validator = KnowledgeFinalAnswerValidator::new();

        assert!(
            validate(
                &validator,
                &run,
                &candidate("answer personalized from learner memory")
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn every_required_evidence_item_must_be_cited() {
        let run = RunContext::new(RunRequest::from_text("question"));
        let first = register_citation(
            &run,
            "course",
            "lesson-1",
            KnowledgeCitationRequirement::Required,
        );
        let second = register_citation(
            &run,
            "course",
            "lesson-2",
            KnowledgeCitationRequirement::Required,
        );
        let validator = KnowledgeFinalAnswerValidator::new();

        let error = validate(
            &validator,
            &run,
            &candidate(&format!("only one source {first}")),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "missing_knowledge_citation");
        assert!(
            validate(
                &validator,
                &run,
                &candidate(&format!("both sources {first} {second}"))
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn optional_citation_cannot_satisfy_required_evidence() {
        let run = RunContext::new(RunRequest::from_text("question"));
        let optional = register_citation(
            &run,
            "memory",
            "preference",
            KnowledgeCitationRequirement::Optional,
        );
        register_citation(
            &run,
            "course",
            "lesson",
            KnowledgeCitationRequirement::Required,
        );
        let validator = KnowledgeFinalAnswerValidator::new();

        let error = validate(
            &validator,
            &run,
            &candidate(&format!("memory only {optional}")),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "missing_knowledge_citation");
    }

    #[tokio::test]
    async fn no_evidence_does_not_require_a_citation() {
        let run = RunContext::new(RunRequest::from_text("question"));
        let validator = KnowledgeFinalAnswerValidator::new();

        assert!(
            validate(
                &validator,
                &run,
                &candidate("answer from general reasoning")
            )
            .await
            .is_ok()
        );
    }

    #[test]
    fn source_rules_override_domains_and_domain_conflicts_fail_closed() {
        let policy = KnowledgeCitationPolicy::builder()
            .default_requirement(KnowledgeCitationRequirement::Optional)
            .domain("course", KnowledgeCitationRequirement::Required)
            .domain("personal", KnowledgeCitationRequirement::Optional)
            .source("memory", KnowledgeCitationRequirement::Optional)
            .build()
            .unwrap();

        assert_eq!(
            policy.requirement_for(&descriptor("course", &["course"])),
            KnowledgeCitationRequirement::Required
        );
        assert_eq!(
            policy.requirement_for(&descriptor("memory", &["course", "personal"])),
            KnowledgeCitationRequirement::Optional
        );
        assert_eq!(
            policy.requirement_for(&descriptor("mixed", &["course", "personal"])),
            KnowledgeCitationRequirement::Required
        );
        assert_eq!(
            policy.requirement_for(&descriptor("other", &["other"])),
            KnowledgeCitationRequirement::Optional
        );
    }

    #[test]
    fn policy_builder_rejects_ambiguous_duplicate_rules() {
        let error = KnowledgeCitationPolicy::builder()
            .source("course", KnowledgeCitationRequirement::Required)
            .source("course", KnowledgeCitationRequirement::Optional)
            .build()
            .unwrap_err();

        assert_eq!(
            error,
            KnowledgeCitationPolicyBuildError::DuplicateSource("course".into())
        );
    }

    #[test]
    fn policy_scope_rejects_unknown_sources_and_domains() {
        let course = descriptor("course", &["course"]);
        let unknown_source = KnowledgeCitationPolicy::builder()
            .source("typo", KnowledgeCitationRequirement::Required)
            .build()
            .unwrap();
        assert_eq!(
            unknown_source.validate_sources([&course]),
            Err(KnowledgeCitationPolicyScopeError::UnknownSource(
                "typo".into()
            ))
        );

        let unknown_domain = KnowledgeCitationPolicy::builder()
            .domain("typo", KnowledgeCitationRequirement::Required)
            .build()
            .unwrap();
        assert_eq!(
            unknown_domain.validate_sources([&course]),
            Err(KnowledgeCitationPolicyScopeError::UnknownDomain(
                "typo".into()
            ))
        );
    }
}
