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
        AgentMessage, AssistantMessage, AssistantMessageKind, ContentBlock, StopReason, UserMessage,
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

/// Live round-trip against a real gateway + LLM. Requires:
///   OHM_MOBILE_DATA_DIR=<tmp writable dir>
///   MOBILE_E2E_GATEWAY_URL / MOBILE_E2E_GATEWAY_TOKEN / MOBILE_E2E_API_KEY
/// Run: cargo test -p llm-harness-mobile-core --test manager -- --ignored
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires live gateway and LLM credentials"]
async fn manager_create_and_resume_roundtrip() {
    let gateway_url = std::env::var("MOBILE_E2E_GATEWAY_URL").expect("gateway url");
    let gateway_token = std::env::var("MOBILE_E2E_GATEWAY_TOKEN").expect("gateway token");
    let api_key = std::env::var("MOBILE_E2E_API_KEY").expect("llm api key");
    let data_dir = std::env::var("OHM_MOBILE_DATA_DIR").expect("data dir");

    let config = llm_harness_mobile_core::MobileConfig {
        gateway_url,
        gateway_token,
        api_key,
        model: "zai-org/GLM-5.2".to_owned(),
        llm_base_url: "https://api.siliconflow.cn".to_owned(),
        provider: "openai".to_owned(),
        allow_insecure_http: true,
        trust_profile: "restricted".to_owned(),
    };
    let manager = llm_harness_mobile_core::MobileSessionManager::new(
        std::env::var("OHM_MOBILE_DATA_DIR").unwrap(),
    );

    let session = manager
        .create_session(config.clone(), "e2e-smoke".to_owned())
        .expect("create session");
    let session_id = session
        .session_id()
        .expect("persisted session has id")
        .clone();
    session.clone().send("say exactly: ok".to_owned());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        match session.clone().poll_event() {
            Some(llm_harness_mobile_core::MobileEvent::RunFinished) => break,
            Some(event) => eprintln!("[probe] {event:?}"),
            None => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
        }
        assert!(std::time::Instant::now() < deadline, "timed out waiting");
    }
    session.shutdown().await.expect("shutdown");

    let resumed = manager
        .open_session(config, session_id.clone())
        .expect("resume session");
    assert!(
        resumed.sandbox_recreated(),
        "conservative resume always recreates"
    );
    let history = resumed.history().expect("history");
    assert!(
        history.iter().any(|m| m.role == "user"),
        "history should contain the user turn"
    );
    resumed.shutdown().await.expect("shutdown");
}
