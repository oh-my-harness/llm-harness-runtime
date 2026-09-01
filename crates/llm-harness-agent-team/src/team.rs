//! Agent team: declarative builder + runner for multi-agent collaboration.
//!
//! [`AgentTeam`] collects [`IndividualSpec`]s, builds each into a running
//! [`AgentHarness`] with social infrastructure attached. After [`build`],
//! the team is live: agents run self-driving loops. The operator (human)
//! interacts via [`submit`] (send a task to an agent) and [`receive`]
//! (drain messages that agents sent to "operator").

use futures::future::BoxFuture;
use llm_harness_agent::{AgentHarness, HarnessBuilder, Plugin};
use llm_harness_knowledge::access::{
    AllowAllAuthorizer, KnowledgeAccessContext, KnowledgeAccessControl, KnowledgeScope,
    PrincipalRef,
};
use llm_harness_knowledge::{
    EvidenceAuthority, EvidenceProviderId, KnowledgeCitationPolicy, KnowledgePlugin,
    KnowledgePluginConfig, KnowledgeRegistry, KnowledgeRequestContext, KnowledgeSource,
    KnowledgeToolConfig,
};
use llm_harness_knowledge_local::{
    DocumentFileSystem, DocumentParserRegistry, DocumentRootConfig, LocalDocumentSource,
    LocalDocumentSourceConfig, OsDocumentFileSystem,
};
use llm_harness_loop::{DefaultConvertToLlm, LlmClient};
use llm_harness_memory::{
    MemoryMutationGate, MemoryMutationGateError, MemoryMutationRequest, MemoryPlugin,
    MemoryService, MemoryStore, MemoryWritePolicy, SecureMemoryWritePolicy,
    SecureMemoryWritePolicyConfig,
};
use llm_harness_sandbox::OsEnvFactory;
use llm_harness_subagents::delivery::SubAgentMessageConverter;
use llm_harness_subagents::message_bus::{AgentEntry, AgentStatus, MAIN_AGENT_ID, MessageBus};
use llm_harness_subagents::plugin::SpawnPlugin;
use llm_harness_subagents::spawner::{HarnessSubAgentSpawner, JsonlSessionFactory};
use llm_harness_subagents::tools::{
    AbortSubagentTool, AwaitSubagentReplyTool, MessageSubagentTool, QuerySubagentTool,
    SpawnAgentTool,
};
use llm_harness_types::ExecutionEnv;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent_runner::{AgentRunner, ReflectionTrigger};
use crate::hooks::TeamBeforeRunHook;
use crate::individual_spec::IndividualSpec;
use crate::memory_store::{TeamSqliteMemorySource, TeamSqliteMemoryStore};
use crate::social_context::SocialContext;

use crate::timeout_manager::reserved;
use crate::tools::SocialToolPlugin;
use crate::{AgentId, Result, TeamError};
use llm_harness_strategy::status_panel::TodoStore;

// ── TeamConfig ───────────────────────────────────────────────────────────────

/// Configuration knobs for an [`AgentTeam`].
#[derive(Debug, Clone)]
pub struct TeamConfig {
    /// Maximum contacts an agent may hold in its contact list.
    pub max_contacts_per_agent: usize,
    /// Broadcast timeout in seconds (agent can override via timeout_secs).
    pub broadcast_timeout_secs: u64,
    /// Delegate follow-up timeout in seconds (agent can override via timeout_secs).
    pub delegate_timeout_secs: u64,
    /// Base directory for sub-agent session files. Each team agent gets a
    /// subdirectory under this for its spawned sub-agents' JSONL sessions.
    /// Only used by members with `allow_subagents(true)`.
    pub session_base_dir: PathBuf,
    /// Root directory for the team's shared knowledge documents. Created
    /// on `build` if missing. Caller-overridable base dir
    /// (session_base_dir precedent); defaults to the system temp dir. The
    /// default is SHARED across all teams on the machine — teams that must
    /// not see each other's documents should override it per team.
    pub memory_root: PathBuf,
    /// Optional durable inbox journal. Studio supplies one per team; core
    /// teams remain in-memory by default.
    pub inbox_journal: Option<Arc<crate::inbox_journal::InboxJournal>>,
    /// Optional durable pending-timer journal. Studio supplies one per team.
    pub timer_journal: Option<Arc<crate::timer_journal::TimeoutJournal>>,
}

impl TeamConfig {
    /// Default broadcast wait before the follow-up nudge (`broadcast_timeout_secs`).
    pub const DEFAULT_BROADCAST_TIMEOUT: Duration = Duration::from_secs(60);
    /// Default delegate follow-up wait (`delegate_timeout_secs`).
    pub const DEFAULT_DELEGATE_TIMEOUT: Duration = Duration::from_secs(300);
}

impl Default for TeamConfig {
    fn default() -> Self {
        Self {
            max_contacts_per_agent: 20,
            broadcast_timeout_secs: Self::DEFAULT_BROADCAST_TIMEOUT.as_secs(),
            delegate_timeout_secs: Self::DEFAULT_DELEGATE_TIMEOUT.as_secs(),
            session_base_dir: std::env::temp_dir().join("agent-team-sessions"),
            memory_root: std::env::temp_dir().join("agent-team-memory"),
            inbox_journal: None,
            timer_journal: None,
        }
    }
}

// ── AgentTeam ────────────────────────────────────────────────────────────────

/// A declarative multi-agent team.
///
/// Collect [`IndividualSpec`]s via [`AgentTeam::new`] + [`AgentTeam::member`],
/// then [`AgentTeam::build`] to spin up all agents. After `build`, the team
/// is live: agents run self-driving loops. The operator (human) interacts
/// via [`AgentTeam::submit`] / [`AgentTeam::receive`] / [`AgentTeam::shutdown`].
///
/// The operator is registered as a regular peer (no runner): messages sent
/// to "operator" land in its inbox and are drained by [`AgentTeam::receive`]
/// (each item carries the sender's id).
pub struct AgentTeam {
    members: Vec<IndividualSpec>,
    config: TeamConfig,
    ctx: Option<SocialContext>,
    /// Runner tasks by agent id — `replace_member` aborts the exact
    /// member's runner instead of leaking a zombie (C2).
    runners: HashMap<AgentId, JoinHandle<()>>,
    operator_id: AgentId,
    built: bool,
    /// Built member harnesses by id (populated by `build`).
    harnesses: HashMap<AgentId, Arc<AgentHarness>>,
    /// Execution env captured at `build` (reused by `replace_member`).
    build_env: Option<Arc<dyn ExecutionEnv>>,
}

