use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use llm_harness_loop::{
    ChatRequest, ConvertToLlmHook, DefaultConvertToLlm, HookedTool, LlmClient, LlmMessage,
    LoopConfig, RequestContent, RetryConfig, agent_loop,
};
use llm_harness_types::*;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::agent::ModelInfo;
use crate::compaction::{CompactionSettings, compact, prepare_compaction};
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
    /// Full registered tool list.
    pub tools: Vec<Arc<dyn Tool>>,
    /// Active tool subset; `None` means all tools are active.
    pub active_tools: Option<HashSet<String>>,
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
}

// ── HarnessHooks ──────────────────────────────────────────────────────────────

/// Collection of optional hooks injected into an [`AgentHarness`].
#[derive(Clone)]
pub struct HarnessHooks {
    /// Called once at the start of a `prompt()` run.
    pub before_run: Option<Arc<dyn BeforeRunHook>>,
    /// Called at the start of each turn.
    pub before_turn: Option<Arc<dyn BeforeTurnHook>>,
    /// Called at the end of each turn (before flush).
    pub after_turn: Option<Arc<dyn AfterTurnHook>>,
    /// Called before each tool execution.
    pub before_tool_call: Option<Arc<dyn BeforeToolCallHook>>,
    /// Called after each tool execution.
    pub after_tool_call: Option<Arc<dyn AfterToolCallHook>>,
    /// Called before each LLM request to transform the context.
    pub transform_context: Option<Arc<dyn TransformContextHook>>,
    /// Called after each turn to configure the next turn.
    pub prepare_next_turn: Option<Arc<dyn PrepareNextTurnHook>>,
    /// Called when LLM stops naturally to decide whether to continue.
    pub should_stop: Option<Arc<dyn ShouldStopHook>>,
    /// Called before each LLM provider request.
    pub before_provider_request: Option<Arc<dyn BeforeProviderRequestHook>>,
    /// Called after each LLM provider response.
    pub after_provider_response: Option<Arc<dyn AfterProviderResponseHook>>,
    /// Called before compaction to decide whether to proceed.
    pub before_compact: Option<Arc<dyn BeforeCompactHook>>,
}

