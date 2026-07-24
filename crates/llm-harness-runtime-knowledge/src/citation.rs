use std::collections::BTreeSet;

use futures::future::BoxFuture;
use llm_harness_types::{
    FinalAnswerValidationCtx, FinalAnswerValidationError, FinalAnswerValidator,
};

use crate::{CitationValidator, KnowledgeRunState};

/// Controls how the knowledge plugin validates citations in final answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KnowledgeCitationPolicy {
    /// Do not install a final-answer citation validator.
    Disabled,
    /// Validate every knowledge citation that appears, but allow answers
    /// without citations.
    #[default]
    ValidateIfPresent,
    /// Once trusted knowledge evidence has been read in this run, require at
    /// least one valid citation in the final answer.
    RequireWhenEvidenceRead,
}

/// Final-answer validator backed by the run-scoped trusted citation registry.
pub struct KnowledgeFinalAnswerValidator {
    policy: KnowledgeCitationPolicy,
}

impl KnowledgeFinalAnswerValidator {
    pub fn new(policy: KnowledgeCitationPolicy) -> Self {
        Self { policy }
    }
}

impl FinalAnswerValidator for KnowledgeFinalAnswerValidator {
    fn validate<'a>(
        &'a self,
        ctx: FinalAnswerValidationCtx<'a>,
    ) -> BoxFuture<'a, Result<(), FinalAnswerValidationError>> {
        Box::pin(async move {
            if self.policy == KnowledgeCitationPolicy::Disabled {
                return Ok(());
            }

            let handles = extract_citation_handles(&ctx.candidate.text_content())?;
            let evidence_was_read = ctx
                .run
                .local()
                .get::<KnowledgeRunState>()
                .is_some_and(|state| state.has_citations());

            if self.policy == KnowledgeCitationPolicy::RequireWhenEvidenceRead
                && evidence_was_read
                && handles.is_empty()
            {
                return Err(FinalAnswerValidationError::new(
                    "missing_knowledge_citation",
                    "the final answer must cite trusted knowledge evidence read during this run",
                ));
            }

            CitationValidator::validate(ctx.run, &handles).map_err(|_| {
                FinalAnswerValidationError::new(
                    "invalid_knowledge_citation",
                    "the final answer contains a citation that was not issued in this run",
                )
            })?;
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
    use crate::{EvidenceProviderId, KnowledgeRef, PersistedKnowledgeEvidence};

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

    fn register_citation(run: &RunContext) -> String {
        run.local()
            .get_or_insert_with(KnowledgeRunState::default)
            .register(
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
        let handle = register_citation(&run);
        let validator =
            KnowledgeFinalAnswerValidator::new(KnowledgeCitationPolicy::ValidateIfPresent);

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
    async fn strict_policy_requires_a_citation_after_evidence_is_read() {
        let run = RunContext::new(RunRequest::from_text("question"));
        register_citation(&run);
        let validator =
            KnowledgeFinalAnswerValidator::new(KnowledgeCitationPolicy::RequireWhenEvidenceRead);

        let error = validate(&validator, &run, &candidate("unsupported answer"))
            .await
            .unwrap_err();
        assert_eq!(error.code, "missing_knowledge_citation");
    }

    #[tokio::test]
    async fn strict_policy_does_not_require_citations_without_evidence() {
        let run = RunContext::new(RunRequest::from_text("question"));
        let validator =
            KnowledgeFinalAnswerValidator::new(KnowledgeCitationPolicy::RequireWhenEvidenceRead);

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
}
