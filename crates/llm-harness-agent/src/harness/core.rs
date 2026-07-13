use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use llm_harness_loop::{
    ChatRequest, ConvertToLlmHook, DefaultConvertToLlm, FinalAnswerMode, LlmClient, LlmMessage,
    RequestContent,
};
use llm_harness_types::*;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::agent::ModelInfo;
use crate::compaction::{
    CompactionSummaryConfig, CompactionThresholdConfig, DEFAULT_CONTEXT_WINDOW,
    DEFAULT_KEEP_RECENT_TOKENS, DEFAULT_RESERVE_TOKENS,
};
use crate::session::{Session, SessionRepo, repo::InMemorySessionRepo, types::*};
use crate::skills::{
    PromptTemplate, Skill, format_skill_invocation, invoke_template, load_prompt_templates,
    load_skills,
};

use super::event::AgentHarnessEvent;
use super::follow_up::FollowUpHandle;
use super::hooks::{AbortCascadeHook, AsyncSpawnHookHandle, IdleWatcherHook};
use super::options::AgentHarnessOptions;
use super::state::{HarnessHooks, HarnessInner, HarnessState};
use crate::skills::{dedup_skills, dedup_templates, filter_readable};

const BROADCAST_CAPACITY: usize = 256;

// ── AgentHarness ──────────────────────────────────────────────────────────────

/// Orchestrates an `agent_loop` with session persistence, hooks, and compaction.
///
/// Unlike [`crate::Agent`], `AgentHarness` does not wrap `Agent` — it drives
/// `agent_loop` directly and manages session writes via `pending_session_writes`.
pub struct AgentHarness {
    pub(super) client: Arc<dyn LlmClient>,
    pub(super) session: Session,
    pub(super) env: Arc<dyn ExecutionEnv>,
    pub(super) inner: Arc<Mutex<HarnessInner>>,
    // AgentHarnessEvent is not Clone (wraps AgentEvent which wraps ToolError<anyhow::Error>).
    pub(super) event_tx: broadcast::Sender<Arc<AgentHarnessEvent>>,
    pub(super) phase_tx: watch::Sender<HarnessPhase>,
    pub(super) phase_rx: watch::Receiver<HarnessPhase>,
    pub(super) convert_to_llm: Arc<dyn ConvertToLlmHook>,
    pub(super) hooks: HarnessHooks,
    pub(super) stream_options: StreamOptions,
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
        assert!(
            opts.queue_capacity > 0,
            "queue_capacity must be > 0; a capacity of 0 creates a rendezvous channel where try_send almost always fails, silently breaking steer/follow-up"
        );
        // Check tool name conflicts: business tools must not use framework-reserved names.
        if let Err(msg) = llm_harness_types::check_tool_name_conflicts(&opts.tools) {
            panic!("{msg}");
        }
        // Deduplicate skills and templates (first-wins). This is the single
        // convergence point for all construction paths; regardless of how
        // opts was assembled, inner.skills / inner.templates will have no
        // duplicate names after this.
        let skills = dedup_skills(opts.skills);
        let templates = dedup_templates(opts.templates);
        let cap = opts.queue_capacity;

        let compaction_context_window = opts
            .model_info
            .as_ref()
            .map(|mi| mi.context_window)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
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
            pending_session_writes: VecDeque::new(),
            queued_next_turn: vec![],
            error_message: None,
        };

        let inner = HarnessInner {
            state,
            skills,
            readable_skills: opts.readable_skills,
            templates,
            steer_tx,
            follow_up_tx,
            cost: opts.cost,
            current_abort: None,
            queue_capacity: cap,
            max_tokens: opts.max_tokens,
            retry: opts.retry,
            compaction_threshold: CompactionThresholdConfig {
                reserve_tokens: opts
                    .compaction_reserve_tokens
                    .map(|t| t as usize)
                    .unwrap_or(DEFAULT_RESERVE_TOKENS),
                keep_recent_tokens: opts
                    .compaction_keep_recent_tokens
                    .map(|t| t as usize)
                    .unwrap_or(DEFAULT_KEEP_RECENT_TOKENS),
                context_window: compaction_context_window,
            },
            compaction_summary: opts.compaction_model.map(|c| {
                (
                    c.client,
                    CompactionSummaryConfig {
                        summary_model: c.model,
                        summary_model_info: c.model_info,
                    },
                )
            }),
            auto_compact: opts.auto_compact,
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
            session,
            env,
            inner: Arc::new(Mutex::new(inner)),
            event_tx,
            phase_tx,
            phase_rx,
            convert_to_llm,
            hooks: opts.hooks,
            stream_options: opts.stream_options,
        }
    }
}

impl AgentHarness {
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

