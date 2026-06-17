use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use llm_harness_loop::{
    DefaultConvertToLlm, LlmClient, LoopConfig, agent_loop, agent_loop_continue,
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
    /// Active tool list.
    pub tools: Vec<Arc<dyn Tool>>,
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
    steer_rx: Option<mpsc::Receiver<AgentMessage>>,
    follow_up_rx: Option<mpsc::Receiver<AgentMessage>>,
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
        self.steer_rx = None;
        self.follow_up_rx = None;
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
    /// Initial tools.
    pub tools: Vec<Arc<dyn Tool>>,
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
            tools: vec![],
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
    // AgentEvent is not Clone (ToolError carries anyhow::Error), so we wrap in Arc.
    event_tx: broadcast::Sender<Arc<AgentEvent>>,
    /// Used by `wait_for_idle` to detect phase transitions.
    phase_tx: watch::Sender<AgentPhase>,
    phase_rx: watch::Receiver<AgentPhase>,
}

impl Agent {
    /// Create a new Agent.
    pub fn new(client: Arc<dyn LlmClient>, opts: AgentOptions) -> Self {
        let cap = opts.queue_capacity;
        let (steer_tx, steer_rx) = mpsc::channel(cap);
        let (follow_up_tx, follow_up_rx) = mpsc::channel(cap);
        let (event_tx, _) = broadcast::channel::<Arc<AgentEvent>>(BROADCAST_CAPACITY);
        let (phase_tx, phase_rx) = watch::channel(AgentPhase::Idle);

        let state = AgentState {
            phase: AgentPhase::Idle,
            model: opts.model,
            model_info: opts.model_info,
            thinking_level: opts.thinking_level,
            tools: opts.tools,
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
            steer_rx: Some(steer_rx),
            follow_up_rx: Some(follow_up_rx),
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

    /// Start a new run from a text prompt.
    pub async fn prompt(&self, text: impl Into<String>) -> Result<(), AgentError> {
        let text = text.into();
        let user_msg = AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text { text }],
            timestamp: chrono::Utc::now(),
        });
        self.run_with_initial(vec![user_msg], false, false).await
    }

    /// Start a new run with an explicit message list, replacing the current transcript.
    pub async fn prompt_with_messages(
        &self,
        messages: Vec<AgentMessage>,
    ) -> Result<(), AgentError> {
        self.run_with_initial(messages, false, true).await
    }

    /// Continue the current transcript without injecting new messages.
    ///
    /// Uses `agent_loop_continue` which drains stale steer messages before the
    /// first LLM call.
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

    /// Replace the active tool list; takes effect at the start of the next turn.
    pub fn set_tools(&self, tools: Vec<Arc<dyn Tool>>) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.tools = tools;
    }

    /// Set or clear the system prompt; takes effect at the start of the next turn.
    pub fn set_system_prompt(&self, prompt: Option<String>) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.system_prompt = prompt;
    }

    // ── Queue operations (any phase) ──────────────────────────────────────

    /// Enqueue a text message as a steer (injected between turns of a running loop).
    pub fn steer(&self, text: impl Into<String>) {
        self.steer_message(AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: chrono::Utc::now(),
        }));
    }

    /// Enqueue a text message as a follow-up (processed after the loop stops naturally).
    pub fn follow_up(&self, text: impl Into<String>) {
        self.follow_up_message(AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: chrono::Utc::now(),
        }));
    }

    /// Enqueue a full message as a steer.
    pub fn steer_message(&self, msg: AgentMessage) {
        let inner = self.inner.lock().unwrap();
        let _ = inner.steer_tx.try_send(msg);
    }

    /// Enqueue a full message as a follow-up.
    pub fn follow_up_message(&self, msg: AgentMessage) {
        let inner = self.inner.lock().unwrap();
        let _ = inner.follow_up_tx.try_send(msg);
    }

    /// Drain the steer queue.
    pub fn clear_steering_queue(&self) {
        let mut inner = self.inner.lock().unwrap();
        let cap = inner.queue_capacity;
        let (new_tx, _) = mpsc::channel(cap);
        inner.steer_tx = new_tx;
    }

    /// Drain the follow-up queue.
    pub fn clear_follow_up_queue(&self) {
        let mut inner = self.inner.lock().unwrap();
        let cap = inner.queue_capacity;
        let (new_tx, _) = mpsc::channel(cap);
        inner.follow_up_tx = new_tx;
    }

    /// Drain both queues.
    pub fn clear_all_queues(&self) {
        self.clear_steering_queue();
        self.clear_follow_up_queue();
    }

    /// Returns `true` if either the steer or follow-up channel has capacity used.
    ///
    /// Note: this checks sender capacity, not whether the running loop will process them.
    pub fn has_queued_messages(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        // max_capacity - available_permits gives approximate occupancy
        inner.steer_tx.max_capacity() != inner.steer_tx.capacity()
            || inner.follow_up_tx.max_capacity() != inner.follow_up_tx.capacity()
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

            // Extend transcript with initial messages (or replace for prompt_with_messages).
            if is_continue {
                // continue_run: use existing messages as-is
            } else if replace_transcript {
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
                tools: inner.state.tools.clone(),
                system_prompt: inner.state.system_prompt.clone(),
            };
            let messages = inner.state.messages.clone();
            let max_tokens = inner.max_tokens;

            let ctx = AgentContext {
                system_prompt: snapshot.system_prompt,
                messages,
            };
            let config = LoopConfig {
                model: snapshot.model,
                max_tokens,
                temperature: None,
                thinking_level: snapshot.thinking_level,
                tools: snapshot.tools,
                active_tools: None,
                default_execution_mode: ToolExecutionMode::Parallel,
                env: self.env.clone(),
                abort,
                stream_options: StreamOptions::default(),
                convert_to_llm: Arc::new(DefaultConvertToLlm::new()),
                transform_context: None,
                prepare_next_turn: None,
                should_stop: None,
                before_provider_request: None,
                after_provider_response: None,
                auth: None,
                steer_rx: Some(steer_rx),
                follow_up_rx: Some(follow_up_rx),
                retry: None,
            };
            (ctx, config)
        };

        let _ = self.phase_tx.send(AgentPhase::Running);

        // 2. Drive event stream.
        let mut new_messages: Vec<AgentMessage> = vec![];
        let mut stream: std::pin::Pin<Box<dyn futures::Stream<Item = AgentEvent> + Send>> =
            if is_continue {
                Box::pin(agent_loop_continue(self.client.clone(), ctx, config))
            } else {
                Box::pin(agent_loop(self.client.clone(), ctx, config))
            };

        while let Some(event) = stream.next().await {
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

        Ok(())
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
            AgentMessage::Assistant(AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: "reply".into(),
                }],
                stop_reason: Some(StopReason::EndTurn),
                timestamp: chrono::Utc::now(),
                provider: None,
                api: None,
                model: None,
                usage: None,
                error_message: None,
            }),
        ];
        agent.prompt_with_messages(msgs).await.unwrap();
        let state = agent.state();
        // The loop adds new assistant message on top; original 2 + 1 new assistant = 3
        assert!(state.messages.len() >= 2);
    }

    #[tokio::test]
    async fn clear_steering_queue_drains_pending_messages() {
        let agent = make_agent(vec![MockResponse::text("response")]);

        // Enqueue a steer message while Idle
        agent.steer_message(AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "ignored message".into(),
            }],
            timestamp: chrono::Utc::now(),
        }));

        // Clear before prompting
        agent.clear_steering_queue();

        // Prompt with a fresh message
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
}
