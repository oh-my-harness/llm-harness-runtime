//! Vision degradation hook (item 20) — repairs requests a text-only provider
//! rejects (issue #145 gap 2).
//!
//! Once image/document blocks enter harness-owned history, a text-only provider
//! rejects the entire request (HTTP 400 `InvalidRequest`). Without repair the
//! run is stuck: the host cannot edit history (it lives inside the loop), so
//! every subsequent turn fails identically.
//!
//! [`VisionDegradeHook`] implements [`ProviderErrorHook`]: on an
//! `InvalidRequest` error it strips visual blocks from both the loop context
//! and the new-messages mirror (two disjoint `&mut` views of the same
//! trajectory), appends a repair-guidance notice so the model understands why
//! earlier tool-produced images vanished, and returns [`ErrorDirective::Retry`]
//! to retry the same turn. Errors of any other kind — or `InvalidRequest` with
//! nothing visual to strip — surface unchanged.
//!
//! The hook is one-shot per run: after the first repair it surfaces further
//! provider errors unchanged, so a provider that keeps rejecting cannot pin
//! the run into a repair/retry livelock.

use futures::future::BoxFuture;
use llm_adapter::LlmError;
use llm_harness_types::{
    AgentMessage, ContentBlock, DataBlock, ErrorDirective, ProviderErrorCtx, ProviderErrorHook,
    RunId,
};
use parking_lot::Mutex;
use tracing::warn;

/// Repair guidance appended after stripping, phrased as environment feedback
/// so the model understands why earlier tool-produced images vanished.
const VISION_REMOVAL_NOTICE: &str = "[system] Image and document blocks were removed from this conversation: the current model does not support multimodal input and rejected the request. Do not attempt to re-attach the removed images. If visual information is essential, tell the user that this model cannot process images.";

/// Whether a data block references visual/document content.
fn is_visual_block(block: &DataBlock) -> bool {
    matches!(block, DataBlock::Image { .. } | DataBlock::Document { .. })
}

/// Strip image/document blocks from one message slice in place.
///
/// Returns the number of removed blocks. Notice insertion mirrors
/// [`strip_vision_from_context`]: the notice lands on the same message whose
/// visual blocks were removed.
///
/// Semantics worth noting: a message that carried *only* visual blocks ends
/// up containing just the notice text, and its `is_error: false` (ToolResult)
/// is left untouched — the message no longer reflects what the tool actually
/// returned. This is accepted by design (issue #145 review, comment 3):
/// keeping the notice inside the same message preserves the
/// request→result trajectory pairing the model reasons over, at the cost of
/// slightly softened semantics. The notice text itself tells the model the
/// blocks were removed *by the system*, not that the tool succeeded.
fn strip_slice(messages: &mut [AgentMessage]) -> usize {
    let mut stripped = 0usize;
    for msg in messages.iter_mut() {
        match msg {
            AgentMessage::ToolResult(t) => {
                let before = t.content.len();
                t.content.retain(|b| !is_visual_block(b));
                if before != t.content.len() {
                    stripped += before - t.content.len();
                    t.content.push(DataBlock::text(VISION_REMOVAL_NOTICE));
                }
            }
            AgentMessage::User(u) => {
                let before = u.content.len();
                u.content.retain(|b| {
                    !matches!(
                        b,
                        ContentBlock::Image { .. } | ContentBlock::Document { .. }
                    )
                });
                if before != u.content.len() {
                    stripped += before - u.content.len();
                    u.content.push(ContentBlock::Text {
                        text: VISION_REMOVAL_NOTICE.into(),
                    });
                }
            }
            _ => {}
        }
    }
    stripped
}

/// Strip visual blocks from the primary context.
///
/// Returns `None` when the context contains no visual blocks (nothing to do —
/// the provider error has a different cause and must surface unchanged).
fn strip_vision_from_context(messages: &mut [AgentMessage]) -> Option<usize> {
    let stripped = strip_slice(messages);
    (stripped > 0).then_some(stripped)
}

/// Strip visual blocks from both the loop context and the new-messages
/// mirror, so the provider request and `AgentEnd` payload stay consistent.
/// Returns the number of removed blocks from the primary context, or `None`
/// when there was nothing to strip (the provider error then surfaces
/// unchanged).
fn strip_vision(
    ctx_messages: &mut [AgentMessage],
    new_messages: &mut [AgentMessage],
) -> Option<usize> {
    let stripped = strip_vision_from_context(ctx_messages)?;
    let mirrored = strip_slice(new_messages);
    if mirrored != stripped {
        // The two views are supposed to be overlapping projections of the
        // same trajectory; when they diverge (e.g. the loop grew new_messages
        // since the failed request), still report the primary-context count.
        tracing::debug!(
            stripped,
            mirrored,
            "vision strip counts diverged between context and new_messages"
        );
    }
    Some(stripped)
}

/// Provider-error hook that repairs text-only-provider rejections.
///
/// See the [module docs](self) for the full strategy.
pub struct VisionDegradeHook {
    /// One repair per run: `Some(run_id)` after the first successful strip.
    repaired_run: Mutex<Option<RunId>>,
}

