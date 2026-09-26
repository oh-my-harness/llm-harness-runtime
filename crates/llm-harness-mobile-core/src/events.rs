//! Map harness events onto the small `MobileEvent` surface the UI consumes.

use std::sync::Arc;

use llm_harness_agent::harness::AgentHarnessEvent;
use llm_harness_types::AgentEvent;

/// Events the mobile UI renders. `#[non_exhaustive]` upstream events that
/// carry nothing displayable are dropped by [`map_event`].
#[derive(Debug, Clone, uniffi::Enum)]
pub enum MobileEvent {
    /// Assistant text delta (streaming reply).
    TextDelta { text: String },
    /// A tool started executing in the sandbox.
    ToolStarted { name: String },
    /// A tool finished; `summary` is a one-line UI hint.
    ToolFinished { name: String, summary: String },
    /// The current run finished.
    RunFinished,
    /// The run failed; message is display-ready.
    Failed { message: String },
}

/// Map one harness event to zero or one mobile event.
pub fn map_event(event: &Arc<AgentHarnessEvent>) -> Option<MobileEvent> {
    let AgentHarnessEvent::Agent(agent_event) = event.as_ref() else {
        return None;
    };
    map_agent_event(agent_event)
}

/// Map a raw loop event; also used directly by unit tests.
pub fn map_agent_event(event: &AgentEvent) -> Option<MobileEvent> {
    match event {
        AgentEvent::TextDelta { text, .. } => Some(MobileEvent::TextDelta { text: text.clone() }),
        AgentEvent::ToolExecutionStart { tool_name, .. } => Some(MobileEvent::ToolStarted {
            name: tool_name.clone(),
        }),
        AgentEvent::ToolExecutionEnd { result, .. } => {
            let (name, summary) = match result {
                Ok(tool_result) => {
                    let text = tool_result
                        .model_content
                        .iter()
                        .find_map(|block| match block {
                            llm_harness_types::DataBlock::Text { text, .. } => Some(text.as_str()),
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
                    ("tool".to_owned(), summary)
                }
                Err(failure) => ("tool".to_owned(), failure.model_message.clone()),
            };
            Some(MobileEvent::ToolFinished { name, summary })
        }
        AgentEvent::AgentEnd { .. } => Some(MobileEvent::RunFinished),
        AgentEvent::Error(error) => Some(MobileEvent::Failed {
            message: error.to_string(),
        }),
        _ => None,
    }
}
