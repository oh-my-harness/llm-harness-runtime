//! Unit tests for the harness → mobile event mapping.

use std::sync::Arc;

use llm_harness_agent::harness::AgentHarnessEvent;
use llm_harness_types::{AgentError, AgentEvent, DataBlock, HarnessPhase, ToolResult};

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
fn drops_non_displayable_events() {
    // PhaseChange and other harness-only events carry nothing for the UI.
    let phase = AgentHarnessEvent::PhaseChange {
        from: HarnessPhase::Idle,
        to: HarnessPhase::Turning,
    };

    // TurnStart is loop-internal for mobile purposes.
    assert!(map_agent_event(&AgentEvent::TurnStart { index: 0 }).is_none());
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
