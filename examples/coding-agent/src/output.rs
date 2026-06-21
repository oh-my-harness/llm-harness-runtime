//! Event-to-terminal rendering for coding-agent.
//!
//! Subscribes to `AgentHarnessEvent` broadcast and renders:
//! - `TextDelta` → stdout (real-time streaming)
//! - `Error` → stderr
//! - `PhaseChange { to: Idle }` → break (run complete)

use std::io::Write;
use std::sync::Arc;

use llm_harness_agent::AgentHarnessEvent;
use llm_harness_types::{AgentEvent, HarnessPhase};
use tokio::sync::broadcast;

/// Stream events from a one-shot run to stdout/stderr.
///
/// Spawn this as a tokio task BEFORE calling `harness.prompt()`.
pub async fn render_one_shot(mut rx: broadcast::Receiver<Arc<AgentHarnessEvent>>) {
    loop {
        match rx.recv().await {
            Ok(ev) => match ev.as_ref() {
                AgentHarnessEvent::Agent(AgentEvent::TextDelta { text, .. }) => {
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                }
                AgentHarnessEvent::Agent(AgentEvent::Error(e)) => {
                    eprintln!("\nAgent error: {e}");
                }
                AgentHarnessEvent::PhaseChange {
                    to: HarnessPhase::Idle,
                    ..
                } => break,
                _ => {}
            },
            Err(broadcast::error::RecvError::Closed) => break,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
        }
    }
}
