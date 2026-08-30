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

use llm_harness_agent_team::{AgentTeam, IndividualSpec, TeamError};
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
    #[error("team already registered: {0}")]
    AlreadyRegistered(String),
    #[error(transparent)]
    Team(#[from] TeamError),
    #[error("no provider for model tier")]
    NoProviders,
    #[error("unknown member: {0}")]
    UnknownMember(String),
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

/// One assembled team instance.
pub struct TeamEntry {
    pub id: String,
    pub spec: TeamSpec,
    pub repo_path: Option<PathBuf>,
    pub team: AgentTeam,
    pub issues: IssueStore,
    pub handoffs: Vec<HandoffRoute>,
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
        std::fs::create_dir_all(team_dir.join("issues"))?;
        let issues = IssueStore::new(team_dir.join("issues"));

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
            },
        );
        Ok(self.teams.get(team_id.as_str()).expect("just inserted"))
    }

    pub fn get(&self, id: &str) -> Option<&TeamEntry> {
        self.teams.get(id)
    }

    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.teams.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Shut down one team and drop it (issues/memory persist).
    pub async fn shutdown(&mut self, id: &str) -> Result<(), TeamManagerError> {
        let mut entry = self
            .teams
            .remove(id)
            .ok_or_else(|| TeamManagerError::UnknownTeam(id.to_string()))?;
        entry.team.shutdown();
        Ok(())
    }

    pub async fn shutdown_all(&mut self) {
        for (_, mut e) in self.teams.drain() {
            e.team.shutdown();
        }
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
        let structural_change = old.toolkits != member.toolkits
            || old.model != member.model
            || old.provider_override != member.provider_override;

        // 成员级 key 只存 0600 secrets 文件（C1）；rebuild 前注入进
        // override 供 build_member_providers 消费。spec 本身不携带明文。
        let mut member = member;
        if let Some(ov) = member.provider_override.as_mut()
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
            model: ModelRef::Explicit("m".into()),
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
            name: "T".into(),
            repo_path: None,
            members: vec![member("a"), member("b")],
        }
    }

    fn install_mock_factory() {
        env_cfg::install_provider_factory(std::sync::Arc::new(|| {
            vec![(
                "m".into(),
                std::sync::Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]))
                    as std::sync::Arc<dyn llm_harness_loop::LlmClient>,
            )]
        }));
    }

    #[tokio::test]
    async fn create_builds_team_and_persists_spec() {
        install_mock_factory();
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
        install_mock_factory();
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
        install_mock_factory();
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
        install_mock_factory();
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
    async fn handoff_route_drives_issue_confirmation() {
        install_mock_factory();
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
        install_mock_factory();
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
        install_mock_factory();
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
