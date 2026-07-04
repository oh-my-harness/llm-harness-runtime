use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use llm_harness_loop::CompositePrepareNextTurnHook;
use llm_harness_loop::composite::CompositeBeforeCompactHook;
use llm_harness_loop::{
    ChatRequest, CompositeAfterProviderResponseHook, CompositeAfterToolCallHook,
    CompositeAfterTurnHook, CompositeBeforeProviderRequestHook, CompositeBeforeRunHook,
    CompositeBeforeToolCallHook, CompositeBeforeTurnHook, CompositeShouldStopHook,
    CompositeTransformContextHook, ConvertToLlmHook, DefaultConvertToLlm, FinalAnswerMode,
    HookedTool, LlmClient, LlmMessage, LoopConfig, RequestContent, RetryConfig, agent_loop,
};
use llm_harness_types::*;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::agent::ModelInfo;
use crate::compaction::{
    CompactionModelConfig, CompactionSettings, FileOperation, compact, prepare_compaction,
};
use crate::session::{Session, SessionRepo, repo::InMemorySessionRepo, types::*};
use crate::skills::{
    PromptTemplate, Skill, SkillDiagnostic, format_skill_invocation, invoke_template,
    load_prompt_templates, load_skills,
};

const BROADCAST_CAPACITY: usize = 256;
const DEFAULT_QUEUE_CAPACITY: usize = 32;
const DEFAULT_MAX_TOKENS: u32 = 8192;

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
    pub streaming_message: Option<AssistantMessage>,
    /// IDs of tools currently executing.
    pub pending_tool_calls: HashSet<String>,
    /// Session entries buffered during a running turn; flushed at turn end.
    pub pending_session_writes: Vec<SessionEntryPayload>,
    /// Messages queued for injection at the start of the next `prompt()`.
    pub queued_next_turn: Vec<AgentMessage>,
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

// ── AgentHarnessEvent ─────────────────────────────────────────────────────────

/// Compaction statistics carried by `AgentHarnessEvent::CompactionEnd`.
#[derive(Debug, Clone)]
pub struct CompactionStats {
    /// Estimated token count before compaction.
    pub tokens_before: usize,
    /// Estimated token count after compaction.
    pub tokens_after: usize,
    /// Number of entries compressed.
    pub compressed_entries: usize,
}

/// Tool call result carried by `AgentHarnessEvent::ToolCallEnd`.
#[derive(Debug, Clone)]
pub struct HarnessToolCallResult {
    /// Result content blocks.
    pub content: Vec<ContentBlock>,
    /// Extension data.
    pub details: serde_json::Value,
    /// Whether the tool returned an error.
    pub is_error: bool,
}

/// Events emitted by [`AgentHarness`].
#[derive(Debug)]
pub enum AgentHarnessEvent {
    /// Wraps a raw `AgentEvent` from the underlying loop.
    Agent(AgentEvent),
    /// Harness phase changed.
    PhaseChange {
        /// Previous phase.
        from: HarnessPhase,
        /// New phase.
        to: HarnessPhase,
    },
    /// Active model changed.
    ModelUpdate {
        /// Previous model ID.
        from: String,
        /// New model ID.
        to: String,
    },
    /// Thinking level changed.
    ThinkingLevelUpdate {
        /// Previous thinking level.
        from: ThinkingLevel,
        /// New thinking level.
        to: ThinkingLevel,
    },
    /// Tool list changed.
    ToolsUpdate {
        /// Tool names added.
        added: Vec<String>,
        /// Tool names removed.
        removed: Vec<String>,
    },
    /// Active tool subset changed.
    ActiveToolsUpdate {
        /// Active tools; `None` means all tools are active.
        active: Option<HashSet<String>>,
    },
    /// Loaded resources changed.
    ResourcesUpdate {
        /// Number of loaded skills.
        skills: usize,
        /// Number of loaded prompt templates.
        templates: usize,
        /// Diagnostics from resource loading.
        diagnostics: Vec<SkillDiagnostic>,
    },
    /// Session name changed.
    SessionInfoUpdate {
        /// New session name.
        name: String,
    },
    /// Compaction started.
    CompactionStart {
        /// Estimated token count before compaction.
        estimated_tokens: usize,
    },
    /// Compaction completed.
    CompactionEnd {
        /// Compaction statistics if successful.
        stats: Option<CompactionStats>,
        /// Error message if failed.
        error: Option<String>,
    },
    /// Steer/follow-up queue length changed.
    QueueUpdate {
        /// Number of steer messages queued.
        steer_len: usize,
        /// Number of follow-up messages queued.
        follow_up_len: usize,
    },
    /// Session writes flushed.
    SavePoint {
        /// Number of entries flushed.
        entries_flushed: usize,
    },
    /// A branch was forked.
    BranchForked {
        /// Source entry ID.
        from: EntryId,
        /// New leaf entry ID.
        new_leaf: EntryId,
        /// Optional branch label.
        label: Option<String>,
    },
    /// Branch cursor was switched.
    BranchSwitched {
        /// Previous entry ID.
        from: EntryId,
        /// New entry ID.
        to: EntryId,
    },
    /// A branch was deleted.
    BranchDeleted {
        /// Leaf entry ID of the deleted branch.
        leaf: EntryId,
    },
    /// A branch summary was generated and persisted.
    BranchSummarized {
        /// Leaf entry ID of the branch.
        leaf: EntryId,
        /// Branch summary text.
        summary: String,
    },
    /// A tool call started.
    ToolCallStart {
        /// Tool call ID.
        tool_use_id: String,
        /// Tool name.
        tool_name: String,
        /// Tool arguments (JSON).
        args: serde_json::Value,
    },
    /// A tool call completed.
    ToolCallEnd {
        /// Tool call ID.
        tool_use_id: String,
        /// Tool name.
        tool_name: String,
        /// Tool call result.
        result: HarnessToolCallResult,
    },
    /// All queued activity finished.
    Settled,
    /// Harness was aborted.
    Aborted,
}

// ── Idle watcher / async spawn hook decoupling traits ────────────────────────

/// Idle watcher 解耦 trait（由 runtime crate 的 `IdleWatcher` 实现）。
/// harness 在 `run_loop` 结束进入 Idle 后调 `on_idle`；在 `prompt_with_messages`
/// 守卫通过后调 `cancel`。watcher 持 continue_run 回调而非 harness 引用。
pub trait IdleWatcherHook: Send + Sync {
    /// loop 结束进入 Idle 后调用。检查 in-flight count，>0 则启动 watcher。
    fn on_idle(&self);
    /// prompt() 启动前调用：取消已有 watcher，让其归还 completion_rx。
    fn cancel(&self);
}

/// AsyncSpawnHook 解耦 trait（由 runtime crate 的 `AsyncSpawnHook` 实现）。
/// harness 在 `run_loop` 开头 borrow、结尾 release completion_rx guard。
pub trait AsyncSpawnHookHandle: Send + Sync {
    /// loop 启动前：从 registry 借出 completion_rx guard 存入 hook。
    fn borrow_completion_rx(&self);
    /// loop 结束后：drop guard，归还 rx 到 registry，让 watcher 能 take。
    fn release_completion_rx(&self);
    /// 回填 `FollowUpHandle`。由 `set_async_spawn_hook` 自动调用，
    /// 保证 should_stop 能注入 follow_up（修复 F-6：生产中 follow_up_handle
    /// 从未 set 导致 should_stop 永远走 handle_ready=false 死循环）。
    fn set_follow_up_handle(&self, handle: FollowUpHandle);
}

/// Abort 级联 hook（由 runtime 层的 `AsyncSpawnRegistry` 实现）。
/// harness `abort()` 时调用，取消所有通过本 harness 派发的 in-flight sub-agent。
pub trait AbortCascadeHook: Send + Sync {
    /// 取消所有通过本 harness 派发的 in-flight sub-agent。
    ///
    /// 调用时机：`AgentHarness::abort()` 中，cancel idle watcher 之后、
    /// cancel 自身 abort token 之前。
    /// 非阻塞：只 cancel token（置标志），不 join sub-agent task。
    /// 幂等：对已 cancel 的 token 再调 cancel 无效；空 registry 无操作。
    /// 生命周期持久：abort 不终止 agent 生命周期（可 re-prompt），
    /// hook 应可重复调用（实现方不应 take/consume 自身）。
    fn cancel_all_subagents(&self);
}

// ── Inner mutable state ───────────────────────────────────────────────────────

struct HarnessInner {
    state: HarnessState,
    skills: Vec<Skill>,
    templates: Vec<PromptTemplate>,
    steer_tx: mpsc::Sender<AgentMessage>,
    follow_up_tx: mpsc::Sender<AgentMessage>,
    current_abort: Option<CancellationToken>,
    queue_capacity: usize,
    max_tokens: u32,
    retry: Option<RetryConfig>,
    compaction_reserve_tokens: Option<u32>,
    compaction_keep_recent_tokens: Option<u32>,
    /// Maps tool_use_id → tool_name for in-flight tool calls.
    active_tool_names: std::collections::HashMap<String, String>,
    /// Idle watcher（opt-in）。harness 持 watcher 的 `Arc`，watcher 的
    /// continue_run 闭包须捕获 `Weak<AgentHarness>` 而非 `Arc`，以打断
    /// `Arc<AgentHarness>` → inner.idle_watcher → 闭包 → `Arc<AgentHarness>`
    /// 的引用环。None 则无 watcher。
    idle_watcher: Option<Arc<dyn IdleWatcherHook>>,
    /// AsyncSpawnHook 引用（opt-in）。None 则无异步投递。
    async_spawn_hook: Option<Arc<dyn AsyncSpawnHookHandle>>,
    /// Abort 级联 hook（opt-in）。None 则 abort 不级联到 sub-agent。
    abort_cascade_hook: Option<Arc<dyn AbortCascadeHook>>,
    /// Shared accumulated cost state. Populated by `CostAccumulatorHook`
    /// (injected via `after_provider_response` by `HarnessBuilder`).
    /// Read by `AgentHarness::usage()`.
    cost: Arc<Mutex<CostAggregate>>,
}

impl HarnessInner {
    fn reset_channels(&mut self) -> (mpsc::Receiver<AgentMessage>, mpsc::Receiver<AgentMessage>) {
        let cap = self.queue_capacity;
        let (steer_tx, steer_rx) = mpsc::channel(cap);
        let (follow_up_tx, follow_up_rx) = mpsc::channel(cap);
        self.steer_tx = steer_tx;
        self.follow_up_tx = follow_up_tx;
        (steer_rx, follow_up_rx)
    }
}

// ── FollowUpHandle ────────────────────────────────────────────────────────────

/// 对 `AgentHarness` follow_up channel 的可克隆句柄。
///
/// 持有与 `AgentHarness` 同一个 `Arc<Mutex<HarnessInner>>`，使外部组件
/// （`AsyncSpawnHook`、`PeerMessageHook`、`IdleWatcher`）能在不持有整个
/// harness 引用的前提下注入 follow_up 消息。
///
/// **不缓存** `follow_up_tx`——每次 `follow_up_message` 都重新 `lock()` 取
/// 当前 sender，保证 `reset_channels()` 替换 sender 后总能拿到最新的。
/// 持 `std::sync::Mutex` 期间不跨 await（`try_send` 非阻塞）。
#[derive(Clone)]
pub struct FollowUpHandle {
    inner: std::sync::Weak<Mutex<HarnessInner>>,
}

impl FollowUpHandle {
    /// 注入一条 follow_up 消息到 harness 当前的 follow_up channel。
    ///
    /// 与既有 `AgentHarness::follow_up_message`（`harness.rs:963`）语义一致：
    /// 正常路径不会失败——should_stop 返回 true 后 loop 在 `loop_fn.rs:519`
    /// 立即 `try_recv` 取走，`reset_channels` 在下一次 `prompt()` 才调。
    ///
    /// 持 `Weak` 而非 `Arc`：打断 `harness.inner → async_spawn_hook/peer_hook
    /// → FollowUpHandle → harness.inner` 的引用环。harness drop 后 upgrade
    /// 失败，静默 no-op（hook 的宿主已不存在）。
    pub fn follow_up_message(&self, msg: AgentMessage) {
        if let Some(inner) = self.inner.upgrade() {
            let inner = inner.lock().unwrap();
            let _ = inner.follow_up_tx.try_send(msg);
        }
    }
}

// ── Construction options ──────────────────────────────────────────────────────

/// Construction options for [`AgentHarness`].
pub struct AgentHarnessOptions {
    /// Initial model ID. Required.
    pub model: String,
    /// Optional model metadata.
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
    /// Maximum output tokens per LLM call.
    pub max_tokens: u32,
    /// mpsc channel capacity for steer/follow-up queues.
    pub queue_capacity: usize,
    /// Stream options for LLM requests.
    pub stream_options: StreamOptions,
    /// Hook collection (all optional).
    pub hooks: HarnessHooks,
    /// Authentication hook.
    pub auth: Option<Arc<dyn AuthHook>>,
    /// Custom context converter; uses `DefaultConvertToLlm` when `None`.
    pub convert_to_llm: Option<Arc<dyn ConvertToLlmHook>>,
    /// Pre-loaded skills.
    pub skills: Vec<Skill>,
    /// Pre-loaded prompt templates.
    pub templates: Vec<PromptTemplate>,
    /// Retry config for transient provider errors; `None` disables retry.
    pub retry: Option<RetryConfig>,
    /// 共享成本状态（`Arc<Mutex<CostAggregate>>`）。由 builder 创建，
    /// 并注入共享同一 Arc 的 `CostAccumulatorHook` 到
    /// `hooks.after_provider_response`。默认为空的全新 aggregate。
    pub cost: Arc<Mutex<CostAggregate>>,
    /// Override tokens to reserve for the LLM response during compaction.
    pub compaction_reserve_tokens: Option<u32>,
    /// Override how many recent context tokens to always preserve during compaction.
    pub compaction_keep_recent_tokens: Option<u32>,
    /// Independent compaction provider/model. `None` → compaction uses the main
    /// conversation client/model. When `Some`, all three fields inside are set
    /// together — half-configuration is impossible at the type level.
    pub compaction_model: Option<CompactionModelConfig>,
}

