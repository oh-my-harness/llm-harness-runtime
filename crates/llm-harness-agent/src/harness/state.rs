use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, RwLock};

use llm_harness_loop::{FinalAnswerMode, LlmClient, RetryConfig};
use llm_harness_types::*;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::ModelInfo;
use crate::compaction::{CompactionPromptSpec, CompactionSummaryConfig, CompactionThresholdConfig};
use crate::session::types::*;
use crate::skills::{PromptTemplate, Skill};

// ── HarnessState ──────────────────────────────────────────────────────────────

/// Observable state of an [`AgentHarness`].
#[derive(Clone)]
pub struct HarnessState {
    /// Current lifecycle phase.
    pub phase: HarnessPhase,
    /// Model ID used for LLM calls.
    pub model: String,
    /// Model metadata (context window, max output tokens).
    pub model_info: Option<ModelInfo>,
    /// Reasoning depth level.
    pub thinking_level: ThinkingLevel,
    /// 采样温度；`None` 表示走 provider 默认。
    pub temperature: Option<f32>,
    /// Optional `response_format` for structured output (e.g. `JsonObject`).
    pub response_format: Option<llm_harness_loop::ResponseFormat>,
    /// Full registered tool list.
    pub tools: Vec<Arc<dyn Tool>>,
    /// Active tool subset; `None` means all tools are active.
    pub active_tools: Option<HashSet<String>>,
    /// Final-answer classification mode.
    pub final_answer_mode: FinalAnswerMode,
    /// Default tool execution mode for batches (configurable via options).
    pub default_execution_mode: ToolExecutionMode,
    /// System prompt.
    pub system_prompt: Option<String>,
    /// Streaming assistant message snapshot; `None` when not streaming.
    pub(crate) streaming_message: Option<AssistantMessage>,
    /// IDs of tools currently executing.
    pub(crate) pending_tool_calls: HashSet<String>,
    /// Session entries buffered during a running turn; flushed at turn end.
    pub(crate) pending_session_writes: VecDeque<SessionEntryPayload>,
    /// Messages queued for injection at the start of the next `prompt()`.
    pub(crate) queued_next_turn: Vec<AgentMessage>,
    /// Error text from the most recent failed LLM call.
    pub error_message: Option<String>,
    /// Whether `set_tools` was called mid-run since the last `build_loop_config`.
    /// Cleanup uses this to avoid rolling back the user's tool change together
    /// with the HookedTool unwrap.
    pub tools_overwritten_mid_run: bool,
}

// ── HarnessHooks ──────────────────────────────────────────────────────────────

/// Collection of hooks injected into an [`AgentHarness`].
///
/// Multiple hooks of the same trait accumulate in registration order and are
/// composed at harness build time via `Composite*` wrappers from `llm-harness-loop`.
#[derive(Clone)]
pub struct HarnessHooks {
    /// Called once at the start of a `prompt()` run.
    pub before_run: Vec<Arc<dyn BeforeRunHook>>,
    /// Called at the start of each turn.
    pub before_turn: Vec<Arc<dyn BeforeTurnHook>>,
    /// Called at the end of each turn (before flush).
    pub after_turn: Vec<Arc<dyn AfterTurnHook>>,
    /// Called before each tool execution.
    pub before_tool_call: Vec<Arc<dyn BeforeToolCallHook>>,
    /// Called after each tool execution.
    pub after_tool_call: Vec<Arc<dyn AfterToolCallHook>>,
    /// Called before each LLM request to transform the context.
    pub transform_context: Vec<Arc<dyn TransformContextHook>>,
    /// Called after each turn to configure the next turn.
    pub prepare_next_turn: Vec<Arc<dyn PrepareNextTurnHook>>,
    /// Called when LLM stops naturally to decide whether to continue.
    pub should_stop: Vec<Arc<dyn ShouldStopHook>>,
    /// Called before each LLM provider request.
    pub before_provider_request: Vec<Arc<dyn BeforeProviderRequestHook>>,
    /// Called after each LLM provider response.
    pub after_provider_response: Vec<Arc<dyn AfterProviderResponseHook>>,
    /// Called before a candidate final answer is committed.
    pub final_answer_validator: Vec<Arc<dyn FinalAnswerValidator>>,
    /// Called before compaction to decide whether to proceed.
    pub before_compact: Vec<Arc<dyn BeforeCompactHook>>,
    /// Called after run_loop ends and harness returns to Idle.
    pub after_run: Vec<Arc<dyn AfterRunHook>>,
    /// Called when harness.abort() is invoked, before AfterRunHook.
    pub on_abort: Vec<Arc<dyn OnAbortHook>>,
}
impl HarnessHooks {
    /// All hooks absent (default state).
    pub fn none() -> Self {
        Self {
            before_run: vec![],
            before_turn: vec![],
            after_turn: vec![],
            before_tool_call: vec![],
            after_tool_call: vec![],
            transform_context: vec![],
            prepare_next_turn: vec![],
            should_stop: vec![],
            before_provider_request: vec![],
            after_provider_response: vec![],
            final_answer_validator: vec![],
            before_compact: vec![],
            after_run: vec![],
            on_abort: vec![],
        }
    }
}