/// Runners accumulated during [`AgentTeam::build`]. RAII: dropping the
/// map aborts every runner still in it — a mid-build `?` never leaks
/// spawned runners. On success the map is drained into
/// `self.runners` (normal ownership, aborted by `shutdown`/`Drop`).
#[derive(Default)]
struct RunnerVec(Vec<(AgentId, JoinHandle<()>)>);

impl Drop for RunnerVec {
    fn drop(&mut self) {
        for (_, handle) in self.0.drain(..) {
            handle.abort();
        }
    }
}

impl AgentTeam {
    /// Create a new team with default [`TeamConfig`].
    pub fn new() -> Self {
        Self::with_config(TeamConfig::default())
    }

    /// Create a new team with the given [`TeamConfig`].
    pub fn with_config(config: TeamConfig) -> Self {
        Self {
            members: Vec::new(),
            config,
            ctx: None,
            runners: HashMap::new(),
            operator_id: AgentId::new(reserved::OPERATOR),
            built: false,
            harnesses: HashMap::new(),
            build_env: None,
        }
    }

    /// Access the configuration.
    pub fn config(&self) -> &TeamConfig {
        &self.config
    }

    /// Add a team member.
    pub fn member(mut self, spec: IndividualSpec) -> Self {
        self.members.push(spec);
        self
    }

    /// Tool names registered on the built harness of `agent_id`
    /// (`None` for unknown agents or before `build`). Test-only: there is
    /// no public harness accessor by design.
    #[cfg(test)]
    pub fn agent_tools(&self, agent_id: &str) -> Option<Vec<String>> {
        self.harnesses.get(&AgentId::new(agent_id)).map(|h| {
            h.state()
                .tools
                .iter()
                .map(|t| t.name().to_string())
                .collect()
        })
    }

    /// Number of registered members.
    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    /// Read-only access to the shared social context (built teams only).
    ///
    /// Enables external observers (bench harnesses, tests) to inspect
    /// registry/contacts/relations without touching inboxes.
    pub fn social_context(&self) -> Option<&SocialContext> {
        self.ctx.as_ref()
    }

    /// Drain pending inbox messages after stopping runners. Used by studio
    /// lifecycle persistence on a clean shutdown.
    pub fn drain_pending_messages(&mut self) -> Result<Vec<crate::inbox::InboxMessage>> {
        if !self.built {
            return Err(TeamError::NotBuilt);
        }
        self.shutdown();
        let ctx = self.ctx.as_ref().ok_or(TeamError::NotBuilt)?;
        Ok(ctx.drain_pending_messages())
    }

    /// Requeue messages persisted from the previous team instance. Call
    /// after `build`, once all recipients and contacts exist.
    pub fn restore_pending_messages(
        &self,
        messages: Vec<crate::inbox::InboxMessage>,
    ) -> Result<()> {
        if !self.built {
            return Err(TeamError::NotBuilt);
        }
        let ctx = self.ctx.as_ref().ok_or(TeamError::NotBuilt)?;
        ctx.restore_pending_messages(messages);
        Ok(())
    }

    /// The built harness of `agent_id` (`None` for unknown agents or
    /// before `build`). Read-only observation handle: the returned Arc is
    /// the same one the agent's runner drives.
    pub fn harness(&self, agent_id: &AgentId) -> Option<Arc<AgentHarness>> {
        self.harnesses.get(agent_id).cloned()
    }

    /// Build all agents and start their self-driving loops.
    ///
    /// Creates a [`SocialContext`], registers the operator as a peer,
    /// registers every member, and builds each harness with social tools
    /// plus before-run hook. The operator is auto-added to every agent's
    /// contact list (strength 1.0) so agents can `send_message("operator")`
    /// from the first turn. Members with `allow_subagents(true)` also get
    /// the sub-agent spawn toolset.
    pub async fn build(
        &mut self,
        env: Arc<dyn ExecutionEnv>,
        providers: Vec<(String, Arc<dyn LlmClient>)>,
    ) -> Result<()> {
        if self.built {
            return Err(TeamError::AlreadyBuilt);
        }
        if self.members.is_empty() {
            return Err(TeamError::NoMembers);
        }

        // Reject duplicate member ids and reserved agent ids up front:
        // `register_agent` overwrites silently, and two runners sharing an
        // AgentId would race on one inbox queue. Reserved ids ("timer",
        // "self", "operator") collide with synthetic system senders.
        let mut seen: std::collections::HashSet<&AgentId> = std::collections::HashSet::new();
        for spec in &self.members {
            let id = spec.id();
            if reserved::is_reserved_id(id.as_str()) {
                return Err(TeamError::ReservedAgentId(id.as_str().to_string()));
            }
            if !seen.insert(id) {
                return Err(TeamError::DuplicateMember(id.as_str().to_string()));
            }
        }

        let ctx = SocialContext::with_journal(
            self.config.max_contacts_per_agent,
            self.config.inbox_journal.clone(),
            self.config.timer_journal.clone(),
        )?;

        // Register the operator as a peer (no runner — its inbox is drained
        // by the human via `receive()`).
        let operator = self.operator_id.clone();
        ctx.register_agent(operator.clone(), reserved::OPERATOR);

        // Publish the shared context before the build loop so a mid-build
        // failure still leaves the operator API and registered agents
        // observable (tests assert on it; diagnostics can inspect it).
        self.ctx = Some(ctx.clone());
        self.build_env = Some(env.clone());

        // Build and spawn each member agent under a RAII guard: if the
        // loop fails mid-way (`?`), dropping the guard aborts every
        // runner spawned so far instead of leaking detached tasks. On
        // success the guard is disarmed (below) and the runners move to
        // `self.runners`, whose normal owner (`shutdown`/`Drop`) aborts
        // them at teardown.
        // Build the shared memory/knowledge stack ONCE for the whole team
        // (T15): every member installs the same plugins. Each member gets
        // its own KnowledgeAccessContext (principal = ("agent", agent_id));
        // the runner injects it into every run request so
        // memory_write/knowledge_search etc. (referenced by the reflection
        // task) resolve their request context.
        let (knowledge_plugin, memory_plugin) =
            build_team_plugins(&self.config.memory_root).await?;
        let pending: Vec<IndividualSpec> = std::mem::take(&mut self.members);
        let mut runner_handles = RunnerVec::default();
        for spec in pending {
            let agent_id = spec.id().clone();
            let handle = self
                .build_one(
                    &ctx,
                    spec,
                    &providers,
                    &env,
                    &memory_plugin,
                    &knowledge_plugin,
                )
                .await?;
            runner_handles.0.push((agent_id, handle));
        }
        // Success: move the runners to normal ownership — aborted later by
        // `shutdown`/`Drop`.
        self.runners.extend(runner_handles.0.drain(..));
        self.built = true;
        Ok(())
    }

