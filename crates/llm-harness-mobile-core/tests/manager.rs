//! Tests for the session manager's local state sidecar and mapping logic.
//!
//! Gateway/LLM integration is covered elsewhere; these tests pin the pure
//! logic: sidecar roundtrip, defaults, and summary ordering inputs.

use llm_harness_mobile_core::manager::state::{SessionState, read_state, write_state};

#[test]
fn state_sidecar_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s1");
    std::fs::create_dir_all(&path).unwrap();
    let state = SessionState {
        sandbox_id: "abc".to_owned(),
        trust_profile: "trusted-lan".to_owned(),
        last_active_at_ms: 42,
    };
    write_state(&path, &state).unwrap();
    let loaded = read_state(&path).unwrap();
    assert_eq!(loaded.sandbox_id, "abc");
    assert_eq!(loaded.trust_profile, "trusted-lan");
    assert_eq!(loaded.last_active_at_ms, 42);
}

#[test]
fn read_state_missing_file_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s2");
    std::fs::create_dir_all(&path).unwrap();
    let loaded = read_state(&path).unwrap();
    assert!(loaded.sandbox_id.is_empty());
    assert!(loaded.trust_profile.is_empty());
    assert_eq!(loaded.last_active_at_ms, 0);
}

#[test]
fn read_state_corrupt_file_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s3");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("state.json"), b"not json").unwrap();
    let loaded = read_state(&path).unwrap();
    assert!(loaded.sandbox_id.is_empty());
}

#[test]
fn state_sidecar_updates_last_active() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s4");
    std::fs::create_dir_all(&path).unwrap();
    let mut state = SessionState::default();
    state.touch(1000);
    write_state(&path, &state).unwrap();
    let loaded = read_state(&path).unwrap();
    assert_eq!(loaded.last_active_at_ms, 1000);
}

mod history_mapping {
    use llm_harness_types::{
        AgentMessage, AssistantMessage, AssistantMessageKind, ContentBlock, DataBlock,
        PersistedToolProjection, ProjectionMetadata, StopReason, ToolProjectionKind,
        ToolResultMessage, UserMessage,
    };

    fn user_msg(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
            timestamp: chrono::Utc::now(),
        })
    }

    fn assistant_msg(text: &str, kind: AssistantMessageKind) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            kind,
            message_id: "m1".into(),
            turn_id: "turn1".into(),
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
            stop_reason: Some(StopReason::EndTurn),
            timestamp: chrono::Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: Default::default(),
            error_message: None,
        })
    }

    #[test]
    fn maps_user_and_assistant_text() {
        let msgs = vec![
            user_msg("hi"),
            assistant_msg(
                "hello",
                llm_harness_types::AssistantMessageKind::FinalAnswer,
            ),
        ];
        let out = llm_harness_mobile_core::manager::history_to_mobile(&msgs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, "user");
        assert_eq!(out[0].text, "hi");
        assert!(!out[0].is_error);
        assert_eq!(out[1].role, "assistant");
        assert_eq!(out[1].text, "hello");
    }

    #[test]
    fn maps_tool_result_with_error_flag() {
        let msgs = vec![AgentMessage::ToolResult(
            llm_harness_types::ToolResultMessage {
                tool_use_id: "t1".into(),
                content: vec![llm_harness_types::DataBlock::text("boom")],
                is_error: true,
                projection: llm_harness_types::PersistedToolProjection {
                    kind: llm_harness_types::ToolProjectionKind::Full,
                    metadata: llm_harness_types::ProjectionMetadata::default(),
                },
                timestamp: chrono::Utc::now(),
            },
        )];
        let out = llm_harness_mobile_core::manager::history_to_mobile(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "tool");
        assert!(out[0].is_error);
        assert_eq!(out[0].text, "boom");
    }

    #[test]
    fn skips_progress_and_non_text_blocks() {
        let msgs = vec![
            assistant_msg(
                "working...",
                llm_harness_types::AssistantMessageKind::Progress,
            ),
            assistant_msg(
                "final answer",
                llm_harness_types::AssistantMessageKind::FinalAnswer,
            ),
        ];
        let out = llm_harness_mobile_core::manager::history_to_mobile(&msgs);
        // Progress messages are skipped: replay shows durable turns only.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "final answer");
    }
}
