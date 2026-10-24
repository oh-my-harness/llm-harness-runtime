use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use llm_harness_loop::{
    DefaultConvertToLlm, FinalAnswerMode, LlmClient, LoopConfig, agent_loop, agent_loop_continue,
};
use llm_harness_types::*;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

const BROADCAST_CAPACITY: usize = 256;
const DEFAULT_QUEUE_CAPACITY: usize = 32;
const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Metadata about a model, used for compaction token estimation.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// Total context window size in tokens.
    pub context_window: u32,
    /// Maximum output tokens for this model.
    pub max_tokens: u32,
}

/// Agent running phase.
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum AgentPhase {
    /// No active run; ready to accept new prompts.
    Idle,
    /// An agent loop is running.
    Running,
}

/// Observable snapshot of agent state; returned by [`Agent::state`].
#[derive(Clone)]
pub struct AgentState {
    /// Current running phase.
    pub phase: AgentPhase,
    /// Model ID sent to the LLM API.
    pub model: String,
    /// Model metadata for token estimation; may be absent.
    pub model_info: Option<ModelInfo>,
    /// Reasoning depth level.
    pub thinking_level: ThinkingLevel,
    /// 采样温度；`None` 表示走 provider 默认。
    pub temperature: Option<f32>,
    /// Active tool list.
    pub tools: Vec<Arc<dyn Tool>>,
    /// Final-answer classification mode.
    pub final_answer_mode: FinalAnswerMode,
    /// Full message transcript (persisted across turns).
    pub messages: Vec<AgentMessage>,
    /// System prompt; `None` means no system prompt is set.
    pub system_prompt: Option<String>,
    /// Real-time snapshot of the assistant message being streamed; `None` when idle.
    pub streaming_message: Option<AssistantMessage>,
    /// IDs of tools currently executing in parallel.
    pub pending_tool_calls: HashSet<String>,
    /// Error text from the most recent failed LLM call; cleared on next prompt start.
    pub error_message: Option<String>,
}

/// Private inner state, also owns the steer/follow-up channels.
struct AgentInner {
    state: AgentState,
    steer_tx: mpsc::Sender<AgentMessage>,
    follow_up_tx: mpsc::Sender<AgentMessage>,
    /// Steers accepted by a completed run but not consumed by its provider turns.
    pending_steers: Vec<AgentMessage>,
    max_tokens: u32,
    queue_capacity: usize,
    /// CancellationToken for the currently running loop; `None` when idle.
    current_abort: Option<CancellationToken>,
}

impl AgentInner {
    /// Replace the steer/follow-up channels with fresh ones, returning the new receivers.
    fn reset_channels(&mut self) -> (mpsc::Receiver<AgentMessage>, mpsc::Receiver<AgentMessage>) {
        let cap = self.queue_capacity;
        let (steer_tx, steer_rx) = mpsc::channel(cap);
        let (follow_up_tx, follow_up_rx) = mpsc::channel(cap);
        self.steer_tx = steer_tx;
        self.follow_up_tx = follow_up_tx;
        (steer_rx, follow_up_rx)
    }
}

/// Construction options for [`Agent`].
pub struct AgentOptions {
    /// Initial model ID. Required.
    pub model: String,
    /// Execution environment for tool calls.
    pub env: Arc<dyn ExecutionEnv>,
    /// Maximum output tokens per LLM call.
    pub max_tokens: u32,
    /// mpsc channel buffer capacity for steer and follow-up queues.
    pub queue_capacity: usize,
    /// Initial model metadata.
    pub model_info: Option<ModelInfo>,
    /// Initial thinking level.
    pub thinking_level: ThinkingLevel,
    /// Initial sampling temperature; `None` uses provider default.
    pub temperature: Option<f32>,
    /// Initial tools.
    pub tools: Vec<Arc<dyn Tool>>,
    /// Final-answer classification mode.
    pub final_answer_mode: FinalAnswerMode,
    /// Initial system prompt.
    pub system_prompt: Option<String>,
}