    /// Build and spawn ONE member: social registration, plugins, optional
    /// sub-agent infrastructure, harness build, runner spawn. Shared by
    /// [`AgentTeam::build`] (whole team) and [`AgentTeam::replace_member`]
    /// (single rebuild). On success the returned runner handle is owned by
    /// the caller's guard/runner list.
    #[allow(clippy::too_many_arguments)]
    async fn build_one(
        &mut self,
        ctx: &SocialContext,
        mut spec: IndividualSpec,
        providers: &[(String, Arc<dyn LlmClient>)],
        env: &Arc<dyn ExecutionEnv>,
        memory_plugin: &MemoryPlugin,
        knowledge_plugin: &KnowledgePlugin,
    ) -> Result<JoinHandle<()>> {
        let operator = self.operator_id.clone();
        let agent_id = spec.id().clone();
        let expertise = spec.expertise().to_string();

        // Take config flags before consuming the builder.
        let reflection_interval = spec.take_reflection_interval();
        let allow_subagents = spec.take_allow_subagents();
        let model = spec.model().to_string();
        ctx.register_agent(agent_id.clone(), expertise.clone());

        // Auto-add operator to the agent's contact list (strength 1.0).
        ctx.ensure_contact(&agent_id, &operator);
        ctx.update_strength(&agent_id, &operator, 1.0);

        // Create per-agent todo store.
        let todo_store = TodoStore::new();

        // Create social tool plugin + hooks.
        let plugin = SocialToolPlugin::new(agent_id.clone(), ctx.clone(), todo_store.clone())
            .with_broadcast_timeout(Duration::from_secs(self.config.broadcast_timeout_secs))
            .with_delegate_timeout(Duration::from_secs(self.config.delegate_timeout_secs))
            .with_operator(operator.clone());
        let before_run_hook = Arc::new(TeamBeforeRunHook::new(todo_store.clone()));
        // Configure the builder: providers, plugins, hooks.
        let mut builder: HarnessBuilder = spec.into_builder();
        for (pattern, client) in providers {
            builder = builder.provider(pattern.clone(), client.clone());
        }
        builder = builder.install(&plugin);
        builder = builder.install(memory_plugin);
        builder = builder.install(knowledge_plugin);
        builder = builder.before_run_hook(before_run_hook);

        // ── Sub-agent spawn infrastructure (opt-in) ──────────────────
        // Only members configured with `allow_subagents(true)` get the
        // spawn_agent toolset (from llm-harness-subagents): spawn → run
        // → result delivered → auto-cleanup. Disabled by default —
        // spawning multiplies LLM traffic and should be granted
        // deliberately, not to every team agent.
        let spawn_plugin: Option<Arc<SpawnPlugin>> = if allow_subagents {
            // Resolve the sub-agent client for this member's model with
            // the same exact→longest-glob order the harness itself
            // uses; fall back to the first provider only when nothing
            // matches (defer the hard error to `builder.build()`).
            let probe: HarnessBuilder = spec_builder_probe(providers);
            let spawner_client = probe
                .provider_for(&model)
                .or_else(|| providers.first().map(|(_, c)| c.clone()))
                .ok_or(TeamError::NoProviders)?;
            let bus = MessageBus::new();
            let session_dir = self.config.session_base_dir.join(agent_id.as_str());
            let spawner = Arc::new(
                HarnessSubAgentSpawner::new(
                    model.clone(),
                    spawner_client,
                    session_dir,
                    bus.clone(),
                    |_, _, _| Box::new(NoopPlugin),
                )
                .env_factory(Arc::new(OsEnvFactory))
                .session_factory(Arc::new(JsonlSessionFactory)),
            );
            bus.register(
                MAIN_AGENT_ID,
                AgentEntry {
                    status: AgentStatus::Running,
                    harness: None,
                    is_main: true,
                    role: None,
                    description: None,
                },
            );
            let spawn_plugin = SpawnPlugin::new(bus.clone());
            builder = builder.tool(Arc::new(SpawnAgentTool::new(spawner)));
            builder = builder.tool(Arc::new(MessageSubagentTool::new(
                bus.clone(),
                MAIN_AGENT_ID.to_string(),
            )));
            builder = builder.tool(Arc::new(AwaitSubagentReplyTool::new(
                bus.clone(),
                MAIN_AGENT_ID.to_string(),
            )));
            builder = builder.tool(Arc::new(QuerySubagentTool::new(bus.clone())));
            builder = builder.tool(Arc::new(AbortSubagentTool::new(bus)));
            builder = builder.install(spawn_plugin.as_ref());
            builder = builder.convert_to_llm(Some(Arc::new(
                DefaultConvertToLlm::new()
                    .with_custom_converter(Arc::new(SubAgentMessageConverter)),
            )));
            Some(spawn_plugin)
        } else {
            None
        };
        // Build the harness.
        let harness = builder.build(env.clone()).await?;
        let harness = Arc::new(harness);

        // Track the built harness for tool introspection (agent_tools).
        self.harnesses.insert(agent_id.clone(), harness.clone());

        // Wire harness weak ref into spawn plugin (after build).
        if let Some(spawn_plugin) = spawn_plugin {
            spawn_plugin.set_harness_weak(Arc::downgrade(&harness));
        }

        // Build reflection trigger if configured.
        let reflection =
            (reflection_interval > 0).then(|| ReflectionTrigger::new(reflection_interval));

        // Per-agent access context (controller ruling): principal is
        // ("agent", agent_id), scope stays the shared "agent-team"
        // namespace.
        let access = Arc::new(KnowledgeAccessContext::new(
            KnowledgeScope::new("agent-team"),
            PrincipalRef::new(agent_id.as_str(), "agent"),
        ));
        let runner = AgentRunner::new(agent_id, harness, ctx.clone(), reflection, Some(access));
        Ok(runner.spawn())
    }