impl AgentHarnessOptions {
    /// Minimal options with only model set.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            model_info: None,
            thinking_level: ThinkingLevel::Off,
            temperature: None,
            tools: vec![],
            final_answer_mode: FinalAnswerMode::default(),
            system_prompt: None,
            max_tokens: DEFAULT_MAX_TOKENS,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            stream_options: StreamOptions::default(),
            hooks: HarnessHooks::none(),
            auth: None,
            convert_to_llm: None,
            skills: vec![],
            cost: Arc::new(Mutex::new(CostAggregate::default())),
            templates: vec![],
            retry: None,
            compaction_reserve_tokens: None,
            compaction_keep_recent_tokens: None,
            compaction_model: None,
        }
    }
}

// ── Wrapper for default PrepareNextTurnHook ───────────────────────────────────

/// Default wrapper around `PrepareNextTurnHook` that reads `HarnessInner` to
/// propagate `active_tools`, `model`, and `thinking_level` to the next turn.
struct DefaultPrepareNextTurn {
    inner: Arc<Mutex<HarnessInner>>,
    user_hook: Option<Arc<dyn PrepareNextTurnHook>>,
}

impl PrepareNextTurnHook for DefaultPrepareNextTurn {
    fn prepare<'a>(
        &'a self,
        ctx: PrepareNextTurnCtx<'a>,
    ) -> futures::future::BoxFuture<'a, Result<NextTurnDirective, AgentError>> {
        Box::pin(async move {
            // Short lock: read current config, then release immediately.
            let (model, thinking_level, temperature, tools, active_tools) = {
                let g = self.inner.lock().unwrap();
                let st = &g.state;
                (
                    st.model.clone(),
                    st.thinking_level,
                    st.temperature,
                    st.tools.clone(),
                    st.active_tools.clone(),
                )
            };

            let mut directive = NextTurnDirective {
                context: None,
                model: Some(model),
                thinking_level: Some(thinking_level),
                temperature: Some(temperature),
                tools: Some(tools),
                active_tools,
            };

            // Chain user-provided hook: its non-None fields win.
            if let Some(ref user_hook) = self.user_hook {
                let user = user_hook.prepare(ctx).await?;
                if user.context.is_some() {
                    directive.context = user.context;
                }
                if user.model.is_some() {
                    directive.model = user.model;
                }
                if user.thinking_level.is_some() {
                    directive.thinking_level = user.thinking_level;
                }
                if user.temperature.is_some() {
                    directive.temperature = user.temperature;
                }
                if user.tools.is_some() {
                    directive.tools = user.tools;
                }
                if user.active_tools.is_some() {
                    directive.active_tools = user.active_tools;
                }
            }

            Ok(directive)
        })
    }
}

// ── AgentHarness ──────────────────────────────────────────────────────────────

/// Orchestrates an `agent_loop` with session persistence, hooks, and compaction.
///
/// Unlike [`crate::Agent`], `AgentHarness` does not wrap `Agent` — it drives
/// `agent_loop` directly and manages session writes via `pending_session_writes`.
pub struct AgentHarness {
    client: Arc<dyn LlmClient>,
    compaction_model: Option<CompactionModelConfig>,
    session: Session,
    env: Arc<dyn ExecutionEnv>,
    inner: Arc<Mutex<HarnessInner>>,
    // AgentHarnessEvent is not Clone (wraps AgentEvent which wraps ToolError<anyhow::Error>).
    event_tx: broadcast::Sender<Arc<AgentHarnessEvent>>,
    phase_tx: watch::Sender<HarnessPhase>,
    phase_rx: watch::Receiver<HarnessPhase>,
    convert_to_llm: Arc<dyn ConvertToLlmHook>,
    hooks: HarnessHooks,
    stream_options: StreamOptions,
    auth: Option<Arc<dyn AuthHook>>,
}

impl AgentHarness {
    /// Create an `AgentHarness` backed by an in-memory session (useful for tests
    /// and one-shot prompts where persistence is not needed).
    pub async fn new_in_memory(
        client: Arc<dyn LlmClient>,
        env: Arc<dyn ExecutionEnv>,
        opts: AgentHarnessOptions,
    ) -> Self {
        let repo = InMemorySessionRepo::new();
        let storage = repo
            .create(CreateSessionOptions::default())
            .await
            .expect("in-memory session creation cannot fail");
        let session = Session::new(storage);
        Self::with_session(client, env, session, opts)
    }

    /// Create an `AgentHarness` with an existing session.
    pub fn with_session(
        client: Arc<dyn LlmClient>,
        env: Arc<dyn ExecutionEnv>,
        session: Session,
        opts: AgentHarnessOptions,
    ) -> Self {
        let cap = opts.queue_capacity;
        let (steer_tx, _steer_rx) = mpsc::channel(cap);
        let (follow_up_tx, _follow_up_rx) = mpsc::channel(cap);

        let (event_tx, _) = broadcast::channel::<Arc<AgentHarnessEvent>>(BROADCAST_CAPACITY);
        let (phase_tx, phase_rx) = watch::channel(HarnessPhase::Idle);

        let state = HarnessState {
            phase: HarnessPhase::Idle,
            model: opts.model,
            model_info: opts.model_info,
            thinking_level: opts.thinking_level,
            temperature: opts.temperature,
            tools: opts.tools,
            active_tools: None,
            final_answer_mode: opts.final_answer_mode,
            system_prompt: opts.system_prompt,
            streaming_message: None,
            tools_overwritten_mid_run: false,
            pending_tool_calls: HashSet::new(),
            pending_session_writes: vec![],
            queued_next_turn: vec![],
            error_message: None,
        };

        let inner = HarnessInner {
            state,
            skills: opts.skills,
            templates: opts.templates,
            steer_tx,
            follow_up_tx,
            cost: opts.cost,
            current_abort: None,
            queue_capacity: cap,
            max_tokens: opts.max_tokens,
            retry: opts.retry,
            compaction_reserve_tokens: opts.compaction_reserve_tokens,
            compaction_keep_recent_tokens: opts.compaction_keep_recent_tokens,
            active_tool_names: std::collections::HashMap::new(),
            idle_watcher: None,
            async_spawn_hook: None,
            abort_cascade_hook: None,
        };

        let convert_to_llm: Arc<dyn ConvertToLlmHook> = opts
            .convert_to_llm
            .unwrap_or_else(|| Arc::new(DefaultConvertToLlm::new()));

        Self {
            client,
            compaction_model: opts.compaction_model,
            session,
            env,
            inner: Arc::new(Mutex::new(inner)),
            event_tx,
            phase_tx,
            phase_rx,
            convert_to_llm,
            hooks: opts.hooks,
            stream_options: opts.stream_options,
            auth: opts.auth,
        }
    }

    // ── Structural operations (Idle only) ─────────────────────────────────