impl AgentOptions {
    /// Create options for an agent that does not need tool environment access.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            env: Arc::new(UnsupportedEnv::new()),
            max_tokens: DEFAULT_MAX_TOKENS,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            model_info: None,
            thinking_level: ThinkingLevel::Off,
            temperature: None,
            tools: vec![],
            final_answer_mode: FinalAnswerMode::default(),
            system_prompt: None,
        }
    }

    /// Create options with an execution environment for tools.
    pub fn new_with_env(model: impl Into<String>, env: Arc<dyn ExecutionEnv>) -> Self {
        Self::new(model).with_env(env)
    }

    /// Set the execution environment used by tools.
    pub fn with_env(mut self, env: Arc<dyn ExecutionEnv>) -> Self {
        self.env = env;
        self
    }

    /// Set the final-answer classification mode.
    pub fn with_final_answer_mode(mut self, mode: FinalAnswerMode) -> Self {
        self.final_answer_mode = mode;
        self
    }
}

/// Stateful wrapper around `agent_loop`.
///
/// Manages the message transcript, steer/follow-up queues, event broadcast,
/// and per-run abort tokens. For session persistence and compaction use
/// `AgentHarness` instead.
pub struct Agent {
    client: Arc<dyn LlmClient>,
    env: Arc<dyn ExecutionEnv>,
    inner: Arc<Mutex<AgentInner>>,
    // Events are shared across subscribers without copying their payloads.
    event_tx: broadcast::Sender<Arc<AgentEvent>>,
    /// Used by `wait_for_idle` to detect phase transitions.
    phase_tx: watch::Sender<AgentPhase>,
    phase_rx: watch::Receiver<AgentPhase>,
}

impl Agent {
    /// Create a new Agent.
    pub fn new(client: Arc<dyn LlmClient>, opts: AgentOptions) -> Self {
        let cap = opts.queue_capacity;
        let (steer_tx, _steer_rx) = mpsc::channel(cap);
        let (follow_up_tx, _follow_up_rx) = mpsc::channel(cap);
        let (event_tx, _) = broadcast::channel::<Arc<AgentEvent>>(BROADCAST_CAPACITY);
        let (phase_tx, phase_rx) = watch::channel(AgentPhase::Idle);

        let state = AgentState {
            phase: AgentPhase::Idle,
            model: opts.model,
            model_info: opts.model_info,
            thinking_level: opts.thinking_level,
            temperature: opts.temperature,
            tools: opts.tools,
            final_answer_mode: opts.final_answer_mode,
            messages: vec![],
            system_prompt: opts.system_prompt,
            streaming_message: None,
            pending_tool_calls: HashSet::new(),
            error_message: None,
        };

        let inner = AgentInner {
            state,
            steer_tx,
            follow_up_tx,
            pending_steers: vec![],
            max_tokens: opts.max_tokens,
            queue_capacity: cap,
            current_abort: None,
        };

        Self {
            client,
            env: opts.env,
            inner: Arc::new(Mutex::new(inner)),
            event_tx,
            phase_tx,
            phase_rx,
        }
    }

    // ── Structural operations (Idle only) ─────────────────────────────────

