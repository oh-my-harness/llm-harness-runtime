//! Team management: one team = one `AgentTeam` assembled from a
//! [`TeamSpec`], fully isolated per instance.
//!
//! Per-team data lives under `<data_root>/teams/<id>/`: `team.json`
//! (persisted spec), `issues/` (IssueStore), `memory/` (team memory),
//! `sessions/` (sub-agent sessions). The team's execution env working
//! dir is the repo when configured, else the team data dir.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use llm_harness_agent_team::{
    AgentTeam, IndividualSpec, TeamError, TimeoutJournal, TimerJournalError,
};
#[cfg(target_os = "linux")]
use llm_harness_platform::sandbox::Sandbox;
use llm_harness_platform::sandbox::{ResourceLimits, SandboxConfig};
use llm_harness_sandbox::{BwrapSandbox, OsEnv};
use llm_harness_types::ExecutionEnv;

use crate::engine::{self, HandoffRoute};
use crate::issue::IssueStore;
use crate::team_spec::TeamSpec;

#[derive(Debug, thiserror::Error)]
pub enum TeamManagerError {
    #[error("not a git repo: {0}")]
    NotAGitRepo(PathBuf),
    #[error("unknown team: {0}")]
    UnknownTeam(String),
    #[error("failed to delete team directory {path}: {source}")]
    DeleteTeam {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("timer journal: {0}")]
    TimerJournal(#[from] TimerJournalError),
    #[error("team already registered: {0}")]
    AlreadyRegistered(String),
    #[error(transparent)]
    Team(#[from] TeamError),
    #[error("no provider for model tier")]
    NoProviders,
    #[error("controlled workflow: {0}")]
    Workflow(String),
    #[error("unknown member: {0}")]
    UnknownMember(String),
    #[error("template membership changed; upgrade requires explicit member management: added={}, removed={}", added.join(", "), removed.join(", "))]
    MembershipChanged {
        added: Vec<String>,
        removed: Vec<String>,
    },
    #[error(transparent)]
    Assemble(#[from] engine::AssembleError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Per-member update outcome: which apply path the engine took.
#[derive(Debug, PartialEq)]
pub enum MemberUpdate {
    /// persona/role_label hot-applied on the live harness.
    HotApplied,
    /// harness rebuilt via replace_member (model/key/url/toolkits).
    Rebuilt,
}

/// Result of applying a template upgrade.
#[derive(Debug, PartialEq)]
pub struct TemplateUpgradeOutcome {
    pub version: String,
    pub upgraded: Vec<String>,
    pub overridden: Vec<String>,
    pub rebuilt: Vec<String>,
}

/// One assembled team instance.
pub struct TeamEntry {
    pub id: String,
    pub spec: TeamSpec,
    pub repo_path: Option<PathBuf>,
    pub team: AgentTeam,
    pub issues: IssueStore,
    pub handoffs: Vec<HandoffRoute>,
    pub workflow: Option<Arc<crate::pipeline::PipelineController>>,
    workflow_task: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Keeps MCP manager alive while the team runs; Drop cancels background tasks.
    pub mcp_lifecycle: Option<crate::engine::McpManagerLifecycle>,
}

/// Persist first, then project the same logical terminal into live chat.
/// A duplicate message ID is already in history and must not produce a
/// second live conversation message.
fn publish_workflow_terminal(
    journal_path: &Path,
    project_id: &str,
    issue_id: &str,
    terminal: &crate::pipeline::TerminalUserOutcome,
    events: Option<&crate::events::EventBus>,
) -> Result<bool, llm_harness_agent_team::ChatHistoryJournalError> {
    let from = match terminal.attribution {
        crate::pipeline::TerminalAttribution::Orchestrator => crate::pipeline::ORCHESTRATOR_ID,
        crate::pipeline::TerminalAttribution::WorkflowStatus => crate::pipeline::WORKFLOW_STATUS_ID,
    };
    // The identity belongs to the whole workflow, not to its attribution.
    let message_id = format!("{issue_id}:terminal");
    let message = llm_harness_agent_team::InboxMessage {
        journal_id: None,
        from: llm_harness_agent_team::AgentId::new(from),
        to: llm_harness_agent_team::AgentId::new("operator"),
        body: terminal.body.clone(),
        message_type: llm_harness_agent_team::MessageType::Direct,
        priority: llm_harness_agent_team::priority::NORMAL,
        task_id: None,
        task_kind: None,
        broadcast_id: Some(issue_id.to_string()),
        message_id: Some(message_id.clone()),
    };
    let inserted = match llm_harness_agent_team::ChatHistoryJournal::open(journal_path)
        .and_then(|journal| journal.append(&message))
    {
        Ok(sequence) => sequence.is_some(),
        Err(error) => {
            if let Some(events) = events {
                events.publish(crate::events::agent_error_event(
                    project_id,
                    crate::pipeline::WORKFLOW_STATUS_ID,
                    &format!("Workflow terminal output could not be saved: {error}"),
                ));
            }
            return Err(error);
        }
    };
    if inserted && let Some(events) = events {
        events.publish(crate::events::team_message_event(
            project_id,
            from,
            "operator",
            &terminal.body,
            "direct",
            Some(message_id),
            Some(issue_id.to_string()),
            llm_harness_agent_team::ChatView::GroupReply.as_str(),
        ));
    }
    Ok(inserted)
}

#[cfg(target_os = "linux")]
async fn start_confined_sandbox(sandbox: BwrapSandbox) -> std::io::Result<Arc<dyn ExecutionEnv>> {
    sandbox
        .start()
        .await
        .map_err(|error| std::io::Error::other(format!("sandbox required: {error}")))?;
    Ok(sandbox.env())
}

#[cfg(not(target_os = "linux"))]
async fn start_confined_sandbox(_sandbox: BwrapSandbox) -> std::io::Result<Arc<dyn ExecutionEnv>> {
    Err(std::io::Error::other(
        "sandbox required: BwrapSandbox is only available on Linux",
    ))
}

impl TeamEntry {
    /// Reserve ownership before spawning: concurrent HTTP requests cannot
    /// create two consumers of the controller inbox.
    pub fn start_workflow(
        &self,
        issue: crate::issue::Issue,
        events: Option<Arc<crate::events::EventBus>>,
    ) -> Result<String, TeamManagerError> {
        let controller = self
            .workflow
            .clone()
            .ok_or_else(|| TeamManagerError::Workflow("team is not controlled".into()))?;
        let mut task = self.workflow_task.lock();
        // Inlined `workflow_running` check: that helper re-locks `workflow_task`,
        // and parking_lot::Mutex is not reentrant — calling it under the guard
        // self-deadlocks every `start_workflow` call.
        if task.as_ref().is_some_and(|task| !task.is_finished()) {
            return Err(TeamManagerError::Workflow(
                "a workflow is already running".into(),
            ));
        }
        if self.spec.members.iter().any(|member| {
            self.team.agent_status(&member.id) == Some(llm_harness_agent_team::AgentStatus::Busy)
        }) {
            return Err(TeamManagerError::Workflow(
                "a previous member turn is still stopping".into(),
            ));
        }
        let issue_id = issue.id.clone();
        let project_id = self.id.clone();
        let chat_history_path = self.data_dir().join("chat.sqlite3");
        // V0.6 evidence trail: project the controller's lifecycle events onto
        // the project's live EventBus so the panel WS stream can reconstruct
        // who did what / which artifact / which round. The durable record of
        // the same lifecycle remains the chat journal + snapshot evidence dirs
        // + the terminal review package; the bus is a transient fan-out.
        if let Some(events) = events.clone() {
            let sink_project = project_id.clone();
            controller.set_sink(Arc::new(move |event: &str, payload: &serde_json::Value| {
                let mut object = payload.clone();
                if object.get("issue_id").is_none() {
                    return;
                }
                object["type"] = serde_json::Value::String(event.to_string());
                object["project"] = serde_json::Value::String(sink_project.clone());
                object["ts"] = serde_json::Value::String(chrono::Utc::now().to_rfc3339());
                events.publish(object);
            }));
        }
        let harnesses: Vec<_> = self
            .spec
            .members
            .iter()
            .filter_map(|member| {
                self.team
                    .harness(&llm_harness_agent_team::AgentId::new(&member.id))
            })
            .collect();
        *task = Some(tokio::spawn(async move {
            // Keep evidence leased beyond run(): terminal send_message may
            // precede worker loop cleanup. Only Idle workers release protection.
            let lease = match controller.snapshot_workflow_lease().await {
                Ok(lease) => lease,
                Err(error) => {
                    // Real storage failure (permissions, `.pipeline` swapped
                    // for a symlink, ...). The chat request already returned
                    // 202, so the workflow MUST publish a terminal outcome
                    // instead of silently vanishing. Transient contention is
                    // WouldBlock and retried inside `snapshot_workflow_lease`.
                    tracing::warn!(%error, "cannot acquire workflow snapshot lease");
                    let terminal = crate::pipeline::TerminalUserOutcome {
                        attribution: crate::pipeline::TerminalAttribution::WorkflowStatus,
                        body: format!(
                            "Workflow stopped: snapshot storage unavailable: {error}. The objective was accepted but no work could start; fix the storage problem and resend."
                        ),
                    };
                    if let Err(publish_error) = publish_workflow_terminal(
                        &chat_history_path,
                        &project_id,
                        &issue.id,
                        &terminal,
                        events.as_deref(),
                    ) {
                        tracing::error!(workflow_id = %issue.id, "failed to persist workflow terminal outcome: {publish_error}");
                    }
                    controller.mark_escalated_without_run(
                        &issue.id,
                        format!("snapshot storage unavailable: {error}"),
                    );
                    return;
                }
            };
            let outcome = controller.run(&issue).await;
            let state = controller.state(&issue.id);
            if let Some(terminal) = state
                .as_ref()
                .and_then(|state| state.terminal_user_outcome.as_ref())
            {
                match publish_workflow_terminal(
                    &chat_history_path,
                    &project_id,
                    &issue.id,
                    terminal,
                    events.as_deref(),
                ) {
                    Ok(_) => {}
                    Err(error) => {
                        tracing::error!(workflow_id = %issue.id, "failed to persist workflow terminal outcome: {error}");
                    }
                }
            }
            // The gate is closed by run() before aborting any timed-out turn.
            for harness in &harnesses {
                harness.abort();
            }
            let idle = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while harnesses
                    .iter()
                    .any(|h| h.state().phase != llm_harness_types::HarnessPhase::Idle)
                {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .is_ok();
            if outcome.is_ok()
                && idle
                && let Err(error) = controller.complete_snapshot_history(&issue.id)
            {
                tracing::warn!(%error, "snapshot history remains protected: completion metadata unavailable");
            }
            drop(lease);
            controller.retain_snapshot_history(&issue.id);
            if let Err(error) = outcome {
                tracing::warn!(workflow_id = %issue.id, "controlled workflow stopped: {error}");
            }
        }));
        Ok(issue_id)
    }

    fn stop_workflow(&self) {
        if let Some(task) = self.workflow_task.lock().take() {
            task.abort();
        }
    }

    /// True while the spawned workflow task is still live. Terminal paths
    /// (FINISH / ESCALATE / error) all end in this task returning, so this is
    /// the single ownership signal the 409 gate checks.
    pub fn workflow_running(&self) -> bool {
        self.workflow_task
            .lock()
            .as_ref()
            .is_some_and(|task| !task.is_finished())
    }

    /// The team's isolated data dir (issues/memory/sessions live here).
    pub fn data_dir(&self) -> &Path {
        self.issues.root().parent().unwrap_or(Path::new("."))
    }
}

/// Manual `Debug`: `AgentTeam` internals (runner tasks, inboxes) are not
/// `Debug` and are noise for diagnostics anyway.
impl std::fmt::Debug for TeamEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamEntry")
            .field("id", &self.id)
            .field("repo_path", &self.repo_path)
            .field("issues", &self.issues)
            .finish_non_exhaustive()
    }
}

/// Owns all registered teams.
pub struct TeamManager {
    data_root: PathBuf,
    teams: HashMap<String, TeamEntry>,
    evaluation_only: bool,
}

impl Drop for TeamManager {
    fn drop(&mut self) {
        for (_, entry) in self.teams.drain() {
            shutdown_entry(entry);
        }
    }
}

impl TeamManager {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
            teams: HashMap::new(),
            evaluation_only: std::env::var_os("STUDIO_EVALUATION").is_some(),
        }
    }

    /// Immutable process policy: every team remains confined, even if an
    /// operator grant exists. There is deliberately no API to turn this off.
    pub fn evaluation_only(mut self) -> Self {
        self.evaluation_only = true;
        self
    }

    pub fn execution_mode(&self) -> &'static str {
        if self.evaluation_only {
            "evaluation"
        } else {
            "development"
        }
    }