    /// Start a new run from a text prompt.
    pub async fn prompt(&self, text: impl Into<String>) -> Result<(), HarnessError> {
        let text = text.into();
        let user_msg = AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.clone() }],
            timestamp: chrono::Utc::now(),
        });
        self.prompt_with_messages(vec![user_msg]).await
    }

    /// Start a new run with an explicit initial message list.
    ///
    /// Known limitation: the Idle check and the `Turning` phase transition
    /// happen in separate lock acquisitions (the latter inside `run_loop`).
    /// A concurrent `continue_run` from an `IdleWatcher` can win the race in
    /// that window. The practical impact is bounded — a `before_run` hook's
    /// `system_prompt` edit may be left applied while the run fails to start
    /// — and `system_prompt` is not persisted, so the stale value is
    /// overwritten on the next successful prompt.
    pub async fn prompt_with_messages(
        &self,
        messages: Vec<AgentMessage>,
    ) -> Result<(), HarnessError> {
        // 1. Guard Idle phase; consume queued_next_turn; clone watcher.
        let (queued, watcher) = {
            let mut inner = self.inner.lock().unwrap();
            if inner.state.phase != HarnessPhase::Idle {
                return Err(HarnessError::NotIdle(inner.state.phase));
            }
            inner.state.error_message = None;
            let q = std::mem::take(&mut inner.state.queued_next_turn);
            let w = inner.idle_watcher.clone();
            (q, w)
        };
        // 取消已有 watcher（让位给用户 prompt；watcher 在 cancel 分支 break，
        // guard drop 归还 rx，run_loop 的 borrow_completion_rx 能取到）
        if let Some(w) = &watcher {
            w.cancel();
        }

        // 2. Merge queued + initial.
        let initial: Vec<AgentMessage> = queued.into_iter().chain(messages).collect();

        // 3. before_run hook.
        let initial = self.run_before_run_hook(initial).await?;

        // 4. Drive the loop.
        self.run_loop(initial).await
    }

    /// Expand `name` as a skill invocation and start a run.
    pub async fn skill(&self, name: &str, additional: Option<&str>) -> Result<(), HarnessError> {
        let text = {
            let inner = self.inner.lock().unwrap();
            let skill = inner
                .skills
                .iter()
                .find(|s| s.name == name)
                .ok_or_else(|| HarnessError::SkillNotFound(name.to_string()))?
                .clone();
            drop(inner);
            format_skill_invocation(&skill, additional)
        };
        self.prompt(text).await
    }

    /// Expand a prompt template and start a run.
    pub async fn prompt_from_template(
        &self,
        name: &str,
        args: Vec<String>,
    ) -> Result<(), HarnessError> {
        let text = {
            let inner = self.inner.lock().unwrap();
            let tmpl = inner
                .templates
                .iter()
                .find(|t| t.name == name)
                .ok_or_else(|| TemplateError::NotFound(name.to_string()))?
                .clone();
            drop(inner);
            invoke_template(&tmpl, &args)
        };
        self.prompt(text).await
    }

    /// Run compaction on the active session path (Idle only).
    ///
    /// Event contract: `CompactionEnd` is emitted while the harness is still
    /// in the `Compacting` phase (before the guard drops and restores Idle).
    /// Consumers observing `PhaseChange { Compacting → Idle }` can rely on the
    /// `CompactionEnd` event having already been emitted.
    pub async fn compact(&self) -> Result<CompactionStats, HarnessError> {
        let _guard = self.enter_phase(HarnessPhase::Compacting)?;

        let result = self.do_compact().await;

        match result {
            Ok(stats) => {
                self.emit(AgentHarnessEvent::CompactionEnd {
                    stats: Some(stats.clone()),
                    error: None,
                });
                Ok(stats)
            }
            Err(e) => {
                let msg = e.to_string();
                self.emit(AgentHarnessEvent::CompactionEnd {
                    stats: None,
                    error: Some(msg.clone()),
                });
                Err(e)
            }
        }
        // _guard 在此 drop，恢复 Idle
    }

    /// Reload skills and templates from the given directories.
    pub async fn reload_resources(
        &self,
        skill_dirs: Vec<PathBuf>,
        template_dirs: Vec<PathBuf>,
    ) -> Result<(), HarnessError> {
        let (skills, mut diags) = load_skills(self.env.as_ref(), &skill_dirs).await;
        let (templates, tmpl_diags) =
            load_prompt_templates(self.env.as_ref(), &template_dirs).await;
        diags.extend(tmpl_diags);

        let (skill_count, tmpl_count) = (skills.len(), templates.len());
        {
            let mut inner = self.inner.lock().unwrap();
            inner.skills = skills;
            inner.templates = templates;
        }
        self.emit(AgentHarnessEvent::ResourcesUpdate {
            skills: skill_count,
            templates: tmpl_count,
            diagnostics: diags,
        });
        Ok(())
    }

    // ── Runtime configuration (any phase) ─────────────────────────────────

    /// Update the model (appends a `ModelChange` entry if Idle).
    pub async fn set_model(
        &self,
        model: String,
        info: Option<ModelInfo>,
    ) -> Result<(), HarnessError> {
        let (old, pending) = {
            let mut inner = self.inner.lock().unwrap();
            let old = inner.state.model.clone();
            if old == model {
                return Ok(());
            }
            let payload = SessionEntryPayload::ModelChange {
                to: model.clone(),
                provider: None,
                model_id: None,
            };
            inner.state.model = model.clone();
            inner.state.model_info = info;
            if inner.state.phase == HarnessPhase::Idle {
                (old, Some(payload))
            } else {
                inner.state.pending_session_writes.push(payload);
                (old, None)
            }
        };
        if let Some(payload) = pending {
            self.session.append(payload).await?;
        }
        self.emit(AgentHarnessEvent::ModelUpdate {
            from: old,
            to: model,
        });
        Ok(())
    }

    /// Update the thinking level.
    pub async fn set_thinking_level(&self, level: ThinkingLevel) -> Result<(), HarnessError> {
        let (old, pending) = {
            let mut inner = self.inner.lock().unwrap();
            let old = inner.state.thinking_level;
            inner.state.thinking_level = level;
            let payload = SessionEntryPayload::ThinkingLevelChange { to: level };
            if inner.state.phase == HarnessPhase::Idle {
                (old, Some(payload))
            } else {
                inner.state.pending_session_writes.push(payload);
                (old, None)
            }
        };
        if let Some(payload) = pending {
            self.session.append(payload).await?;
        }
        self.emit(AgentHarnessEvent::ThinkingLevelUpdate {
            from: old,
            to: level,
        });
        Ok(())
    }

    /// Set the sampling temperature; takes effect at the start of the next turn.
    ///
    /// Note: unlike `set_thinking_level`, this change is NOT persisted to the
    /// session log. On session restore the temperature reverts to the
    /// `AgentHarnessOptions.temperature` initial value.
    pub async fn set_temperature(&self, temperature: Option<f32>) -> Result<(), HarnessError> {
        let mut inner = self.inner.lock().unwrap();
        inner.state.temperature = temperature;
        drop(inner);
        Ok(())
    }

    /// Set or clear the system prompt; takes effect at the start of the next turn.
    ///
    /// Note: this change is NOT persisted to the session log. On session restore
    /// the system prompt reverts to the `AgentHarnessOptions.system_prompt` initial
    /// value. If a `before_run` hook returns a system_prompt, it overrides this
    /// value for that turn.
    pub fn set_system_prompt(&self, prompt: Option<String>) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.system_prompt = prompt;
    }

    /// Set the maximum output tokens per LLM call; takes effect at the next turn.
    ///
    /// Note: this change is NOT persisted to the session log. On session restore
    /// `max_tokens` reverts to the `AgentHarnessOptions.max_tokens` initial value.
    pub fn set_max_tokens(&self, max_tokens: u32) {
        let mut inner = self.inner.lock().unwrap();
        inner.max_tokens = max_tokens;
    }

    /// Override tokens to reserve for the LLM response during compaction.
    /// Takes effect at the next `compact()` call. `None` clears the
    /// override, falling back to the built-in default (16,384) at the next
    /// `compact()` call — not the `AgentHarnessOptions` initial value.
    ///
    /// Note: this change is NOT persisted to the session log. On session
    /// restore the value reverts to the initial option.
    pub fn set_compaction_reserve_tokens(&self, tokens: Option<u32>) {
        let mut inner = self.inner.lock().unwrap();
        inner.compaction_reserve_tokens = tokens;
    }

    /// Override how many recent context tokens to always preserve during
    /// compaction. Takes effect at the next `compact()` call. `None` clears
    /// the override, falling back to the built-in default (20,000) at the
    /// next `compact()` call — not the `AgentHarnessOptions` initial value.
    ///
    /// Note: this change is NOT persisted to the session log. On session
    /// restore the value reverts to the initial option.
    pub fn set_compaction_keep_recent_tokens(&self, tokens: Option<u32>) {
        let mut inner = self.inner.lock().unwrap();
        inner.compaction_keep_recent_tokens = tokens;
    }

    /// Replace the registered tool list.
    pub async fn set_tools(&self, tools: Vec<Arc<dyn Tool>>) -> Result<(), HarnessError> {
        let (added, removed, pending) = {
            let mut inner = self.inner.lock().unwrap();
            let old_names: HashSet<String> = inner
                .state
                .tools
                .iter()
                .map(|t| t.name().to_string())
                .collect();
            let new_names: HashSet<String> = tools.iter().map(|t| t.name().to_string()).collect();
            let added: Vec<String> = new_names.difference(&old_names).cloned().collect();
            let removed: Vec<String> = old_names.difference(&new_names).cloned().collect();

            let names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
            let payload = SessionEntryPayload::ActiveToolsChange { active: names };
            inner.state.tools = tools;
            if inner.state.phase == HarnessPhase::Idle {
                (added, removed, Some(payload))
            } else {
                inner.state.tools_overwritten_mid_run = true;
                inner.state.pending_session_writes.push(payload);
                (added, removed, None)
            }
        };
        if let Some(payload) = pending {
            self.session.append(payload).await?;
        }
        self.emit(AgentHarnessEvent::ToolsUpdate { added, removed });
        Ok(())
    }

    /// Control the active tool subset. `None` enables all registered tools.
    pub async fn set_active_tools(
        &self,
        active: Option<HashSet<String>>,
    ) -> Result<(), HarnessError> {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.state.active_tools = active.clone();
        }
        self.emit(AgentHarnessEvent::ActiveToolsUpdate { active });
        Ok(())
    }

    /// Update final-answer classification mode; takes effect at the start of
    /// the next turn.
    pub async fn set_final_answer_mode(&self, mode: FinalAnswerMode) -> Result<(), HarnessError> {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.state.final_answer_mode = mode;
        }
        Ok(())
    }

    /// Update the session name.
    pub async fn set_session_name(&self, name: String) -> Result<(), HarnessError> {
        let payload = SessionEntryPayload::SessionInfo { name: name.clone() };
        {
            let inner = self.inner.lock().unwrap();
            if inner.state.phase != HarnessPhase::Idle {
                return Err(HarnessError::NotIdle(inner.state.phase));
            }
        }
        self.session.append(payload).await?;
        self.emit(AgentHarnessEvent::SessionInfoUpdate { name });
        Ok(())
    }

    // ── Session direct operations ──────────────────────────────────────────

    /// Append an `AgentMessage` to the session and return its entry ID.
    pub async fn append_message(&self, msg: AgentMessage) -> Result<EntryId, HarnessError> {
        {
            let inner = self.inner.lock().unwrap();
            if inner.state.phase != HarnessPhase::Idle {
                return Err(HarnessError::NotIdle(inner.state.phase));
            }
        }
        Ok(self.session.append_message(msg).await?)
    }

    /// Append a custom entry to the session and return its entry ID.
    pub async fn append_custom_entry(
        &self,
        custom_type: String,
        data: serde_json::Value,
    ) -> Result<EntryId, HarnessError> {
        {
            let inner = self.inner.lock().unwrap();
            if inner.state.phase != HarnessPhase::Idle {
                return Err(HarnessError::NotIdle(inner.state.phase));
            }
        }
        Ok(self
            .session
            .append(SessionEntryPayload::Custom { custom_type, data })
            .await?)
    }

    // ── Branch operations (Idle only) ──────────────────────────────────────

    /// Fork the session at `from_entry`.
    pub async fn fork_branch(
        &self,
        from_entry: EntryId,
        label: Option<String>,
    ) -> Result<EntryId, HarnessError> {
        let _guard = self.enter_phase(HarnessPhase::Branching)?;
        let new_leaf = self.session.fork_branch(from_entry, label.clone()).await?;
        self.emit(AgentHarnessEvent::BranchForked {
            from: from_entry,
            new_leaf,
            label,
        });
        Ok(new_leaf)
    }

    /// Switch the active cursor to `target`.
    pub async fn navigate_tree(&self, target: EntryId) -> Result<(), HarnessError> {
        let _guard = self.enter_phase(HarnessPhase::Branching)?;
        let meta = self.session.metadata().await?;
        let from = meta.active_cursor.unwrap_or(target);
        self.session.navigate_to(target).await?;
        self.emit(AgentHarnessEvent::BranchSwitched { from, to: target });
        Ok(())
    }

    /// List all branches (leaves) in the session.
    pub async fn list_branches(&self) -> Result<Vec<BranchInfo>, HarnessError> {
        Ok(self.session.list_branches().await?)
    }
    /// Return all entries on the active cursor's path (root-first).
    ///
    /// UI calls this after `BranchSwitched` to reload the conversation.
    pub async fn read_active_path(&self) -> Result<Vec<SessionEntry>, HarnessError> {
        Ok(self.session.read_active_path().await?)
    }

    /// Return all session entries (every node in the tree, unordered).
    ///
    /// UI calls this with `/branch list` to reconstruct the full tree
    /// for ASCII rendering. Entries carry `id` + `parent_id` for tree
    /// reconstruction.
    pub async fn read_all_entries(&self) -> Result<Vec<SessionEntry>, HarnessError> {
        Ok(self.session.storage().all_entries().await?)
    }

    /// Delete the branch ending at `leaf` (Idle only).
    pub async fn delete_branch(&self, leaf: EntryId) -> Result<(), HarnessError> {
        let _guard = self.enter_phase(HarnessPhase::Branching)?;
        self.session.delete_branch(leaf).await?;
        self.emit(AgentHarnessEvent::BranchDeleted { leaf });
        Ok(())
    }

    /// Generate an AI summary for the branch ending at `leaf` and persist it (Idle only).
    ///
    /// Calls the current model to summarize the branch conversation, then appends
    /// a `BranchSummary` entry to the session.
    pub async fn generate_branch_summary(
        &self,
        leaf: EntryId,
    ) -> Result<BranchSummaryEntry, HarnessError> {
        let _guard = self.enter_phase(HarnessPhase::Branching)?;

        let path = self.session.read_path_of(leaf).await?;
        if path.is_empty() {
            return Err(SessionError::EntryNotFound(leaf).into());
        }

        let (model, max_tokens) = {
            let inner = self.inner.lock().unwrap();
            (inner.state.model.clone(), inner.max_tokens)
        };

        let conversation_text = crate::compaction::format_entries_as_text(&path);
        let user_content = format!(
            "Here is the conversation branch to summarize:\n\n\
            <conversation>\n{conversation_text}\n</conversation>\n\n\
            Produce a concise summary of what was accomplished in this branch, \
            key decisions made, and any outstanding work."
        );

        let req = ChatRequest::builder(model, max_tokens.min(4096))
            .messages(vec![
                LlmMessage::System(
                    "You are a context summarization assistant. Summarize the provided \
                    conversation branch concisely, preserving goals, decisions, and next steps."
                        .to_string(),
                ),
                LlmMessage::User(vec![RequestContent::Text(user_content)]),
            ])
            .build();

        let response = self
            .client
            .chat(&req)
            .await
            .map_err(|e| CompactionError::SummaryFailed(e.to_string()))?;

        let summary = response.text();

        let from_entry = path.first().map(|e| e.id).unwrap_or(leaf);
        let token_count: usize = path
            .iter()
            .map(crate::compaction::estimate_tokens_for_entry)
            .sum();

        let entry = BranchSummaryEntry {
            leaf_id: leaf,
            from_entry,
            summary,
            token_count,
        };

        self.session
            .append(SessionEntryPayload::BranchSummary(entry.clone()))
            .await?;

        self.emit(AgentHarnessEvent::BranchSummarized {
            leaf,
            summary: entry.summary.clone(),
        });

        Ok(entry)
    }

    /// Build the effective LLM context from the active session path.
    pub async fn build_context(&self) -> Result<BuiltContext, HarnessError> {
        Ok(self.session.build_context().await?)
    }

    // ── Queue operations (any phase) ──────────────────────────────────────

    /// Enqueue a text message as a steer.
    pub fn steer(&self, text: impl Into<String>) {
        self.steer_message(AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: chrono::Utc::now(),
        }));
    }

    /// Enqueue a full message as a steer.
    pub fn steer_message(&self, msg: AgentMessage) {
        let inner = self.inner.lock().unwrap();
        let _ = inner.steer_tx.try_send(msg);
    }

    /// Enqueue a text message as a follow-up.
    pub fn follow_up(&self, text: impl Into<String>) {
        self.follow_up_message(AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: chrono::Utc::now(),
        }));
    }

    /// Enqueue a full message as a follow-up.
    pub fn follow_up_message(&self, msg: AgentMessage) {
        let inner = self.inner.lock().unwrap();
        let _ = inner.follow_up_tx.try_send(msg);
    }

    /// 返回一个可克隆的 `FollowUpHandle`，供外部组件注入 follow_up 消息。
    ///
    /// 持 `Weak` 引用（见 `FollowUpHandle` 文档），不延长 harness 生命周期。
    pub fn follow_up_handle(&self) -> FollowUpHandle {
        FollowUpHandle {
            inner: Arc::downgrade(&self.inner),
        }
    }
    /// 配置 Idle watcher（opt-in）。不设则无自动唤醒（行为退化为旧设计）。
    pub fn set_idle_watcher(&self, watcher: Arc<dyn IdleWatcherHook>) {
        self.inner.lock().unwrap().idle_watcher = Some(watcher);
    }

    /// 配置 AsyncSpawnHook 引用（loop 启动时 borrow completion_rx）。
    /// 自动回填 `FollowUpHandle`（修复 F-6：生产中 follow_up_handle 从未 set
    /// 导致 should_stop 永远走 handle_ready=false 死循环）。
    pub fn set_async_spawn_hook(&self, hook: Arc<dyn AsyncSpawnHookHandle>) {
        hook.set_follow_up_handle(self.follow_up_handle());
        self.inner.lock().unwrap().async_spawn_hook = Some(hook);
    }

    /// 配置 Abort 级联 hook（opt-in）。abort() 时取消所有 in-flight sub-agent。
    pub fn set_abort_cascade_hook(&self, hook: Arc<dyn AbortCascadeHook>) {
        self.inner.lock().unwrap().abort_cascade_hook = Some(hook);
    }

    /// 不注入初始消息，驱动一轮 agent loop 直到再次 Idle。
    /// 供 Idle watcher 收到 completion 后唤醒主 agent。
    /// 自带 Idle 守卫（与 `prompt_with_messages` 对称）。
    ///
    /// Known limitation: the Idle check and the `Turning` phase transition
    /// happen in separate lock acquisitions (the latter inside `run_loop`),
    /// the same TOCTOU window documented on `prompt_with_messages`. A
    /// concurrent caller can win the race in that window.
    pub async fn continue_run(&self) -> Result<(), HarnessError> {
        {
            let inner = self.inner.lock().unwrap();
            if inner.state.phase != HarnessPhase::Idle {
                return Err(HarnessError::NotIdle(inner.state.phase));
            }
        }
        self.run_loop(vec![]).await
    }

    /// Buffer a text message to be injected at the start of the next `prompt()`.
    pub fn next_turn(&self, text: impl Into<String>) {
        self.next_turn_message(AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: chrono::Utc::now(),
        }));
    }

    /// Buffer a message to be injected at the start of the next `prompt()`.
    pub fn next_turn_message(&self, msg: AgentMessage) {
        self.inner.lock().unwrap().state.queued_next_turn.push(msg);
    }

    /// Drain the steer queue. Only callable when Idle.
    pub fn clear_steering_queue(&self) -> Result<(), HarnessError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.state.phase != HarnessPhase::Idle {
            return Err(HarnessError::NotIdle(inner.state.phase));
        }
        let (tx, _) = mpsc::channel(inner.queue_capacity);
        inner.steer_tx = tx;
        Ok(())
    }

    /// Drain the follow-up queue. Only callable when Idle.
    pub fn clear_follow_up_queue(&self) -> Result<(), HarnessError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.state.phase != HarnessPhase::Idle {
            return Err(HarnessError::NotIdle(inner.state.phase));
        }
        let (tx, _) = mpsc::channel(inner.queue_capacity);
        inner.follow_up_tx = tx;
        Ok(())
    }

    /// Drain all queues (steer, follow-up, next_turn). Only callable when Idle.
    pub fn clear_all_queues(&self) -> Result<(), HarnessError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.state.phase != HarnessPhase::Idle {
            return Err(HarnessError::NotIdle(inner.state.phase));
        }
        let cap = inner.queue_capacity;
        let (steer_tx, _) = mpsc::channel(cap);
        let (follow_up_tx, _) = mpsc::channel(cap);
        inner.steer_tx = steer_tx;
        inner.follow_up_tx = follow_up_tx;
        inner.state.queued_next_turn.clear();
        Ok(())
    }

    /// Returns `true` if any queue is non-empty.
    pub fn has_queued_messages(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.steer_tx.max_capacity() != inner.steer_tx.capacity()
            || inner.follow_up_tx.max_capacity() != inner.follow_up_tx.capacity()
            || !inner.state.queued_next_turn.is_empty()
    }

    /// Cancel the current run, idle watcher, and all in-flight sub-agents.
    ///
    /// 先取消 Idle watcher，再级联取消所有 in-flight sub-agent（通过
    /// `AbortCascadeHook`），最后 cancel 自己的 abort token。顺序很重要：
    /// 先 cancel watcher 使其在 aborted completion 到达前退出 recv 循环，
    /// 避免 aborted completion 触发 continue_run 唤醒已 abort 的主 agent。
    pub fn abort(&self) {
        // 先取 cascade hook 和 watcher（持锁），释放锁后再调用
        // （hook/watcher 内部可能加自己的锁，避免死锁）。
        let (cascade_hook, watcher) = {
            let inner = self.inner.lock().unwrap();
            let hook = inner.abort_cascade_hook.clone();
            let w = inner.idle_watcher.clone();
            (hook, w)
        };
        // 先取消 watcher，再级联取消 sub-agent。顺序很重要：
        // 若先 cancel sub-agent，它们的 aborted completion 会被 watcher 接收并
        // 触发 continue_run 唤醒已 abort 的主 agent。先 cancel watcher 使其在
        // aborted completion 到达前退出 recv 循环。
        if let Some(w) = watcher {
            w.cancel();
        }
        if let Some(hook) = cascade_hook {
            hook.cancel_all_subagents();
        }
        let mut inner = self.inner.lock().unwrap();
        if let Some(token) = inner.current_abort.take() {
            token.cancel();
        }
    }

    /// 为下一次 `prompt()` 运行设置外部提供的 abort token。
    ///
    /// 设置后，此 token 将替代 `run_loop()` 内部创建的 token。
    /// 适用于 sub-agent 派发场景（父 agent 控制 abort 信号）。
    /// 此 token 在下次 `prompt()` 调用时被消费；后续未调用
    /// 此方法的运行将恢复自行创建 token。
    pub fn set_abort_token(&self, token: CancellationToken) {
        self.inner.lock().unwrap().current_abort = Some(token);
    }

    // ── Observation ───────────────────────────────────────────────────────

    /// Return a snapshot of the current harness state.
    pub fn state(&self) -> HarnessState {
        self.inner.lock().unwrap().state.clone()
    }

    /// 返回当前累积的会话成本快照。
    ///
    /// 由内置的 `CostAccumulatorHook`（由 `HarnessBuilder` 注入）填充。
    /// 若未配置 pricing provider，返回的 `CostAggregate` 中 cost 为 0
    ///（token 计数仍会累积）。
    pub fn usage(&self) -> CostAggregate {
        // Clone the Arc out first, then lock the inner CostAggregate.
        // This avoids holding the `inner` mutex while locking `cost`,
        // keeping the lock-ordering contract simple: `cost` is only ever
        // locked on its own (by CostAccumulatorHook / BudgetControlAdapter),
        // never nested under `inner`.
        let cost = self.inner.lock().unwrap().cost.clone();
        cost.lock().unwrap().clone()
    }

    /// 重置累积的会话成本归零。
    ///
    /// 将共享的 `CostAggregate` 恢复为 `default()`。适用于长生命周期
    /// harness 的周期性成本统计（如在新的任务开始时清零）。
    /// 调用后 `usage()` 从 0 重新开始累积。
    pub fn reset_usage(&self) {
        let cost = self.inner.lock().unwrap().cost.clone();
        *cost.lock().unwrap() = CostAggregate::default();
    }

    /// Return a copy of the current skill list.
    pub fn skills(&self) -> Vec<Skill> {
        self.inner.lock().unwrap().skills.clone()
    }

    /// Return a copy of the current template list.
    pub fn templates(&self) -> Vec<PromptTemplate> {
        self.inner.lock().unwrap().templates.clone()
    }

    /// Subscribe to the event broadcast stream.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<AgentHarnessEvent>> {
        self.event_tx.subscribe()
    }

    /// Async-wait until the harness returns to `Idle`.
    pub async fn wait_for_idle(&self) {
        let mut rx = self.phase_rx.clone();
        loop {
            if *rx.borrow() == HarnessPhase::Idle {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Async-wait until all pending session writes have been flushed to disk.
    ///
    /// Returns immediately when the harness is already idle (writes already flushed).
    /// Otherwise waits for the next `Settled` or `Aborted` event.
    pub async fn wait_for_settled(&self) {
        // Subscribe before the phase check to avoid missing the Settled event.
        let mut rx = self.event_tx.subscribe();
        if *self.phase_rx.borrow() == HarnessPhase::Idle {
            return;
        }
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if matches!(
                        ev.as_ref(),
                        AgentHarnessEvent::Settled | AgentHarnessEvent::Aborted
                    ) {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    }

    // ── Private helpers ───────────────────────────────────────────────────

    fn emit(&self, event: AgentHarnessEvent) {
        let _ = self.event_tx.send(Arc::new(event));
    }

    fn notify_phase_change(&self, old: HarnessPhase, new: HarnessPhase) {
        let _ = self.phase_tx.send(new);
        self.emit(AgentHarnessEvent::PhaseChange { from: old, to: new });
    }

    fn set_phase(&self, phase: HarnessPhase) {
        let old = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let old = inner.state.phase;
            inner.state.phase = phase;
            old
        };
        self.notify_phase_change(old, phase);
    }
    /// 原子地检查 Idle 并切换到 `phase`。失败返回 Err。
    fn enter_phase(&self, phase: HarnessPhase) -> Result<PhaseGuard<'_>, HarnessError> {
        let old = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if inner.state.phase != HarnessPhase::Idle {
                return Err(HarnessError::NotIdle(inner.state.phase));
            }
            inner.state.phase = phase;
            HarnessPhase::Idle
        };
        self.notify_phase_change(old, phase);
        Ok(PhaseGuard { harness: self })
    }

    fn push_pending_write(&self, payload: SessionEntryPayload) {
        self.inner
            .lock()
            .unwrap()
            .state
            .pending_session_writes
            .push(payload);
    }

    async fn flush_pending_writes(&self) -> Result<usize, HarnessError> {
        let mut count = 0;
        loop {
            let payload = {
                let inner = self.inner.lock().unwrap();
                inner.state.pending_session_writes.first().cloned()
            };
            let Some(payload) = payload else { break };
            // Write before removing. If this fails, payload remains in pending.
            // The run_loop cleanup in the Err branch will clear the remainder.
            self.session.append(payload).await?;
            self.inner
                .lock()
                .unwrap()
                .state
                .pending_session_writes
                .remove(0);
            count += 1;
        }
        Ok(count)
    }

    async fn run_before_run_hook(
        &self,
        mut initial: Vec<AgentMessage>,
    ) -> Result<Vec<AgentMessage>, HarnessError> {
        if self.hooks.before_run.is_empty() {
            return Ok(initial);
        }
        let composite = Arc::new(CompositeBeforeRunHook::new(self.hooks.before_run.clone()));
        let (skills, templates) = {
            let inner = self.inner.lock().unwrap();
            let snames: Vec<String> = inner.skills.iter().map(|s| s.name.clone()).collect();
            let tnames: Vec<String> = inner.templates.iter().map(|t| t.name.clone()).collect();
            (snames, tnames)
        };
        let resources = AgentHarnessResources {
            skill_names: skills,
            template_names: templates,
        };
        let prompt_text = initial
            .iter()
            .find_map(|m| {
                if let AgentMessage::User(u) = m {
                    u.content.iter().find_map(|c| {
                        if let ContentBlock::Text { text } = c {
                            Some(text.clone())
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            })
            .unwrap_or_default();

        let mut system_prompt = self.inner.lock().unwrap().state.system_prompt.clone();
        let result = composite
            .before_run(BeforeRunCtx {
                prompt_text: &prompt_text,
                initial_messages: &mut initial,
                system_prompt: &mut system_prompt,
                resources: &resources,
            })
            .await
            .map_err(HarnessError::from)?;

        // Apply hook results.
        initial.extend(result.additional_messages);
        if let Some(sp) = result.system_prompt.or(system_prompt) {
            self.inner.lock().unwrap().state.system_prompt = Some(sp);
        }
        Ok(initial)
    }

    fn build_loop_config(
        &self,
        steer_rx: mpsc::Receiver<AgentMessage>,
        follow_up_rx: mpsc::Receiver<AgentMessage>,
        abort: CancellationToken,
    ) -> LoopConfig {
        // ── Snapshot state (lock is held via `inner` until `drop(inner)`) ──
        let (
            tools,
            retry,
            model,
            thinking_level,
            temperature,
            active_tools,
            final_answer_mode,
            max_tokens,
        ) = {
            let inner = self.inner.lock().unwrap();
            let st = &inner.state;
            (
                st.tools.clone(),
                inner.retry.clone(),
                st.model.clone(),
                st.thinking_level,
                st.temperature,
                st.active_tools.clone(),
                st.final_answer_mode.clone(),
                inner.max_tokens,
            )
        };

        // Wrap tools with HookedTool if before/after tool call hooks exist.
        let before = (!self.hooks.before_tool_call.is_empty()).then(|| {
            Arc::new(CompositeBeforeToolCallHook::new(
                self.hooks.before_tool_call.clone(),
            )) as Arc<dyn BeforeToolCallHook>
        });
        let after = (!self.hooks.after_tool_call.is_empty()).then(|| {
            Arc::new(CompositeAfterToolCallHook::new(
                self.hooks.after_tool_call.clone(),
            )) as Arc<dyn AfterToolCallHook>
        });
        let tools: Vec<Arc<dyn Tool>> = if before.is_some() || after.is_some() {
            let wrapped: Vec<Arc<dyn Tool>> = tools
                .into_iter()
                .map(|t| {
                    Arc::new(HookedTool {
                        inner: t,
                        before: before.clone(),
                        after: after.clone(),
                    }) as Arc<dyn Tool>
                })
                .collect();
            // Persist wrapped tools to state so that `DefaultPrepareNextTurn`
            // returns them on subsequent turns, preserving before/after hooks
            // across the agent loop.
            {
                let mut inner = self.inner.lock().unwrap();
                inner.state.tools.clone_from(&wrapped);
            }
            wrapped
        } else {
            tools
        };

        // Build the default prepare_next_turn wrapper.
        let prepare_next_turn: Arc<dyn PrepareNextTurnHook> = Arc::new(DefaultPrepareNextTurn {
            inner: self.inner.clone(),
            user_hook: (!self.hooks.prepare_next_turn.is_empty()).then(|| {
                Arc::new(CompositePrepareNextTurnHook::new(
                    self.hooks.prepare_next_turn.clone(),
                )) as Arc<dyn PrepareNextTurnHook>
            }),
        });

        LoopConfig {
            model,
            max_tokens,
            temperature,
            thinking_level,
            tools,
            active_tools,
            default_execution_mode: ToolExecutionMode::Parallel,
            final_answer_mode,
            env: self.env.clone(),
            abort,
            stream_options: self.stream_options.clone(),
            convert_to_llm: self.convert_to_llm.clone(),
            transform_context: (!self.hooks.transform_context.is_empty()).then(|| {
                Arc::new(CompositeTransformContextHook::new(
                    self.hooks.transform_context.clone(),
                )) as Arc<dyn TransformContextHook>
            }),
            prepare_next_turn: Some(prepare_next_turn),
            should_stop: (!self.hooks.should_stop.is_empty()).then(|| {
                Arc::new(CompositeShouldStopHook::new(self.hooks.should_stop.clone()))
                    as Arc<dyn ShouldStopHook>
            }),
            before_provider_request: (!self.hooks.before_provider_request.is_empty()).then(|| {
                Arc::new(CompositeBeforeProviderRequestHook::new(
                    self.hooks.before_provider_request.clone(),
                )) as Arc<dyn BeforeProviderRequestHook>
            }),
            after_provider_response: (!self.hooks.after_provider_response.is_empty()).then(|| {
                Arc::new(CompositeAfterProviderResponseHook::new(
                    self.hooks.after_provider_response.clone(),
                )) as Arc<dyn AfterProviderResponseHook>
            }),
            auth: self.auth.clone(),
            steer_rx: Some(steer_rx),
            follow_up_rx: Some(follow_up_rx),
            retry,
        }
    }

    async fn run_loop(&self, initial: Vec<AgentMessage>) -> Result<(), HarnessError> {
        // 1. Setup: channels, abort token, system prompt, hooks.
        let (steer_rx, follow_up_rx, abort, system_prompt, original_tools, async_hook, watcher) = {
            let mut inner = self.inner.lock().unwrap();
            inner.state.streaming_message = None;
            inner.state.pending_tool_calls.clear();
            let (steer_rx, follow_up_rx) = inner.reset_channels();
            let abort = inner.current_abort.take().unwrap_or_default();
            inner.current_abort = Some(abort.clone());
            let sp = inner.state.system_prompt.clone();
            // clone Vec<Arc<dyn Tool>>（浅拷贝指针，非工具本体），用于 loop 结束后
            // 恢复未包装的 tools，避免下次 build_loop_config 二次包装 HookedTool。
            let ot = inner.state.tools.clone();
            let async_hook = inner.async_spawn_hook.clone();
            let watcher = inner.idle_watcher.clone();
            (steer_rx, follow_up_rx, abort, sp, ot, async_hook, watcher)
        };
        let _guard = self.enter_phase(HarnessPhase::Turning)?;
        if let Some(h) = &async_hook {
            h.borrow_completion_rx();
        }

        let result = self
            .drive_loop(initial, steer_rx, follow_up_rx, abort, system_prompt)
            .await;

        // Cleanup always runs, success or failure.
        {
            let mut inner = self.inner.lock().unwrap();
            inner.state.streaming_message = None;
            inner.state.pending_tool_calls.clear();
            inner.active_tool_names.clear();
            inner.current_abort = None;
            // On error, the run is discarded: reset the mid-run flag so the
            // selective restore below rolls back to original_tools.
            if let Err(ref e) = result {
                inner.state.error_message = Some(e.to_string());
                inner.state.pending_session_writes.clear();
                inner.state.tools_overwritten_mid_run = false;
            }
            // Restore original unwrapped tools so the next call to `build_loop_config`
            // doesn't re-wrap already-HookedTool-wrapped tools (double-wrapping) —
            // unless `set_tools` overwrote the tools mid-run, in which case keep the
            // new (unwrapped) set; next `build_loop_config` will wrap them as needed.
            if inner.state.tools_overwritten_mid_run {
                inner.state.tools_overwritten_mid_run = false;
            } else {
                inner.state.tools = original_tools;
            }
        }
        // Release the completion_rx borrow BEFORE dropping the guard (which
        // restores Idle). Otherwise a concurrent caller seeing Idle could
        // borrow_completion_rx before the previous run released it.
        if let Some(h) = &async_hook {
            h.release_completion_rx();
        }
        drop(_guard);
        // Idle 后：若配置了 watcher，让其检查 in-flight 并按需启动
        if let Some(w) = &watcher {
            w.on_idle();
        }

        result
    }

    async fn drive_loop(
        &self,
        initial: Vec<AgentMessage>,
        steer_rx: mpsc::Receiver<AgentMessage>,
        follow_up_rx: mpsc::Receiver<AgentMessage>,
        abort: CancellationToken,
        system_prompt: Option<String>,
    ) -> Result<(), HarnessError> {
        // 2. Build context from session.
        let built = self.session.build_context().await?;
        let messages: Vec<AgentMessage> =
            built.messages.into_iter().chain(initial.clone()).collect();

        let ctx = AgentContext {
            system_prompt,
            messages,
        };

        // 3. Build LoopConfig.
        let config = self.build_loop_config(steer_rx, follow_up_rx, abort.clone());

        // 4. Drive the stream. Push initial messages to pending before the loop starts
        //    (AgentStart is a signal-only event; initial messages are persisted here).
        for msg in &initial {
            self.push_pending_write(SessionEntryPayload::Message(msg.clone()));
        }

        let mut stream = Box::pin(agent_loop(self.client.clone(), ctx, config));
        let mut turn_messages: Vec<AgentMessage> = vec![];

        while let Some(event) = stream.next().await {
            match &event {
                AgentEvent::AgentStart => {
                    self.emit(AgentHarnessEvent::Agent(event));
                }
                AgentEvent::TurnStart { index } => {
                    if !self.hooks.before_turn.is_empty() {
                        let composite =
                            Arc::new(CompositeBeforeTurnHook::new(self.hooks.before_turn.clone()));
                        let snapshot = {
                            let inner = self.inner.lock().unwrap();
                            TurnSnapshot {
                                model: inner.state.model.clone(),
                                thinking_level: inner.state.thinking_level,
                                temperature: inner.state.temperature,
                                tools: inner.state.tools.clone(),
                                system_prompt: inner.state.system_prompt.clone(),
                            }
                        };
                        composite
                            .before_turn(BeforeTurnCtx {
                                turn_index: *index,
                                snapshot: &snapshot,
                            })
                            .await;
                    }
                    turn_messages.clear();
                    self.emit(AgentHarnessEvent::Agent(event));
                }
                AgentEvent::MessageUpdate { partial, .. } => {
                    self.inner.lock().unwrap().state.streaming_message = Some(partial.clone());
                    self.emit(AgentHarnessEvent::Agent(event));
                }
                AgentEvent::MessageEnd { message, .. } => {
                    self.inner.lock().unwrap().state.streaming_message = None;
                    let msg = AgentMessage::Assistant(message.clone());
                    self.push_pending_write(SessionEntryPayload::Message(msg.clone()));
                    turn_messages.push(msg);
                    self.emit(AgentHarnessEvent::Agent(event));
                }
                AgentEvent::ToolExecutionStart {
                    tool_use_id,
                    tool_name,
                    args,
                } => {
                    {
                        let mut inner = self.inner.lock().unwrap();
                        inner.state.pending_tool_calls.insert(tool_use_id.clone());
                        inner
                            .active_tool_names
                            .insert(tool_use_id.clone(), tool_name.clone());
                    }
                    self.emit(AgentHarnessEvent::ToolCallStart {
                        tool_use_id: tool_use_id.clone(),
                        tool_name: tool_name.clone(),
                        args: args.clone(),
                    });
                    self.emit(AgentHarnessEvent::Agent(event));
                }
                AgentEvent::ToolExecutionEnd {
                    tool_use_id,
                    result,
                } => {
                    self.inner
                        .lock()
                        .unwrap()
                        .state
                        .pending_tool_calls
                        .remove(tool_use_id.as_str());

                    // Build ToolResultMessage for session persistence.
                    let (content, is_error) = match result {
                        Ok(r) => (r.content.clone(), false),
                        Err(e) => (
                            vec![ContentBlock::Text {
                                text: e.to_string(),
                            }],
                            true,
                        ),
                    };
                    let msg = AgentMessage::ToolResult(ToolResultMessage {
                        tool_use_id: tool_use_id.clone(),
                        content: content.clone(),
                        is_error,
                        timestamp: chrono::Utc::now(),
                    });
                    self.push_pending_write(SessionEntryPayload::Message(msg.clone()));
                    turn_messages.push(msg);

                    // Emit ToolCallEnd harness event.
                    let details = match result {
                        Ok(r) => r.details.clone(),
                        Err(_) => serde_json::Value::Null,
                    };
                    let resolved_name = self
                        .inner
                        .lock()
                        .unwrap()
                        .active_tool_names
                        .remove(tool_use_id.as_str())
                        .unwrap_or_default();
                    self.emit(AgentHarnessEvent::ToolCallEnd {
                        tool_use_id: tool_use_id.clone(),
                        tool_name: resolved_name,
                        result: HarnessToolCallResult {
                            content,
                            details,
                            is_error,
                        },
                    });
                    self.emit(AgentHarnessEvent::Agent(event));
                }
                AgentEvent::TurnEnd { index, .. } => {
                    // Call after_turn hook before flush.
                    if !self.hooks.after_turn.is_empty() {
                        let composite =
                            Arc::new(CompositeAfterTurnHook::new(self.hooks.after_turn.clone()));
                        composite
                            .after_turn(AfterTurnCtx {
                                turn_index: *index,
                                new_messages: &turn_messages,
                            })
                            .await;
                    }
                    // Flush pending writes (save point).
                    let flushed = self.flush_pending_writes().await?;
                    self.emit(AgentHarnessEvent::SavePoint {
                        entries_flushed: flushed,
                    });
                    self.emit(AgentHarnessEvent::Agent(event));
                }
                AgentEvent::AgentEnd { .. } => {
                    self.emit(AgentHarnessEvent::Agent(event));
                    self.emit(AgentHarnessEvent::Settled);
                    break;
                }
                AgentEvent::Error(e) => {
                    {
                        let mut inner = self.inner.lock().unwrap();
                        inner.state.error_message = Some(e.to_string());
                        // Clear pending writes so the failed turn's user message is not
                        // flushed to the session on a future successful turn, which would
                        // produce two consecutive user messages and cause Anthropic to reject.
                        inner.state.pending_session_writes.clear();
                    }
                    self.emit(AgentHarnessEvent::Agent(event));
                }
                _ => {
                    self.emit(AgentHarnessEvent::Agent(event));
                }
            }
        }

        Ok(())
    }

    async fn do_compact(&self) -> Result<CompactionStats, HarnessError> {
        let path = self.session.read_active_path().await?;
        if path.is_empty() {
            return Err(CompactionError::InsufficientTokens.into());
        }

        // Find last compaction entry on path.
        let last_compaction = path.iter().rev().find_map(|e| {
            if let SessionEntryPayload::Compaction(c) = &e.payload {
                Some(c.clone())
            } else {
                None
            }
        });

        // Read config and assemble CompactionSettings BEFORE the hook so the
        // hook receives context_window / reserve_tokens / keep_recent_tokens.
        let (model_info, model, max_tokens, reserve_tokens, keep_recent_tokens) = {
            let inner = self.inner.lock().unwrap();
            (
                inner.state.model_info.clone(),
                inner.state.model.clone(),
                inner.max_tokens,
                inner.compaction_reserve_tokens,
                inner.compaction_keep_recent_tokens,
            )
        };

        let m_info = model_info.unwrap_or(ModelInfo {
            context_window: 200_000,
            max_tokens,
        });

        // Compaction uses the independent compaction config if set, otherwise
        // falls back to the main conversation client/model. The Option<struct>
        // makes half-configuration impossible at the type level.
        let (summary_client, summary_model, summary_model_info) = match &self.compaction_model {
            Some(c) => (c.client.clone(), c.model.clone(), c.model_info.clone()),
            None => (self.client.clone(), model.clone(), m_info.clone()),
        };

        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: reserve_tokens.unwrap_or(16_384) as usize,
            keep_recent_tokens: keep_recent_tokens.unwrap_or(20_000) as usize,
            summary_model,
            summary_model_info,
        };

        // before_compact hook is called BEFORE prepare_compaction so that
        // BeforeCompactDecision::Override can bypass the token-threshold check entirely.
        if !self.hooks.before_compact.is_empty() {
            let composite = Arc::new(CompositeBeforeCompactHook::new(
                self.hooks.before_compact.clone(),
            ));
            let built = self.session.build_context().await?;
            let estimated_tokens: usize = path
                .iter()
                .map(crate::compaction::estimate_tokens_for_entry)
                .sum();
            let decision = composite
                .before_compact(BeforeCompactCtx {
                    estimated_tokens,
                    messages: &built.messages,
                    context_window: m_info.context_window,
                    reserve_tokens: settings.reserve_tokens,
                    keep_recent_tokens: settings.keep_recent_tokens,
                })
                .await;
            match decision {
                BeforeCompactDecision::Skip => {
                    return Err(CompactionError::InsufficientTokens.into());
                }
                BeforeCompactDecision::Override(result) => {
                    return self.apply_compaction_result(result, vec![], &path).await;
                }
                BeforeCompactDecision::Proceed => {}
            }
        }

        let prep = prepare_compaction(&path, last_compaction.as_ref(), &settings, &m_info);
        let prep = match prep {
            Some(p) => p,
            None => return Err(CompactionError::InsufficientTokens.into()),
        };

        let estimated_tokens = prep.estimated_tokens;
        let file_operations = prep.file_operations.clone(); // clone so compact() still has access for summary
        self.emit(AgentHarnessEvent::CompactionStart { estimated_tokens });

        let result = compact(
            summary_client.as_ref(),
            prep,
            &settings,
            self.auth
                .as_ref()
                .map(|a| a.as_ref() as &(dyn AuthHook + Send + Sync)),
        )
        .await?;
        self.apply_compaction_result(result, file_operations, &path)
            .await
    }

    async fn apply_compaction_result(
        &self,
        result: CompactionResult,
        file_operations: Vec<FileOperation>,
        path: &[SessionEntry],
    ) -> Result<CompactionStats, HarnessError> {
        if !path.iter().any(|e| e.id == result.first_kept_entry) {
            return Err(CompactionError::InvalidFirstKeptEntry(result.first_kept_entry).into());
        }

        let tokens_before = result.tokens_before;
        let tokens_after = result.tokens_after;
        let compressed_entries = path
            .iter()
            .position(|e| e.id == result.first_kept_entry)
            .expect("first_kept_entry validated before apply_compaction_result");

        let details = if file_operations.is_empty() {
            None
        } else {
            serde_json::to_value(&file_operations).ok()
        };

        let entry = CompactionEntry {
            summary_message: result.summary_message,
            first_kept_entry: result.first_kept_entry,
            tokens_before,
            from_hook: false,
            details,
        };
        self.session
            .append(SessionEntryPayload::Compaction(entry))
            .await?;

        Ok(CompactionStats {
            tokens_before,
            tokens_after,
            compressed_entries,
        })
    }
}

// ── PhaseGuard ────────────────────────────────────────────────────────────────

/// RAII guard：进入非 Idle phase，离开作用域时自动恢复 Idle。
///
/// 构造时在一次锁内完成"检查当前为 Idle → 设为目标 phase"，
/// 消除 check-then-set 的 TOCTOU 窗口。失败返回 `NotIdle`。
/// Drop 时无条件恢复为 Idle 并发射 `PhaseChange` 事件。
struct PhaseGuard<'a> {
    harness: &'a AgentHarness,
}

impl std::fmt::Debug for PhaseGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhaseGuard").finish_non_exhaustive()
    }
}