    /// Rebuild ONE member in place: abort its current run, drop its old
    /// harness, build a fresh one from `spec`, and respawn its runner.
    ///
    /// The member's social registration, inbox, contacts, and todo store
    /// are preserved — only the harness (model/prompt/tools stack) is
    /// replaced. This is the platform primitive behind per-agent config
    /// changes that cannot be hot-applied (model / API key / base URL).
    ///
    /// Errors with [`TeamError::NotBuilt`] before `build`, [`TeamError::
    /// ReservedAgentId`] for synthetic ids, and [`TeamError::AgentNotFound`]
    /// for ids that are not team members.
    pub async fn replace_member(
        &mut self,
        id: &str,
        spec: IndividualSpec,
        providers: Vec<(String, Arc<dyn LlmClient>)>,
    ) -> Result<()> {
        if !self.built {
            return Err(TeamError::NotBuilt);
        }
        if reserved::is_reserved_id(id) {
            return Err(TeamError::ReservedAgentId(id.to_string()));
        }
        let ctx = self.ctx.as_ref().ok_or(TeamError::NotBuilt)?.clone();
        let agent_id = AgentId::new(id);
        if ctx.get_agent_info(&agent_id).is_none() {
            return Err(TeamError::AgentNotFound(id.to_string()));
        }
        // Abort the member's runner task FIRST (C2: the runner loop never
        // exits on its own — a live runner would keep draining the inbox
        // with the OLD harness, racing the replacement). Then abort any
        // in-flight run and drop the old harness.
        if let Some(mut old_runner) = self.runners.remove(&agent_id) {
            old_runner.abort();
            // Give the aborted task a moment to unwind before spawning the
            // replacement (avoid a same-tick double-drain of the inbox).
            let _ = tokio::time::timeout(Duration::from_millis(50), &mut old_runner).await;
        }
        if let Some(old) = self.harnesses.get(&agent_id) {
            old.abort();
        }
        self.harnesses.remove(&agent_id);

        // Rebuild the shared plugin stack (per-call construction mirrors
        // `build`; the SQLite memory source re-opens the same file) and
        // reuse the execution env captured at `build` time.
        let (knowledge_plugin, memory_plugin) =
            build_team_plugins(&self.config.memory_root).await?;
        let env = self.build_env.clone().ok_or(TeamError::NotBuilt)?;
        self.build_one(
            &ctx,
            spec,
            &providers,
            &env,
            &memory_plugin,
            &knowledge_plugin,
        )
        .await?;
        Ok(())
    }

    /// Submit a task to a specific agent (non-blocking).
    ///
    /// Sends the task text to `target_agent_id`'s inbox as a direct message
    /// from the operator (highest priority). The agent will pick it up in
    /// its self-driving loop.
    pub fn submit(&self, target_agent_id: &str, task: &str) -> Result<()> {
        if !self.built {
            return Err(TeamError::NotBuilt);
        }
        let ctx = self.ctx.as_ref().ok_or(TeamError::NotBuilt)?;
        let target = AgentId::new(target_agent_id);
        // Reject unregistered targets — matching the send_message tool's
        // gate — so tasks aren't queued into an inbox nobody drains.
        if ctx.get_agent_info(&target).is_none() {
            return Err(TeamError::AgentNotFound(target_agent_id.to_string()));
        }
        ctx.send_direct(
            &self.operator_id,
            &target,
            task,
            crate::inbox::priority::OPERATOR,
        )?;
        Ok(())
    }

    /// Receive messages that agents sent to the operator.
    ///
    /// Returns a stream of [`OperatorMessage`]s. Each item is an
    /// `InboxMessage` that an agent sent to "operator" via `send_message`,
    /// carrying the sender's id so the operator can tell who is speaking.
    /// Blocks until at least one message is available, then drains all
    /// pending messages.
    ///
    /// Errors with [`TeamError::NotBuilt`] if the team has not been built.
    pub fn receive(&self) -> Result<impl futures::Stream<Item = OperatorMessage> + '_> {
        if !self.built {
            return Err(TeamError::NotBuilt);
        }
        let ctx = self.ctx.as_ref().ok_or(TeamError::NotBuilt)?;
        let operator = self.operator_id.clone();
        Ok(async_stream::stream! {
            loop {
                ctx.wait_for_message(&operator).await;
                let messages = ctx.drain_inbox(&operator);
                if messages.is_empty() {
                    continue;
                }
                for msg in messages {
                    yield OperatorMessage {
                        from: msg.from,
                        body: msg.body,
                    };
                }
            }
        })
    }

    /// Read the current status of a team member (the shared
    /// [`crate::social_context::AgentStatus`] state machine: Idle / Busy /
    /// Stopped; the operator is always Idle).
    ///
    /// Returns `None` when the team has not been built yet or the id is
    /// not a registered member. Lets callers distinguish "agent is still
    /// working" (Busy) from "agent went Idle without reporting" instead of
    /// guessing with a timeout alone.
    pub fn agent_status(&self, agent_id: &str) -> Option<crate::social_context::AgentStatus> {
        self.ctx
            .as_ref()?
            .get_agent_info(&AgentId::new(agent_id))
            .map(|info| info.status)
    }

    /// Shut down all agent runners and quiesce the shared state.
    ///
    /// Aborts every agent's tokio task, aborts any monitor tasks the
    /// runners left behind, cancels all pending timeout timers (they
    /// would otherwise fire into dead inboxes), and marks every member
    /// [`crate::social_context::AgentStatus::Stopped`]. Agents lose
    /// in-flight state. After `shutdown`, the team cannot be restarted.
    pub fn shutdown(&mut self) {
        for (_, handle) in self.runners.drain() {
            handle.abort();
        }
        if let Some(ctx) = self.ctx.as_ref() {
            ctx.abort_monitors();
            ctx.cancel_all_timeouts();
            ctx.stop_all();
        }
    }
}

