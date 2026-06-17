use std::collections::HashMap;

use chrono::Utc;
use llm_adapter::types::{ContentKind, StreamEvent as AdapterEvent};
use llm_harness_types::*;
use uuid::Uuid;

/// Accumulates streaming state for one LLM response.
pub(crate) struct StreamingState {
    /// Stable message ID generated at stream start.
    pub message_id: String,
    /// Model name from the provider.
    pub model: String,
    text_blocks: HashMap<usize, String>,
    reasoning_blocks: HashMap<usize, String>,
    // index → (id, name, accumulated_args)
    tool_blocks: HashMap<usize, (String, String, String)>,
    block_order: Vec<(usize, BlockKind)>,
}

enum BlockKind {
    Text,
    Reasoning,
    Tool,
}

impl StreamingState {
    pub(crate) fn new(model: String) -> Self {
        Self {
            message_id: Uuid::now_v7().to_string(),
            model,
            text_blocks: Default::default(),
            reasoning_blocks: Default::default(),
            tool_blocks: Default::default(),
            block_order: vec![],
        }
    }

    /// Process one adapter `StreamEvent`; returns zero or more `AgentEvent`s to emit.
    pub(crate) fn process(&mut self, event: AdapterEvent) -> Vec<AgentEvent> {
        use crate::type_bridge::{convert_stop_reason, convert_usage};
        let mid = self.message_id.clone();

        match event {
            AdapterEvent::ContentStart { index, kind } => match kind {
                ContentKind::Text => {
                    self.text_blocks.insert(index, String::new());
                    self.block_order.push((index, BlockKind::Text));
                    vec![]
                }
                ContentKind::Reasoning => {
                    self.reasoning_blocks.insert(index, String::new());
                    self.block_order.push((index, BlockKind::Reasoning));
                    vec![]
                }
                ContentKind::ToolInvocation { id, name } => {
                    self.tool_blocks
                        .insert(index, (id.clone(), name.clone(), String::new()));
                    self.block_order.push((index, BlockKind::Tool));
                    vec![AgentEvent::ToolCallStart {
                        message_id: mid,
                        tool_use_id: id,
                        name,
                    }]
                }
            },
            AdapterEvent::TextDelta { index, text } => {
                self.text_blocks.entry(index).or_default().push_str(&text);
                let partial = self.partial_message();
                vec![
                    AgentEvent::TextDelta {
                        message_id: mid.clone(),
                        text,
                    },
                    AgentEvent::MessageUpdate {
                        message_id: mid,
                        partial,
                    },
                ]
            }
            AdapterEvent::ReasoningDelta { index, text } => {
                self.reasoning_blocks
                    .entry(index)
                    .or_default()
                    .push_str(&text);
                let partial = self.partial_message();
                vec![
                    AgentEvent::ThinkingDelta {
                        message_id: mid.clone(),
                        thinking: text,
                        signature: None,
                    },
                    AgentEvent::MessageUpdate {
                        message_id: mid,
                        partial,
                    },
                ]
            }
            AdapterEvent::ToolDelta { index, arguments } => {
                if let Some((id, _, args)) = self.tool_blocks.get_mut(&index) {
                    args.push_str(&arguments);
                    let id = id.clone();
                    vec![AgentEvent::ToolCallArgsDelta {
                        tool_use_id: id,
                        partial_input: arguments,
                    }]
                } else {
                    vec![]
                }
            }
            AdapterEvent::ContentStop { index } => {
                if let Some((id, _, args)) = self.tool_blocks.get(&index) {
                    let (parsed, parse_error) = parse_tool_args(id, args);
                    let mut events = Vec::new();
                    if let Some(err) = parse_error {
                        events.push(err);
                    }
                    events.push(AgentEvent::ToolCallEnd {
                        tool_use_id: id.clone(),
                        args: parsed,
                    });
                    events
                } else {
                    vec![]
                }
            }
            AdapterEvent::MessageStop { stop_reason, usage } => {
                let content = self.build_final_content();
                let message = AssistantMessage {
                    content,
                    stop_reason: Some(convert_stop_reason(stop_reason)),
                    timestamp: Utc::now(),
                    // The current adapter stream does not expose provider/api metadata.
                    provider: None,
                    api: None,
                    model: Some(self.model.clone()),
                    usage: Some(convert_usage(usage)),
                    error_message: None,
                };
                vec![AgentEvent::MessageEnd {
                    message_id: mid,
                    message,
                }]
            }
        }
    }

    /// Build a partial `AssistantMessage` from the current accumulated state.
    pub(crate) fn partial_message(&self) -> AssistantMessage {
        AssistantMessage {
            content: self.build_final_content(),
            stop_reason: None,
            timestamp: Utc::now(),
            // The current adapter stream does not expose provider/api metadata.
            provider: None,
            api: None,
            model: Some(self.model.clone()),
            usage: None,
            error_message: None,
        }
    }

    fn build_final_content(&self) -> Vec<ContentBlock> {
        let mut indexed: Vec<(usize, ContentBlock)> = Vec::new();
        for (idx, kind) in &self.block_order {
            let cb = match kind {
                BlockKind::Text => {
                    let text = self.text_blocks.get(idx).cloned().unwrap_or_default();
                    ContentBlock::Text { text }
                }
                BlockKind::Reasoning => {
                    let thinking = self.reasoning_blocks.get(idx).cloned().unwrap_or_default();
                    ContentBlock::Thinking {
                        thinking,
                        signature: None,
                    }
                }
                BlockKind::Tool => {
                    if let Some((id, name, args)) = self.tool_blocks.get(idx) {
                        let input = serde_json::from_str(args).unwrap_or_else(|err| {
                            serde_json::json!({
                                "_parse_error": err.to_string(),
                                "raw": args,
                            })
                        });
                        ContentBlock::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input,
                        }
                    } else {
                        continue;
                    }
                }
            };
            indexed.push((*idx, cb));
        }
        indexed.sort_by_key(|(i, _)| *i);
        indexed.into_iter().map(|(_, cb)| cb).collect()
    }
}