impl Drop for PhaseGuard<'_> {
    fn drop(&mut self) {
        self.harness.set_phase(HarnessPhase::Idle);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[cfg(feature = "test-utils")]
mod tests {
    use super::*;
    use crate::session::storage::SessionStorage;
    use crate::session::types::{SessionEntry, SessionEntryKind, SessionMetadata};
    use futures::future::BoxFuture;
    use llm_harness_loop::test_utils::{MockLlmClient, MockResponse, NoOpEnv};

    /// Test helper: wraps any SessionStorage and fails `append_entry` after `fail_after` successes.
    struct FailAfterNStorage {
        inner: Arc<dyn SessionStorage>,
        fail_after: usize,
        call_count: std::sync::Mutex<usize>,
    }

    impl FailAfterNStorage {
        fn new(inner: Arc<dyn SessionStorage>, fail_after: usize) -> Self {
            Self {
                inner,
                fail_after,
                call_count: std::sync::Mutex::new(0),
            }
        }
    }

    impl SessionStorage for FailAfterNStorage {
        fn append_entry(
            &self,
            entry: SessionEntry,
        ) -> BoxFuture<'_, Result<(), llm_harness_types::SessionError>> {
            Box::pin(async move {
                {
                    let mut n = self.call_count.lock().unwrap();
                    if *n >= self.fail_after {
                        return Err(llm_harness_types::SessionError::ConcurrentModification);
                    }
                    *n += 1;
                } // lock dropped here before await
                self.inner.append_entry(entry).await
            })
        }
        fn metadata(
            &self,
        ) -> BoxFuture<'_, Result<SessionMetadata, llm_harness_types::SessionError>> {
            self.inner.metadata()
        }
        fn create_entry_id(&self) -> llm_harness_types::EntryId {
            self.inner.create_entry_id()
        }
        fn get_entry(
            &self,
            id: llm_harness_types::EntryId,
        ) -> BoxFuture<'_, Result<Option<SessionEntry>, llm_harness_types::SessionError>> {
            self.inner.get_entry(id)
        }
        fn children(
            &self,
            parent: llm_harness_types::EntryId,
        ) -> BoxFuture<'_, Result<Vec<SessionEntry>, llm_harness_types::SessionError>> {
            self.inner.children(parent)
        }
        fn all_leaves(
            &self,
        ) -> BoxFuture<'_, Result<Vec<llm_harness_types::EntryId>, llm_harness_types::SessionError>>
        {
            self.inner.all_leaves()
        }
        fn active_cursor(
            &self,
        ) -> BoxFuture<
            '_,
            Result<Option<llm_harness_types::EntryId>, llm_harness_types::SessionError>,
        > {
            self.inner.active_cursor()
        }
        fn set_active_cursor(
            &self,
            id: llm_harness_types::EntryId,
        ) -> BoxFuture<'_, Result<(), llm_harness_types::SessionError>> {
            self.inner.set_active_cursor(id)
        }
        fn path_to_root(
            &self,
            target: llm_harness_types::EntryId,
        ) -> BoxFuture<'_, Result<Vec<SessionEntry>, llm_harness_types::SessionError>> {
            self.inner.path_to_root(target)
        }
        fn common_ancestor(
            &self,
            a: llm_harness_types::EntryId,
            b: llm_harness_types::EntryId,
        ) -> BoxFuture<
            '_,
            Result<Option<llm_harness_types::EntryId>, llm_harness_types::SessionError>,
        > {
            self.inner.common_ancestor(a, b)
        }
        fn label_at(
            &self,
            id: llm_harness_types::EntryId,
        ) -> BoxFuture<'_, Result<Option<String>, llm_harness_types::SessionError>> {
            self.inner.label_at(id)
        }
        fn paths_to_all_leaves(
            &self,
        ) -> BoxFuture<'_, Result<Vec<Vec<SessionEntry>>, llm_harness_types::SessionError>>
        {
            self.inner.paths_to_all_leaves()
        }
        fn find_entries_by_type(
            &self,
            kind: SessionEntryKind,
        ) -> BoxFuture<'_, Result<Vec<llm_harness_types::EntryId>, llm_harness_types::SessionError>>
        {
            self.inner.find_entries_by_type(kind)
        }
        fn update_metadata_name(
            &self,
            name: Option<String>,
        ) -> BoxFuture<'_, Result<(), llm_harness_types::SessionError>> {
            self.inner.update_metadata_name(name)
        }
        fn update_metadata_model(
            &self,
            model: Option<String>,
        ) -> BoxFuture<'_, Result<(), llm_harness_types::SessionError>> {
            self.inner.update_metadata_model(model)
        }
        fn delete_entries(
            &self,
            ids: Vec<llm_harness_types::EntryId>,
        ) -> BoxFuture<'_, Result<(), llm_harness_types::SessionError>> {
            self.inner.delete_entries(ids)
        }
        fn all_entries(
            &self,
        ) -> BoxFuture<'_, Result<Vec<SessionEntry>, llm_harness_types::SessionError>> {
            self.inner.all_entries()
        }
    }

    fn make_harness_failing_storage(
        fail_after: usize,
        responses: Vec<MockResponse>,
    ) -> AgentHarness {
        let repo = crate::session::repo::InMemorySessionRepo::new();
        let storage = futures::executor::block_on(
            repo.create(crate::session::types::CreateSessionOptions::default()),
        )
        .unwrap();
        let failing = Arc::new(FailAfterNStorage::new(storage, fail_after));
        let session = crate::session::session::Session::new(failing as Arc<dyn SessionStorage>);
        let client = Arc::new(MockLlmClient::new(responses));
        let env = Arc::new(NoOpEnv);
        AgentHarness::with_session(
            client as Arc<dyn LlmClient>,
            env,
            session,
            AgentHarnessOptions::new("test-model"),
        )
    }

    fn make_harness(responses: Vec<MockResponse>) -> AgentHarness {
        let client = Arc::new(MockLlmClient::new(responses));
        let env = Arc::new(NoOpEnv);
        let opts = AgentHarnessOptions::new("test-model");
        futures::executor::block_on(AgentHarness::new_in_memory(
            client as Arc<dyn LlmClient>,
            env,
            opts,
        ))
    }

    #[tokio::test]
    async fn enter_phase_rejects_when_not_idle() {
        let h = make_harness(vec![]);
        {
            let mut inner = h.inner.lock().unwrap();
            inner.state.phase = HarnessPhase::Turning;
        }
        let err = h.enter_phase(HarnessPhase::Branching).unwrap_err();
        assert!(
            matches!(err, HarnessError::NotIdle(HarnessPhase::Turning)),
            "expected NotIdle(Turning), got {err:?}"
        );
        // phase 不变
        assert_eq!(h.state().phase, HarnessPhase::Turning);
        h.inner.lock().unwrap().state.phase = HarnessPhase::Idle;
    }

    #[tokio::test]
    async fn guard_drop_restores_idle() {
        let h = make_harness(vec![]);
        {
            let _guard = h.enter_phase(HarnessPhase::Branching).unwrap();
            assert_eq!(h.state().phase, HarnessPhase::Branching);
        }
        // guard drop 后恢复 Idle
        assert_eq!(h.state().phase, HarnessPhase::Idle);
    }
    #[tokio::test]
    async fn enter_phase_is_atomic() {
        let h = Arc::new(make_harness(vec![]));
        let h1 = h.clone();
        let h2 = h.clone();
        // 两个并发 task 同时 enter_phase，只有一个应成功
        let (tx, rx) = tokio::sync::oneshot::channel();
        let jh1 = tokio::spawn(async move {
            let r1 = h1.enter_phase(HarnessPhase::Branching);
            let ok = r1.is_ok();
            if let Ok(_g) = &r1 {
                // 持有 guard 堵住第二个调用方
                tx.send(()).unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            // guard 在 _g 离开作用域时 drop，归还借用
            drop(r1);
            ok
        });
        // 等 jh1 拿到 guard 后再启动 jh2
        rx.await.unwrap();
        let r2 = h2.enter_phase(HarnessPhase::Branching);
        let ok1 = jh1.await.unwrap();
        let ok2 = r2.is_ok();
        assert!(
            ok1 ^ ok2,
            "exactly one enter_phase must succeed; got ok1={ok1} ok2={ok2}"
        );
        if !ok2 {
            assert!(matches!(r2, Err(HarnessError::NotIdle(_))));
        }
    }

    #[tokio::test]
    async fn flush_error_returns_phase_to_idle() {
        // Storage fails immediately on append_entry (fail_after=0)
        let h = make_harness_failing_storage(0, vec![MockResponse::text("hi")]);

        let result = h.prompt("hello").await;

        // Bug: phase stays Turning. Fix: phase must return to Idle.
        assert_eq!(
            h.state().phase,
            HarnessPhase::Idle,
            "phase must be Idle after flush error"
        );
        assert!(result.is_err(), "prompt must return Err when storage fails");
    }

    #[tokio::test]
    async fn compact_sets_compacting_phase() {
        // do_compact 会因 InsufficientTokens 失败，但 phase 在失败路径上应仍是
        // Compacting 直到 guard drop。此测试用并发 task 观察 phase。
        // 简化：直接调 enter_phase(Compacting) 验证 drop 恢复——已在 guard_drop_restores_idle 覆盖。
        // 这里测 compact() 的 observable 行为：compact 失败后 phase 回 Idle。
        let h = make_harness(vec![]);
        let _ = h.compact().await; // InsufficientTokens
        assert_eq!(h.state().phase, HarnessPhase::Idle);
    }

    #[tokio::test]
    async fn prompt_completes_and_returns_idle() {
        let h = make_harness(vec![MockResponse::text("Hello!")]);
        h.prompt("hi").await.unwrap();
        assert_eq!(h.state().phase, HarnessPhase::Idle);
    }

    #[tokio::test]
    async fn prompt_persists_final_answer_identity() {
        let h = make_harness(vec![MockResponse::text("Hello!")]);
        h.prompt("hi").await.unwrap();

        let path = h.read_active_path().await.unwrap();
        let assistant = path
            .iter()
            .find_map(|entry| {
                if let SessionEntryPayload::Message(AgentMessage::Assistant(message)) =
                    &entry.payload
                {
                    Some(message)
                } else {
                    None
                }
            })
            .expect("assistant message should be persisted");

        assert_eq!(assistant.kind, AssistantMessageKind::FinalAnswer);
        assert_eq!(assistant.text_content(), "Hello!");
        assert!(!assistant.message_id.is_empty());
        assert!(!assistant.turn_id.is_empty());
    }

    #[tokio::test]
    async fn prompt_while_running_returns_not_idle() {
        let h = make_harness(vec![]);
        {
            let mut inner = h.inner.lock().unwrap();
            inner.state.phase = HarnessPhase::Turning;
        }
        let err = h.prompt("hi").await;
        assert!(matches!(err, Err(HarnessError::NotIdle(_))));
        h.inner.lock().unwrap().state.phase = HarnessPhase::Idle;
    }

    #[tokio::test]
    async fn append_message_while_turning_returns_not_idle() {
        let h = make_harness(vec![]);
        {
            let mut inner = h.inner.lock().unwrap();
            inner.state.phase = HarnessPhase::Turning;
        }

        let msg = AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "test".into(),
            }],
            timestamp: chrono::Utc::now(),
        });
        let err = h.append_message(msg).await;

        h.inner.lock().unwrap().state.phase = HarnessPhase::Idle;
        assert!(matches!(err, Err(HarnessError::NotIdle(_))));
    }

    #[tokio::test]
    async fn append_custom_entry_while_turning_returns_not_idle() {
        let h = make_harness(vec![]);
        {
            let mut inner = h.inner.lock().unwrap();
            inner.state.phase = HarnessPhase::Turning;
        }

        let err = h
            .append_custom_entry("test_type".into(), serde_json::json!({}))
            .await;

        h.inner.lock().unwrap().state.phase = HarnessPhase::Idle;
        assert!(matches!(err, Err(HarnessError::NotIdle(_))));
    }

    #[tokio::test]
    async fn session_has_messages_after_prompt() {
        let h = make_harness(vec![MockResponse::text("response")]);
        h.prompt("question").await.unwrap();
        let ctx = h.session.build_context().await.unwrap();
        // At least user message + assistant message.
        assert!(ctx.messages.len() >= 2);
    }

    #[tokio::test]
    async fn set_model_updates_state() {
        let h = make_harness(vec![]);
        h.set_model("claude-opus-4-7".into(), None).await.unwrap();
        assert_eq!(h.state().model, "claude-opus-4-7");
    }

    #[test]
    fn set_system_prompt_sets_value() {
        let h = make_harness(vec![]);
        h.set_system_prompt(Some("new prompt".into()));
        assert_eq!(h.state().system_prompt.as_deref(), Some("new prompt"));
    }

    #[test]
    fn set_system_prompt_clears_value() {
        let h = make_harness(vec![]);
        h.set_system_prompt(Some("initial".into()));
        h.set_system_prompt(None);
        assert_eq!(h.state().system_prompt, None);
    }

    #[test]
    fn set_max_tokens_updates_inner() {
        let h = make_harness(vec![]);
        h.set_max_tokens(2048);
        assert_eq!(h.inner.lock().unwrap().max_tokens, 2048);
    }

    #[test]
    fn set_max_tokens_persists_across_reads() {
        let h = make_harness(vec![]);
        h.set_max_tokens(4096);
        // Read twice — verifies the value sticks (not a one-shot injection).
        assert_eq!(h.inner.lock().unwrap().max_tokens, 4096);
        assert_eq!(h.inner.lock().unwrap().max_tokens, 4096);
    }

    #[tokio::test]
    async fn skill_not_found_returns_error() {
        let h = make_harness(vec![]);
        let err = h.skill("nonexistent", None).await;
        assert!(matches!(err, Err(HarnessError::SkillNotFound(_))));
    }

    #[tokio::test]
    async fn template_not_found_returns_error() {
        let h = make_harness(vec![]);
        let err = h.prompt_from_template("nonexistent", vec![]).await;
        assert!(matches!(
            err,
            Err(HarnessError::Template(TemplateError::NotFound(_)))
        ));
    }

    #[tokio::test]
    async fn subscribe_receives_events() {
        let h = make_harness(vec![MockResponse::text("hi")]);
        let mut rx = h.subscribe();
        h.prompt("hello").await.unwrap();
        assert!(rx.try_recv().is_ok());
    }

    /// A `ShouldStopHook` that coordinates with the test on its **first** call:
    /// signals `entered`, waits for `inject` before returning `true`. On
    /// subsequent calls it returns `true` immediately. This eliminates timing
    /// races — the follow_up is injected while should_stop is held, exactly
    /// how real hooks (`AsyncSpawnHook`, `PeerMessageHook`) behave.
    struct CoordinatingShouldStop {
        entered: Arc<tokio::sync::Notify>,
        inject: Arc<tokio::sync::Notify>,
        call_count: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ShouldStopHook for CoordinatingShouldStop {
        fn should_stop<'a>(
            &'a self,
            _ctx: ShouldStopCtx<'a>,
        ) -> futures::future::BoxFuture<'a, bool> {
            let entered = self.entered.clone();
            let inject = self.inject.clone();
            let count = self.call_count.clone();
            Box::pin(async move {
                if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    entered.notify_one();
                    inject.notified().await;
                }
                true
            })
        }
    }

    #[tokio::test]
    async fn follow_up_handle_injects_message_into_loop() {
        // The loop calls should_stop after the first assistant turn. Our
        // coordinating hook signals entry, then waits for the test to inject
        // a follow_up via FollowUpHandle before returning true. The loop then
        // checks follow_up_rx, finds the message, and continues 'main —
        // consuming the second mock response.
        let entered = Arc::new(tokio::sync::Notify::new());
        let inject = Arc::new(tokio::sync::Notify::new());
        let hook = Arc::new(CoordinatingShouldStop {
            entered: entered.clone(),
            inject: inject.clone(),
            call_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });

        let client = Arc::new(MockLlmClient::new(vec![
            MockResponse::text("first"),
            MockResponse::text("saw follow-up"),
        ]));
        let env = Arc::new(NoOpEnv);
        let opts = AgentHarnessOptions {
            hooks: HarnessHooks {
                should_stop: vec![hook],
                ..HarnessHooks::none()
            },
            ..AgentHarnessOptions::new("test-model")
        };
        let h =
            Arc::new(AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, env, opts).await);

        let handle = h.follow_up_handle();
        let prompt_task = tokio::spawn({
            let h = h.clone();
            async move { h.prompt("say first").await }
        });

        // Wait until the loop enters should_stop, then inject via the handle.
        entered.notified().await;
        handle.follow_up_message(AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "please say 'saw follow-up'".into(),
            }],
            timestamp: chrono::Utc::now(),
        }));
        inject.notify_one();

        prompt_task.await.unwrap().unwrap();

        // The follow_up must have triggered a second turn consuming the second
        // mock response — so the session now holds two assistant messages.
        let ctx = h.session.build_context().await.unwrap();
        let assistant_count = ctx
            .messages
            .iter()
            .filter(|m| matches!(m, AgentMessage::Assistant(_)))
            .count();
        assert_eq!(
            assistant_count, 2,
            "follow_up should trigger a second turn consuming the second mock response"
        );
    }

    #[tokio::test]
    async fn wait_for_idle_returns_immediately_when_idle() {
        let h = make_harness(vec![]);
        tokio::time::timeout(std::time::Duration::from_millis(100), h.wait_for_idle())
            .await
            .expect("wait_for_idle timed out when already idle");
    }

    #[tokio::test]
    async fn abort_cancels_token() {
        let h = make_harness(vec![]);
        let token = CancellationToken::new();
        h.inner.lock().unwrap().current_abort = Some(token.clone());
        h.abort();
        assert!(token.is_cancelled());
    }
    #[tokio::test]
    async fn abort_invokes_cascade_hook() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountingHook(Arc<AtomicUsize>);
        impl AbortCascadeHook for CountingHook {
            fn cancel_all_subagents(&self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let h = make_harness(vec![]);
        let counter = Arc::new(AtomicUsize::new(0));
        h.set_abort_cascade_hook(Arc::new(CountingHook(counter.clone())));
        h.abort();
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "abort should call cascade hook once"
        );
    }
    #[tokio::test]
    async fn abort_invokes_cascade_hook_twice_after_reabort() {
        // 回归测试：abort_cascade_hook 是生命周期持久配置（plan: "abort 不终止
        // agent 生命周期，可再被 prompt"）。take() 会消耗 hook 导致第二次 abort
        // 静默跳过级联；必须用 clone() 保留。
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountingHook(Arc<AtomicUsize>);
        impl AbortCascadeHook for CountingHook {
            fn cancel_all_subagents(&self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let h = make_harness(vec![]);
        let counter = Arc::new(AtomicUsize::new(0));
        h.set_abort_cascade_hook(Arc::new(CountingHook(counter.clone())));
        // 模拟一次运行中的 abort。
        let token = CancellationToken::new();
        h.inner.lock().unwrap().current_abort = Some(token.clone());
        h.abort();
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "first abort should call cascade hook"
        );
        // re-prompt 后再次 abort（plan 支持的场景），hook 应仍被调用。
        let token2 = CancellationToken::new();
        h.inner.lock().unwrap().current_abort = Some(token2.clone());
        h.abort();
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "second abort should STILL call cascade hook (hook is lifetime-persistent)"
        );
    }
    #[tokio::test]
    async fn abort_cancels_idle_watcher() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountingWatcher(Arc<AtomicUsize>);
        impl IdleWatcherHook for CountingWatcher {
            fn on_idle(&self) {}
            fn cancel(&self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let h = make_harness(vec![]);
        let counter = Arc::new(AtomicUsize::new(0));
        h.set_idle_watcher(Arc::new(CountingWatcher(counter.clone())));
        h.abort();
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "abort should cancel idle watcher"
        );
    }

    #[tokio::test]
    async fn next_turn_injected_on_next_prompt() {
        let h = make_harness(vec![
            MockResponse::text("first"),
            MockResponse::text("second"),
        ]);
        h.prompt("hello").await.unwrap();
        h.next_turn("injected context");
        h.prompt("next question").await.unwrap();
        let ctx = h.session.build_context().await.unwrap();
        // Should have messages from both runs.
        assert!(ctx.messages.len() >= 4);
    }

    #[tokio::test]
    async fn set_active_tools_filters_to_subset() {
        let h = make_harness(vec![]);
        let active: HashSet<String> = vec!["grep".to_string()].into_iter().collect();
        h.set_active_tools(Some(active.clone())).await.unwrap();
        assert_eq!(h.state().active_tools, Some(active));
    }

    #[tokio::test]
    async fn clear_all_queues_empties_next_turn() {
        let h = make_harness(vec![]);
        h.next_turn("msg");
        h.clear_all_queues().unwrap();
        assert!(!h.has_queued_messages());
    }

    #[tokio::test]
    async fn clear_queues_rejected_when_not_idle() {
        let h = make_harness(vec![]);
        // 强制进入非 Idle 阶段
        h.inner.lock().unwrap().state.phase = HarnessPhase::Turning;
        assert!(matches!(
            h.clear_steering_queue(),
            Err(HarnessError::NotIdle(HarnessPhase::Turning))
        ));
        assert!(matches!(
            h.clear_follow_up_queue(),
            Err(HarnessError::NotIdle(HarnessPhase::Turning))
        ));
        assert!(matches!(
            h.clear_all_queues(),
            Err(HarnessError::NotIdle(HarnessPhase::Turning))
        ));
        // 恢复 Idle 供后续清理
        h.inner.lock().unwrap().state.phase = HarnessPhase::Idle;
    }

    struct DummyTool {
        name: String,
        schema: serde_json::Value,
    }
    impl DummyTool {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_owned(),
                schema: serde_json::json!({"type":"object","properties":{}}),
            }
        }
    }
    impl llm_harness_types::Tool for DummyTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "test tool"
        }
        fn parameters_schema(&self) -> &serde_json::Value {
            &self.schema
        }
        fn execute<'a>(
            &'a self,
            _args: serde_json::Value,
            _ctx: &'a llm_harness_types::ToolContext,
        ) -> futures::future::BoxFuture<
            'a,
            Result<llm_harness_types::ToolResult, llm_harness_types::ToolError>,
        > {
            Box::pin(async {
                Ok(llm_harness_types::ToolResult {
                    content: vec![llm_harness_types::ContentBlock::Text {
                        text: "ok".to_string(),
                    }],
                    details: serde_json::Value::Null,
                    terminate: false,
                })
            })
        }
    }

    #[tokio::test]
    async fn tool_call_end_event_carries_tool_name() {
        use std::sync::Mutex as StdMutex;

        let h = make_harness(vec![
            MockResponse::tool_use("id-1", "my_tool", "{}"),
            MockResponse::text("done"),
        ]);

        let tool: Arc<dyn llm_harness_types::Tool> = Arc::new(DummyTool::new("my_tool"));
        h.set_tools(vec![tool]).await.unwrap();

        let received_name: Arc<StdMutex<Option<String>>> = Arc::new(StdMutex::new(None));
        let received_name_clone = received_name.clone();

        let mut rx = h.subscribe();
        let handle = tokio::spawn(async move {
            while let Ok(event) = rx.recv().await {
                if let AgentHarnessEvent::ToolCallEnd { tool_name, .. } = event.as_ref() {
                    *received_name_clone.lock().unwrap() = Some(tool_name.clone());
                    break;
                }
            }
        });

        h.prompt("use tool").await.unwrap();
        handle.await.unwrap();

        assert_eq!(
            received_name.lock().unwrap().as_deref(),
            Some("my_tool"),
            "ToolCallEnd must carry the correct tool_name"
        );
    }

    #[tokio::test]
    async fn compact_override_with_invalid_first_kept_entry_returns_error() {
        use futures::future::BoxFuture;
        use llm_harness_types::{
            BeforeCompactCtx, BeforeCompactDecision, BeforeCompactHook, CompactionResult, EntryId,
        };

        struct BadOverrideHook;

        impl BeforeCompactHook for BadOverrideHook {
            fn before_compact<'a>(
                &'a self,
                _ctx: BeforeCompactCtx<'a>,
            ) -> BoxFuture<'a, BeforeCompactDecision> {
                Box::pin(async move {
                    BeforeCompactDecision::Override(CompactionResult {
                        summary_message: AgentMessage::User(UserMessage {
                            content: vec![ContentBlock::Text {
                                text: "summary".into(),
                            }],
                            timestamp: chrono::Utc::now(),
                        }),
                        first_kept_entry: EntryId::new(),
                        tokens_before: 100,
                        tokens_after: 10,
                    })
                })
            }
        }

        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("hello")]));
        let env = Arc::new(NoOpEnv);
        let mut opts = AgentHarnessOptions::new("test-model");
        opts.hooks.before_compact = vec![Arc::new(BadOverrideHook)];
        let h = futures::executor::block_on(AgentHarness::new_in_memory(client, env, opts));

        h.prompt("hello").await.unwrap();

        let err = h.compact().await;
        assert!(
            matches!(
                err,
                Err(HarnessError::Compaction(
                    CompactionError::InvalidFirstKeptEntry(_)
                ))
            ),
            "compact() must return InvalidFirstKeptEntry for a nonexistent entry: {err:?}"
        );
    }
    #[tokio::test]
    async fn continue_run_returns_not_idle_when_turning() {
        let h = make_harness(vec![]);
        {
            let mut inner = h.inner.lock().unwrap();
            inner.state.phase = HarnessPhase::Turning;
        }
        let err = h.continue_run().await.unwrap_err();
        assert!(matches!(err, HarnessError::NotIdle(_)));
    }

    #[tokio::test]
    async fn continue_run_drives_loop_with_empty_initial() {
        let h = make_harness(vec![
            MockResponse::text("first"),
            MockResponse::text("handled follow-up"),
        ]);
        h.prompt("initial").await.unwrap(); // 消费 "first"，phase 回到 Idle
        // 注入 follow_up
        h.follow_up_message(AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "please respond".into(),
            }],
            timestamp: chrono::Utc::now(),
        }));
        h.continue_run().await.unwrap(); // 应消费 "handled follow-up"
        let ctx = h.session.build_context().await.unwrap();
        let assistant_count = ctx
            .messages
            .iter()
            .filter(|m| matches!(m, AgentMessage::Assistant(_)))
            .count();
        assert_eq!(assistant_count, 2);
    }

    /// Verifies that when `compaction_model` is configured, `do_compact`
    /// routes the summary request through the independent compaction client
    /// (not the main conversation client). The compaction client's response
    /// ("compacted summary") should land in the session as a CompactionSummary.
    #[tokio::test]
    async fn do_compact_uses_independent_compaction_client() {
        // Main client: two short text responses for two prompt() calls.
        let main_client = Arc::new(MockLlmClient::new(vec![
            MockResponse::text("response1"),
            MockResponse::text("response2"),
        ]));
        // Compaction client: returns the summary text.
        let compaction_client = Arc::new(MockLlmClient::new(vec![MockResponse::text(
            "compacted summary",
        )]));

        let env = Arc::new(NoOpEnv);
        let mut opts = AgentHarnessOptions::new("test-model");
        // Tiny context window + zero reserve so compaction triggers easily.
        opts.model_info = Some(ModelInfo {
            context_window: 1,
            max_tokens: 50,
        });
        opts.compaction_reserve_tokens = Some(0);
        opts.compaction_keep_recent_tokens = Some(0);
        opts.compaction_model = Some(CompactionModelConfig {
            client: compaction_client as Arc<dyn LlmClient>,
            model: "compaction-model".into(),
            model_info: ModelInfo {
                context_window: 200_000,
                max_tokens: 8192,
            },
        });
        let h =
            AgentHarness::new_in_memory(main_client.clone() as Arc<dyn LlmClient>, env, opts).await;

        // Two prompts → 4 entries (user, assistant, user, assistant).
        // Each ~1 token (char-based estimate), total ~4 > threshold 3.
        h.prompt("hi").await.unwrap();
        h.prompt("yo").await.unwrap();

        // Trigger compaction. Should use compaction_client, not main_client.
        h.compact().await.unwrap();
        // Main client should only receive the two prompt() calls, NOT the
        // compaction summary request — that must route through compaction_client.
        assert_eq!(
            main_client
                .call_count
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "main client must not receive compaction request"
        );

        // The compaction summary from compaction_client should be in the session.
        let path = h.session.read_active_path().await.unwrap();
        let has_summary = path.iter().any(|e| {
            if let SessionEntryPayload::Compaction(c) = &e.payload {
                matches!(
                    &c.summary_message,
                    AgentMessage::CompactionSummary(cs) if cs.summary.contains("compacted summary")
                )
            } else {
                false
            }
        });
        assert!(
            has_summary,
            "compaction summary from independent client not found in session"
        );
    }
    /// `set_tools` called mid-run (via a `before_turn` hook) must survive the
    /// cleanup that unwraps `HookedTool`. The new tool must still be present
    /// after the run ends.
    #[tokio::test]
    async fn set_tools_mid_run_survives_cleanup() {
        use std::sync::OnceLock;

        // A before_tool_call hook is registered so `build_loop_config` wraps
        // tools with HookedTool — this is the path whose cleanup previously
        // rolled back mid-run set_tools.
        struct NoOpBeforeToolCall;
        impl BeforeToolCallHook for NoOpBeforeToolCall {
            fn on_call<'a>(
                &'a self,
                _ctx: BeforeToolCallCtx<'a>,
            ) -> futures::future::BoxFuture<'a, BeforeToolCallDecision> {
                Box::pin(async { BeforeToolCallDecision::Allow })
            }
        }
        struct SetToolsOnTurn {
            harness: Arc<OnceLock<Arc<AgentHarness>>>,
            done: Arc<std::sync::atomic::AtomicBool>,
        }
        impl BeforeTurnHook for SetToolsOnTurn {
            fn before_turn<'a>(
                &'a self,
                _ctx: BeforeTurnCtx<'a>,
            ) -> futures::future::BoxFuture<'a, ()> {
                let harness_cell = self.harness.clone();
                let done = self.done.clone();
                Box::pin(async move {
                    if done.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    let h = harness_cell.get().unwrap().clone();
                    let new_tool: Arc<dyn llm_harness_types::Tool> =
                        Arc::new(DummyTool::new("mid_run_tool"));
                    h.set_tools(vec![new_tool]).await.unwrap();
                })
            }
        }

        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
        let env = Arc::new(NoOpEnv);
        let harness_cell: Arc<OnceLock<Arc<AgentHarness>>> = Arc::new(OnceLock::new());
        let opts = AgentHarnessOptions {
            hooks: HarnessHooks {
                before_turn: vec![Arc::new(SetToolsOnTurn {
                    harness: harness_cell.clone(),
                    done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                })],
                // A no-op before_tool_call hook forces HookedTool wrapping.
                before_tool_call: vec![Arc::new(NoOpBeforeToolCall)],
                ..HarnessHooks::none()
            },
            ..AgentHarnessOptions::new("test-model")
        };
        let h =
            Arc::new(AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, env, opts).await);
        harness_cell.get_or_init(|| h.clone());

        h.prompt("hi").await.unwrap();

        let names: Vec<String> = h
            .state()
            .tools
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n == "mid_run_tool"),
            "mid-run set_tools must survive cleanup; tools = {names:?}"
        );
        assert!(
            !h.state().tools_overwritten_mid_run,
            "flag must be reset after cleanup"
        );
    }

    /// Without mid-run `set_tools`, cleanup must still unwrap HookedTool so the
    /// next `build_loop_config` does not double-wrap. Two consecutive prompts
    /// with a before_tool_call hook must succeed.
    #[tokio::test]
    async fn cleanup_unwraps_hooked_tool_without_mid_run_set() {
        struct NoOpBeforeToolCall;
        impl BeforeToolCallHook for NoOpBeforeToolCall {
            fn on_call<'a>(
                &'a self,
                _ctx: BeforeToolCallCtx<'a>,
            ) -> futures::future::BoxFuture<'a, BeforeToolCallDecision> {
                Box::pin(async { BeforeToolCallDecision::Allow })
            }
        }

        let client = Arc::new(MockLlmClient::new(vec![
            MockResponse::text("first"),
            MockResponse::text("second"),
        ]));
        let env = Arc::new(NoOpEnv);
        let opts = AgentHarnessOptions {
            hooks: HarnessHooks {
                before_tool_call: vec![Arc::new(NoOpBeforeToolCall)],
                ..HarnessHooks::none()
            },
            ..AgentHarnessOptions::new("test-model")
        };
        let h = AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, env, opts).await;

        // Two prompts: the second `build_loop_config` would double-wrap if
        // cleanup failed to unwrap. No panic / no error => pass.
        h.prompt("one").await.unwrap();
        h.prompt("two").await.unwrap();
        assert!(
            !h.state().tools_overwritten_mid_run,
            "flag must remain false without mid-run set_tools"
        );
    }

    /// `set_tools` in Idle phase must not set the mid-run flag (it takes the
    /// direct `session.append` path, not the pending path).
    #[tokio::test]
    async fn set_tools_in_idle_does_not_set_flag() {
        let h = make_harness(vec![]);
        let tool: Arc<dyn llm_harness_types::Tool> = Arc::new(DummyTool::new("idle_tool"));
        h.set_tools(vec![tool]).await.unwrap();
        assert!(
            !h.state().tools_overwritten_mid_run,
            "Idle set_tools must not set the mid-run flag"
        );
    }

    /// On an LLM error (`AgentEvent::Error`), the run ends but the mid-run
    /// `set_tools` change is NOT rolled back — LLM errors are part of the run,
    /// not a run-level failure. Only storage/session errors (which make
    /// `drive_loop` return `Err`) trigger rollback.
    #[tokio::test]
    async fn set_tools_mid_run_preserved_on_llm_error() {
        use std::sync::OnceLock;

        struct SetToolsOnTurn {
            harness: Arc<OnceLock<Arc<AgentHarness>>>,
            done: Arc<std::sync::atomic::AtomicBool>,
        }
        impl BeforeTurnHook for SetToolsOnTurn {
            fn before_turn<'a>(
                &'a self,
                _ctx: BeforeTurnCtx<'a>,
            ) -> futures::future::BoxFuture<'a, ()> {
                let harness_cell = self.harness.clone();
                let done = self.done.clone();
                Box::pin(async move {
                    if done.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    let h = harness_cell.get().unwrap().clone();
                    let new_tool: Arc<dyn llm_harness_types::Tool> =
                        Arc::new(DummyTool::new("error_run_tool"));
                    h.set_tools(vec![new_tool]).await.unwrap();
                })
            }
        }

        // First response errors; before_turn fires first and calls set_tools.
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::rate_limit_error()]));
        let env = Arc::new(NoOpEnv);
        let harness_cell: Arc<OnceLock<Arc<AgentHarness>>> = Arc::new(OnceLock::new());
        let opts = AgentHarnessOptions {
            hooks: HarnessHooks {
                before_turn: vec![Arc::new(SetToolsOnTurn {
                    harness: harness_cell.clone(),
                    done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                })],
                ..HarnessHooks::none()
            },
            ..AgentHarnessOptions::new("test-model")
        };
        let h =
            Arc::new(AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, env, opts).await);
        harness_cell.get_or_init(|| h.clone());

        // The run errors; prompt returns Ok (LLM error is an event, not a Result).
        h.prompt("hi").await.unwrap();

        let names: Vec<String> = h
            .state()
            .tools
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n == "error_run_tool"),
            "mid-run set_tools must survive LLM error; tools = {names:?}"
        );
        assert!(
            !h.state().tools_overwritten_mid_run,
            "flag must be reset after cleanup"
        );
    }

    /// On a storage/session error (`drive_loop` returns `Err`), the run is
    /// discarded: mid-run `set_tools` must be rolled back to `original_tools`
    /// and the flag reset. This is the counterpart to
    /// `set_tools_mid_run_preserved_on_llm_error` — LLM errors are events
    /// (preserve), storage errors are `Result::Err` (rollback).
    #[tokio::test]
    async fn set_tools_mid_run_rolled_back_on_storage_error() {
        use std::sync::OnceLock;

        struct SetToolsOnTurn {
            harness: Arc<OnceLock<Arc<AgentHarness>>>,
            done: Arc<std::sync::atomic::AtomicBool>,
        }
        impl BeforeTurnHook for SetToolsOnTurn {
            fn before_turn<'a>(
                &'a self,
                _ctx: BeforeTurnCtx<'a>,
            ) -> futures::future::BoxFuture<'a, ()> {
                let harness_cell = self.harness.clone();
                let done = self.done.clone();
                Box::pin(async move {
                    if done.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    let h = harness_cell.get().unwrap().clone();
                    let new_tool: Arc<dyn llm_harness_types::Tool> =
                        Arc::new(DummyTool::new("storage_err_tool"));
                    h.set_tools(vec![new_tool]).await.unwrap();
                })
            }
        }

        // fail_after=0: the first flush_pending_writes at TurnEnd fails,
        // making drive_loop return Err — the storage-error path.
        let repo = crate::session::repo::InMemorySessionRepo::new();
        let storage = futures::executor::block_on(
            repo.create(crate::session::types::CreateSessionOptions::default()),
        )
        .unwrap();
        let failing = Arc::new(FailAfterNStorage::new(storage, 0));
        let session = crate::session::session::Session::new(failing as Arc<dyn SessionStorage>);
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
        let env = Arc::new(NoOpEnv);
        let harness_cell: Arc<OnceLock<Arc<AgentHarness>>> = Arc::new(OnceLock::new());
        let opts = AgentHarnessOptions {
            hooks: HarnessHooks {
                before_turn: vec![Arc::new(SetToolsOnTurn {
                    harness: harness_cell.clone(),
                    done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                })],
                ..HarnessHooks::none()
            },
            ..AgentHarnessOptions::new("test-model")
        };
        let h = Arc::new(AgentHarness::with_session(
            client as Arc<dyn LlmClient>,
            env,
            session,
            opts,
        ));
        harness_cell.get_or_init(|| h.clone());

        // Storage error makes prompt return Err.
        let result = h.prompt("hi").await;
        assert!(result.is_err(), "prompt must return Err when storage fails");

        let names: Vec<String> = h
            .state()
            .tools
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(
            !names.iter().any(|n| n == "storage_err_tool"),
            "mid-run set_tools must be rolled back on storage error; tools = {names:?}"
        );
        assert!(
            !h.state().tools_overwritten_mid_run,
            "flag must be reset after storage-error rollback"
        );
    }

    /// `set_compaction_reserve_tokens` / `set_compaction_keep_recent_tokens`
    /// must take effect at the next `compact()` call: with a large context
    /// window and default reserve, compaction does not trigger; raising
    /// reserve_tokens via the setter lowers the threshold so the same
    /// session becomes compactable.
    #[tokio::test]
    async fn set_compaction_params_enables_early_compact() {
        // Two prompt responses + one compaction summary.
        let client = Arc::new(MockLlmClient::new(vec![
            MockResponse::text("response1"),
            MockResponse::text("response2"),
            MockResponse::text("compacted summary"),
        ]));
        let env = Arc::new(NoOpEnv);
        let mut opts = AgentHarnessOptions::new("test-model");
        // Large context window + default reserve → threshold too high to trigger.
        // estimated_tokens for "hi"+"response1"+"yo"+"response2" ≈ 2; default
        // threshold = 200_000 - 16_384 = 183_616 ≫ 2.
        opts.model_info = Some(ModelInfo {
            context_window: 200_000,
            max_tokens: 8192,
        });
        let h = AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, env, opts).await;

        // Two prompts → 4 entries (user, assistant, user, assistant).
        h.prompt("hi").await.unwrap();
        h.prompt("yo").await.unwrap();

        // Default threshold: compaction must NOT trigger.
        let err = h.compact().await.unwrap_err();
        assert!(
            matches!(
                err,
                HarnessError::Compaction(CompactionError::InsufficientTokens)
            ),
            "expected InsufficientTokens with default threshold, got: {err:?}"
        );

        // Raise reserve_tokens so threshold drops to 0. estimated_tokens = 1
        // (user "hi" = 1 token; assistant usage.output_tokens = 0 via MockResponse::text).
        // threshold = 200_000 - 200_000 = 0 < 1 → triggers.
        h.set_compaction_reserve_tokens(Some(200_000));
        // keep_recent_tokens = 0 so the cut-point scan can find a boundary.
        h.set_compaction_keep_recent_tokens(Some(0));

        let stats = h
            .compact()
            .await
            .expect("compaction must trigger after lowering threshold");
        assert!(
            stats.compressed_entries > 0,
            "expected at least one entry compressed, got stats: {stats:?}"
        );
    }
    // ── Branch ops: Branching phase guard ────────────────────────────────────

    #[tokio::test]
    async fn branch_ops_reject_during_turning() {
        let h = make_harness(vec![]);
        {
            let mut inner = h.inner.lock().unwrap();
            inner.state.phase = HarnessPhase::Turning;
        }

        // fork_branch
        let err = h.fork_branch(EntryId::new(), None).await;
        assert!(matches!(
            err,
            Err(HarnessError::NotIdle(HarnessPhase::Turning))
        ));

        // navigate_tree
        let err = h.navigate_tree(EntryId::new()).await;
        assert!(matches!(
            err,
            Err(HarnessError::NotIdle(HarnessPhase::Turning))
        ));

        // delete_branch
        let err = h.delete_branch(EntryId::new()).await;
        assert!(matches!(
            err,
            Err(HarnessError::NotIdle(HarnessPhase::Turning))
        ));

        // generate_branch_summary
        let err = h.generate_branch_summary(EntryId::new()).await;
        assert!(matches!(
            err,
            Err(HarnessError::NotIdle(HarnessPhase::Turning))
        ));

        h.inner.lock().unwrap().state.phase = HarnessPhase::Idle;
    }

    #[tokio::test]
    async fn branch_ops_restore_idle_after_completion() {
        let h = make_harness(vec![]);

        // fork_branch: failure path (empty session → entry not found) still
        // must restore Idle via PhaseGuard::drop.
        let entries = h.read_active_path().await.unwrap();
        let from = entries.last().map(|e| e.id).unwrap_or(EntryId::new());
        let _ = h.fork_branch(from, Some("test".into())).await;
        assert_eq!(h.state().phase, HarnessPhase::Idle);

        // navigate_tree: same target id; phase must return to Idle.
        let _ = h.navigate_tree(from).await;
        assert_eq!(h.state().phase, HarnessPhase::Idle);
    }
}