// ── Inner mutable state ───────────────────────────────────────────────────────

pub(super) struct HarnessInner {
    pub(super) state: HarnessState,
    pub(super) skills: Vec<Skill>,
    /// Shared readable-skills store, kept in sync with `skills` (filtered to
    /// `!disable_model_invocation`). `SkillReadTool` holds a clone of this Arc
    /// so it sees updates from `reload_resources`/`add_skills` without needing
    /// access to `inner`.
    pub(super) readable_skills: Option<Arc<RwLock<Vec<Skill>>>>,
    pub(super) templates: Vec<PromptTemplate>,
    pub(super) steer_tx: mpsc::Sender<AgentMessage>,
    pub(super) follow_up_tx: mpsc::Sender<AgentMessage>,
    pub(super) current_abort: Option<CancellationToken>,
    pub(super) queue_capacity: usize,
    pub(super) max_tokens: u32,
    pub(super) retry: Option<RetryConfig>,
    pub(super) compaction_threshold: CompactionThresholdConfig,
    /// 已解析的 summary client + config。`None` → compaction 使用主对话 client/model。
    pub(super) compaction_summary: Option<(Arc<dyn LlmClient>, CompactionSummaryConfig)>,
    pub(super) auto_compact: bool,
    /// Maps tool_use_id → tool_name for in-flight tool calls.
    pub(super) active_tool_names: std::collections::HashMap<String, String>,
    /// Shared accumulated cost state. Populated by `CostAccumulatorHook`
    /// (injected via `after_provider_response` by `HarnessBuilder`).
    /// Read by `AgentHarness::usage()`.
    pub(super) cost: Arc<Mutex<CostAggregate>>,
    /// Max consecutive failures before auto-compact circuit breaker trips.
    pub(super) compaction_max_failures: u32,
    /// Consecutive compaction failure count for circuit breaker.
    pub(super) compaction_failure_count: u32,
    /// Custom compaction prompt spec, propagated to CompactionSummaryConfig.
    pub(super) compaction_prompt: Option<CompactionPromptSpec>,
    /// Current compaction query intent, set by caller via builder/options.
    /// `None` → non-task-aware compaction.
    pub(super) compaction_query: Option<String>,
}

impl HarnessInner {
    pub(super) fn reset_channels(
        &mut self,
    ) -> (mpsc::Receiver<AgentMessage>, mpsc::Receiver<AgentMessage>) {
        let cap = self.queue_capacity;
        let (steer_tx, steer_rx) = mpsc::channel(cap);
        let (follow_up_tx, follow_up_rx) = mpsc::channel(cap);
        self.steer_tx = steer_tx;
        self.follow_up_tx = follow_up_tx;
        (steer_rx, follow_up_rx)
    }
}