    /// Root directory holding all per-team state (for config/display).
    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    /// Create a team from a spec: validate the repo (if configured),
    /// assemble via the engine, persist the spec.
    pub async fn create(
        &mut self,
        spec: TeamSpec,
        repo_path: Option<PathBuf>,
        settings: &crate::settings::Settings,
        api_key: &str,
    ) -> Result<&TeamEntry, TeamManagerError> {
        // Validate identity before using it for policy or filesystem paths.
        engine::validate_spec(&spec)?;
        if self.teams.contains_key(&spec.id) {
            return Err(TeamManagerError::AlreadyRegistered(spec.id.clone()));
        }
        if let Some(repo) = &repo_path
            && !repo.join(".git").exists()
        {
            return Err(TeamManagerError::NotAGitRepo(repo.clone()));
        }
        let snapshot_policy = crate::snapshot_storage::SnapshotPolicy::load(&self.data_root)?;
        let team_id = spec.id.clone();
        let team_dir = self.data_root.join("teams").join(&team_id);

        // The issues directory is the one sanctioned write target outside
        // the repo workspace (scout patrol, review bookkeeping). It must
        // exist — and must not be a symlink or other redirection — before
        // sandbox construction, or BwrapSandbox::new rejects the allowlist
        // entry. Confinement failures must fail closed.
        let team_issues_dir = team_dir.join("issues");
        if let Some(parent) = team_issues_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::symlink_metadata(&team_issues_dir) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(TeamManagerError::Io(std::io::Error::other(format!(
                    "team issues directory must be a real directory, got symlink or other: {}",
                    team_issues_dir.display()
                ))));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&team_issues_dir)?;
            }
            Err(error) => return Err(error.into()),
        }

        let working_dir = repo_path.clone().unwrap_or_else(|| team_dir.clone());
        crate::execution_policy::protect_policy_store(&self.data_root, &working_dir)?;
        // sandbox_mode is an observation, never an authorization input.
        // Evaluation overrides all grants and never falls back to host access.
        let trusted_host = !self.evaluation_only
            && crate::execution_policy::is_authorized(&self.data_root, &spec.id, &working_dir)?;
        let (env, sandbox_mode): (Arc<dyn ExecutionEnv>, &'static str) = if trusted_host {
            (Arc::new(OsEnv::new(working_dir.clone())), "trusted-host")
        } else {
            let sandbox = BwrapSandbox::new(SandboxConfig {
                fs_allowlist: vec![team_dir.join("issues")],
                fs_denylist: vec![],
                net_allowlist: vec![],
                resource_limits: ResourceLimits {
                    max_cpus: None,
                    max_processes: None,
                    max_memory_mb: None,
                    max_disk_mb: None,
                    timeout: None,
                },
                work_dir: Some(working_dir.clone()),
                trust_profile: Default::default(),
                host_binds: Vec::new(),
            })
            .map_err(|e| std::io::Error::other(format!("sandbox required: {e}")))?;
            (start_confined_sandbox(sandbox).await?, "bwrap")
        };
        let mut spec = spec;
        spec.sandbox_mode = Some(sandbox_mode.to_string());
        let assembled = engine::assemble(engine::AssembleInputs {
            spec: spec.clone(),
            settings: settings.clone(),
            api_key: api_key.to_string(),
            env,
            data_dir: team_dir.clone(),
            repo_path: repo_path.clone(),
        })
        .await?;

        std::fs::create_dir_all(team_dir.join("issues"))?;
        let issues = IssueStore::new(team_dir.join("issues"));

        let workflow = assembled
            .workflow_gate
            .as_ref()
            .map(|gate| -> std::io::Result<_> {
                Ok(Arc::new(
                    crate::pipeline::PipelineController::new(
                        &team_id,
                        assembled
                            .team
                            .social_context()
                            .expect("assembled team")
                            .clone(),
                        repo_path.clone().unwrap_or_else(|| team_dir.clone()),
                        &spec
                            .members
                            .iter()
                            .map(|m| m.id.clone())
                            .collect::<Vec<_>>(),
                        crate::pipeline::DEFAULT_MAX_STEPS,
                    )
                    .with_gate(gate.clone())
                    .with_snapshot_policy(snapshot_policy.clone())?,
                ))
            })
            .transpose()?;
        persist_spec(&self.data_root, &spec);
        self.teams.insert(
            team_id.clone(),
            TeamEntry {
                id: team_id.clone(),
                spec,
                repo_path,
                team: assembled.team,
                issues,
                handoffs: assembled.handoffs,
                workflow,
                workflow_task: parking_lot::Mutex::new(None),
                mcp_lifecycle: assembled.mcp_lifecycle,
            },
        );
        Ok(self.teams.get(team_id.as_str()).expect("just inserted"))
    }

    pub fn get(&self, id: &str) -> Option<&TeamEntry> {
        self.teams.get(id)
    }

    /// Mutable test/management access to one registered team.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut TeamEntry> {
        self.teams.get_mut(id)
    }

    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.teams.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Cancel one team's running controlled workflow task without shutting
    /// the team down. Publishes the operator terminal outcome to the chat
    /// journal + live bus (the aborted task can no longer do it), then aborts
    /// the spawned task and the member harnesses. The team itself stays alive
    /// so the operator can start the next workflow on the same instance.
    pub fn stop_workflow(
        &mut self,
        id: &str,
        events: Option<Arc<crate::events::EventBus>>,
    ) -> Result<(), TeamManagerError> {
        let (issue_id, chat_history_path, project_id, terminal) = {
            let entry = self
                .teams
                .get(id)
                .ok_or_else(|| TeamManagerError::UnknownTeam(id.to_string()))?;
            let controller = entry
                .workflow
                .as_ref()
                .ok_or_else(|| TeamManagerError::Workflow("team is not controlled".into()))?;
            let issue_id = controller
                .all_states()
                .into_iter()
                .find(|state| state.terminal_user_outcome.is_some())
                .map(|state| state.issue_id)
                .ok_or_else(|| TeamManagerError::Workflow("no terminal workflow state".into()))?;
            (
                issue_id.clone(),
                entry.data_dir().join("chat.sqlite3"),
                entry.id.clone(),
                controller
                    .state(&issue_id)
                    .and_then(|state| state.terminal_user_outcome),
            )
        };
        // The chat journal's `message_id` dedup keeps this a no-op when the
        // aborted task still managed to publish on its own.
        if let Some(terminal) = terminal.as_ref()
            && let Err(error) = publish_workflow_terminal(
                &chat_history_path,
                &project_id,
                &issue_id,
                terminal,
                events.as_deref(),
            )
        {
            tracing::error!(workflow_id = %issue_id, "failed to persist workflow terminal outcome: {error}");
        }
        let entry = self
            .teams
            .get_mut(id)
            .ok_or_else(|| TeamManagerError::UnknownTeam(id.to_string()))?;
        entry.stop_workflow();
        // Abort in-flight member turns; the team itself stays alive so the
        // operator can start the next workflow on the same instance.
        for member in &entry.spec.members {
            if let Some(harness) = entry
                .team
                .harness(&llm_harness_agent_team::AgentId::new(&member.id))
            {
                harness.abort();
            }
        }
        Ok(())
    }

    /// Shut down one team and drop it (issues/memory persist).
    pub async fn shutdown(&mut self, id: &str) -> Result<(), TeamManagerError> {
        let entry = self
            .teams
            .remove(id)
            .ok_or_else(|| TeamManagerError::UnknownTeam(id.to_string()))?;
        shutdown_entry(entry);
        Ok(())
    }

    pub async fn shutdown_all(&mut self) {
        for (_, entry) in self.teams.drain() {
            shutdown_entry(entry);
        }
    }

    /// Delete a team and its isolated data directory, including chat,
    /// issues, memory, sessions, inbox and timers.
    pub async fn delete(&mut self, id: &str) -> Result<(), TeamManagerError> {
        if !self.teams.contains_key(id) {
            return Err(TeamManagerError::UnknownTeam(id.into()));
        }
        crate::execution_policy::revoke(&self.data_root, id)?;
        let entry = self
            .teams
            .remove(id)
            .ok_or_else(|| TeamManagerError::UnknownTeam(id.to_string()))?;
        let data_dir = entry.data_dir().to_path_buf();
        shutdown_entry(entry);
        std::fs::remove_dir_all(&data_dir).map_err(|source| TeamManagerError::DeleteTeam {
            path: data_dir.clone(),
            source,
        })?;
        Ok(())
    }

    /// Cleanly stop and immediately rebuild one team from its persisted spec.
    /// Pending timer rows are captured and rewritten so a deliberate restart
    /// does not lose scheduled work.
    pub async fn restart(
        &mut self,
        id: &str,
        settings: &crate::settings::Settings,
        api_key: &str,
    ) -> Result<(), TeamManagerError> {
        let entry = self
            .teams
            .get(id)
            .ok_or_else(|| TeamManagerError::UnknownTeam(id.to_string()))?;
        let spec = entry.spec.clone();
        let repo_path = entry.repo_path.clone();
        let timer_path = entry.data_dir().join("timers.sqlite3");
        let pending_timers = TimeoutJournal::open(&timer_path)?.restore()?;

        self.shutdown(id).await?;
        let journal = TimeoutJournal::open(&timer_path)?;
        for timer in pending_timers {
            journal.schedule(&timer.request)?;
        }
        self.create(spec, repo_path, settings, api_key).await?;
        Ok(())
    }

    /// Update one member: persona + role_label always hot-apply
    /// (`set_system_prompt`); toolkits/model/provider changes rebuild the
    /// member's harness via [`AgentTeam::replace_member`]. The instance
    /// spec is updated and persisted either way.
    pub async fn update_member(
        &mut self,
        team_id: &str,
        member: crate::team_spec::MemberSpec,
        settings: &crate::settings::Settings,
        api_key: &str,
        key_changed: bool,
    ) -> Result<MemberUpdate, TeamManagerError> {
        let entry = self
            .teams
            .get_mut(team_id)
            .ok_or_else(|| TeamManagerError::UnknownTeam(team_id.to_string()))?;
        if entry.workflow_running() {
            return Err(TeamManagerError::Workflow(
                "cannot reconfigure an active workflow".into(),
            ));
        }
        let old = entry
            .spec
            .members
            .iter()
            .find(|m| m.id == member.id)
            .ok_or_else(|| TeamManagerError::UnknownMember(member.id.clone()))?
            .clone();

        engine::validate_toolkits(&member)?;
        // Persona hot application must preserve the same Novel V1 binding
        // checked by initial assembly; never reintroduce a coding persona.
        if crate::novel_v1::is_novel(&entry.spec) {
            let mut candidate = entry.spec.clone();
            if let Some(slot) = candidate.members.iter_mut().find(|m| m.id == member.id) {
                *slot = member.clone();
            }
            crate::novel_v1::validate_binding(&candidate)?;
        }

        // Structural fields: always persisted into the spec.
        let structural_change = key_changed
            || old.toolkits != member.toolkits
            || old.model != member.model
            || old.role_label != member.role_label
            || old.provider_override != member.provider_override;

        let update = if structural_change {
            // Rebuild: resolve the effective model and build providers.
            let model = member
                .provider_override
                .as_ref()
                .and_then(|o| o.model.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| member.model.resolve(&settings.models));
            let mut member_for_providers = member.clone();
            member_for_providers.model = crate::team_spec::ModelRef::Explicit(model.clone());

            // 成员级 key 只存 0600 secrets 文件（C1）；rebuild 前注入进
            // override 供 build_member_providers 消费。这个带明文的副本只
            // 用于装配，不进入 entry.spec / team.json。
            if let Some(ov) = member_for_providers.provider_override.as_mut()
                && ov.api_key.as_deref().map(str::is_empty).unwrap_or(false)
            {
                let path = self
                    .data_root
                    .join("teams")
                    .join(team_id)
                    .join("secrets")
                    .join(&member.id);
                if let Ok(key) = std::fs::read_to_string(&path) {
                    ov.api_key = Some(key);
                }
            }
            let mut providers =
                engine::build_member_providers(&member_for_providers, settings, api_key);
            if providers.is_empty() {
                providers = crate::env_cfg::providers_from_factory();
            }
            if providers.is_empty() {
                return Err(TeamManagerError::NoProviders);
            }
            // 显式模型名必须能匹配到 provider：若没有恰好等于该 model 的
            // pattern，补一条把现有 client 映射到显式模型名（与 build 的
            // fallback 语义一致：未匹配时用第一个 client）。
            if !providers.iter().any(|(m, _)| *m == model) {
                let client = providers[0].1.clone();
                providers.push((model.clone(), client));
            }
            let spec = IndividualSpec::new(&member.id, &member.role_label, model).configure(|b| {
                let mut b = b.system_prompt(Some(member.persona.clone()));
                if let Some(workflow) = &entry.workflow {
                    b = b.before_tool_call_hook(Arc::new(crate::pipeline::WorkflowToolGuard::new(
                        &member.id,
                        workflow.gate(),
                    )));
                }
                for tk in &member.toolkits {
                    b = match tk.as_str() {
                        "fs" => b.install(&llm_harness_tools::FsToolsPlugin::new(None)),
                        "web" => b.install(&llm_harness_tools::WebToolsPlugin::default()),
                        _ => b,
                    };
                }
                b
            });
            entry
                .team
                .replace_member(&member.id, spec, providers)
                .await?;
            MemberUpdate::Rebuilt
        } else {
            // Hot-apply persona on the live harness.
            if let Some(h) = entry
                .team
                .harness(&llm_harness_agent_team::AgentId::new(&member.id))
            {
                h.set_system_prompt(Some(member.persona.clone()));
            }
            MemberUpdate::HotApplied
        };

        // Persist the new spec (replace in place).
        if let Some(slot) = entry.spec.members.iter_mut().find(|m| m.id == member.id) {
            *slot = member;
        }
        crate::team_manager::persist_spec(&self.data_root, &entry.spec);
        Ok(update)
    }

    /// Apply the current template to members that still match the creation
    /// baseline. User-overridden members are left untouched.
    pub async fn apply_template_upgrade(
        &mut self,
        team_id: &str,
        template: crate::template_store::LoadedTemplate,
        settings: &crate::settings::Settings,
        api_key: &str,
    ) -> Result<TemplateUpgradeOutcome, TeamManagerError> {
        crate::engine::validate_spec(&template.spec)?;
        let template_members = template.spec.members;
        template_members
            .iter()
            .try_for_each(crate::engine::validate_toolkits)?;
        let old_members = self
            .teams
            .get(team_id)
            .ok_or_else(|| TeamManagerError::UnknownTeam(team_id.to_string()))?
            .spec
            .members
            .clone();
        let baseline_members = self
            .teams
            .get(team_id)
            .ok_or_else(|| TeamManagerError::UnknownTeam(team_id.to_string()))?
            .spec
            .template_baseline
            .as_ref()
            .map(|baseline| baseline.members.clone());

        let old_ids: std::collections::HashSet<&str> =
            old_members.iter().map(|m| m.id.as_str()).collect();
        let template_ids: std::collections::HashSet<&str> =
            template_members.iter().map(|m| m.id.as_str()).collect();
        let added: Vec<String> = template_members
            .iter()
            .filter(|m| !old_ids.contains(m.id.as_str()))
            .map(|m| m.id.clone())
            .collect();
        let removed: Vec<String> = old_members
            .iter()
            .filter(|m| !template_ids.contains(m.id.as_str()))
            .map(|m| m.id.clone())
            .collect();
        if !added.is_empty() || !removed.is_empty() {
            return Err(TeamManagerError::MembershipChanged { added, removed });
        }

        let template_by_id = |id: &str| {
            template_members
                .iter()
                .find(|m| m.id == id)
                .cloned()
                .expect("membership equality checked before upgrade")
        };
        let would_follow = |member: &crate::team_spec::MemberSpec| match &baseline_members {
            Some(baseline) => baseline
                .iter()
                .find(|old| old.id == member.id)
                .is_some_and(|old| old == member),
            None => template_by_id(&member.id) == *member,
        };
        let mut outcome = TemplateUpgradeOutcome {
            version: template.version.clone(),
            upgraded: Vec::new(),
            overridden: Vec::new(),
            rebuilt: Vec::new(),
        };
        for old in &old_members {
            let member = template_by_id(&old.id);
            if !would_follow(old) {
                outcome.overridden.push(old.id.clone());
                continue;
            }
            if old == &member {
                continue;
            }
            let behaviors_changed = old.behaviors != member.behaviors;
            let update = self
                .update_member(team_id, member.clone(), settings, api_key, false)
                .await?;
            if update == MemberUpdate::Rebuilt {
                outcome.rebuilt.push(member.id.clone());
            }
            outcome.upgraded.push(member.id.clone());
            if behaviors_changed
                && member
                    .behaviors
                    .iter()
                    .any(|b| matches!(b, crate::team_spec::Behavior::Patrol { .. }))
            {
                let entry = self
                    .teams
                    .get(team_id)
                    .expect("entry must exist after member update");
                for behavior in &member.behaviors {
                    if let crate::team_spec::Behavior::Patrol {
                        interval_secs,
                        task,
                        issues_dir,
                    } = behavior
                    {
                        let dir = issues_dir.clone().unwrap_or_else(|| {
                            entry
                                .data_dir()
                                .join("issues")
                                .to_string_lossy()
                                .to_string()
                        });
                        let text = if task.is_empty() {
                            engine::default_patrol_task(&dir, *interval_secs)
                        } else {
                            task.clone()
                        };
                        if let Err(e) = entry.team.submit(&member.id, &text) {
                            tracing::error!("patrol arming failed for {}: {e}", member.id);
                        }
                    }
                }
            }
        }

        let entry = self
            .teams
            .get_mut(team_id)
            .expect("entry must exist after member updates");
        entry.spec.template = Some(crate::team_spec::TemplateRef {
            template_id: template.id,
            version: template.version,
        });
        entry.spec.template_baseline = Some(crate::team_spec::TemplateBaseline {
            version: outcome.version.clone(),
            members: template_members,
        });
        entry.handoffs = engine::handoff_routes(&entry.spec);
        crate::team_manager::persist_spec(&self.data_root, &entry.spec);
        Ok(outcome)
    }

    /// Apply the confirmed-issue handoff routes: for each route whose
    /// resource is "issues", submit the rendered template to the target
    /// via the operator channel. Returns the targets that were notified.
    pub fn apply_issue_handoff(
        &self,
        team_id: &str,
        issue: &crate::issue::Issue,
        events: Option<Arc<crate::events::EventBus>>,
    ) -> Result<Vec<String>, TeamManagerError> {
        let entry = self
            .teams
            .get(team_id)
            .ok_or_else(|| TeamManagerError::UnknownTeam(team_id.to_string()))?;
        if entry.workflow.is_some() {
            entry.start_workflow(issue.clone(), events)?;
            return Ok(vec![crate::pipeline::ORCHESTRATOR_ID.into()]);
        }
        let mut notified = Vec::new();
        for route in &entry.handoffs {
            if route.resource != "issues" {
                continue;
            }
            let text = route
                .message_template
                .replace("{id}", &issue.id)
                .replace("{title}", &issue.title)
                .replace("{body}", &issue.body);
            if let Err(e) = entry.team.submit(&route.to, &text) {
                tracing::error!("handoff to {} failed: {e}", route.to);
                continue;
            }
            notified.push(route.to.clone());
        }
        Ok(notified)
    }
}

