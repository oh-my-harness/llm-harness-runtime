use std::collections::HashMap;

use chrono::Utc;
use llm_adapter::types::{ContentKind, StreamEvent as AdapterEvent};
use llm_harness_types::*;
use uuid::Uuid;

/// Accumulates streaming state for one LLM response.
pub(crate) struct StreamingState {
    /// Stable message ID generated at stream start.
    pub message_id: String,
    /// Stable user-turn ID owning this message.
    pub turn_id: String,
    /// Model name from the provider.
    pub model: String,
    terminal_text_classification: TerminalTextClassification,
    text_blocks: HashMap<usize, String>,
    reasoning_blocks: HashMap<usize, String>,
    reasoning_signatures: HashMap<usize, String>,
    // index → (id, name, accumulated_args)
    tool_blocks: HashMap<usize, (String, String, String)>,
    block_order: Vec<(usize, BlockKind)>,
}

enum BlockKind {
    Text,
    Reasoning,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalTextClassification {
    FinalAnswer,
    Progress,
}

impl StreamingState {
    pub(crate) fn new_with_terminal_text_classification(
        model: String,
        turn_id: String,
        terminal_text_classification: TerminalTextClassification,
    ) -> Self {
        Self {
            message_id: Uuid::now_v7().to_string(),
            turn_id,
            model,
            terminal_text_classification,
            text_blocks: Default::default(),
            reasoning_blocks: Default::default(),
            reasoning_signatures: Default::default(),
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
            AdapterEvent::ContentStop { index, signature } => {
                if let Some(sig) = signature {
                    self.reasoning_signatures.insert(index, sig);
                }
                if let Some((id, _, args)) = self.tool_blocks.get(&index) {
                    let parsed = parse_tool_args(args);
                    vec![AgentEvent::ToolCallEnd {
                        tool_use_id: id.clone(),
                        args: parsed,
                    }]
                } else {
                    vec![]
                }
            }
            AdapterEvent::MessageStop { stop_reason, usage } => {
                let content = self.build_final_content();
                let stop_reason = convert_stop_reason(stop_reason);
                // Some OpenAI-compatible providers return EndTurn/Stop even
                // when the completed message contains tool calls. The content
                // is the authoritative signal for whether the agent loop must
                // continue; committing such a message as FinalAnswer would
                // allow a later MessageStart after a terminal message boundary.
                let contains_tool_use = content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolUse { .. }));
                let kind = if contains_tool_use || stop_reason == StopReason::ToolUse {
                    AssistantMessageKind::Progress
                } else {
                    match self.terminal_text_classification {
                        TerminalTextClassification::FinalAnswer => {
                            AssistantMessageKind::FinalAnswer
                        }
                        TerminalTextClassification::Progress => AssistantMessageKind::Progress,
                    }
                };
                let message = AssistantMessage {
                    kind,
                    message_id: mid.clone(),
                    turn_id: self.turn_id.clone(),
                    content,
                    stop_reason: Some(stop_reason),
                    timestamp: Utc::now(),
                    // The current adapter stream does not expose provider/api metadata.
                    provider: None,
                    api: None,
                    model: Some(self.model.clone()),
                    usage: Some(convert_usage(usage)),
                    error_message: None,
                };
                let events = vec![AgentEvent::MessageEnd {
                    message_id: mid,
                    message: message.clone(),
                    discarded: false,
                }];
                events
            }
        }
    }

    /// Build a partial `AssistantMessage` from the current accumulated state.
    pub(crate) fn partial_message(&self) -> AssistantMessage {
        AssistantMessage {
            kind: AssistantMessageKind::Progress,
            message_id: self.message_id.clone(),
            turn_id: self.turn_id.clone(),
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

    /// Build a discarded `AssistantMessage` — a Progress-kind message with
    /// empty content, used for the discard `MessageEnd` emitted before an
    /// idle-timeout retry. Unlike `partial_message()`, this does NOT clone
    /// accumulated content blocks (they would be immediately cleared anyway).
    pub(crate) fn discarded_message(&self) -> AssistantMessage {
        AssistantMessage {
            kind: AssistantMessageKind::Progress,
            message_id: self.message_id.clone(),
            turn_id: self.turn_id.clone(),
            content: Vec::new(),
            stop_reason: None,
            timestamp: Utc::now(),
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
                    let signature = self.reasoning_signatures.get(idx).cloned();
                    ContentBlock::Thinking {
                        thinking,
                        signature,
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

fn parse_tool_args(args: &str) -> serde_json::Value {
    // Some LLMs send empty string for no-arg tools; treat as empty object.
    let effective = if args.trim().is_empty() { "{}" } else { args };
    serde_json::from_str(effective).unwrap_or_else(|err| {
        // Graceful degradation: non-OpenAI models (GLM, DeepSeek, Qwen, etc.)
        // occasionally emit tool-call arguments with invalid JSON (unescaped
        // quotes, truncation, non-ASCII keys). Returning an Error event here
        // would crash the agent loop via loop_error / error_message pollution
        // (issue #29). Instead, surface the parse error inside the args JSON
        // itself — consistent with build_final_content's degradation path —
        // so the tool executes, its result feeds back to the model, and the
        // loop continues normally.
        serde_json::json!({
            "_parse_error": err.to_string(),
            "raw": args,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_adapter::types::{StopReason as AdapterStop, Usage};

    fn make_state() -> StreamingState {
        StreamingState::new_with_terminal_text_classification(
            "test-model".into(),
            "turn_1".into(),
            TerminalTextClassification::FinalAnswer,
        )
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
        let events = s.process(AdapterEvent::ContentStop {
            index: 0,
            signature: None,
        });
        assert!(
            matches!(&events[0], AgentEvent::ToolCallEnd { tool_use_id, args }
                if tool_use_id == "c1" && args["cmd"] == "ls")
        );
    }

    #[test]
    fn invalid_tool_args_emit_degraded_json_without_error_event() {
        // Issue #29: invalid tool-call JSON must NOT produce an AgentEvent::Error.
        // Instead, the parse error is embedded in the args as a degradation JSON
        // (consistent with build_final_content), so the loop continues and the
        // tool result feeds back to the model for self-correction.
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
        let events = s.process(AdapterEvent::ContentStop {
            index: 0,
            signature: None,
        });

        // Exactly one event: ToolCallEnd with degraded args (no Error event).
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ToolCallEnd { tool_use_id, args } => {
                assert_eq!(tool_use_id, "c1");
                assert!(
                    args.get("_parse_error").is_some(),
                    "expected _parse_error field in degraded args, got {args}"
                );
                assert_eq!(args["raw"], r#"{"cmd":"ls""#);
            }
            other => panic!("expected ToolCallEnd, got {other:?}"),
        }
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
            assert_eq!(message.kind, AssistantMessageKind::FinalAnswer);
            assert_eq!(message.turn_id, "turn_1");
            assert!(!message.message_id.is_empty());
            assert!(matches!(message.stop_reason, Some(StopReason::EndTurn)));
            assert_eq!(message.usage.as_ref().unwrap().input_tokens, 5);
        }
        assert!(events.iter().any(|e| {
            if let Some((_id, turn, text)) = e.as_final_answer() {
                turn == "turn_1" && text == "done"
            } else {
                false
            }
        }));
    }
    #[test]
    fn tool_use_text_emits_progress_not_final_answer() {
        let mut s = make_state();
        s.process(AdapterEvent::ContentStart {
            index: 0,
            kind: ContentKind::Text,
        });
        s.process(AdapterEvent::TextDelta {
            index: 0,
            text: "checking first".into(),
        });
        let events = s.process(AdapterEvent::MessageStop {
            stop_reason: AdapterStop::ToolUse,
            usage: Usage::default(),
        });

        let msg_end = events
            .iter()
            .find(|e| matches!(e, AgentEvent::MessageEnd { .. }))
            .unwrap();
        if let AgentEvent::MessageEnd { message, .. } = msg_end {
            assert_eq!(message.kind, AssistantMessageKind::Progress);
            assert_eq!(message.turn_id, "turn_1");
            assert!(!message.message_id.is_empty());
        }

        assert!(events.iter().any(|e| {
            if let Some((_id, turn, text)) = e.as_progress() {
                turn == "turn_1" && text == "checking first"
            } else {
                false
            }
        }));
        assert!(!events.iter().any(|e| e.as_final_answer().is_some()));
    }
    #[test]
    fn reasoning_signature_captured_in_final_message() {
        let mut s = make_state();
        s.process(AdapterEvent::ContentStart {
            index: 0,
            kind: ContentKind::Reasoning,
        });
        s.process(AdapterEvent::ReasoningDelta {
            index: 0,
            text: "I think therefore I am".into(),
        });
        s.process(AdapterEvent::ContentStop {
            index: 0,
            signature: Some("test-signature-abc".into()),
        });
        let events = s.process(AdapterEvent::MessageStop {
            stop_reason: AdapterStop::EndTurn,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            },
        });
        let msg_end = events
            .iter()
            .find(|e| matches!(e, AgentEvent::MessageEnd { .. }))
            .unwrap();
        if let AgentEvent::MessageEnd { message, .. } = msg_end {
            let thinking_block = message
                .content
                .iter()
                .find(|cb| matches!(cb, ContentBlock::Thinking { .. }));
            assert!(thinking_block.is_some(), "expected a Thinking block");
            if let ContentBlock::Thinking { signature, .. } = thinking_block.unwrap() {
                assert_eq!(signature.as_deref(), Some("test-signature-abc"));
            }
        }
    }

    #[test]
    fn tool_use_block_with_end_turn_emits_progress_not_final_answer() {
        let mut s = make_state();
        s.process(AdapterEvent::ContentStart {
            index: 0,
            kind: ContentKind::ToolInvocation {
                id: "call_1".into(),
                name: "echo".into(),
            },
        });
        s.process(AdapterEvent::ToolDelta {
            index: 0,
            arguments: r#"{"value":"ok"}"#.into(),
        });
        s.process(AdapterEvent::ContentStop {
            index: 0,
            signature: None,
        });

        let events = s.process(AdapterEvent::MessageStop {
            stop_reason: AdapterStop::EndTurn,
            usage: Usage::default(),
        });
        let msg_end = events
            .iter()
            .find(|event| matches!(event, AgentEvent::MessageEnd { .. }))
            .expect("MessageStop must emit MessageEnd");

        if let AgentEvent::MessageEnd { message, .. } = msg_end {
            assert_eq!(message.kind, AssistantMessageKind::Progress);
            assert!(matches!(message.stop_reason, Some(StopReason::EndTurn)));
            assert!(message.content.iter().any(|block| matches!(
                block,
                ContentBlock::ToolUse { name, .. } if name == "echo"
            )));
        }
        assert!(msg_end.as_progress().is_some());
        assert!(msg_end.as_final_answer().is_none());
    }

    #[test]
    fn reasoning_without_signature_stays_none() {
        let mut s = make_state();
        s.process(AdapterEvent::ContentStart {
            index: 0,
            kind: ContentKind::Reasoning,
        });
        s.process(AdapterEvent::ReasoningDelta {
            index: 0,
            text: "thinking...".into(),
        });
        s.process(AdapterEvent::ContentStop {
            index: 0,
            signature: None,
        });
        let events = s.process(AdapterEvent::MessageStop {
            stop_reason: AdapterStop::EndTurn,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            },
        });
        let msg_end = events
            .iter()
            .find(|e| matches!(e, AgentEvent::MessageEnd { .. }))
            .unwrap();
        if let AgentEvent::MessageEnd { message, .. } = msg_end {
            let thinking_block = message
                .content
                .iter()
                .find(|cb| matches!(cb, ContentBlock::Thinking { .. }));
            assert!(thinking_block.is_some(), "expected a Thinking block");
            if let ContentBlock::Thinking { signature, .. } = thinking_block.unwrap() {
                assert!(signature.is_none());
            }
        }
    }

    #[test]
    fn multiple_reasoning_blocks_with_distinct_signatures() {
        let mut s = make_state();
        // Block 0: thinking with signature
        s.process(AdapterEvent::ContentStart {
            index: 0,
            kind: ContentKind::Reasoning,
        });
        s.process(AdapterEvent::ReasoningDelta {
            index: 0,
            text: "first thought".into(),
        });
        s.process(AdapterEvent::ContentStop {
            index: 0,
            signature: Some("sig-a".into()),
        });
        // Block 1: text (no signature)
        s.process(AdapterEvent::ContentStart {
            index: 1,
            kind: ContentKind::Text,
        });
        s.process(AdapterEvent::TextDelta {
            index: 1,
            text: "hello".into(),
        });
        s.process(AdapterEvent::ContentStop {
            index: 1,
            signature: None,
        });
        // Block 2: thinking without signature
        s.process(AdapterEvent::ContentStart {
            index: 2,
            kind: ContentKind::Reasoning,
        });
        s.process(AdapterEvent::ReasoningDelta {
            index: 2,
            text: "second thought".into(),
        });
        s.process(AdapterEvent::ContentStop {
            index: 2,
            signature: None,
        });

        let events = s.process(AdapterEvent::MessageStop {
            stop_reason: AdapterStop::EndTurn,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            },
        });
        if let Some(AgentEvent::MessageEnd { message, .. }) = events
            .iter()
            .find(|e| matches!(e, AgentEvent::MessageEnd { .. }))
        {
            // Block 0: Thinking with signature
            let block0 = &message.content[0];
            if let ContentBlock::Thinking {
                thinking,
                signature,
            } = block0
            {
                assert_eq!(thinking, "first thought");
                assert_eq!(signature.as_deref(), Some("sig-a"));
            } else {
                panic!("expected Thinking block at index 0");
            }
            // Block 1: Text
            assert!(matches!(&message.content[1], ContentBlock::Text { text } if text == "hello"));
            // Block 2: Thinking without signature
            let block2 = &message.content[2];
            if let ContentBlock::Thinking {
                thinking,
                signature,
            } = block2
            {
                assert_eq!(thinking, "second thought");
                assert!(signature.is_none());
            } else {
                panic!("expected Thinking block at index 2");
            }
        } else {
            panic!("expected MessageEnd event");
        }
    }

    #[test]
    fn non_reasoning_block_with_signature_is_ignored() {
        // 防御性测试：若上游适配器对非推理块错误地提供了签名，不应崩溃或串扰。
        let mut s = make_state();
        s.process(AdapterEvent::ContentStart {
            index: 0,
            kind: ContentKind::Text,
        });
        s.process(AdapterEvent::TextDelta {
            index: 0,
            text: "ok".into(),
        });
        // 异常：文本块的 ContentStop 携带了签名
        s.process(AdapterEvent::ContentStop {
            index: 0,
            signature: Some("bogus-sig".into()),
        });
        let events = s.process(AdapterEvent::MessageStop {
            stop_reason: AdapterStop::EndTurn,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            },
        });
        // 文本块不应受签名影响，签名被静默忽略
        if let Some(AgentEvent::MessageEnd { message, .. }) = events
            .iter()
            .find(|e| matches!(e, AgentEvent::MessageEnd { .. }))
        {
            assert!(matches!(&message.content[0], ContentBlock::Text { text } if text == "ok"));
            // 不 crash 即通过
        }
    }
}
