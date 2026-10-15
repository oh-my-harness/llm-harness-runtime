//! Map harness events onto the small `MobileEvent` surface the UI consumes.

use std::sync::Arc;

use llm_harness_agent::harness::AgentHarnessEvent;
use llm_harness_types::AgentEvent;

/// Events the mobile UI renders. `#[non_exhaustive]` upstream events that
/// carry nothing displayable are dropped by [`map_event`].
#[derive(Debug, Clone, uniffi::Enum)]
pub enum MobileEvent {
    /// Assistant text delta (streaming reply).
    TextDelta {
        text: String,
    },
    /// A tool started executing in the sandbox.
    ToolStarted {
        tool_use_id: String,
        name: String,
    },
    /// Streaming tool output chunk (throttled by the harness caller);
    /// `text` carries the accumulated tail for the live preview.
    ToolUpdate {
        tool_use_id: String,
        text: String,
    },
    /// A tool finished; `summary` is a one-line UI hint and `full_output`
    /// carries the complete (untruncated) tool result for expandable cards.
    ToolFinished {
        tool_use_id: String,
        name: String,
        summary: String,
        full_output: String,
    },
    RunFinished,
    /// The run was cancelled by the user.
    RunAborted,
    /// The run failed; message is display-ready.
    Failed {
        message: String,
    },
}

/// Map one harness event to zero or one mobile event.
pub fn map_event(event: &Arc<AgentHarnessEvent>) -> Option<MobileEvent> {
    match event.as_ref() {
        AgentHarnessEvent::Agent(agent_event) => map_agent_event(agent_event),
        AgentHarnessEvent::Aborted => Some(MobileEvent::RunAborted),
        _ => None,
    }
}

/// Map a raw loop event; also used directly by unit tests.
pub fn map_agent_event(event: &AgentEvent) -> Option<MobileEvent> {
    match event {
        AgentEvent::TextDelta { text, .. } => Some(MobileEvent::TextDelta { text: text.clone() }),
        AgentEvent::ToolExecutionStart {
            tool_use_id,
            tool_name,
            ..
        } => Some(MobileEvent::ToolStarted {
            tool_use_id: tool_use_id.clone(),
            name: tool_name.clone(),
        }),
        AgentEvent::ToolExecutionEnd {
            tool_use_id,
            result,
            ..
        } => {
            let (name, summary, full_output) = match result {
                Ok(tool_result) => {
                    let text = tool_result
                        .model_content
                        .iter()
                        .find_map(|block| match block {
                            llm_harness_types::DataBlock::Text { text, .. } => Some(text.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    let summary: String = text.lines().next().unwrap_or_default().to_owned();
                    let summary = if summary.chars().count() > 120 {
                        let truncated: String = summary.chars().take(120).collect();
                        format!("{truncated}…")
                    } else {
                        summary
                    };
                    ("tool".to_owned(), summary, text.to_owned())
                }
                Err(failure) => (
                    "tool".to_owned(),
                    failure.model_message.clone(),
                    String::new(),
                ),
            };
            Some(MobileEvent::ToolFinished {
                tool_use_id: tool_use_id.clone(),
                name,
                summary,
                full_output,
            })
        }
        AgentEvent::ToolExecutionUpdate {
            tool_use_id,
            partial,
            ..
        } => {
            // Tail preview: last 8 lines of the accumulated tool output.
            let text: String = partial
                .content
                .iter()
                .find_map(|block| match block {
                    llm_harness_types::DataBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let tail: Vec<&str> = text.lines().rev().take(8).collect::<Vec<_>>();
            let tail: String = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
            Some(MobileEvent::ToolUpdate {
                tool_use_id: tool_use_id.clone(),
                text: tail,
            })
        }
        AgentEvent::AgentEnd { .. } => Some(MobileEvent::RunFinished),
        AgentEvent::Error(error) => Some(MobileEvent::Failed {
            message: error.to_string(),
        }),
        _ => None,
    }
}