/// Write the spec to `<data_root>/teams/<id>/team.json`.
pub fn persist_spec(data_root: &Path, spec: &TeamSpec) {
    let dir = data_root.join("teams").join(&spec.id);
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string_pretty(spec) {
        let _ = std::fs::write(dir.join("team.json"), json + "\n");
    }
}

/// Load all persisted specs under `<data_root>/teams/`.
pub fn list_persisted(data_root: &Path) -> Vec<TeamSpec> {
    let dir = data_root.join("teams");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    let mut specs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path().join("team.json");
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(spec) = serde_json::from_str::<TeamSpec>(&text)
        {
            specs.push(spec);
        }
    }
    specs.sort_by(|a, b| a.id.cmp(&b.id));
    specs
}

/// Remove the persisted spec (team delete).
pub fn unpersist_spec(data_root: &Path, id: &str) {
    let path = data_root.join("teams").join(id).join("team.json");
    let _ = std::fs::remove_file(path);
}

/// Stop runners; durable pending rows stay in the SQLite inbox journal.
fn shutdown_entry(mut entry: TeamEntry) {
    entry.stop_workflow();
    entry.team.shutdown();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team_spec::{Behavior, MemberSpec, ModelRef, Trigger};
    use crate::{env_cfg, roles::ModelTier};

    fn unique_dir(prefix: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()))
    }

    fn make_repo() -> PathBuf {
        let dir = unique_dir("ct-tm-repo");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    fn settings() -> crate::settings::Settings {
        crate::settings::Settings {
            models: ModelTier {
                strong: "m".into(),
                main: "m".into(),
                cheap: "m".into(),
            },
            scout_interval_secs: 0,
            base_url: String::new(),
        }
    }

    fn member(id: &str) -> MemberSpec {
        MemberSpec {
            id: id.into(),
            persona: "worker".into(),
            role_label: "r".into(),
            model: ModelRef::Tier("main".into()),
            toolkits: vec![],
            behaviors: vec![],
            provider_override: None,
            persona_file: None,
        }
    }

    fn blank_spec() -> TeamSpec {
        TeamSpec {
            id: "t".into(),
            template: None,
            template_baseline: None,
            name: "T".into(),
            repo_path: None,
            sandbox_mode: None,
            members: vec![member("a"), member("b")],
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn unsupported_sandbox_remains_fail_closed() {
        let error = start_confined_sandbox(BwrapSandbox)
            .await
            .err()
            .expect("unsupported sandbox must reject execution");
        assert_eq!(
            error.to_string(),
            "sandbox required: BwrapSandbox is only available on Linux"
        );
        let data = tempfile::tempdir().unwrap();
        let mut manager = TeamManager::new(data.path().to_path_buf());
        let error = manager
            .create(blank_spec(), None, &settings(), "mock")
            .await
            .expect_err("an ungranted team must not receive a host environment");
        assert!(matches!(&error, TeamManagerError::Io(_)));
        assert_eq!(
            error.to_string(),
            "sandbox required: BwrapSandbox is only available on Linux"
        );
        assert!(manager.get("t").is_none());
    }

    #[must_use]
    fn install_mock_factory() -> env_cfg::TestProviderFactoryGuard {
        env_cfg::install_test_provider_factory(std::sync::Arc::new(|| {
            vec![(
                "m".into(),
                std::sync::Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]))
                    as std::sync::Arc<dyn llm_harness_loop::LlmClient>,
            )]
        }))
    }

    #[test]
    fn workflow_terminal_is_journaled_and_projected_only_once() {
        let root = std::env::temp_dir().join(format!("ct-terminal-once-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let journal_path = root.join("chat.sqlite3");
        let events = crate::events::EventBus::new();
        let finish = crate::pipeline::TerminalUserOutcome {
            attribution: crate::pipeline::TerminalAttribution::Orchestrator,
            body: "## Human review package\nFinal answer".into(),
        };
        let later = crate::pipeline::TerminalUserOutcome {
            attribution: crate::pipeline::TerminalAttribution::WorkflowStatus,
            body: "later failure".into(),
        };
        assert!(
            publish_workflow_terminal(&journal_path, "team", "issue-1", &finish, Some(&events))
                .unwrap()
        );
        assert!(
            !publish_workflow_terminal(&journal_path, "team", "issue-1", &later, Some(&events))
                .unwrap()
        );
        let journal = llm_harness_agent_team::ChatHistoryJournal::open(&journal_path).unwrap();
        let history = journal.group_history(10, None).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0].message.from.as_str(),
            crate::pipeline::ORCHESTRATOR_ID
        );
        assert_eq!(history[0].message.body, finish.body);
        assert_eq!(
            history[0].message.message_id.as_deref(),
            Some("issue-1:terminal")
        );
        let projected: serde_json::Value = serde_json::from_str(&events.replay()[0]).unwrap();
        assert_eq!(events.replay().len(), 1);
        assert_eq!(projected["message_id"], "issue-1:terminal");
        assert_eq!(projected["text"], history[0].message.body);
        assert_eq!(projected["from"], crate::pipeline::ORCHESTRATOR_ID);
        assert!(
            publish_workflow_terminal(&journal_path, "team", "issue-2", &later, Some(&events))
                .unwrap()
        );
        let second: serde_json::Value = serde_json::from_str(&events.replay()[1]).unwrap();
        assert_eq!(second["from"], crate::pipeline::WORKFLOW_STATUS_ID);
        assert_eq!(second["message_id"], "issue-2:terminal");
        assert_eq!(
            journal.group_history(10, None).unwrap()[1].message.body,
            later.body
        );
    }

    #[test]
    fn terminal_persistence_failure_emits_error_without_chat_message() {
        let root =
            std::env::temp_dir().join(format!("ct-terminal-failure-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let journal_path = root.join("chat.sqlite3");
        std::fs::create_dir(&journal_path).unwrap();
        let events = crate::events::EventBus::new();
        let terminal = crate::pipeline::TerminalUserOutcome {
            attribution: crate::pipeline::TerminalAttribution::WorkflowStatus,
            body: "Workflow stopped: real failure".into(),
        };
        assert!(
            publish_workflow_terminal(&journal_path, "team", "issue-2", &terminal, Some(&events))
                .is_err()
        );
        let projected: serde_json::Value = serde_json::from_str(&events.replay()[0]).unwrap();
        assert_eq!(events.replay().len(), 1);
        assert_eq!(projected["type"], "agent_error");
        assert_eq!(projected["agent"], crate::pipeline::WORKFLOW_STATUS_ID);
        assert!(
            projected["text"]
                .as_str()
                .unwrap()
                .contains("could not be saved")
        );
    }

    #[tokio::test]
    async fn create_builds_team_and_persists_spec() {
        let _factory = install_mock_factory();
        let root = unique_dir("ct-tm");
        let mut tm = TeamManager::new(&root);
        tm.create(blank_spec(), Some(make_repo()), &settings(), "")
            .await
            .unwrap();
        assert!(root.join("teams").join("t").join("team.json").exists());
        assert!(tm.get("t").is_some());
        assert!(tm.get("t").unwrap().issues.root().exists());
        assert_eq!(tm.ids(), vec!["t"]);
    }

    #[tokio::test]
    async fn create_without_repo_works() {
        let _factory = install_mock_factory();
        let root = unique_dir("ct-tm-norepo");
        let mut tm = TeamManager::new(&root);
        tm.create(blank_spec(), None, &settings(), "")
            .await
            .unwrap();
        let entry = tm.get("t").unwrap();
        assert!(entry.repo_path.is_none());
    }

    /// Regression: `start_workflow` locked `workflow_task` and then called
    /// `workflow_running()`, which re-locks the same non-reentrant
    /// parking_lot::Mutex — a guaranteed self-deadlock on every call, so the
    /// first operator chat never returned. The bounded watchdog below fails
    /// (instead of hanging) on the old implementation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_workflow_does_not_self_deadlock_on_running_check() {
        use std::time::Duration;

        let _factory = install_mock_factory();
        let root = unique_dir("ct-tm-wf");
        let mut spec = blank_spec();
        spec.id = "coding-team".into(); // is_controlled → workflow controller attached
        spec.template = Some(crate::team_spec::TemplateRef {
            template_id: "coding-team".into(),
            version: "test".into(),
        });
        let mut tm = TeamManager::new(&root);
        tm.create(spec, Some(make_repo()), &settings(), "")
            .await
            .unwrap();
        assert!(tm.get("coding-team").unwrap().workflow.is_some());

        let issue = crate::issue::Issue {
            id: "wf-deadlock-1".into(),
            status: crate::issue::IssueStatus::Confirmed,
            source: "test".into(),
            created: chrono::Utc::now().to_rfc3339(),
            title: "deadlock regression".into(),
            body: "noop".into(),
        };

        // start_workflow used to deadlock here before returning. The spawn
        // below isolates the blocking call from the async test thread; the
        // watchdog aborts it, converting the old hang into a test failure.
        let tm_shared = std::sync::Arc::new(tokio::sync::RwLock::new(tm));
        let call = {
            let tm_shared = tm_shared.clone();
            let issue = issue.clone();
            tokio::spawn(async move {
                let pm = tm_shared.read().await;
                let entry = pm.get("coding-team").unwrap();
                entry.start_workflow(issue, None)
            })
        };
        let result = tokio::time::timeout(Duration::from_secs(5), call).await;
        let outcome = result
            .expect("start_workflow must not deadlock: it re-locked workflow_task under the guard")
            .expect("start_workflow task panicked");
        let issue_id = outcome.expect("first workflow must start on a fresh team");
        assert_eq!(issue_id, issue.id);
        assert!(pm_shared_reads_ok(&tm_shared).await);
    }

    /// A2-3 regression: a workflow that cannot acquire its snapshot lease
    /// (real IO error, not WouldBlock) must terminalize — the chat journal
    /// gets exactly one Escalated terminal message and the controller state
    /// is Escalated with a terminal outcome — instead of silently vanishing
    /// after the 202 was already returned. WouldBlock contention keeps its
    /// internal 50 ms retry and is unaffected.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_lease_failure_publishes_terminal_outcome() {
        let _factory = install_mock_factory();
        let root = unique_dir("ct-tm-lease");
        let repo = make_repo();
        let mut spec = blank_spec();
        spec.id = "coding-team".into(); // is_controlled → workflow controller attached
        spec.template = Some(crate::team_spec::TemplateRef {
            template_id: "coding-team".into(),
            version: "test".into(),
        });
        let mut tm = TeamManager::new(&root);
        tm.create(spec, Some(repo.clone()), &settings(), "")
            .await
            .unwrap();
        let events = std::sync::Arc::new(crate::events::EventBus::new());
        let issue = crate::issue::Issue {
            id: "wf-lease-1".into(),
            status: crate::issue::IssueStatus::Confirmed,
            source: "test".into(),
            created: chrono::Utc::now().to_rfc3339(),
            title: "lease failure regression".into(),
            body: "noop".into(),
        };

        // Sabotage lease acquisition with a permanent (non-WouldBlock) error:
        // replace the repo's .pipeline directory with a symlink, which
        // safe_dir rejects.
        let pipeline = repo.join(".pipeline");
        std::fs::create_dir_all(&pipeline).unwrap();
        std::fs::remove_dir_all(&pipeline).unwrap();
        let elsewhere = repo.join(".pipeline-real-elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &pipeline).unwrap();

        let tm_shared = std::sync::Arc::new(tokio::sync::RwLock::new(tm));
        {
            let tm_shared = tm_shared.clone();
            let issue = issue.clone();
            let events = events.clone();
            tokio::spawn(async move {
                let pm = tm_shared.read().await;
                let entry = pm.get("coding-team").unwrap();
                entry.start_workflow(issue, Some(events))
            })
            .await
            .unwrap()
            .expect("workflow accepted");
        };

        // The spawned workflow task must publish the terminal outcome even
        // though the lease failed: chat journal has exactly one terminal
        // message, and the controller state is Escalated with an outcome.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let done = tm_shared
                    .read()
                    .await
                    .get("coding-team")
                    .unwrap()
                    .workflow
                    .as_ref()
                    .unwrap()
                    .state(&issue.id)
                    .is_some_and(|state| state.terminal_user_outcome.is_some());
                if done {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("lease failure must terminalize the workflow");

        let journal = llm_harness_agent_team::ChatHistoryJournal::open(
            root.join("teams").join("coding-team").join("chat.sqlite3"),
        )
        .unwrap();
        let history = journal.group_history(10, None).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0].message.from.as_str(),
            crate::pipeline::WORKFLOW_STATUS_ID
        );
        assert!(
            history[0]
                .message
                .body
                .contains("snapshot storage unavailable")
        );
        let state = tm_shared
            .read()
            .await
            .get("coding-team")
            .unwrap()
            .workflow
            .as_ref()
            .unwrap()
            .state(&issue.id)
            .unwrap();
        assert_eq!(state.phase, "escalated");
        assert_eq!(
            state.terminal_user_outcome.unwrap().attribution,
            crate::pipeline::TerminalAttribution::WorkflowStatus
        );
    }

    /// The workflow task keeps running against the mock client; give it a
    /// bounded window to finish, then confirm ownership released (the 409
    /// gate checks this via `workflow_running`, whose public semantics the
    /// fix must preserve).
    async fn pm_shared_reads_ok(pm: &std::sync::Arc<tokio::sync::RwLock<TeamManager>>) -> bool {
        use std::time::Duration;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if !pm
                    .read()
                    .await
                    .get("coding-team")
                    .unwrap()
                    .workflow_running()
                {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or(false)
    }

    #[tokio::test]
    async fn create_rejects_missing_repo() {
        let _factory = install_mock_factory();
        let root = unique_dir("ct-tm-badrepo");
        let mut tm = TeamManager::new(&root);
        let err = tm
            .create(
                blank_spec(),
                Some(unique_dir("no-such-repo")),
                &settings(),
                "",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, TeamManagerError::NotAGitRepo(_)));
    }

    #[tokio::test]
    async fn create_rejects_duplicate_id() {
        let _factory = install_mock_factory();
        let root = unique_dir("ct-tm-dup");
        let mut tm = TeamManager::new(&root);
        tm.create(blank_spec(), None, &settings(), "")
            .await
            .unwrap();
        let err = tm
            .create(blank_spec(), None, &settings(), "")
            .await
            .unwrap_err();
        assert!(matches!(err, TeamManagerError::AlreadyRegistered(_)));
    }

    #[tokio::test]
    async fn invalid_spec_does_not_create_team_directory() {
        let _factory = install_mock_factory();
        let root = unique_dir("ct-tm-invalid-id");
        let mut tm = TeamManager::new(&root);
        let mut spec = blank_spec();
        spec.id = "../escaped".into();
        let err = tm.create(spec, None, &settings(), "").await.unwrap_err();
        assert!(matches!(
            err,
            TeamManagerError::Assemble(engine::AssembleError::InvalidTeamId(_))
        ));
        assert!(!root.join("teams").exists());
    }

    #[tokio::test]
    async fn template_upgrade_updates_unmodified_members_only() {
        let _factory = install_mock_factory();
        let root = unique_dir("ct-tm-upgrade");
        let mut tm = TeamManager::new(&root);
        let old_member = member("a");
        let baseline_member = member("b");
        let mut spec = blank_spec();
        spec.template = Some(crate::team_spec::TemplateRef {
            template_id: "example".into(),
            version: "1".into(),
        });
        spec.template_baseline = Some(crate::team_spec::TemplateBaseline {
            version: "1".into(),
            members: vec![old_member.clone(), baseline_member.clone()],
        });
        let mut overridden_member = baseline_member.clone();
        overridden_member.persona = "user override".into();
        spec.members = vec![old_member.clone(), overridden_member.clone()];
        tm.create(spec, None, &settings(), "").await.unwrap();

        let mut upgraded_member = old_member.clone();
        upgraded_member.persona = "upgraded persona".into();
        upgraded_member.behaviors = vec![Behavior::Handoff {
            on: Trigger::ResourceConfirmed {
                resource: "issues".into(),
            },
            to: "b".into(),
            message_template: "issue: {body}".into(),
        }];
        let mut template_member = overridden_member.clone();
        template_member.persona = "new template persona".into();
        let template = crate::template_store::LoadedTemplate {
            id: "example".into(),
            version: "2".into(),
            spec: crate::team_spec::TeamSpec {
                id: "example".into(),
                template: None,
                template_baseline: None,
                name: "Example".into(),
                repo_path: None,
                sandbox_mode: None,
                members: vec![upgraded_member.clone(), template_member],
            },
        };

        let outcome = tm
            .apply_template_upgrade("t", template, &settings(), "")
            .await
            .unwrap();
        assert_eq!(outcome.version, "2");
        assert_eq!(outcome.upgraded, vec!["a"]);
        assert_eq!(outcome.overridden, vec!["b"]);

        let entry = tm.get("t").unwrap();
        let upgraded_spec = entry.spec.members.iter().find(|m| m.id == "a").unwrap();
        let overridden_spec = entry.spec.members.iter().find(|m| m.id == "b").unwrap();
        assert_eq!(upgraded_spec.persona, "upgraded persona");
        assert_eq!(overridden_spec.persona, overridden_member.persona);
        assert_eq!(entry.spec.template.as_ref().unwrap().version, "2");
        assert_eq!(
            entry.handoffs,
            vec![engine::HandoffRoute {
                resource: "issues".into(),
                to: "b".into(),
                message_template: "issue: {body}".into(),
            }]
        );
        let harness = entry
            .team
            .harness(&llm_harness_agent_team::AgentId::new("a"))
            .unwrap();
        assert_eq!(
            harness.state().system_prompt,
            Some("upgraded persona".into())
        );
    }

    #[tokio::test]
    async fn template_upgrade_rejects_membership_changes() {
        let _factory = install_mock_factory();
        let root = unique_dir("ct-tm-upgrade-members");
        let mut tm = TeamManager::new(&root);
        let old_member = member("a");
        let mut spec = blank_spec();
        spec.template = Some(crate::team_spec::TemplateRef {
            template_id: "example".into(),
            version: "1".into(),
        });
        spec.template_baseline = Some(crate::team_spec::TemplateBaseline {
            version: "1".into(),
            members: vec![old_member.clone()],
        });
        spec.members = vec![old_member.clone(), member("b")];
        tm.create(spec, None, &settings(), "").await.unwrap();

        let template = crate::template_store::LoadedTemplate {
            id: "example".into(),
            version: "2".into(),
            spec: crate::team_spec::TeamSpec {
                id: "example".into(),
                template: None,
                template_baseline: None,
                name: "Example".into(),
                repo_path: None,
                sandbox_mode: None,
                members: vec![old_member.clone(), member("b"), member("c")],
            },
        };
        let err = tm
            .apply_template_upgrade("t", template, &settings(), "")
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "template membership changed; upgrade requires explicit member management: added=c, removed="
        );
    }

    #[tokio::test]
    async fn handoff_route_drives_issue_confirmation() {
        let _factory = install_mock_factory();
        let root = unique_dir("ct-tm-handoff");
        let mut spec = blank_spec();
        spec.members[0].behaviors = vec![Behavior::Handoff {
            on: Trigger::ResourceConfirmed {
                resource: "issues".into(),
            },
            to: "b".into(),
            message_template: "已确认: {body}".into(),
        }];
        let mut tm = TeamManager::new(&root);
        tm.create(spec, None, &settings(), "").await.unwrap();
        let store = IssueStore::new(root.join("teams").join("t").join("issues"));
        let issue = store.create("scout", "t", "body-x").unwrap();
        let loaded = store.load(&issue.id).unwrap();
        let notified = tm.apply_issue_handoff("t", &loaded.0, None).unwrap();
        assert_eq!(notified, vec!["b"]);
        let ctx = tm.get("t").unwrap().team.social_context().unwrap().clone();
        let got = ctx.drain_inbox(&llm_harness_agent_team::AgentId::new("b"));
        assert!(
            got.iter().any(|m| m.body.contains("已确认: body-x")),
            "target inbox must carry rendered handoff"
        );
    }

    #[tokio::test]
    async fn no_handoff_route_means_no_submission() {
        let _factory = install_mock_factory();
        let root = unique_dir("ct-tm-nohandoff");
        let mut tm = TeamManager::new(&root);
        tm.create(blank_spec(), None, &settings(), "")
            .await
            .unwrap();
        let store = IssueStore::new(root.join("teams").join("t").join("issues"));
        let issue = store.create("scout", "t", "b").unwrap();
        let notified = tm
            .apply_issue_handoff("t", &store.load(&issue.id).unwrap().0, None)
            .unwrap();
        assert!(notified.is_empty());
    }

    #[tokio::test]
    async fn shutdown_removes_but_persists_data() {
        let _factory = install_mock_factory();
        let root = unique_dir("ct-tm-shutdown");
        let mut tm = TeamManager::new(&root);
        tm.create(blank_spec(), None, &settings(), "")
            .await
            .unwrap();
        tm.shutdown("t").await.unwrap();
        assert!(tm.get("t").is_none());
        // 数据留存：issues 目录仍在
        assert!(root.join("teams").join("t").join("issues").exists());
    }

    #[tokio::test]
    async fn completed_chat_history_survives_restart() {
        let _factory = install_mock_factory();
        let root = unique_dir("ct-tm-chat-restart");
        let mut tm = TeamManager::new(&root);
        tm.create(blank_spec(), None, &settings(), "")
            .await
            .unwrap();
        let operator = llm_harness_agent_team::AgentId::new("operator");
        let member_a = llm_harness_agent_team::AgentId::new("a");

        let ctx = tm.get("t").unwrap().team.social_context().unwrap().clone();
        let broadcast = ctx
            .broadcast_with_message_id(&operator, "group prompt", None)
            .unwrap();
        ctx.send_direct_with_message_id(
            &member_a,
            &operator,
            "group reply",
            0,
            None,
            Some(broadcast.broadcast_id),
            None,
        )
        .unwrap();
        ctx.send_direct_with_message_id(
            &operator,
            &member_a,
            "private note",
            llm_harness_agent_team::priority::OPERATOR,
            None,
            None,
            None,
        )
        .unwrap();

        tm.shutdown("t").await.unwrap();
        let persisted_specs = list_persisted(&root);
        assert_eq!(persisted_specs.len(), 1);
        let mut restored = TeamManager::new(&root);
        restored
            .create(
                persisted_specs[0].clone(),
                persisted_specs[0].repo_hint(),
                &settings(),
                "",
            )
            .await
            .unwrap();

        let ctx = restored
            .get("t")
            .unwrap()
            .team
            .social_context()
            .unwrap()
            .clone();
        let group = ctx.group_history_entries(10, None).unwrap();
        assert_eq!(
            group
                .iter()
                .map(|entry| entry.message.body.as_str())
                .collect::<Vec<_>>(),
            vec!["group prompt", "group reply"]
        );
        let private = ctx
            .private_history_entries(&operator, &member_a, 10, None)
            .unwrap();
        assert_eq!(private.len(), 1);
        assert_eq!(private[0].message.body, "private note");
    }

    #[tokio::test]
    async fn delete_removes_chat_history_before_recreating_same_id() {
        let _factory = install_mock_factory();
        let root = unique_dir("ct-tm-chat-delete");
        let mut tm = TeamManager::new(&root);
        let spec = blank_spec();
        tm.create(spec.clone(), None, &settings(), "")
            .await
            .unwrap();
        let operator = llm_harness_agent_team::AgentId::new("operator");
        let ctx = tm.get("t").unwrap().team.social_context().unwrap().clone();
        ctx.broadcast_with_message_id(&operator, "old conversation", None)
            .unwrap();
        tm.delete("t").await.unwrap();
        assert!(!root.join("teams").join("t").exists());

        tm.create(spec, None, &settings(), "").await.unwrap();
        let ctx = tm.get("t").unwrap().team.social_context().unwrap().clone();
        assert!(ctx.group_history_entries(10, None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn persisted_team_restarts_with_issues_contacts_and_runner() {
        let _factory = env_cfg::install_test_provider_factory(std::sync::Arc::new(|| {
            vec![(
                "m".into(),
                std::sync::Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
                    llm_harness_loop::test_utils::MockResponse::tool_use_end_turn(
                        "restore-1",
                        "send_message",
                        r#"{"to": "operator", "body": "confirm restart"}"#,
                    ),
                ])) as std::sync::Arc<dyn llm_harness_loop::LlmClient>,
            )]
        }));
        let root = unique_dir("ct-tm-restart");
        let repo = make_repo();
        let mut tm = TeamManager::new(&root);
        tm.create(blank_spec(), Some(repo), &settings(), "")
            .await
            .unwrap();

        let issue_store = IssueStore::new(root.join("teams").join("t").join("issues"));
        let issue = issue_store
            .create("scout", "survives restart", "body")
            .unwrap();
        drop(issue_store);

        let expected_spec = tm.get("t").unwrap().spec.clone();
        assert_eq!(expected_spec.sandbox_mode.as_deref(), Some("bwrap"));

        tm.shutdown_all().await;
        assert!(tm.get("t").is_none());

        let specs = list_persisted(&root);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0], expected_spec);

        let mut restored = TeamManager::new(&root);
        let spec = specs.into_iter().next().unwrap();
        restored
            .create(spec.clone(), spec.repo_hint(), &settings(), "")
            .await
            .unwrap();

        let issue_store = IssueStore::new(root.join("teams").join("t").join("issues"));
        let (restored_issue, restored_path) = issue_store.load(&issue.id).unwrap();
        assert_eq!(restored_issue.title, "survives restart");
        assert!(restored_path.is_file());

        let ctx = restored
            .get("t")
            .unwrap()
            .team
            .social_context()
            .unwrap()
            .clone();
        let agent_a = llm_harness_agent_team::AgentId::new("a");
        let agent_b = llm_harness_agent_team::AgentId::new("b");
        let operator = llm_harness_agent_team::AgentId::new("operator");
        assert!(ctx.has_contact(&agent_a, &agent_b));
        assert!(ctx.has_contact(&agent_b, &agent_a));
        assert!(ctx.has_contact(&agent_a, &operator));

        restored
            .get("t")
            .unwrap()
            .team
            .submit("a", "confirm restart")
            .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let messages = ctx.drain_inbox(&operator);
            if messages
                .iter()
                .any(|message| message.body == "confirm restart")
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "restored runner did not process operator message"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn clean_shutdown_persists_and_restores_pending_inbox() {
        let _factory = install_mock_factory();
        let root = unique_dir("ct-tm-inbox-recovery");
        let mut tm = TeamManager::new(&root);
        tm.create(blank_spec(), Some(make_repo()), &settings(), "")
            .await
            .unwrap();
        let operator = llm_harness_agent_team::AgentId::new("operator");
        let agent_a = llm_harness_agent_team::AgentId::new("a");
        tm.get("t")
            .unwrap()
            .team
            .submit("a", "recover this task")
            .unwrap();

        tm.shutdown_all().await;
        assert!(root.join("teams/t/inbox.sqlite3").exists());

        let specs = list_persisted(&root);
        let mut restored = TeamManager::new(&root);
        let spec = specs.into_iter().next().unwrap();
        restored
            .create(spec.clone(), spec.repo_hint(), &settings(), "")
            .await
            .unwrap();
        let ctx = restored
            .get("t")
            .unwrap()
            .team
            .social_context()
            .unwrap()
            .clone();
        assert!(ctx.has_contact(&agent_a, &operator));
        assert!(!ctx.drain_inbox(&agent_a).is_empty());
    }

    #[tokio::test]
    async fn restored_team_owns_pending_timer_journal_row() {
        let _factory = install_mock_factory();
        let root = unique_dir("ct-tm-timer-recovery");
        let mut tm = TeamManager::new(&root);
        tm.create(blank_spec(), Some(make_repo()), &settings(), "")
            .await
            .unwrap();
        tm.shutdown_all().await;

        let timer_path = root.join("teams/t/timers.sqlite3");
        let timer_journal = llm_harness_agent_team::TimeoutJournal::open(&timer_path).unwrap();
        timer_journal
            .schedule(&llm_harness_agent_team::TimeoutRequest {
                agent_id: llm_harness_agent_team::AgentId::new("a"),
                key: Some("reminder:restart".into()),
                task_id: None,
                peer_id: None,
                body: "check results".into(),
                kind: llm_harness_agent_team::TimeoutKind::SelfReminder,
                fire_at_ms: i64::MAX,
            })
            .unwrap();

        let specs = list_persisted(&root);
        let mut restored = TeamManager::new(&root);
        let spec = specs.into_iter().next().unwrap();
        restored
            .create(spec.clone(), spec.repo_hint(), &settings(), "")
            .await
            .unwrap();

        assert_eq!(timer_journal.pending_count().unwrap(), 1);
        restored.shutdown_all().await;
        assert_eq!(timer_journal.pending_count().unwrap(), 0);
    }

    #[tokio::test]
    async fn restart_preserves_pending_inbox_and_timer() {
        let _factory = install_mock_factory();
        let root = unique_dir("ct-tm-restart-recovery");
        let mut tm = TeamManager::new(&root);
        tm.create(blank_spec(), Some(make_repo()), &settings(), "")
            .await
            .unwrap();

        let agent_a = llm_harness_agent_team::AgentId::new("a");
        let ctx = tm.get("t").unwrap().team.social_context().unwrap().clone();
        let timer = llm_harness_agent_team::TimeoutManager::new(ctx.clone());
        timer
            .set_self_reminder_durably(
                agent_a.clone(),
                "check after restart",
                std::time::Duration::from_secs(60),
                Some("restart-check".into()),
            )
            .unwrap();
        tm.get("t")
            .unwrap()
            .team
            .submit("a", "recover this task")
            .unwrap();

        tm.restart("t", &settings(), "").await.unwrap();

        let ctx = tm.get("t").unwrap().team.social_context().unwrap().clone();
        assert!(
            !ctx.drain_inbox(&agent_a).is_empty(),
            "pending inbox must survive a deliberate restart"
        );
        assert_eq!(
            ctx.pending_timer_count(),
            1,
            "pending timer must survive a deliberate restart"
        );

        tm.shutdown_all().await;
    }

    #[tokio::test]
    async fn operator_pump_acks_messages_after_publishing() {
        let _factory = env_cfg::install_test_provider_factory(std::sync::Arc::new(|| {
            vec![(
                "m".into(),
                std::sync::Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![
                    llm_harness_loop::test_utils::MockResponse::tool_use_end_turn(
                        "operator-1",
                        "send_message",
                        r#"{"to": "operator", "body": "operator ack test"}"#,
                    ),
                ])) as std::sync::Arc<dyn llm_harness_loop::LlmClient>,
            )]
        }));
        let root = unique_dir("ct-tm-operator-ack");
        let mut tm = TeamManager::new(&root);
        tm.create(blank_spec(), Some(make_repo()), &settings(), "")
            .await
            .unwrap();

        let ctx = tm.get("t").unwrap().team.social_context().unwrap().clone();
        let events = crate::events::EventBus::new();
        let mut event_rx = events.subscribe();
        crate::routes::spawn_operator_pump(
            Arc::new(events.clone()),
            Arc::new(parking_lot::Mutex::new(HashMap::new())),
            "t".into(),
            ctx.clone(),
        );

        tm.get("t")
            .unwrap()
            .team
            .submit("a", "produce operator ack test")
            .unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match event_rx.try_recv() {
                Ok(event)
                    if event.contains("\"team_message\"")
                        && event.contains("operator ack test") =>
                {
                    break;
                }
                _ => {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "operator pump did not publish the message"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        }

        let journal_path = root.join("teams").join("t").join("inbox.sqlite3");
        loop {
            let journal = llm_harness_agent_team::InboxJournal::open(&journal_path).unwrap();
            if journal.pending_count().unwrap() == 0 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "operator pump did not ack the published message"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        tm.shutdown_all().await;
        let specs = list_persisted(&root);
        let mut restored = TeamManager::new(&root);
        let spec = specs.into_iter().next().unwrap();
        let repo_hint = spec.repo_hint();
        restored
            .create(spec, repo_hint, &settings(), "")
            .await
            .unwrap();
        let restored_ctx = restored
            .get("t")
            .unwrap()
            .team
            .social_context()
            .unwrap()
            .clone();
        assert!(
            restored_ctx
                .drain_inbox(&llm_harness_agent_team::AgentId::new("operator"))
                .is_empty()
        );
    }

    #[test]
    fn list_persisted_reads_specs() {
        let root = unique_dir("ct-tm-list");
        let mut spec = blank_spec();
        spec.id = "p2".into();
        persist_spec(&root, &spec);
        let mut spec2 = blank_spec();
        spec2.id = "p1".into();
        persist_spec(&root, &spec2);
        let specs = list_persisted(&root);
        assert_eq!(
            specs.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["p1", "p2"]
        );
    }
}
