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
use llm_harness_sandbox::OsEnv;
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
    #[error("timer journal: {0}")]
    TimerJournal(#[from] TimerJournalError),
    #[error("team already registered: {0}")]
    AlreadyRegistered(String),
    #[error(transparent)]
    Team(#[from] TeamError),
    #[error("no provider for model tier")]
    NoProviders,
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
    /// Keeps MCP manager alive while the team runs; Drop cancels background tasks.
    pub mcp_lifecycle: Option<crate::engine::McpManagerLifecycle>,
}

impl TeamEntry {
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
        if self.teams.contains_key(&spec.id) {
            return Err(TeamManagerError::AlreadyRegistered(spec.id.clone()));
        }
        if let Some(repo) = &repo_path
            && !repo.join(".git").exists()
        {
            return Err(TeamManagerError::NotAGitRepo(repo.clone()));
        }
        let team_id = spec.id.clone();
        let team_dir = self.data_root.join("teams").join(&team_id);

        let working_dir = repo_path.clone().unwrap_or_else(|| team_dir.clone());
        let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new(working_dir));
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
        let old = entry
            .spec
            .members
            .iter()
            .find(|m| m.id == member.id)
            .ok_or_else(|| TeamManagerError::UnknownMember(member.id.clone()))?
            .clone();

        engine::validate_toolkits(&member)?;

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
                let b = b.system_prompt(Some(member.persona.clone()));
                let mut b = b;
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
    ) -> Result<Vec<String>, TeamManagerError> {
        let entry = self
            .teams
            .get(team_id)
            .ok_or_else(|| TeamManagerError::UnknownTeam(team_id.to_string()))?;
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
            members: vec![member("a"), member("b")],
        }
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
        let notified = tm.apply_issue_handoff("t", &loaded.0).unwrap();
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
            .apply_issue_handoff("t", &store.load(&issue.id).unwrap().0)
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

        tm.shutdown_all().await;
        assert!(tm.get("t").is_none());

        let specs = list_persisted(&root);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0], blank_spec());

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
                    if event.contains("\"operator_msg\"")
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