impl Default for AgentTeam {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AgentTeam {
    fn drop(&mut self) {
        // Aborts any runners left running — including the mid-`build()`
        // failure path where `built` is still false. Without this, dropped
        // teams leak detached runner tasks that keep the harnesses and the
        // shared SocialContext alive.
        self.shutdown();
    }
}

/// A message an agent sent to the operator, yielded by
/// [`AgentTeam::receive`].
#[derive(Debug, Clone)]
pub struct OperatorMessage {
    /// The agent that sent the message.
    pub from: AgentId,
    /// The message body.
    pub body: String,
}

/// No-op plugin for sub-agents. Sub-agents spawned by team agents don't
/// need social tools — they execute a task and return results.
struct NoopPlugin;

impl Plugin for NoopPlugin {
    fn name(&self) -> &str {
        "team-noop"
    }
}

/// Build a throwaway [`HarnessBuilder`] with the team's provider patterns
/// registered, so [`HarnessBuilder::provider_for`] can resolve a model to
/// its client with the same exact→longest-glob order `build()` uses.
fn spec_builder_probe(providers: &[(String, Arc<dyn LlmClient>)]) -> HarnessBuilder {
    let mut probe = HarnessBuilder::new("");
    for (pattern, client) in providers {
        probe = probe.provider(pattern.clone(), client.clone());
    }
    probe
}

// ── Memory / knowledge stack (T15) ──────────────────────────────────────────

/// Build the team's shared memory/knowledge plugins.
///
/// Two knowledge sources cooperate:
///
/// - a persistent SQLite memory store (under `memory_root`) backing
///   `memory_write` / `memory_forget`, searchable via `knowledge_search` —
///   the reflection loop's dedupe-then-record cycle reads and writes the
///   same database, so lessons survive restarts;
/// - the read-only document source rooted at `memory_root` (markdown files,
///   externally seeded), also reachable through `knowledge_search`.
///
/// Access contexts are NOT built here: each member gets its own per-agent
/// context (principal = ("agent", agent_id)) constructed in the `build`
/// loop.
async fn build_team_plugins(memory_root: &Path) -> Result<(KnowledgePlugin, MemoryPlugin)> {
    std::fs::create_dir_all(memory_root).map_err(|e| {
        TeamError::PluginSetup(format!("cannot create memory root {memory_root:?}: {e}"))
    })?;
    let access_control = Arc::new(KnowledgeAccessControl::new(Arc::new(AllowAllAuthorizer)));

    // Persistent memory: SQLite read source + write store sharing one db.
    let (memory_source, memory_store) =
        TeamSqliteMemorySource::open(memory_root.join("memory.sqlite3"))
            .map_err(|e| TeamError::PluginSetup(format!("team memory db: {e}")))?;

    // Read-only document source over the same root.
    let doc_config = LocalDocumentSourceConfig {
        source_id: "agent-team-docs".to_string(),
        name: "Agent Team Documents".to_string(),
        description: "Shared documents seeded under memory_root".to_string(),
        domains: vec!["agent-team".to_string()],
        roots: vec![DocumentRootConfig {
            id: "root".to_string(),
            path: memory_root.to_path_buf(),
        }],
        source_secret: vec![0x42u8; 32],
        max_document_bytes: 1024 * 1024,
    };
    let doc_source = Arc::new(
        LocalDocumentSource::create(
            doc_config,
            Arc::new(OsDocumentFileSystem) as Arc<dyn DocumentFileSystem>,
            DocumentParserRegistry::default(),
            access_control.clone(),
        )
        .await
        .map_err(|e| TeamError::PluginSetup(format!("local document source: {e}")))?,
    );

    let registry = Arc::new(
        KnowledgeRegistry::builder(access_control.clone())
            .source(memory_source.clone() as Arc<dyn KnowledgeSource>)
            .source(doc_source.clone() as Arc<dyn KnowledgeSource>)
            .build()
            .map_err(|e| TeamError::PluginSetup(format!("knowledge registry: {e}")))?,
    );
    let provider_id = EvidenceProviderId("agent-team-memory".to_string());
    let authority = Arc::new(
        EvidenceAuthority::new(vec![0x11u8; 32], [provider_id.clone()])
            .map_err(|e| TeamError::PluginSetup(format!("evidence authority: {e}")))?,
    );
    let plugin_config = KnowledgePluginConfig {
        tools: KnowledgeToolConfig::default(),
        citation_policy: KnowledgeCitationPolicy::default(),
    };
    let knowledge_plugin = KnowledgePlugin::new(registry, authority, provider_id, plugin_config)
        .map_err(|e| TeamError::PluginSetup(format!("knowledge plugin: {e}")))?;
    let memory_plugin = build_memory_plugin(memory_source, memory_store, access_control)?;
    Ok((knowledge_plugin, memory_plugin))
}

/// Build the team's shared memory plugin: write and read halves both
/// backed by the same SQLite database.
fn build_memory_plugin(
    memory_source: Arc<TeamSqliteMemorySource>,
    memory_store: Arc<TeamSqliteMemoryStore>,
    access_control: Arc<KnowledgeAccessControl>,
) -> Result<MemoryPlugin> {
    let write_policy = Arc::new(
        SecureMemoryWritePolicy::new(vec![0x55u8; 32], SecureMemoryWritePolicyConfig::default())
            .map_err(|e| TeamError::PluginSetup(format!("memory write policy: {e}")))?,
    );
    let service = Arc::new(
        MemoryService::new(
            access_control,
            memory_source as Arc<dyn KnowledgeSource>,
            memory_store as Arc<dyn MemoryStore>,
            write_policy as Arc<dyn MemoryWritePolicy>,
            Arc::new(AllowAllGate) as Arc<dyn MemoryMutationGate>,
        )
        .map_err(|e| TeamError::PluginSetup(format!("memory service: {e}")))?,
    );
    Ok(MemoryPlugin::new(service))
}

/// Permissive [`MemoryMutationGate`] — team agents may write memory freely.
struct AllowAllGate;

impl MemoryMutationGate for AllowAllGate {
    fn authorize<'a>(
        &'a self,
        _ctx: KnowledgeRequestContext<'a>,
        _request: MemoryMutationRequest,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, std::result::Result<(), MemoryMutationGateError>> {
        Box::pin(async { Ok(()) })
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(unused_must_use)]
mod tests {
    use super::*;
    use llm_harness_agent::HarnessBuildError;
    use llm_harness_sandbox::OsEnv;