impl HarnessHooks {
    /// All hooks absent (default state).
    pub fn none() -> Self {
        Self {
            before_run: None,
            before_turn: None,
            after_turn: None,
            before_tool_call: None,
            after_tool_call: None,
            transform_context: None,
            prepare_next_turn: None,
            should_stop: None,
            before_provider_request: None,
            after_provider_response: None,
            before_compact: None,
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

// ── Construction options ──────────────────────────────────────────────────────

/// Construction options for [`AgentHarness`].
pub struct AgentHarnessOptions {
    /// Initial model ID. Required.
    pub model: String,
    /// Optional model metadata.
    pub model_info: Option<ModelInfo>,
    /// Initial thinking level.
    pub thinking_level: ThinkingLevel,
    /// Initial tools.
    pub tools: Vec<Arc<dyn Tool>>,
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
    /// Override tokens to reserve for the LLM response during compaction.
    pub compaction_reserve_tokens: Option<u32>,
    /// Override how many recent context tokens to always preserve during compaction.
    pub compaction_keep_recent_tokens: Option<u32>,
}

impl AgentHarnessOptions {
    /// Minimal options with only model set.
    pub fn new(model: impl Into<String>) -> Self {
        let cap = DEFAULT_QUEUE_CAPACITY;
        let (steer_tx, _) = mpsc::channel::<AgentMessage>(cap);
        let (follow_up_tx, _) = mpsc::channel::<AgentMessage>(cap);
        // Dummy channels to satisfy borrow checker; overwritten in AgentHarness::new.
        drop(steer_tx);
        drop(follow_up_tx);
        Self {
            model: model.into(),
            model_info: None,
            thinking_level: ThinkingLevel::Off,
            tools: vec![],
            system_prompt: None,
            max_tokens: DEFAULT_MAX_TOKENS,
            queue_capacity: cap,
            stream_options: StreamOptions::default(),
            hooks: HarnessHooks::none(),
            auth: None,
            convert_to_llm: None,
            skills: vec![],
            templates: vec![],
            retry: None,
            compaction_reserve_tokens: None,
            compaction_keep_recent_tokens: None,
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
            let (model, thinking_level, tools, active_tools) = {
                let g = self.inner.lock().unwrap();
                let st = &g.state;
                (
                    st.model.clone(),
                    st.thinking_level,
                    st.tools.clone(),
                    st.active_tools.clone(),
                )
            };

            let mut directive = NextTurnDirective {
                context: None,
                model: Some(model),
                thinking_level: Some(thinking_level),
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
            tools: opts.tools,
            active_tools: None,
            system_prompt: opts.system_prompt,
            streaming_message: None,
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
            current_abort: None,
            queue_capacity: cap,
            max_tokens: opts.max_tokens,
            retry: opts.retry,
            compaction_reserve_tokens: opts.compaction_reserve_tokens,
            compaction_keep_recent_tokens: opts.compaction_keep_recent_tokens,
            active_tool_names: std::collections::HashMap::new(),
        };

        let convert_to_llm: Arc<dyn ConvertToLlmHook> = opts
            .convert_to_llm
            .unwrap_or_else(|| Arc::new(DefaultConvertToLlm::new()));

        Self {
            client,
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
    pub async fn prompt_with_messages(
        &self,
        messages: Vec<AgentMessage>,
    ) -> Result<(), HarnessError> {
        // 1. Guard Idle phase; consume queued_next_turn.
        let queued = {
            let mut inner = self.inner.lock().unwrap();
            if inner.state.phase != HarnessPhase::Idle {
                return Err(HarnessError::NotIdle(inner.state.phase));
            }
            inner.state.error_message = None;
            std::mem::take(&mut inner.state.queued_next_turn)
        };

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
                .ok_or_else(|| HarnessError::TemplateNotFound(name.to_string()))?
                .clone();
            drop(inner);
            invoke_template(&tmpl, &args)
        };
        self.prompt(text).await
    }

    /// Run compaction on the active session path (Idle only).
    pub async fn compact(&self) -> Result<CompactionStats, HarnessError> {
        {
            let inner = self.inner.lock().unwrap();
            if inner.state.phase != HarnessPhase::Idle {
                return Err(HarnessError::NotIdle(inner.state.phase));
            }
        }

        self.set_phase(HarnessPhase::Compacting);

        let result = self.do_compact().await;

        self.set_phase(HarnessPhase::Idle);

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
        {
            let inner = self.inner.lock().unwrap();
            if inner.state.phase != HarnessPhase::Idle {
                return Err(HarnessError::NotIdle(inner.state.phase));
            }
        }
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
        {
            let inner = self.inner.lock().unwrap();
            if inner.state.phase != HarnessPhase::Idle {
                return Err(HarnessError::NotIdle(inner.state.phase));
            }
        }
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

    /// Delete the branch ending at `leaf` (Idle only).
    pub async fn delete_branch(&self, leaf: EntryId) -> Result<(), HarnessError> {
        {
            let inner = self.inner.lock().unwrap();
            if inner.state.phase != HarnessPhase::Idle {
                return Err(HarnessError::NotIdle(inner.state.phase));
            }
        }
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
        {
            let inner = self.inner.lock().unwrap();
            if inner.state.phase != HarnessPhase::Idle {
                return Err(HarnessError::NotIdle(inner.state.phase));
            }
        }

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

    /// Drain the steer queue (resets the sender channel).
    pub fn clear_steering_queue(&self) {
        let mut inner = self.inner.lock().unwrap();
        let (tx, _) = mpsc::channel(inner.queue_capacity);
        inner.steer_tx = tx;
    }

    /// Drain the follow-up queue.
    pub fn clear_follow_up_queue(&self) {
        let mut inner = self.inner.lock().unwrap();
        let (tx, _) = mpsc::channel(inner.queue_capacity);
        inner.follow_up_tx = tx;
    }

    /// Drain all queues (steer, follow-up, next_turn).
    pub fn clear_all_queues(&self) {
        let mut inner = self.inner.lock().unwrap();
        let cap = inner.queue_capacity;
        let (steer_tx, _) = mpsc::channel(cap);
        let (follow_up_tx, _) = mpsc::channel(cap);
        inner.steer_tx = steer_tx;
        inner.follow_up_tx = follow_up_tx;
        inner.state.queued_next_turn.clear();
    }

    /// Returns `true` if any queue is non-empty.
    pub fn has_queued_messages(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.steer_tx.max_capacity() != inner.steer_tx.capacity()
            || inner.follow_up_tx.max_capacity() != inner.follow_up_tx.capacity()
            || !inner.state.queued_next_turn.is_empty()
    }

    /// Cancel the current run, if any.
    pub fn abort(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(token) = inner.current_abort.take() {
            token.cancel();
        }
    }

    // ── Observation ───────────────────────────────────────────────────────

    /// Return a snapshot of the current harness state.
    pub fn state(&self) -> HarnessState {
        self.inner.lock().unwrap().state.clone()
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

    fn set_phase(&self, phase: HarnessPhase) {
        let old = {
            let mut inner = self.inner.lock().unwrap();
            let old = inner.state.phase;
            inner.state.phase = phase;
            old
        };
        let _ = self.phase_tx.send(phase);
        self.emit(AgentHarnessEvent::PhaseChange {
            from: old,
            to: phase,
        });
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
        let Some(ref hook) = self.hooks.before_run else {
            return Ok(initial);
        };
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
        let result = hook
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
        let inner = self.inner.lock().unwrap();
        let st = &inner.state;
        let retry = inner.retry.clone();

        // Wrap tools with HookedTool if needed.
        let before = self.hooks.before_tool_call.clone();
        let after = self.hooks.after_tool_call.clone();
        let tools: Vec<Arc<dyn Tool>> = if before.is_some() || after.is_some() {
            st.tools
                .clone()
                .into_iter()
                .map(|t| {
                    Arc::new(HookedTool {
                        inner: t,
                        before: before.clone(),
                        after: after.clone(),
                    }) as Arc<dyn Tool>
                })
                .collect()
        } else {
            st.tools.clone()
        };

        // Build the default prepare_next_turn wrapper.
        let prepare_next_turn: Arc<dyn PrepareNextTurnHook> = Arc::new(DefaultPrepareNextTurn {
            inner: self.inner.clone(),
            user_hook: self.hooks.prepare_next_turn.clone(),
        });

        LoopConfig {
            model: st.model.clone(),
            max_tokens: inner.max_tokens,
            temperature: None,
            thinking_level: st.thinking_level,
            tools,
            active_tools: st.active_tools.clone(),
            default_execution_mode: ToolExecutionMode::Parallel,
            env: self.env.clone(),
            abort,
            stream_options: self.stream_options.clone(),
            convert_to_llm: self.convert_to_llm.clone(),
            transform_context: self.hooks.transform_context.clone(),
            prepare_next_turn: Some(prepare_next_turn),
            should_stop: self.hooks.should_stop.clone(),
            before_provider_request: self.hooks.before_provider_request.clone(),
            after_provider_response: self.hooks.after_provider_response.clone(),
            auth: self.auth.clone(),
            steer_rx: Some(steer_rx),
            follow_up_rx: Some(follow_up_rx),
            retry,
        }
    }

    async fn run_loop(&self, initial: Vec<AgentMessage>) -> Result<(), HarnessError> {
        // 1. Setup: channels, abort token, system prompt.
        let (steer_rx, follow_up_rx, abort, system_prompt) = {
            let mut inner = self.inner.lock().unwrap();
            inner.state.streaming_message = None;
            inner.state.pending_tool_calls.clear();
            let (steer_rx, follow_up_rx) = inner.reset_channels();
            let abort = CancellationToken::new();
            inner.current_abort = Some(abort.clone());
            let sp = inner.state.system_prompt.clone();
            (steer_rx, follow_up_rx, abort, sp)
        };
        self.set_phase(HarnessPhase::Turning);

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
            if let Err(ref e) = result {
                inner.state.error_message = Some(e.to_string());
                inner.state.pending_session_writes.clear();
            }
        }
        self.set_phase(HarnessPhase::Idle);
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
        //    (agent_loop emits AgentStart with empty initial_messages, so we write them here).
        for msg in &initial {
            self.push_pending_write(SessionEntryPayload::Message(msg.clone()));
        }

        let mut stream = Box::pin(agent_loop(self.client.clone(), ctx, config));
        let mut turn_messages: Vec<AgentMessage> = vec![];

        while let Some(event) = stream.next().await {
            match &event {
                AgentEvent::AgentStart { .. } => {
                    self.emit(AgentHarnessEvent::Agent(event));
                }
                AgentEvent::TurnStart { index } => {
                    if let Some(ref h) = self.hooks.before_turn {
                        let snapshot = {
                            let inner = self.inner.lock().unwrap();
                            TurnSnapshot {
                                model: inner.state.model.clone(),
                                thinking_level: inner.state.thinking_level,
                                tools: inner.state.tools.clone(),
                                system_prompt: inner.state.system_prompt.clone(),
                            }
                        };
                        h.before_turn(BeforeTurnCtx {
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
                    if let Some(ref h) = self.hooks.after_turn {
                        h.after_turn(AfterTurnCtx {
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
                    self.inner.lock().unwrap().state.error_message = Some(e.to_string());
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

        // before_compact hook is called BEFORE prepare_compaction so that
        // BeforeCompactDecision::Override can bypass the token-threshold check entirely.
        if let Some(ref h) = self.hooks.before_compact {
            let built = self.session.build_context().await?;
            let estimated_tokens: usize = path
                .iter()
                .map(crate::compaction::estimate_tokens_for_entry)
                .sum();
            let decision = h
                .before_compact(BeforeCompactCtx {
                    estimated_tokens,
                    messages: &built.messages,
                })
                .await;
            match decision {
                BeforeCompactDecision::Skip => {
                    return Err(CompactionError::InsufficientTokens.into());
                }
                BeforeCompactDecision::Override(result) => {
                    return self.apply_compaction_result(result, &path).await;
                }
                BeforeCompactDecision::Proceed => {}
            }
        }

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

        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: reserve_tokens.unwrap_or(16_384) as usize,
            keep_recent_tokens: keep_recent_tokens.unwrap_or(20_000) as usize,
            summary_model: model.clone(),
            summary_model_info: m_info.clone(),
        };

        let prep = prepare_compaction(&path, last_compaction.as_ref(), &settings, &m_info);
        let prep = match prep {
            Some(p) => p,
            None => return Err(CompactionError::InsufficientTokens.into()),
        };

        let estimated_tokens = prep.estimated_tokens;
        self.emit(AgentHarnessEvent::CompactionStart { estimated_tokens });

        let result = compact(
            self.client.as_ref(),
            prep,
            &settings,
            self.auth
                .as_ref()
                .map(|a| a.as_ref() as &(dyn AuthHook + Send + Sync)),
        )
        .await?;
        self.apply_compaction_result(result, &path).await
    }

    async fn apply_compaction_result(
        &self,
        result: CompactionResult,
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

        let details = if result.file_operations.is_empty() {
            None
        } else {
            serde_json::to_value(&result.file_operations).ok()
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
    async fn prompt_completes_and_returns_idle() {
        let h = make_harness(vec![MockResponse::text("Hello!")]);
        h.prompt("hi").await.unwrap();
        assert_eq!(h.state().phase, HarnessPhase::Idle);
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
        assert!(matches!(err, Err(HarnessError::TemplateNotFound(_))));
    }

    #[tokio::test]
    async fn subscribe_receives_events() {
        let h = make_harness(vec![MockResponse::text("hi")]);
        let mut rx = h.subscribe();
        h.prompt("hello").await.unwrap();
        assert!(rx.try_recv().is_ok());
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
        h.clear_all_queues();
        assert!(!h.has_queued_messages());
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
                        file_operations: vec![],
                    })
                })
            }
        }

        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("hello")]));
        let env = Arc::new(NoOpEnv);
        let mut opts = AgentHarnessOptions::new("test-model");
        opts.hooks.before_compact = Some(Arc::new(BadOverrideHook));
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
}
