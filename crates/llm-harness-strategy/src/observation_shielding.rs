//! observation_shielding.rs — Hides stale tool observations from the model.
//!
//! The hook rewrites the *model-visible* context only; it does not mutate the
//! durable session. Results are grouped by the assistant turn that issued the
//! corresponding `ToolUse`, and the newest `retained_turns` assistant turns
//! remain intact. Unmapped results are left unchanged as a fail-open safety
//! net for replayed or imported histories.
//!
//! Unlike compaction, this is a deterministic, lossless-at-the-source transform
//! for noisy tool output: raw data remains in the session while the next LLM
//! request sees a compact placeholder.

use std::collections::HashMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_agent::{HarnessHooks, Plugin};
use llm_harness_types::{
    AgentContext, AgentError, AgentMessage, ContentBlock, DataBlock, TransformContextCtx,
    TransformContextHook,
};

const DEFAULT_RETAINED_TURNS: usize = 5;
const DEFAULT_PLACEHOLDER: &str =
    "[observation shielding: older tool output is hidden to preserve context]";

#[derive(Debug, Clone)]
pub struct ObservationShieldingConfig {
    pub retained_turns: usize,
    pub placeholder: String,
}

impl Default for ObservationShieldingConfig {
    fn default() -> Self {
        Self {
            retained_turns: DEFAULT_RETAINED_TURNS,
            placeholder: DEFAULT_PLACEHOLDER.to_string(),
        }
    }
}

pub struct ObservationShieldingHook {
    config: ObservationShieldingConfig,
}

impl ObservationShieldingHook {
    pub fn new(config: ObservationShieldingConfig) -> Self {
        Self { config }
    }

    fn shield(&self, mut context: AgentContext) -> AgentContext {
        let turn_ids: Vec<String> = context
            .messages
            .iter()
            .filter_map(|message| match message {
                AgentMessage::Assistant(assistant) => Some(assistant.turn_id.clone()),
                _ => None,
            })
            .collect();
        if turn_ids.len() <= self.config.retained_turns {
            return context;
        }
        let oldest_retained = turn_ids.len() - self.config.retained_turns;

        let turn_index_by_id: HashMap<&str, usize> = turn_ids
            .iter()
            .enumerate()
            .map(|(index, turn_id)| (turn_id.as_str(), index))
            .collect();
        let tool_turn: HashMap<String, usize> = context
            .messages
            .iter()
            .filter_map(|message| match message {
                AgentMessage::Assistant(assistant) => Some(
                    assistant
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::ToolUse { id, .. } => turn_index_by_id
                                .get(assistant.turn_id.as_str())
                                .map(|&turn_index| (id.clone(), turn_index)),
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .flatten()
            .collect();

        for message in &mut context.messages {
            let AgentMessage::ToolResult(result) = message else {
                continue;
            };
            let Some(&turn_index) = tool_turn.get(&result.tool_use_id) else {
                continue;
            };
            if turn_index < oldest_retained {
                result.content = vec![DataBlock::text(self.config.placeholder.clone())];
            }
        }
        context
    }
}

impl TransformContextHook for ObservationShieldingHook {
    fn transform<'a>(
        &'a self,
        mut ctx: TransformContextCtx<'a>,
    ) -> BoxFuture<'a, Result<AgentContext, AgentError>> {
        Box::pin(async move {
            ctx.context = self.shield(ctx.context);
            Ok(ctx.context)
        })
    }
}

pub struct ObservationShieldingPlugin {
    hook: Arc<ObservationShieldingHook>,
}

impl ObservationShieldingPlugin {
    pub fn new(config: ObservationShieldingConfig) -> Self {
        Self {
            hook: Arc::new(ObservationShieldingHook::new(config)),
        }
    }
}

impl Default for ObservationShieldingPlugin {
    fn default() -> Self {
        Self::new(ObservationShieldingConfig::default())
    }
}

impl Plugin for ObservationShieldingPlugin {
    fn name(&self) -> &str {
        "observation-shielding"
    }