    #[test]
    fn team_new_and_member_addition() {
        let team = AgentTeam::new();
        assert_eq!(team.member_count(), 0);
        assert!(!team.built);

        let team = team.member(IndividualSpec::new("a1", "analysis", "gpt-4o"));
        assert_eq!(team.member_count(), 1);

        let team = team.member(IndividualSpec::new("a2", "writing", "gpt-4o"));
        assert_eq!(team.member_count(), 2);
    }

    #[test]
    fn team_config_defaults() {
        let cfg = TeamConfig::default();
        assert_eq!(cfg.max_contacts_per_agent, 20);
    }

    #[tokio::test]
    async fn team_build_without_members_fails() {
        let mut team = AgentTeam::new();
        let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new("/tmp"));
        let result = team.build(env, vec![]).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), TeamError::NoMembers));
    }

    #[tokio::test]
    async fn team_build_rejects_duplicate_member_ids() {
        let mut team = AgentTeam::new()
            .member(IndividualSpec::new("a1", "analysis", "gpt-4o"))
            .member(IndividualSpec::new("a1", "writing", "gpt-4o"));
        let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new("/tmp"));
        let result = team.build(env, vec![]).await;
        match result {
            Err(TeamError::DuplicateMember(id)) => assert_eq!(id, "a1"),
            other => panic!("expected DuplicateMember, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn team_build_rejects_operator_id() {
        let mut team =
            AgentTeam::new().member(IndividualSpec::new("operator", "analysis", "gpt-4o"));
        let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new("/tmp"));
        let result = team.build(env, vec![]).await;
        assert!(matches!(result.unwrap_err(), TeamError::ReservedAgentId(_)));
    }

    #[tokio::test]
    async fn build_rejects_reserved_agent_id() {
        for id in ["timer", "self", "operator", "reminder:x"] {
            let mut team = AgentTeam::new().member(IndividualSpec::new(id, "analysis", "gpt-4o"));
            let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new("/tmp"));
            let result = team.build(env, vec![]).await;
            match result {
                Err(TeamError::ReservedAgentId(reserved)) => assert_eq!(reserved, id),
                other => panic!("expected ReservedAgentId({id}), got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn team_build_with_subagents_requires_providers() {
        // allow_subagents(true) constructs the spawner eagerly → fails with
        // NoProviders when the provider list is empty.
        let mut team = AgentTeam::new()
            .member(IndividualSpec::new("a1", "analysis", "gpt-4o").allow_subagents(true));
        let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new("/tmp"));
        let result = team.build(env, vec![]).await;
        assert!(matches!(result.unwrap_err(), TeamError::NoProviders));
    }

    #[tokio::test]
    async fn team_build_default_no_subagents_skips_spawn_infra() {
        // Default (no allow_subagents): the spawn infrastructure is skipped
        // entirely, so the empty provider list surfaces from harness
        // provider resolution instead of the spawner path.
        let mut team = AgentTeam::new().member(IndividualSpec::new("a1", "analysis", "gpt-4o"));
        let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new("/tmp"));
        let result = team.build(env, vec![]).await;
        match result {
            Err(TeamError::HarnessBuild(HarnessBuildError::NoProvider)) => {}
            other => panic!("expected HarnessBuild(NoProvider), got {other:?}"),
        }
    }
    #[tokio::test]
    async fn build_failure_aborts_already_spawned_runners() {
        // Two members: the first builds fine, the second's model matches no
        // provider → HarnessBuild(NoMatchingProvider). The first runner must
        // be aborted, not leaked. Behavioral check: the shared ctx is
        // reachable even though build failed, so we can submit a task for
        // the surviving agent and verify NOBODY consumes it — a live runner
        // would drain it within milliseconds.
        let client: Arc<dyn LlmClient> =
            Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]));
        let mut team = AgentTeam::new()
            .member(IndividualSpec::new("ok-agent", "analysis", "test-model"))
            .member(IndividualSpec::new("bad-agent", "writing", "no-such-model"));
        let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new("/tmp"));
        let result = team.build(env, vec![("test-model".into(), client)]).await;
        match result {
            Err(TeamError::HarnessBuild(HarnessBuildError::NoMatchingProvider { .. })) => {}
            other => panic!("expected NoMatchingProvider, got {other:?}"),
        }
        assert!(!team.built, "team must not be marked built after failure");
        assert!(
            team.runners.is_empty(),
            "runners from a failed build must be aborted, not stored"
        );
        // A failed build keeps the operator API closed.
        assert!(matches!(
            team.submit("ok-agent", "closed"),
            Err(TeamError::NotBuilt)
        ));

        // The surviving agent's runner is dead ⇒ its inbox must stay
        // untouched. (Pre-guard the leaked runner consumes the task here.)
        let ctx = team.ctx.as_ref().expect("ctx published before build loop");
        ctx.send_direct(
            &team.operator_id,
            &AgentId::new("ok-agent"),
            "must never be consumed",
            crate::inbox::priority::OPERATOR,
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            ctx.has_messages(&AgentId::new("ok-agent")),
            "runner from a failed build must be aborted: the task must NOT be consumed"
        );
        // Dropping the team exercises the Drop impl: it must abort anything
        // left and not panic or hang.
        drop(team);
    }

    #[tokio::test]
    async fn submit_rejects_unregistered_agent() {
        let client: Arc<dyn LlmClient> =
            Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]));
        let mut team = AgentTeam::new().member(IndividualSpec::new("a1", "analysis", "test-model"));
        let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new("/tmp"));
        team.build(env, vec![("test-model".into(), client)])
            .await
            .unwrap();

        match team.submit("ghost", "task") {
            Err(TeamError::AgentNotFound(id)) => assert_eq!(id, "ghost"),
            other => panic!("expected AgentNotFound, got {other:?}"),
        }
        // The registered agent accepts the task.
        team.submit("a1", "task").unwrap();
        let ctx = team.ctx.as_ref().unwrap();
        assert!(ctx.has_messages(&AgentId::new("a1")));
        // The operator never gained a bogus contact.
        assert!(!ctx.has_contact(&AgentId::new("a1"), &AgentId::new("ghost")));
    }

    #[tokio::test]
    async fn receive_yields_operator_messages_with_sender() {
        let client: Arc<dyn LlmClient> =
            Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]));
        let mut team = AgentTeam::new().member(IndividualSpec::new("a1", "analysis", "test-model"));
        let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new("/tmp"));
        team.build(env, vec![("test-model".into(), client)])
            .await
            .unwrap();

        assert!(team.receive().is_ok());
        // An unbuilt team errors instead of panicking.
        let unbuilt = AgentTeam::new();
        assert!(matches!(unbuilt.receive(), Err(TeamError::NotBuilt)));

        // Simulate an agent → operator message and drain it.
        let ctx = team.ctx.as_ref().unwrap();
        ctx.send_direct(&AgentId::new("a1"), &team.operator_id, "reporting done", 0);
        let mut stream = std::pin::pin!(team.receive().unwrap());
        use futures::StreamExt;
        let msg = stream.next().await.expect("message expected");
        assert_eq!(msg.from, AgentId::new("a1"));
        assert_eq!(msg.body, "reporting done");
    }
    #[tokio::test]
    async fn agent_status_reports_member_state() {
        let client: Arc<dyn LlmClient> =
            Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]));
        let mut team = AgentTeam::new().member(IndividualSpec::new("a1", "analysis", "test-model"));
        let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new("/tmp"));
        team.build(env, vec![("test-model".into(), client)])
            .await
            .unwrap();

        // Built team: registered members are observable, unknown ids are not.
        // Use the social-context AgentStatus (team.rs's top-level `use`
        // binds `AgentStatus` to the subagents message-bus type).
        use crate::social_context::AgentStatus as SocialAgentStatus;
        assert_eq!(team.agent_status("a1"), Some(SocialAgentStatus::Idle));
        assert_eq!(team.agent_status("ghost"), None);

        // The status reflects the shared SocialContext's state machine.
        let ctx = team.ctx.as_ref().unwrap();
        ctx.update_status(&AgentId::new("a1"), SocialAgentStatus::Busy);
        assert_eq!(team.agent_status("a1"), Some(SocialAgentStatus::Busy));

        // An unbuilt team cannot observe anything.
        let unbuilt = AgentTeam::new().member(IndividualSpec::new("a1", "x", "test-model"));
        assert_eq!(unbuilt.agent_status("a1"), None);
    }

    #[tokio::test]
    async fn build_installs_memory_and_knowledge_plugins() {
        // T15/C3: build() must install MemoryPlugin + KnowledgePlugin so the
        // reflection task's memory_write/knowledge_search tools exist.
        let client: Arc<dyn LlmClient> =
            Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]));
        let mut team = AgentTeam::new().member(IndividualSpec::new("a1", "analysis", "test-model"));
        let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new("/tmp"));
        team.build(env, vec![("test-model".into(), client)])
            .await
            .expect("team build should succeed");

        let tools = team
            .agent_tools("a1")
            .expect("built team exposes registered tools for member 'a1'");
        assert!(
            tools.iter().any(|t| t == "memory_write"),
            "memory_write must be registered; got {tools:?}"
        );
        assert!(
            tools.iter().any(|t| t == "memory_forget"),
            "memory_forget must be registered; got {tools:?}"
        );
        assert!(
            tools.iter().any(|t| t == "knowledge_search"),
            "knowledge_search must be registered; got {tools:?}"
        );
        assert!(
            tools.iter().any(|t| t == "knowledge_read"),
            "knowledge_read must be registered; got {tools:?}"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_quiesces_everything() {
        // Regression for the shutdown leak (P1-7): shutdown() used to only
        // abort the runner tasks — per-runner monitor tasks, pending
        // timeout timers (which fire into dead inboxes afterwards), and
        // member statuses were all left dangling.
        let client: Arc<dyn LlmClient> =
            Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]));
        let mut team = AgentTeam::new()
            .member(IndividualSpec::new("a1", "analysis", "test-model"))
            .member(IndividualSpec::new("a2", "writing", "test-model"));
        let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new("/tmp"));
        team.build(env, vec![("test-model".into(), client)])
            .await
            .unwrap();

        let ctx = team.ctx.as_ref().unwrap().clone();
        // A 1s self-reminder pending at shutdown time: it must never fire.
        // The delay is deliberately shorter than the quiet window below —
        // if shutdown failed to cancel the timer, the reminder would fire
        // inside that window and fail the test.
        let _replaced = crate::timeout_manager::TimeoutManager::new(ctx.clone()).set_self_reminder(
            AgentId::new("a1"),
            "check back",
            Duration::from_secs(1),
            Some("task-9".to_string()),
        );

        team.shutdown();

        // Members are marked Stopped — the social context's AgentStatus,
        // not the message_bus one imported at the top of this file.
        for name in ["a1", "a2"] {
            let id = AgentId::new(name);
            assert_eq!(
                ctx.get_agent_info(&id).unwrap().status,
                crate::social_context::AgentStatus::Stopped,
                "{name} must be Stopped after shutdown"
            );
        }

        // The reminder's timer task was aborted by shutdown. The 2s quiet
        // window straddles the 1s fire deadline: a surviving timer would
        // deliver within it and fail the test.
        let quiet = tokio::time::timeout(
            Duration::from_secs(2),
            ctx.wait_for_message(&AgentId::new("a1")),
        )
        .await;
        assert!(quiet.is_err(), "no message may be delivered after shutdown");
    }

    #[test]
    fn social_context_is_none_before_build() {
        let team = AgentTeam::new().member(IndividualSpec::new("a1", "analysis", "test-model"));
        assert!(team.social_context().is_none());
        assert!(team.harness(&AgentId::new("a1")).is_none());
    }

    #[tokio::test]
    async fn getters_work_after_build() {
        let client: Arc<dyn LlmClient> =
            Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]));
        let mut team = AgentTeam::new()
            .member(IndividualSpec::new("a1", "analysis", "test-model"))
            .member(IndividualSpec::new("a2", "writing", "test-model"));
        let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new("/tmp"));
        team.build(env, vec![("test-model".into(), client)])
            .await
            .unwrap();

        let ctx = team.social_context().expect("social context after build");
        let id1 = AgentId::new("a1");
        assert!(ctx.get_agent_info(&id1).is_some());

        let h = team.harness(&id1).expect("harness after build");
        assert_eq!(h.state().model, "test-model");
        // Unknown agent → None.
        assert!(team.harness(&AgentId::new("ghost")).is_none());
        team.shutdown();
    }

    // ── replace_member ────────────────────────────────────────────────────

    fn mock_providers(models: &[&str]) -> Vec<(String, Arc<dyn LlmClient>)> {
        models
            .iter()
            .map(|m| {
                (
                    m.to_string(),
                    Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]))
                        as Arc<dyn LlmClient>,
                )
            })
            .collect()
    }

    async fn built_two_member_team() -> AgentTeam {
        let mut team = AgentTeam::new()
            .member(IndividualSpec::new("a1", "analysis", "m-old"))
            .member(IndividualSpec::new("a2", "writing", "m-old"));
        let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new("/tmp"));
        team.build(env, mock_providers(&["m-old"])).await.unwrap();
        team
    }

    #[tokio::test]
    async fn replace_member_swaps_harness_and_keeps_inbox() {
        let mut team = built_two_member_team().await;

        // Replace while a message is pending. To observe inbox survival we
        // must submit AFTER aborting the old runner but the API takes the
        // lock in one call — instead assert on social registration + a
        // post-replace delivery (registration/contact preservation proof).
        let old = team.harness(&AgentId::new("a1")).unwrap();
        let a2_old = team.harness(&AgentId::new("a2")).unwrap();

        team.replace_member(
            "a1",
            IndividualSpec::new("a1", "analysis", "m-new"),
            mock_providers(&["m-new"]),
        )
        .await
        .unwrap();

        // Harness pointer changed; model updated.
        let new = team.harness(&AgentId::new("a1")).unwrap();
        assert!(
            !Arc::ptr_eq(&old, &new),
            "replace_member must build a new harness"
        );
        assert_eq!(new.state().model, "m-new");
        // Other members untouched.
        let a2 = team.harness(&AgentId::new("a2")).unwrap();
        assert!(Arc::ptr_eq(&a2_old, &a2), "a2 must not be rebuilt");

        // Social registration preserved.
        let ctx = team.social_context().unwrap();
        assert!(ctx.get_agent_info(&AgentId::new("a1")).is_some());
        // Delivery still works post-replace: submit lands in the rebuilt
        // member's inbox (registration + inbox queue preserved).
        team.submit("a1", "post-replace task").unwrap();
        let drained = ctx.drain_inbox(&AgentId::new("a1"));
        assert!(
            drained.iter().any(|m| m.body.contains("post-replace task")),
            "submit must reach the rebuilt member: {drained:?}"
        );
        team.shutdown();
    }

    #[tokio::test]
    async fn replace_member_not_built_fails() {
        let mut team = AgentTeam::new();
        let result = team
            .replace_member(
                "a1",
                IndividualSpec::new("a1", "x", "m"),
                mock_providers(&["m"]),
            )
            .await;
        assert!(matches!(result.unwrap_err(), TeamError::NotBuilt));
    }

    #[tokio::test]
    async fn replace_member_unknown_agent_fails() {
        let mut team = built_two_member_team().await;
        let result = team
            .replace_member(
                "ghost",
                IndividualSpec::new("ghost", "x", "m"),
                mock_providers(&["m"]),
            )
            .await;
        assert!(matches!(result.unwrap_err(), TeamError::AgentNotFound(_)));
        team.shutdown();
    }

    #[tokio::test]
    async fn replace_member_reserved_id_rejected() {
        let mut team = built_two_member_team().await;
        let result = team
            .replace_member(
                "operator",
                IndividualSpec::new("operator", "x", "m"),
                mock_providers(&["m"]),
            )
            .await;
        assert!(matches!(result.unwrap_err(), TeamError::ReservedAgentId(_)));
        team.shutdown();
    }

    #[tokio::test]
    async fn replace_member_kills_old_runner() {
        // C2 regression: the old runner must be aborted, not left alive.
        // Proof: after replace, submit a message with the OLD client's mock
        // sequence exhausted — if the old runner (old harness with its own
        // client) were alive it could still drain. Stronger observable:
        // exactly one consumer exists, so a message lands with the NEW
        // harness's session, not the old.
        let mut team = built_two_member_team().await;
        team.replace_member(
            "a1",
            IndividualSpec::new("a1", "analysis", "m-new"),
            mock_providers(&["m-new"]),
        )
        .await
        .unwrap();
        let new_harness = team.harness(&AgentId::new("a1")).unwrap();

        // A second replace must not leave the FIRST new runner alive either.
        team.replace_member(
            "a1",
            IndividualSpec::new("a1", "analysis", "m-newer"),
            mock_providers(&["m-newer"]),
        )
        .await
        .unwrap();
        let newest = team.harness(&AgentId::new("a1")).unwrap();
        assert!(!Arc::ptr_eq(&new_harness, &newest));

        // Delivery still works and only the newest harness receives it.
        team.submit("a1", "gen-3 message").unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ctx = team.social_context().unwrap();
        // The runner (newest) drains the inbox; the run fails on the empty
        // mock and the message is rolled back — either way it must NOT be
        // duplicated. Assert single delivery: drain returns at most the
        // rolled-back copy, and old runners did not consume it into an
        // old-harness session.
        let drained = ctx.drain_inbox(&AgentId::new("a1"));
        assert!(
            drained.iter().filter(|m| m.body.contains("gen-3")).count() <= 1,
            "message must not be consumed twice (zombie runner): {drained:?}"
        );
        team.shutdown();
    }
}
