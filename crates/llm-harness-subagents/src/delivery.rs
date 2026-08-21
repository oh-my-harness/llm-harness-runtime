//! Async sub-agent delivery: AsyncSpawnHandle + formatting helpers.

use crate::spawn_types::SubAgentResult;
use llm_harness_types::AgentError;
use tokio_util::sync::CancellationToken;

/// 不持 join 能力（已被 track task 消费）。
pub struct AsyncSpawnHandle {
    agent_id: String,
    abort: CancellationToken,
}

impl AsyncSpawnHandle {
    pub(crate) fn new(agent_id: String, abort: CancellationToken) -> Self {
        Self { agent_id, abort }
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn abort(&self) {
        self.abort.cancel();
    }
}

/// 格式化单条成功结果（设计 §8.1）。
pub(crate) fn format_success(agent_id: &str, result: &SubAgentResult) -> String {
    format!(
        "[Sub-agent '{id}' completed]\n{answer}\n\nTool calls: {n}\nSession: {sid}",
        id = agent_id,
        answer = result.final_answer,
        n = result.tool_calls_count,
        sid = result.session_id,
    )
}

/// 格式化单条失败结果（设计 §8.1）。
pub(crate) fn format_failure(agent_id: &str, error: &AgentError) -> String {
    format!(
        "[Sub-agent '{id}' failed]\n{error}",
        id = agent_id,
        error = error,
    )
}

/// Convert `CustomMessage{type:"sub_agent_message"}` (injected by
/// [`crate::plugin::SpawnPlugin`]) into a user message so the LLM can
/// see sub-agent results in context.
///
/// Register via `HarnessBuilder::convert_to_llm(Some(Arc::new(
///     DefaultConvertToLlm::new().with_custom_converter(Arc::new(SubAgentMessageConverter))
/// )))` whenever `SpawnPlugin` is used.
pub struct SubAgentMessageConverter;

impl llm_harness_loop::convert::CustomMessageConverter for SubAgentMessageConverter {
    fn convert<'a>(
        &'a self,
        msg: &'a llm_harness_types::CustomMessage,
    ) -> futures::future::BoxFuture<'a, Result<llm_harness_loop::LlmMessage, AgentError>> {
        Box::pin(async move {
            let text = msg
                .data
                .get("messages")
                .and_then(|v| v.as_str())
                .unwrap_or("sub-agent event");
            Ok(llm_harness_loop::LlmMessage::User(vec![
                llm_harness_loop::RequestContent::Text(text.to_string()),
            ]))
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::message_bus::{MessageBus, SubAgentEvent};
    use crate::spawn_types::{SubAgentHandle, SubAgentResult};
    use llm_harness_types::{AgentError, CostAggregate};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn test_bus() -> Arc<MessageBus> {
        MessageBus::new()
    }

    // helper：建一个会立即完成的 SubAgentHandle。
    fn quick_handle(output: &str) -> SubAgentHandle {
        let abort = CancellationToken::new();
        let final_answer = output.to_string();
        let join = tokio::spawn(async move {
            Ok(SubAgentResult {
                final_answer,
                tool_calls_count: 0,
                session_id: "test-session".into(),
                cost: CostAggregate::default(),
            })
        });
        SubAgentHandle::new(join, abort)
    }

    #[tokio::test]
    async fn track_completes_and_sends_event() {
        let bus = test_bus();
        let events = Arc::new(std::sync::Mutex::new(vec![]));
        let events_clone = events.clone();
        bus.set_event_sink(Arc::new(move |event| {
            events_clone.lock().unwrap().push(event);
        }));
        let handle = quick_handle("done");
        let async_handle = bus.track(handle, "agent-1".into());
        assert_eq!(async_handle, "agent-1");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let evs = events.lock().unwrap();
        assert_eq!(evs.len(), 1, "one event should be pushed");
        match &evs[0] {
            SubAgentEvent::Done { agent_id, result } => {
                assert_eq!(agent_id, "agent-1");
                let r = result.as_ref().unwrap();
                assert_eq!(r.final_answer, "done");
            }
            _ => panic!("expected Done event"),
        }
    }

    #[tokio::test]
    async fn in_flight_count_increments_then_decrements() {
        let bus = test_bus();
        let abort = CancellationToken::new();
        let join = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(SubAgentResult {
                final_answer: "late".into(),
                tool_calls_count: 0,
                session_id: "s".into(),
                cost: CostAggregate::default(),
            })
        });
        let handle = SubAgentHandle::new(join, abort);
        let _id = bus.track(handle, "late-agent".into());
        assert_eq!(bus.in_flight_count(), 1);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(bus.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn async_spawn_handle_abort_cancels_sub_agent() {
        let bus = test_bus();
        let abort = CancellationToken::new();
        let abort_clone = abort.clone();
        let join = tokio::spawn(async move {
            abort_clone.cancelled().await;
            Err(AgentError::Aborted)
        });
        let handle = SubAgentHandle::new(join, abort);
        let id = bus.track(handle, "cancellable".into());
        // track returns agent_id, not handle — use register_subagent_abort + abort_agent
        bus.abort_agent(&id);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(bus.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn cancel_all_cancels_tracked_tokens() {
        let bus = test_bus();
        let t1 = CancellationToken::new();
        let t2 = CancellationToken::new();
        bus.register_subagent_abort("a", t1.clone());
        bus.register_subagent_abort("b", t2.clone());
        bus.cancel_all();
        assert!(t1.is_cancelled(), "token a should be cancelled");
        assert!(t2.is_cancelled(), "token b should be cancelled");
    }
}