    fn register_hooks(&self, hooks: &mut HarnessHooks) {
        hooks.transform_context.push(self.hook.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use llm_harness_types::{
        PersistedToolProjection, ProjectionMetadata, RunContext, RunRequest, ToolResultMessage,
    };

    fn assistant(turn: usize, tool_use_id: Option<&str>) -> AgentMessage {
        let mut content = vec![ContentBlock::Text {
            text: format!("assistant {turn}"),
        }];
        if let Some(tool_use_id) = tool_use_id {
            content.push(ContentBlock::ToolUse {
                id: tool_use_id.into(),
                name: "read".into(),
                input: serde_json::json!({"path": format!("{turn}.txt")}),
            });
        }
        let mut message = llm_harness_loop::test_utils::test_assistant_message(content);
        message.turn_id = format!("turn-{turn}");
        AgentMessage::Assistant(message)
    }

    fn tool_result(tool_use_id: &str, text: &str) -> AgentMessage {
        AgentMessage::ToolResult(ToolResultMessage {
            tool_use_id: tool_use_id.into(),
            content: vec![DataBlock::text(text)],
            is_error: false,
            projection: PersistedToolProjection {
                kind: llm_harness_types::ToolProjectionKind::Full,
                metadata: ProjectionMetadata::default(),
            },
            timestamp: Utc::now(),
        })
    }

    fn context(turn_count: usize) -> AgentContext {
        let mut messages = Vec::new();
        for turn in 0..turn_count {
            let tool_use_id = format!("tool-{turn}");
            messages.push(assistant(turn, Some(&tool_use_id)));
            messages.push(tool_result(&tool_use_id, &format!("result {turn}")));
        }
        AgentContext {
            system_prompt: Some("test".into()),
            messages,
        }
    }

    fn tool_text(message: &AgentMessage) -> &str {
        let AgentMessage::ToolResult(result) = message else {
            panic!("expected tool result");
        };
        match &result.content[0] {
            DataBlock::Text { text, .. } => text,
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn keeps_recent_turns_by_default() {
        let hook = ObservationShieldingHook::new(ObservationShieldingConfig::default());
        let context = hook.shield(context(5));

        assert!(context.messages.iter().all(|message| {
            !matches!(message, AgentMessage::ToolResult(_))
                || tool_text(message).starts_with("result ")
        }));
    }

    #[test]
    fn shields_older_tool_results() {
        let hook = ObservationShieldingHook::new(ObservationShieldingConfig {
            retained_turns: 2,
            ..Default::default()
        });
        let context = hook.shield(context(5));

        for (turn, message) in context
            .messages
            .iter()
            .filter(|message| matches!(message, AgentMessage::ToolResult(_)))
            .enumerate()
        {
            if turn < 3 {
                assert_eq!(tool_text(message), DEFAULT_PLACEHOLDER);
            } else {
                assert_eq!(tool_text(message), format!("result {turn}"));
            }
        }
    }

    #[test]
    fn preserves_tool_use_blocks() {
        let hook = ObservationShieldingHook::new(ObservationShieldingConfig {
            retained_turns: 0,
            ..Default::default()
        });
        let context = hook.shield(context(2));

        for message in &context.messages {
            let AgentMessage::Assistant(result) = message else {
                continue;
            };
            assert!(
                result
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
            );
        }
    }

    #[test]
    fn leaves_unmapped_tool_results_unchanged() {
        let hook = ObservationShieldingHook::new(ObservationShieldingConfig {
            retained_turns: 0,
            ..Default::default()
        });
        let mut context = context(1);
        context
            .messages
            .push(tool_result("orphan", "important old result"));

        let context = hook.shield(context);
        assert_eq!(
            tool_text(context.messages.last().unwrap()),
            "important old result"
        );
    }

    #[tokio::test]
    async fn transform_applies_shielding() {
        let hook = ObservationShieldingHook::new(ObservationShieldingConfig::default());
        let run = RunContext::new(RunRequest::from_text("test"));
        let ctx = TransformContextCtx {
            run: &run,
            turn_index: 7,
            abort: tokio_util::sync::CancellationToken::new(),
            context: context(7),
        };

        let result = hook.transform(ctx).await.unwrap();
        let old_result = result
            .messages
            .iter()
            .find(|message| matches!(message, AgentMessage::ToolResult(_)));

        assert_eq!(tool_text(old_result.unwrap()), DEFAULT_PLACEHOLDER);
    }
}