#[cfg(test)]
mod cost_state_tests {
    use super::*;

    #[test]
    fn usage_returns_default_when_no_accumulation() {
        // 用默认 cost 构造 options。
        let opts = AgentHarnessOptions::new("test-model");
        // 模拟 harness 持有同一 Arc（不跑 loop）。
        let cost_arc = opts.cost.clone();
        // 没有 harness 实例也能验证共享状态契约：
        // 修改 Arc 对持有同一 Arc 的读者可见。
        {
            let mut c = cost_arc.lock().unwrap();
            c.total_input_tokens = 42;
            c.total_cost = 1.5;
        }
        let snapshot = cost_arc.lock().unwrap().clone();
        assert_eq!(snapshot.total_input_tokens, 42);
        assert_eq!(snapshot.total_cost, 1.5);
    }

    #[test]
    fn options_new_initializes_empty_cost() {
        let opts = AgentHarnessOptions::new("test-model");
        let c = opts.cost.lock().unwrap().clone();
        assert_eq!(c.total_input_tokens, 0);
        assert_eq!(c.total_cost, 0.0);
        assert!(c.by_model.is_empty());
    }
}

#[cfg(test)]
#[cfg(feature = "test-utils")]
mod cost_reset_tests {
    use super::*;
    use llm_harness_loop::test_utils::{MockLlmClient, MockResponse, NoOpEnv};