fn parse_tool_args(tool_use_id: &str, args: &str) -> (serde_json::Value, Option<AgentEvent>) {
    match serde_json::from_str(args) {
        Ok(value) => (value, None),
        Err(err) => (
            serde_json::Value::Null,
            Some(AgentEvent::Error(AgentError::Internal(format!(
                "failed to parse tool args for {tool_use_id}: {err}"
            )))),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_adapter::types::{StopReason as AdapterStop, Usage};

    fn make_state() -> StreamingState {
        StreamingState::new("test-model".into())
    }

    #[test]
    fn content_start_text_returns_no_events() {
        let mut s = make_state();
        let events = s.process(AdapterEvent::ContentStart {
            index: 0,
            kind: ContentKind::Text,
        });
        assert!(events.is_empty());
    }

    #[test]
    fn text_delta_emits_text_delta_and_message_update() {
        let mut s = make_state();
        s.process(AdapterEvent::ContentStart {
            index: 0,
            kind: ContentKind::Text,
        });
        let events = s.process(AdapterEvent::TextDelta {
            index: 0,
            text: "hello".into(),
        });
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], AgentEvent::TextDelta { text, .. } if text == "hello"));
        assert!(matches!(&events[1], AgentEvent::MessageUpdate { .. }));
    }

    #[test]
    fn reasoning_delta_emits_thinking_delta() {
        let mut s = make_state();
        s.process(AdapterEvent::ContentStart {
            index: 0,
            kind: ContentKind::Reasoning,
        });
        let events = s.process(AdapterEvent::ReasoningDelta {
            index: 0,
            text: "think".into(),
        });
        assert!(
            matches!(&events[0], AgentEvent::ThinkingDelta { thinking, signature: None, .. }
                if thinking == "think")
        );
    }

    #[test]
    fn tool_content_start_emits_tool_call_start() {
        let mut s = make_state();
        let events = s.process(AdapterEvent::ContentStart {
            index: 0,
            kind: ContentKind::ToolInvocation {
                id: "c1".into(),
                name: "bash".into(),
            },
        });
        assert!(
            matches!(&events[0], AgentEvent::ToolCallStart { tool_use_id, name, .. }
                if tool_use_id == "c1" && name == "bash")
        );
    }

    #[test]
    fn tool_delta_emits_args_delta() {
        let mut s = make_state();
        s.process(AdapterEvent::ContentStart {
            index: 0,
            kind: ContentKind::ToolInvocation {
                id: "c1".into(),
                name: "bash".into(),
            },
        });
        let events = s.process(AdapterEvent::ToolDelta {
            index: 0,
            arguments: r#"{"cmd":"#.into(),
        });
        assert!(
            matches!(&events[0], AgentEvent::ToolCallArgsDelta { tool_use_id, partial_input }
                if tool_use_id == "c1" && partial_input.contains("cmd"))
        );
    }

    #[test]
    fn content_stop_for_tool_emits_tool_call_end_with_parsed_args() {
        let mut s = make_state();
        s.process(AdapterEvent::ContentStart {
            index: 0,
            kind: ContentKind::ToolInvocation {
                id: "c1".into(),
                name: "bash".into(),
            },
        });
        s.process(AdapterEvent::ToolDelta {
            index: 0,
            arguments: r#"{"cmd":"ls"}"#.into(),
        });
        let events = s.process(AdapterEvent::ContentStop { index: 0 });
        assert!(
            matches!(&events[0], AgentEvent::ToolCallEnd { tool_use_id, args }
                if tool_use_id == "c1" && args["cmd"] == "ls")
        );
    }

    #[test]
    fn invalid_tool_args_emit_diagnostic_before_tool_call_end() {
        let mut s = make_state();
        s.process(AdapterEvent::ContentStart {
            index: 0,
            kind: ContentKind::ToolInvocation {
                id: "c1".into(),
                name: "bash".into(),
            },
        });
        s.process(AdapterEvent::ToolDelta {
            index: 0,
            arguments: r#"{"cmd":"ls""#.into(),
        });
        let events = s.process(AdapterEvent::ContentStop { index: 0 });

        assert!(
            matches!(&events[0], AgentEvent::Error(AgentError::Internal(msg))
            if msg.contains("failed to parse tool args for c1"))
        );
        assert!(
            matches!(&events[1], AgentEvent::ToolCallEnd { tool_use_id, args }
            if tool_use_id == "c1" && args.is_null())
        );
    }

    #[test]
    fn message_stop_emits_message_end_with_complete_assistant_message() {
        let mut s = make_state();
        s.process(AdapterEvent::ContentStart {
            index: 0,
            kind: ContentKind::Text,
        });
        s.process(AdapterEvent::TextDelta {
            index: 0,
            text: "done".into(),
        });
        let events = s.process(AdapterEvent::MessageStop {
            stop_reason: AdapterStop::EndTurn,
            usage: Usage {
                input_tokens: 5,
                output_tokens: 3,
                ..Default::default()
            },
        });
        let msg_end = events
            .iter()
            .find(|e| matches!(e, AgentEvent::MessageEnd { .. }));
        assert!(msg_end.is_some());
        if let AgentEvent::MessageEnd { message, .. } = msg_end.unwrap() {
            assert!(matches!(message.stop_reason, Some(StopReason::EndTurn)));
            assert_eq!(message.usage.as_ref().unwrap().input_tokens, 5);
        }
    }
}