        // 2. Auto-compaction (Idle phase, before run starts).
        // Non-InsufficientTokens errors propagate to abort the run — the
        // context is too large and continuing would fail downstream.
        self.try_auto_compact().await?;
        let initial: Vec<AgentMessage> = queued.into_iter().chain(messages).collect();

        // 3. before_run hook.
        let initial = self.run_before_run_hook(initial).await?;

        // 4. Drive the loop.
        self.run_loop(initial).await
    }

    /// Expand `name` as a skill invocation and start a run.
    pub async fn skill(&self, name: &str, additional: Option<&str>) -> Result<(), HarnessError> {
        let (skill, abort) = {
            let inner = self.inner.lock().unwrap();
            let skill = inner
                .skills
                .iter()
                .find(|s| s.name == name)
                .ok_or_else(|| HarnessError::SkillNotFound(name.to_string()))?
                .clone();
            let abort = inner.current_abort.clone().unwrap_or_default();
            (skill, abort)
        };
        let text = format_skill_invocation(self.env.as_ref(), &skill, additional, abort).await?;
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
        let skills = dedup_skills(skills);
        let templates = dedup_templates(templates);
        let (skill_count, tmpl_count) = (skills.len(), templates.len());
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(ref store) = inner.readable_skills {
                *store.write().unwrap_or_else(|e| e.into_inner()) = filter_readable(&skills);
            }
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
    pub fn add_skills(&self, skills: Vec<Skill>) {
        let (skill_count, template_count) = {
            let mut inner = self.inner.lock().unwrap();
            let existing: HashSet<String> = inner.skills.iter().map(|s| s.name.clone()).collect();
            let new_skills: Vec<Skill> = skills
                .into_iter()
                .filter(|s| !existing.contains(&s.name))
                .collect();
            let new_readable: Vec<Skill> = new_skills
                .iter()
                .filter(|s| !s.disable_model_invocation)
                .cloned()
                .collect();
            inner.skills.extend(new_skills);
            if let Some(ref store) = inner.readable_skills {
                store
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(new_readable);
            }
            (inner.skills.len(), inner.templates.len())
        };
        self.emit(AgentHarnessEvent::ResourcesUpdate {
            skills: skill_count,
            templates: template_count,
            diagnostics: vec![],
        });
    }

    /// Append prompt templates to the existing list without replacing it.
    pub fn add_templates(&self, templates: Vec<PromptTemplate>) {
        let (skill_count, template_count) = {
            let mut inner = self.inner.lock().unwrap();
            let existing: HashSet<String> =
                inner.templates.iter().map(|t| t.name.clone()).collect();
            inner.templates.extend(
                templates
                    .into_iter()
                    .filter(|t| !existing.contains(&t.name)),
            );
            (inner.skills.len(), inner.templates.len())
        };
        self.emit(AgentHarnessEvent::ResourcesUpdate {
            skills: skill_count,
            templates: template_count,
            diagnostics: vec![],
        });
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
                inner.state.pending_session_writes.push_back(payload);
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
                inner.state.pending_session_writes.push_back(payload);
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

    /// Replace the registered tool list.
    pub async fn set_tools(&self, tools: Vec<Arc<dyn Tool>>) -> Result<(), HarnessError> {
        if let Err(msg) = llm_harness_types::check_tool_name_conflicts(&tools) {
            return Err(HarnessError::Agent(AgentError::Internal(msg)));
        }
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
                inner.state.pending_session_writes.push_back(payload);
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
        let token_count: usize = crate::compaction::estimate_tokens(&path);

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
        // 预排空 completion channel：把 sub-agent 结果格式化为 AgentMessage，
        // 作为 initial 传入 run_loop，让 LLM 第一轮就能看到结果。
        // borrow → drain → release → run_loop 内部再 borrow。
        let initial: Vec<AgentMessage> = {
            let async_hook = self.inner.lock().unwrap().async_spawn_hook.clone();
            if let Some(h) = &async_hook {
                h.borrow_completion_rx();
                let drained = h.drain_completions();
                h.release_completion_rx();
                drained.into_iter().collect()
            } else {
                vec![]
            }
        };
        // No auto-compaction here: continue_run is called by IdleWatcher after
        // a sub-agent completion. The completion result (if any) is now in
        // `initial` and will be persisted by run_loop. Auto-compaction runs at
        // prompt_with_messages entry instead.
        self.run_loop(initial).await
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

    /// Return the list of framework built-in tool names.
    ///
    /// Business tools must not use these names; the harness rejects such
    /// registrations at construction / `set_tools` time. Use this to
    /// avoid collisions when naming user tools.
    pub fn builtin_tool_names() -> &'static [&'static str] {
        llm_harness_types::BUILTIN_TOOL_NAMES
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

    pub(super) fn emit(&self, event: AgentHarnessEvent) {
        let _ = self.event_tx.send(Arc::new(event));
    }
}