    #[test]
    fn reset_usage_zeroes_accumulated_cost() {
        let h = make_harness_for_cost(vec![]);
        // Inject cost into the shared Arc that the harness holds.
        {
            let cost = h.inner.lock().unwrap().cost.clone();
            let mut c = cost.lock().unwrap();
            c.total_input_tokens = 100;
            c.total_output_tokens = 50;
            c.total_cost = 2.5;
            c.by_model.insert(
                "anthropic/claude".into(),
                ModelCost {
                    input_tokens: 100,
                    output_tokens: 50,
                    cost: 2.5,
                    call_count: 1,
                    ..Default::default()
                },
            );
        }
        // Precondition: usage reflects the injected values.
        let before = h.usage();
        assert_eq!(before.total_input_tokens, 100);
        assert_eq!(before.total_cost, 2.5);
        assert_eq!(before.by_model.len(), 1);

        h.reset_usage();

        let after = h.usage();
        assert_eq!(after.total_input_tokens, 0);
        assert_eq!(after.total_output_tokens, 0);
        assert_eq!(after.total_cache_read_tokens, 0);
        assert_eq!(after.total_cache_write_tokens, 0);
        assert_eq!(after.total_cost, 0.0);
        assert!(after.by_model.is_empty());
    }

    /// Minimal harness constructor (parallel to `tests::make_harness`).
    fn make_harness_for_cost(responses: Vec<MockResponse>) -> AgentHarness {
        let client = Arc::new(MockLlmClient::new(responses));
        let env = Arc::new(NoOpEnv);
        let opts = AgentHarnessOptions::new("test-model");
        futures::executor::block_on(AgentHarness::new_in_memory(
            client as Arc<dyn LlmClient>,
            env,
            opts,
        ))
    }
}
