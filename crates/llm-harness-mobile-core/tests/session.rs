//! Unit tests for the harness → mobile event mapping.

use std::sync::Arc;

use llm_harness_agent::harness::AgentHarnessEvent;
use llm_harness_types::{
    AgentError, AgentEvent, DataBlock, HarnessPhase, ToolProgress, ToolResult,
};

use llm_harness_mobile_core::{MobileEvent, map_agent_event, map_event};

fn harness_event(agent_event: AgentEvent) -> Arc<AgentHarnessEvent> {
    Arc::new(AgentHarnessEvent::Agent(agent_event))
}

#[test]
fn maps_text_delta() {
    let mobile = map_event(&harness_event(AgentEvent::TextDelta {
        message_id: "m1".to_owned(),
        text: "hello".to_owned(),
    }))
    .expect("text delta must map");
    assert!(matches!(mobile, MobileEvent::TextDelta { .. }));
}

#[test]
fn maps_tool_lifecycle() {
    let started = map_event(&harness_event(AgentEvent::ToolExecutionStart {
        tool_use_id: "t1".to_owned(),
        tool_name: "bash".to_owned(),
        args: serde_json::json!({}),
    }))
    .expect("tool start must map");
    assert!(matches!(started, MobileEvent::ToolStarted { .. }));
}

#[test]
fn maps_agent_end_to_run_finished() {
    let mobile = map_event(&harness_event(AgentEvent::AgentEnd {
        new_messages: vec![],
    }))
    .expect("agent end must map");
    assert!(matches!(mobile, MobileEvent::RunFinished));
}

#[test]
fn maps_error_to_failed() {
    let mobile =
        map_event(&harness_event(AgentEvent::Error(AgentError::Aborted))).expect("error must map");
    assert!(matches!(mobile, MobileEvent::Failed { .. }));
}

#[test]
fn maps_tool_execution_update_to_tool_update() {
    let mobile = map_agent_event(&AgentEvent::ToolExecutionUpdate {
        tool_use_id: "tu_1".into(),
        partial: ToolProgress {
            content: vec![DataBlock::Text {
                text: "l1\nl2\nl3".into(),
                mime_type: None,
            }],
            details: serde_json::Value::Null,
        },
    })
    .expect("update must map");
    assert!(matches!(mobile, MobileEvent::ToolUpdate { .. }));
}

#[test]
fn drops_non_displayable_events() {
    // PhaseChange and other harness-only events carry nothing for the UI.
    let _phase = AgentHarnessEvent::PhaseChange {
        from: HarnessPhase::Idle,
        to: HarnessPhase::Turning,
    };

    // TurnStart is loop-internal for mobile purposes.
    assert!(map_agent_event(&AgentEvent::TurnStart { index: 0 }).is_none());
}

#[test]
fn maps_harness_abort_to_run_aborted() {
    let mobile = map_event(&Arc::new(AgentHarnessEvent::Aborted)).expect("abort must map");
    assert!(matches!(mobile, MobileEvent::RunAborted));
}

#[test]
fn tool_finished_summary_is_first_line_truncated() {
    let result = ToolResult::full(
        vec![DataBlock::Text {
            text: "first line\nsecond line".to_owned(),
            mime_type: None,
        }],
        serde_json::json!({}),
        false,
    );
    let mobile = map_agent_event(&AgentEvent::ToolExecutionEnd {
        tool_use_id: "t1".to_owned(),
        result: Ok(result),
    })
    .expect("tool end must map");
    match mobile {
        MobileEvent::ToolFinished { summary, .. } => assert_eq!(summary, "first line"),
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn tool_finished_carries_full_output() {
    let text = "line one\nline two\nline three";
    let result = ToolResult::full(
        vec![DataBlock::Text {
            text: text.to_owned(),
            mime_type: None,
        }],
        serde_json::json!({}),
        false,
    );
    let mobile = map_agent_event(&AgentEvent::ToolExecutionEnd {
        tool_use_id: "t1".to_owned(),
        result: Ok(result),
    })
    .expect("tool end must map");
    match mobile {
        MobileEvent::ToolFinished {
            summary,
            full_output,
            ..
        } => {
            assert_eq!(summary, "line one");
            assert_eq!(full_output, text);
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn parses_trust_profile_strings() {
    use llm_harness_platform::trust::TrustProfile;
    assert_eq!(
        llm_harness_mobile_core::parse_trust_profile_pub("restricted").unwrap(),
        TrustProfile::Restricted
    );
    assert_eq!(
        llm_harness_mobile_core::parse_trust_profile_pub("").unwrap(),
        TrustProfile::Restricted
    );
    assert_eq!(
        llm_harness_mobile_core::parse_trust_profile_pub("trusted-lan").unwrap(),
        TrustProfile::TrustedLan
    );
    assert_eq!(
        llm_harness_mobile_core::parse_trust_profile_pub("trusted-full").unwrap(),
        TrustProfile::TrustedFull
    );
    assert!(llm_harness_mobile_core::parse_trust_profile_pub("root").is_err());
}
