use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use llm_harness_loop::{FinalAnswerMode, RetryConfig};
use llm_harness_types::*;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::ModelInfo;
use crate::session::types::*;
use crate::skills::{PromptTemplate, Skill};

use super::{AbortCascadeHook, AsyncSpawnHookHandle, IdleWatcherHook};

pub(super) const BROADCAST_CAPACITY: usize = 256;
pub(super) const DEFAULT_QUEUE_CAPACITY: usize = 32;
pub(super) const DEFAULT_MAX_TOKENS: u32 = 8192;

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
    /// Full registered tool list.
    pub tools: Vec<Arc<dyn Tool>>,
    /// Active tool subset; `None` means all tools are active.
    pub active_tools: Option<HashSet<String>>,
    /// Final-answer classification mode.
    pub final_answer_mode: FinalAnswerMode,
    /// System prompt.
    pub system_prompt: Option<String>,
    /// Streaming assistant message snapshot; `None` when not streaming.
    pub(crate) streaming_message: Option<AssistantMessage>,
    /// IDs of tools currently executing.
    pub(crate) pending_tool_calls: HashSet<String>,
    /// Session entries buffered during a running turn; flushed at turn end.
    pub(crate) pending_session_writes: Vec<SessionEntryPayload>,
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
    /// Called before compaction to decide whether to proceed.
    pub before_compact: Vec<Arc<dyn BeforeCompactHook>>,
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
            before_compact: vec![],
        }
    }
}

// ── Inner mutable state ───────────────────────────────────────────────────────

pub(super) struct HarnessInner {
    pub(super) state: HarnessState,
    pub(super) skills: Vec<Skill>,
    pub(super) templates: Vec<PromptTemplate>,
    pub(super) steer_tx: mpsc::Sender<AgentMessage>,
    pub(super) follow_up_tx: mpsc::Sender<AgentMessage>,
    pub(super) current_abort: Option<CancellationToken>,
    pub(super) queue_capacity: usize,
    pub(super) max_tokens: u32,
    pub(super) retry: Option<RetryConfig>,
    pub(super) compaction_reserve_tokens: Option<u32>,
    pub(super) compaction_keep_recent_tokens: Option<u32>,
    pub(super) auto_compact: bool,
    /// Maps tool_use_id → tool_name for in-flight tool calls.
    pub(super) active_tool_names: std::collections::HashMap<String, String>,
    /// Idle watcher（opt-in）。harness 持 watcher 的 `Arc`，watcher 的
    /// continue_run 闭包须捕获 `Weak<AgentHarness>` 而非 `Arc`，以打断
    /// `Arc<AgentHarness>` → inner.idle_watcher → 闭包 → `Arc<AgentHarness>`
    /// 的引用环。None 则无 watcher。
    pub(super) idle_watcher: Option<Arc<dyn IdleWatcherHook>>,
    /// AsyncSpawnHook 引用（opt-in）。None 则无异步投递。
    pub(super) async_spawn_hook: Option<Arc<dyn AsyncSpawnHookHandle>>,
    /// Abort 级联 hook（opt-in）。None 则 abort 不级联到 sub-agent。
    pub(super) abort_cascade_hook: Option<Arc<dyn AbortCascadeHook>>,
    /// Shared accumulated cost state. Populated by `CostAccumulatorHook`
    /// (injected via `after_provider_response` by `HarnessBuilder`).
    /// Read by `AgentHarness::usage()`.
    pub(super) cost: Arc<Mutex<CostAggregate>>,
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

// ── Dedup helpers ─────────────────────────────────────────────────────────────

/// Deduplicate skills by name, keeping the first occurrence (first-wins).
pub(super) fn dedup_skills(skills: Vec<Skill>) -> Vec<Skill> {
    let mut seen = HashSet::new();
    skills
        .into_iter()
        .filter(|s| seen.insert(s.name.clone()))
        .collect()
}

/// Deduplicate templates by name, keeping the first occurrence (first-wins).
pub(super) fn dedup_templates(templates: Vec<PromptTemplate>) -> Vec<PromptTemplate> {
    let mut seen = HashSet::new();
    templates
        .into_iter()
        .filter(|t| seen.insert(t.name.clone()))
        .collect()
}