    /// Start a new run from a text prompt, after any unconsumed steers.
    pub async fn prompt(&self, text: impl Into<String>) -> Result<(), AgentError> {
        let text = text.into();
        let user_msg = AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text { text }],
            timestamp: chrono::Utc::now(),
        });
        self.run_with_initial(vec![user_msg], false, false).await
    }

    /// Start a new run with an explicit message list, replacing the current transcript
    /// and discarding unconsumed steers from previous runs.
    pub async fn prompt_with_messages(
        &self,
        messages: Vec<AgentMessage>,
    ) -> Result<(), AgentError> {
        self.run_with_initial(messages, false, true).await
    }

    /// Continue the current transcript, first appending any unconsumed steers.
    ///
    /// Uses `agent_loop_continue` which drains stale steer messages before the
    /// first LLM call. Steers recovered from an earlier run are appended to the
    /// transcript before that drain, so they are preserved.
    pub async fn continue_run(&self) -> Result<(), AgentError> {
        self.run_with_initial(vec![], true, false).await
    }

    /// Clear messages and runtime state, preserving model/tools/system_prompt.
    pub fn reset(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.messages.clear();
        inner.state.streaming_message = None;
        inner.state.pending_tool_calls.clear();
        inner.state.error_message = None;
        inner.pending_steers.clear();
    }

    // ── Runtime configuration (any phase) ─────────────────────────────────

    /// Change model; takes effect at the start of the next turn.
    pub fn set_model(&self, model: String, info: Option<ModelInfo>) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.model = model;
        inner.state.model_info = info;
    }

    /// Change thinking level; takes effect at the start of the next turn.
    pub fn set_thinking_level(&self, level: ThinkingLevel) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.thinking_level = level;
    }

    /// Set the sampling temperature; takes effect at the start of the next turn.
    ///
    /// Note: this change is NOT persisted to the session log. On session restore
    /// the temperature reverts to the `AgentOptions.temperature` initial value.
    pub fn set_temperature(&self, temperature: Option<f32>) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.temperature = temperature;
    }

    /// Replace the active tool list; takes effect at the start of the next turn.
    pub fn set_tools(&self, tools: Vec<Arc<dyn Tool>>) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.tools = tools;
    }

    /// Change final-answer classification mode; takes effect at the start of the next turn.
    pub fn set_final_answer_mode(&self, mode: FinalAnswerMode) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.final_answer_mode = mode;
    }

    /// Set or clear the system prompt; takes effect at the start of the next turn.
    pub fn set_system_prompt(&self, prompt: Option<String>) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.system_prompt = prompt;
    }

    // ── Queue operations (any phase) ──────────────────────────────────────

    /// Enqueue a text message as a steer (injected between turns of a running loop).
    pub fn steer(&self, text: impl Into<String>) {
        let _ = self.steer_message(AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: chrono::Utc::now(),
        }));
    }

    /// Enqueue a text message as a follow-up (processed after the loop stops naturally).
    ///
    /// ⚠️ **仅在 run 进行中有效。** Idle 阶段调用会静默丢失消息（错误被忽略）。
    /// 需要检测丢失时用 [`follow_up_message`](Agent::follow_up_message)。
    pub fn follow_up(&self, text: impl Into<String>) {
        let _ = self.follow_up_message(AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: chrono::Utc::now(),
        }));
    }

    /// Enqueue a full message as a steer.
    ///
    /// Returns `Err` if the steer channel is full or closed.
    /// An accepted message not consumed before the run ends is retained for
    /// the next `prompt` or `continue_run`, unless explicitly cleared or replaced.
    pub fn steer_message(&self, msg: AgentMessage) -> Result<(), ChannelError> {
        let inner = self.inner.lock().unwrap();
        inner.steer_tx.try_send(msg).map_err(ChannelError::from)
    }

    /// Enqueue a full message as a follow-up (processed after the loop stops naturally).
    ///
    /// 返回 `Err(ChannelError::Closed)` 表示消息未投递——通常因为 agent 处于
    /// Idle 阶段（上一个 run 已结束，follow_up_rx 被 `reset_channels()` drop）。
    /// 返回 `Err(ChannelError::Full)` 表示队列已满。
    ///
    /// **设计意图**：follow_up 用于 run 进行中 should_stop 触发后注入消息。
    /// Idle 阶段的多轮对话应使用 `prompt()`。
    pub fn follow_up_message(&self, msg: AgentMessage) -> Result<(), ChannelError> {
        let inner = self.inner.lock().unwrap();
        inner.follow_up_tx.try_send(msg).map_err(ChannelError::from)
    }

    /// Drain the steer queue, including recovered unconsumed steers. Only callable when Idle.
    pub fn clear_steering_queue(&self) -> Result<(), AgentError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.state.phase != AgentPhase::Idle {
            return Err(AgentError::NotIdle);
        }
        let cap = inner.queue_capacity;
        let (new_tx, _) = mpsc::channel(cap);
        inner.steer_tx = new_tx;
        inner.pending_steers.clear();
        Ok(())
    }

    /// Drain the follow-up queue. Only callable when Idle.
    pub fn clear_follow_up_queue(&self) -> Result<(), AgentError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.state.phase != AgentPhase::Idle {
            return Err(AgentError::NotIdle);
        }
        let cap = inner.queue_capacity;
        let (new_tx, _) = mpsc::channel(cap);
        inner.follow_up_tx = new_tx;
        Ok(())
    }

    /// Drain both queues. Only callable when Idle.
    pub fn clear_all_queues(&self) -> Result<(), AgentError> {
        self.clear_steering_queue()?;
        self.clear_follow_up_queue()
    }

    /// Returns `true` if recovered steers are pending or either channel has capacity used.
    ///
    /// Channel occupancy uses sender capacity; it does not promise the current
    /// loop will consume the queued messages.
    pub fn has_queued_messages(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        // max_capacity - available_permits gives approximate occupancy
        inner.steer_tx.max_capacity() != inner.steer_tx.capacity()
            || inner.follow_up_tx.max_capacity() != inner.follow_up_tx.capacity()
            || !inner.pending_steers.is_empty()
    }

    /// Cancel the currently running loop, if any.
    pub fn abort(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(token) = inner.current_abort.take() {
            token.cancel();
        }
    }

    // ── Observation ───────────────────────────────────────────────────────

    /// Return a snapshot clone of the current agent state.
    pub fn state(&self) -> AgentState {
        self.inner.lock().unwrap().state.clone()
    }

    /// Subscribe to the agent event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<AgentEvent>> {
        self.event_tx.subscribe()
    }

    /// Async-wait until the agent phase returns to `Idle`.
    pub async fn wait_for_idle(&self) {
        let mut rx = self.phase_rx.clone();
        loop {
            if *rx.borrow() == AgentPhase::Idle {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    // ── Private helpers ───────────────────────────────────────────────────

    async fn run_with_initial(
        &self,
        initial: Vec<AgentMessage>,
        is_continue: bool,
        replace_transcript: bool,
    ) -> Result<(), AgentError> {
        // 1. Transition to Running; snapshot config; create channels + abort token.
        let (ctx, config) = {
            let mut inner = self.inner.lock().unwrap();
            if inner.state.phase != AgentPhase::Idle {
                return Err(AgentError::NotIdle);
            }
            inner.state.phase = AgentPhase::Running;
            inner.state.error_message = None;
            inner.state.streaming_message = None;
            inner.state.pending_tool_calls.clear();

            // Promote recovered steers into the transcript exactly once. They
            // must not enter the new steer channel: agent_loop_continue drains
            // that channel before its first provider request. If this run fails,
            // the messages remain in the transcript rather than being requeued
            // and duplicated on the next prompt/continue.
            let initial = if replace_transcript {
                inner.pending_steers.clear();
                initial
            } else {
                std::mem::take(&mut inner.pending_steers)
                    .into_iter()
                    .chain(initial)
                    .collect::<Vec<_>>()
            };
            if replace_transcript {
                inner.state.messages = initial.clone();
            } else if !initial.is_empty() {
                inner.state.messages.extend(initial.iter().cloned());
            }

            let (steer_rx, follow_up_rx) = inner.reset_channels();

            let abort = CancellationToken::new();
            inner.current_abort = Some(abort.clone());

            let snapshot = TurnSnapshot {
                model: inner.state.model.clone(),
                thinking_level: inner.state.thinking_level,
                temperature: inner.state.temperature,
                tools: inner.state.tools.clone(),
                system_prompt: inner.state.system_prompt.clone(),
            };
            let final_answer_mode = inner.state.final_answer_mode.clone();
            let messages = inner.state.messages.clone();
            let max_tokens = inner.max_tokens;
            let run = Arc::new(RunContext::new(RunRequest::new(initial.clone())));

            let ctx = AgentContext {
                system_prompt: snapshot.system_prompt,
                messages,
            };
            let config = LoopConfig {
                run,
                model: snapshot.model,
                max_tokens,
                temperature: snapshot.temperature,
                thinking_level: snapshot.thinking_level,
                tools: snapshot.tools,
                active_tools: None,
                default_execution_mode: ToolExecutionMode::Parallel,
                final_answer_mode,
                env: self.env.clone(),
                abort,
                stream_options: StreamOptions::default(),
                convert_to_llm: Arc::new(DefaultConvertToLlm::new()),
                transform_context: None,
                prepare_next_turn: None,
                should_stop: None,
                before_provider_request: None,
                after_provider_response: None,
                final_answer_validator: None,
                provider_error: None,
                max_provider_error_retries: None,
                steer_rx: Some(steer_rx),
                follow_up_rx: Some(follow_up_rx),
                retry: None,
                response_format: None,
            };
            (ctx, config)
        };

        let _ = self.phase_tx.send(AgentPhase::Running);

        // 2. Drive event stream.
        let mut new_messages: Vec<AgentMessage> = vec![];
        let mut loop_error: Option<AgentError> = None;
        let mut stream: std::pin::Pin<Box<dyn futures::Stream<Item = AgentEvent> + Send>> =
            if is_continue {
                Box::pin(agent_loop_continue(self.client.clone(), ctx, config))
            } else {
                Box::pin(agent_loop(self.client.clone(), ctx, config))
            };

        while let Some(event) = stream.next().await {
            if let AgentEvent::Error(ref e) = event {
                loop_error = Some(e.clone());
            }
            self.apply_event(&event, &mut new_messages);
            let _ = self.event_tx.send(Arc::new(event));
        }

        // 3. Persist new messages and reset phase.
        {
            let mut inner = self.inner.lock().unwrap();
            inner.state.messages.extend(new_messages);
            inner.state.streaming_message = None;
            inner.state.pending_tool_calls.clear();
            inner.state.phase = AgentPhase::Idle;
            inner.current_abort = None;
        }
        let _ = self.phase_tx.send(AgentPhase::Idle);

        if let Some(e) = loop_error {
            Err(e)
        } else {
            Ok(())
        }
    }

    /// Update observable state fields from an event (called while driving the stream).
    fn apply_event(&self, event: &AgentEvent, new_messages: &mut Vec<AgentMessage>) {
        let mut inner = self.inner.lock().unwrap();
        match event {
            AgentEvent::MessageUpdate { partial, .. } => {
                inner.state.streaming_message = Some(partial.clone());
            }
            AgentEvent::MessageEnd { .. } => {
                inner.state.streaming_message = None;
            }
            AgentEvent::ToolExecutionStart { tool_use_id, .. } => {
                inner.state.pending_tool_calls.insert(tool_use_id.clone());
            }
            AgentEvent::ToolExecutionEnd { tool_use_id, .. } => {
                inner.state.pending_tool_calls.remove(tool_use_id);
            }
            AgentEvent::Error(e) => {
                inner.state.error_message = Some(e.to_string());
            }
            AgentEvent::AgentEnd { new_messages: msgs } => {
                new_messages.extend(msgs.iter().cloned());
            }
            AgentEvent::SteerOrphaned { messages } => {
                inner.pending_steers.extend(messages.iter().cloned());
            }
            _ => {}
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[cfg(feature = "test-utils")]
mod tests {
    use super::*;
    use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
    use std::sync::Arc;

    fn make_agent(responses: Vec<MockResponse>) -> Agent {
        let client = Arc::new(MockLlmClient::new(responses));
        Agent::new(
            client as Arc<dyn LlmClient>,
            AgentOptions::new("test-model"),
        )
    }

    fn user_message(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: chrono::Utc::now(),
        })
    }

    /// Pause after the first text delta, before the first response can stop.
    /// This keeps the public steer sender alive at a deterministic point.
    fn gated_agent(
        later_responses: Vec<MockResponse>,
    ) -> (
        Agent,
        Arc<MockLlmClient>,
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Notify>,
    ) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut first = MockResponse::text("first answer");
        let mut events = std::mem::take(&mut first.events).into_iter();
        let prefix: Vec<_> = events.by_ref().take(2).collect();
        let paused_event = events.next().unwrap();
        let entered_stream = entered.clone();
        let release_stream = release.clone();
        first.stream_impl = Some(Box::pin(
            futures::stream::iter(prefix)
                .chain(futures::stream::once(async move {
                    entered_stream.notify_one();
                    release_stream.notified().await;
                    paused_event
                }))
                .chain(futures::stream::iter(events)),
        ));
        let responses = std::iter::once(first).chain(later_responses).collect();
        let client = Arc::new(MockLlmClient::new(responses));
        let agent = Agent::new(
            client.clone() as Arc<dyn LlmClient>,
            AgentOptions::new("test-model"),
        );
        (agent, client, entered, release)
    }

    async fn finish_first_run_with_orphaned_steer(
        agent: &Agent,
        entered: &tokio::sync::Notify,
        release: &tokio::sync::Notify,
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (result, ()) = tokio::join!(agent.prompt("first prompt"), async {
                entered.notified().await;
                assert!(agent.state().streaming_message.is_some());
                agent
                    .steer_message(user_message("late steer"))
                    .expect("streaming run must accept the steer");
                release.notify_one();
            });
            result.unwrap();
        })
        .await
        .expect("gated first run must finish");
        assert!(
            agent.has_queued_messages(),
            "accepted late steer must be recoverable"
        );
    }

    fn request_user_texts(client: &MockLlmClient, index: usize) -> Vec<String> {
        use llm_adapter::types::{Message, RequestContent};
        client.captured_requests.lock()[index]
            .messages()
            .iter()
            .filter_map(|message| match message {
                Message::User(content) => Some(content),
                _ => None,
            })
            .flatten()
            .filter_map(|content| match content {
                RequestContent::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn orphaned_steer_reaches_next_prompt_request_once_before_new_prompt() {
        let (agent, client, entered, release) = gated_agent(vec![
            MockResponse::text("second answer"),
            MockResponse::text("third answer"),
        ]);
        finish_first_run_with_orphaned_steer(&agent, &entered, &release).await;
        assert_eq!(request_user_texts(&client, 0), ["first prompt"]);

        agent.prompt("second prompt").await.unwrap();
        assert_eq!(
            request_user_texts(&client, 1),
            ["first prompt", "late steer", "second prompt"]
        );
        assert!(!agent.has_queued_messages());
        agent.prompt("third prompt").await.unwrap();
        assert_eq!(
            request_user_texts(&client, 2),
            [
                "first prompt",
                "late steer",
                "second prompt",
                "third prompt"
            ]
        );
    }

    #[tokio::test]
    async fn orphaned_steer_reaches_continue_request_without_stale_channel_loss() {
        let (agent, client, entered, release) = gated_agent(vec![MockResponse::text("continued")]);
        finish_first_run_with_orphaned_steer(&agent, &entered, &release).await;

        agent.continue_run().await.unwrap();
        assert_eq!(
            request_user_texts(&client, 1),
            ["first prompt", "late steer"]
        );
        assert!(!agent.has_queued_messages());
    }

    #[tokio::test]
    async fn orphaned_steer_is_not_duplicated_after_failed_next_run() {
        let (agent, client, entered, release) = gated_agent(vec![
            MockResponse::rate_limit_error(),
            MockResponse::text("recovered"),
        ]);
        finish_first_run_with_orphaned_steer(&agent, &entered, &release).await;

        assert!(agent.prompt("second prompt").await.is_err());
        assert_eq!(
            request_user_texts(&client, 1),
            ["first prompt", "late steer", "second prompt"]
        );
        agent.continue_run().await.unwrap();
        assert_eq!(
            request_user_texts(&client, 2),
            ["first prompt", "late steer", "second prompt"]
        );
        assert!(!agent.has_queued_messages());
    }

    #[tokio::test]
    async fn explicit_reset_replacement_and_queue_clear_discard_orphaned_steers() {
        for action in ["reset", "replace", "clear"] {
            let (agent, client, entered, release) = gated_agent(vec![MockResponse::text("next")]);
            finish_first_run_with_orphaned_steer(&agent, &entered, &release).await;
            match action {
                "reset" => {
                    agent.reset();
                    assert!(!agent.has_queued_messages());
                    agent.prompt("new prompt").await.unwrap();
                }
                "replace" => {
                    agent
                        .prompt_with_messages(vec![user_message("new prompt")])
                        .await
                        .unwrap();
                }
                "clear" => {
                    agent.clear_steering_queue().unwrap();
                    assert!(!agent.has_queued_messages());
                    agent.prompt("new prompt").await.unwrap();
                }
                _ => unreachable!(),
            }
            let texts = request_user_texts(&client, 1);
            assert!(
                !texts.iter().any(|text| text == "late steer"),
                "{action}: {texts:?}"
            );
            assert_eq!(texts.last().map(String::as_str), Some("new prompt"));
            if action != "clear" {
                assert_eq!(texts, ["new prompt"]);
            }
        }
    }

    #[tokio::test]
    async fn prompt_appends_user_message_and_returns_idle() {
        let agent = make_agent(vec![MockResponse::text("Hello!")]);
        agent.prompt("hi").await.unwrap();
        let state = agent.state();
        assert_eq!(state.phase, AgentPhase::Idle);
        // transcript: [user, assistant]
        assert_eq!(state.messages.len(), 2);
        assert!(matches!(state.messages[0], AgentMessage::User(_)));
        assert!(matches!(state.messages[1], AgentMessage::Assistant(_)));
    }

    #[tokio::test]
    async fn second_prompt_while_running_returns_not_idle() {
        // Simulate Running phase directly
        let agent = make_agent(vec![MockResponse::text("ok")]);
        {
            let mut inner = agent.inner.lock().unwrap();
            inner.state.phase = AgentPhase::Running;
        }
        let result = agent.prompt("hi").await;
        assert!(matches!(result, Err(AgentError::NotIdle)));
        // Restore idle for cleanup
        agent.inner.lock().unwrap().state.phase = AgentPhase::Idle;
    }

    #[tokio::test]
    async fn reset_clears_messages_preserves_model() {
        let agent = make_agent(vec![MockResponse::text("Hello!")]);
        agent.prompt("hi").await.unwrap();
        assert!(!agent.state().messages.is_empty());
        agent.reset();
        let state = agent.state();
        assert!(state.messages.is_empty());
        assert_eq!(state.model, "test-model");
    }

    #[tokio::test]
    async fn set_model_reflected_in_state() {
        let agent = make_agent(vec![]);
        agent.set_model("claude-opus-4-7".into(), None);
        assert_eq!(agent.state().model, "claude-opus-4-7");
    }

    #[tokio::test]
    async fn subscribe_receives_agent_events() {
        let agent = make_agent(vec![MockResponse::text("hi")]);
        let mut rx = agent.subscribe();
        agent.prompt("hello").await.unwrap();
        // At least one event should have been sent.
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn abort_cancels_running_loop() {
        // This test verifies abort() sets current_abort to None after cancellation.
        let agent = make_agent(vec![]);
        // Simulate a running abort token
        let token = CancellationToken::new();
        agent.inner.lock().unwrap().current_abort = Some(token.clone());
        agent.abort();
        assert!(token.is_cancelled());
        assert!(agent.inner.lock().unwrap().current_abort.is_none());
    }

    #[tokio::test]
    async fn wait_for_idle_returns_immediately_when_idle() {
        let agent = make_agent(vec![]);
        // Already idle — should complete without blocking.
        tokio::time::timeout(std::time::Duration::from_millis(100), agent.wait_for_idle())
            .await
            .expect("wait_for_idle timed out when agent was already Idle");
    }

    #[tokio::test]
    async fn prompt_with_messages_replaces_transcript() {
        let agent = make_agent(vec![MockResponse::text("ok")]);
        let msgs = vec![
            AgentMessage::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: "first".into(),
                }],
                timestamp: chrono::Utc::now(),
            }),
            AgentMessage::Assistant(llm_harness_loop::test_utils::test_assistant_message(vec![
                ContentBlock::Text {
                    text: "reply".into(),
                },
            ])),
        ];
        agent.prompt_with_messages(msgs).await.unwrap();
        let state = agent.state();
        // The loop adds new assistant message on top; original 2 + 1 new assistant = 3
        assert!(state.messages.len() >= 2);
    }

    #[tokio::test]
    async fn clear_steering_queue_drains_pending_messages() {
        let agent = make_agent(vec![MockResponse::text("response")]);

        // Agent 在 Idle 时没有 active loop receiver；为这个队列单元测试
        // 安装一个存活的 receiver，避免把 Closed channel 当成待清理队列。
        let cap = agent.inner.lock().unwrap().queue_capacity;
        let (steer_tx, _steer_rx) = mpsc::channel(cap);
        agent.inner.lock().unwrap().steer_tx = steer_tx;

        // Enqueue a steer message while Idle, then drain it.
        agent
            .steer_message(AgentMessage::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: "ignored message".into(),
                }],
                timestamp: chrono::Utc::now(),
            }))
            .unwrap();
        assert!(
            agent.has_queued_messages(),
            "steer message must be enqueued"
        );
        agent.clear_steering_queue().unwrap();
        assert!(
            !agent.has_queued_messages(),
            "steer queue must be empty after clear"
        );

        // Prompt with a fresh message; the cleared steer queue must not inject
        // any stale messages into the transcript.
        agent.prompt("actual prompt").await.unwrap();

        let state = agent.state();
        // Messages: [user("actual prompt"), assistant("response")] = 2
        assert_eq!(
            state.messages.len(),
            2,
            "steer message should have been cleared"
        );
    }
    #[tokio::test]
    async fn clear_queues_rejected_when_not_idle() {
        let agent = make_agent(vec![]);
        // 强制进入 Running 阶段
        agent.inner.lock().unwrap().state.phase = AgentPhase::Running;
        assert!(matches!(
            agent.clear_steering_queue(),
            Err(AgentError::NotIdle)
        ));
        assert!(matches!(
            agent.clear_follow_up_queue(),
            Err(AgentError::NotIdle)
        ));
        assert!(matches!(agent.clear_all_queues(), Err(AgentError::NotIdle)));
        // 恢复 Idle 供后续清理
        agent.inner.lock().unwrap().state.phase = AgentPhase::Idle;
    }

    #[tokio::test]
    async fn prompt_with_messages_single_user_replaces_transcript() {
        let agent = make_agent(vec![
            MockResponse::text("first response"),
            MockResponse::text("second response"),
        ]);

        // Build up an existing transcript
        agent.prompt("first prompt").await.unwrap();
        let before_len = agent.state().messages.len();

        // Single-user message should REPLACE, not append
        let single_user = vec![AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "clean start".into(),
            }],
            timestamp: chrono::Utc::now(),
        })];
        agent.prompt_with_messages(single_user).await.unwrap();

        let state = agent.state();
        // Correct: transcript replaced → 1 user + 1 assistant = 2 total
        // Bug: transcript appended → before_len + 2 messages
        assert_eq!(
            state.messages.len(),
            2,
            "single-user prompt_with_messages must replace transcript (was {before_len} before)"
        );
    }

    #[tokio::test]
    async fn prompt_returns_err_when_loop_emits_error_event() {
        // A rate-limit error with no retry config causes the loop to emit
        // AgentEvent::Error(Provider(...)) and terminate. prompt() must
        // propagate that as Err rather than swallowing it as Ok(()).
        let agent = make_agent(vec![MockResponse::rate_limit_error()]);
        let result = agent.prompt("hi").await;
        assert!(
            matches!(result, Err(AgentError::ProviderTyped { .. })),
            "prompt must return Err when the loop emits an error event, got {result:?}"
        );
        // Phase must still reset to Idle even on error.
        assert_eq!(agent.state().phase, AgentPhase::Idle);
    }

    /// Issue #29: when a non-OpenAI model returns tool call arguments with
    /// invalid JSON, the agent must NOT crash. The parse error is embedded
    /// in the tool args as a degradation JSON, the tool executes, and its
    /// result feeds back to the model for self-correction.
    #[tokio::test]
    async fn invalid_tool_args_json_recovers_instead_of_crashing() {
        use std::sync::atomic::Ordering;

        struct EchoTool;
        impl Tool for EchoTool {
            fn name(&self) -> &str {
                "echo"
            }
            fn description(&self) -> &str {
                "echoes input"
            }
            fn parameters_schema(&self) -> &serde_json::Value {
                use serde_json::json;
                static SCHEMA: std::sync::LazyLock<serde_json::Value> =
                    std::sync::LazyLock::new(|| {
                        json!({
                            "type": "object",
                            "properties": { "msg": { "type": "string" } },
                            "required": ["msg"]
                        })
                    });
                &SCHEMA
            }
            fn execute<'a>(
                &'a self,
                _args: serde_json::Value,
                _ctx: &'a ToolContext,
            ) -> futures::future::BoxFuture<'a, Result<ToolResult, ToolFailure>> {
                Box::pin(async {
                    Ok(ToolResult::full(
                        vec![DataBlock::text("ok")],
                        serde_json::Value::Null,
                        false,
                    ))
                })
            }
        }

        // First response: tool_use with broken JSON (missing closing brace).
        // Second response: clean text — the model "self-corrects" after
        // receiving the tool result containing the parse error.
        let client = Arc::new(MockLlmClient::new(vec![
            MockResponse::tool_use("bad-1", "echo", r#"{"msg":"hello""#),
            MockResponse::text("recovered"),
        ]));
        let opts = AgentOptions {
            tools: vec![Arc::new(EchoTool) as Arc<dyn Tool>],
            ..AgentOptions::new("test-model")
        };
        let agent = Agent::new(client.clone() as Arc<dyn LlmClient>, opts);

        let result = agent.prompt("echo hello").await;

        // After the fix: prompt() returns Ok — no fatal error.
        assert!(
            result.is_ok(),
            "expected agent to recover from invalid tool args, got {result:?}"
        );
        assert_eq!(agent.state().phase, AgentPhase::Idle);
        // No error_message should be set.
        assert!(
            agent.state().error_message.is_none(),
            "expected no error_message, got {:?}",
            agent.state().error_message
        );
        // Two LLM calls: broken tool call + recovery text response.
        assert_eq!(
            client.call_count.load(Ordering::SeqCst),
            2,
            "expected 2 LLM calls (broken tool call + recovery), got {}",
            client.call_count.load(Ordering::SeqCst)
        );
    }
}