impl Default for VisionDegradeHook {
    fn default() -> Self {
        Self::new()
    }
}

impl VisionDegradeHook {
    /// Create a fresh hook with no repairs recorded.
    pub fn new() -> Self {
        Self {
            repaired_run: Mutex::new(None),
        }
    }

    /// Whether this hook already repaired `run_id` (one-shot guard).
    pub fn has_repaired(&self, run_id: RunId) -> bool {
        *self.repaired_run.lock() == Some(run_id)
    }
}

impl ProviderErrorHook for VisionDegradeHook {
    fn on_provider_error<'a>(&'a self, ctx: ProviderErrorCtx<'a>) -> BoxFuture<'a, ErrorDirective> {
        Box::pin(async move {
            // Only repair errors the provider raised about the request itself.
            if !matches!(ctx.error, LlmError::InvalidRequest(_)) {
                return ErrorDirective::Surface;
            }
            let run_id = ctx.run.id();
            {
                let repaired = self.repaired_run.lock();
                if *repaired == Some(run_id) {
                    return ErrorDirective::Surface;
                }
            }
            let Some(_) = strip_vision(ctx.context.messages.as_mut_slice(), ctx.new_messages)
            else {
                // Nothing visual to remove: the error has a different cause.
                return ErrorDirective::Surface;
            };
            *self.repaired_run.lock() = Some(run_id);
            warn!(
                run = %run_id,
                turn = ctx.turn_index,
                "provider rejected multimodal input; stripped visual blocks and retrying"
            );
            ErrorDirective::Retry
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use llm_harness_types::{
        AgentContext, BinarySource, ImageSource, RunContext, RunRequest, ToolResultMessage,
        UserMessage,
    };

    fn image_tool_message() -> AgentMessage {
        AgentMessage::ToolResult(ToolResultMessage {
            tool_use_id: "t1".into(),
            content: vec![
                DataBlock::text("screenshot saved"),
                DataBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "AAAA".into(),
                    },
                },
            ],
            is_error: false,
            projection: llm_harness_types::PersistedToolProjection {
                kind: llm_harness_types::ToolProjectionKind::Full,
                metadata: llm_harness_types::ProjectionMetadata::default(),
            },
            timestamp: Utc::now(),
        })
    }

    fn image_user_message() -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: vec![
                ContentBlock::Text {
                    text: "look".into(),
                },
                ContentBlock::Image {
                    source: ImageSource::Url {
                        url: "https://x/y.png".into(),
                    },
                },
            ],
            timestamp: Utc::now(),
        })
    }

    fn visual_count(messages: &[AgentMessage]) -> usize {
        messages
            .iter()
            .map(|m| match m {
                AgentMessage::ToolResult(t) => {
                    t.content.iter().filter(|b| is_visual_block(b)).count()
                }
                AgentMessage::User(u) => u
                    .content
                    .iter()
                    .filter(|b| {
                        matches!(
                            b,
                            ContentBlock::Image { .. } | ContentBlock::Document { .. }
                        )
                    })
                    .count(),
                _ => 0,
            })
            .sum()
    }

    const VISION_REJECT: &str = "model does not support image input";

    fn vision_reject_error() -> LlmError {
        LlmError::InvalidRequest(VISION_REJECT.into())
    }

    fn error_ctx<'a>(
        run: &'a RunContext,
        error: &'a LlmError,
        context: &'a mut AgentContext,
        new_messages: &'a mut Vec<AgentMessage>,
    ) -> ProviderErrorCtx<'a> {
        ProviderErrorCtx {
            run,
            turn_index: 0,
            error,
            context,
            new_messages,
        }
    }

    #[tokio::test]
    async fn strips_both_views_retries_and_marks_run_repaired() {
        let hook = VisionDegradeHook::new();
        let run = RunContext::new(RunRequest::default());
        let mut context = AgentContext {
            system_prompt: None,
            messages: vec![image_tool_message(), image_user_message()],
        };
        let mut new_messages: Vec<AgentMessage> = vec![
            image_tool_message(),
            AgentMessage::User(UserMessage {
                content: vec![ContentBlock::Text { text: "q".into() }],
                timestamp: Utc::now(),
            }),
        ];
        assert_eq!(visual_count(&context.messages), 2);
        assert_eq!(visual_count(&new_messages), 1);

        let directive = hook
            .on_provider_error(error_ctx(
                &run,
                &vision_reject_error(),
                &mut context,
                &mut new_messages,
            ))
            .await;

        assert_eq!(directive, ErrorDirective::Retry);
        assert_eq!(visual_count(&context.messages), 0, "context stripped");
        assert_eq!(visual_count(&new_messages), 0, "mirror stripped");
        assert!(hook.has_repaired(run.id()), "run marked repaired");
        // Repair notice appended to the affected messages.
        let AgentMessage::ToolResult(t) = &context.messages[0] else {
            panic!("expected tool result");
        };
        assert!(
            t.content
                .iter()
                .any(|b| matches!(b, DataBlock::Text { text, .. } if text.contains("removed")))
        );
    }

    #[tokio::test]
    async fn second_error_in_same_run_surfaces() {
        let hook = VisionDegradeHook::new();
        let run = RunContext::new(RunRequest::default());
        let mut context = AgentContext {
            system_prompt: None,
            messages: vec![image_tool_message()],
        };
        let mut new_messages: Vec<AgentMessage> = vec![];

        let first = hook
            .on_provider_error(error_ctx(
                &run,
                &vision_reject_error(),
                &mut context,
                &mut new_messages,
            ))
            .await;
        assert_eq!(first, ErrorDirective::Retry);

        // Re-add a visual block; the second error must surface (one-shot).
        context.messages.push(image_tool_message());
        let second = hook
            .on_provider_error(error_ctx(
                &run,
                &vision_reject_error(),
                &mut context,
                &mut new_messages,
            ))
            .await;
        assert_eq!(second, ErrorDirective::Surface);
    }

    #[tokio::test]
    async fn different_run_gets_fresh_one_shot_budget() {
        let hook = VisionDegradeHook::new();
        let run_a = RunContext::new(RunRequest::default());
        let run_b = RunContext::new(RunRequest::default());
        let mut context = AgentContext {
            system_prompt: None,
            messages: vec![image_tool_message()],
        };
        let mut new_messages: Vec<AgentMessage> = vec![];

        assert_eq!(
            hook.on_provider_error(error_ctx(
                &run_a,
                &vision_reject_error(),
                &mut context,
                &mut new_messages
            ))
            .await,
            ErrorDirective::Retry
        );

        // Run A's repair consumed the visual block; give run B fresh damage
        // to repair so the per-run budget is actually exercised.
        context.messages.push(image_tool_message());
        assert_eq!(
            hook.on_provider_error(error_ctx(
                &run_b,
                &vision_reject_error(),
                &mut context,
                &mut new_messages
            ))
            .await,
            ErrorDirective::Retry,
            "one-shot guard is per-run, not global"
        );
    }

    #[tokio::test]
    async fn non_invalid_request_errors_surface_without_touching_history() {
        let hook = VisionDegradeHook::new();
        let run = RunContext::new(RunRequest::default());
        let mut context = AgentContext {
            system_prompt: None,
            messages: vec![image_tool_message()],
        };
        let mut new_messages: Vec<AgentMessage> = vec![];
        let ctx = ProviderErrorCtx {
            run: &run,
            turn_index: 0,
            error: &LlmError::RateLimit {
                retry_after: Some(std::time::Duration::from_secs(1)),
            },
            context: &mut context,
            new_messages: &mut new_messages,
        };
        assert_eq!(hook.on_provider_error(ctx).await, ErrorDirective::Surface);
        assert_eq!(visual_count(&context.messages), 1, "history untouched");
        assert!(!hook.has_repaired(run.id()));
    }

    #[tokio::test]
    async fn invalid_request_without_visual_blocks_surfaces() {
        let hook = VisionDegradeHook::new();
        let run = RunContext::new(RunRequest::default());
        let mut context = AgentContext {
            system_prompt: None,
            messages: vec![AgentMessage::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
                timestamp: Utc::now(),
            })],
        };
        let mut new_messages: Vec<AgentMessage> = vec![];
        assert_eq!(
            hook.on_provider_error(error_ctx(
                &run,
                &vision_reject_error(),
                &mut context,
                &mut new_messages
            ))
            .await,
            ErrorDirective::Surface,
            "no visual blocks — different cause, must surface unchanged"
        );
        assert!(!hook.has_repaired(run.id()));
    }

    #[test]
    fn returns_none_when_no_visual_blocks() {
        let mut msgs = vec![image_tool_message()];
        if let AgentMessage::ToolResult(t) = &mut msgs[0] {
            t.content.pop(); // drop the image
        }
        assert!(strip_vision_from_context(&mut msgs).is_none());
    }

    #[test]
    fn document_blocks_are_stripped_too() {
        let mut msgs = vec![image_tool_message()];
        if let AgentMessage::ToolResult(t) = &mut msgs[0] {
            t.content.push(DataBlock::Document {
                name: Some("d.pdf".into()),
                media_type: "application/pdf".into(),
                data: BinarySource::Base64 {
                    data: "AAAA".into(),
                },
            });
        }
        let mut mirror: Vec<AgentMessage> = vec![];
        assert_eq!(strip_vision(&mut msgs, &mut mirror).unwrap(), 2);
    }

    #[test]
    fn mirror_divergence_is_tolerated_and_logged() {
        let mut ctx_msgs = vec![image_tool_message()];
        let mut mirror = vec![image_tool_message()];
        // Mirror already lost its image (e.g. the loop grew new_messages
        // since the failed request): the strip still succeeds on the primary
        // context, and the count divergence is logged, not fatal.
        if let AgentMessage::ToolResult(t) = &mut mirror[0] {
            t.content.retain(|b| !is_visual_block(b));
        }
        assert_eq!(strip_vision(&mut ctx_msgs, &mut mirror).unwrap(), 1);
    }
}
