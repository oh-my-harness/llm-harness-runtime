//! Domain HTTP routes mounted into the inspector via `extra_routes`.
//!
//! All handlers share one `Arc<RwLock<ProjectManager>>` (tokio's async
//! lock: register awaits team build while holding the write guard, which
//! serializes registrations without blocking the runtime). `submit` is
//! `&self` on `AgentTeam`, so a read-guarded lookup suffices for chat and
//! issue confirmation.

use axum::Router;
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task::AbortHandle;

use parking_lot::Mutex;

use llm_harness_agent_team::{ChatView, reserved};

use crate::issue::IssueStatus;
use crate::team_manager::{TeamManager, TeamManagerError};
use crate::workspace::WorkspaceGrants;

/// Studio-facing view selector for the shared team conversation. Runtime
/// itself only has the human operator; this alias is not a member ID.
const TEAM_TARGET: &str = "team";

/// Router state: the shared manager plus the process-wide model tier and
/// scout interval (config-driven, injected once when the router is built).
#[derive(Clone)]
pub struct AppState {
    pub pm: SharedPm,
    /// Project-data root: settings.json, apikey, per-project dirs.
    pub data_root: PathBuf,
    /// Current model tier (re-read from settings on save; model changes
    /// take effect for projects registered afterwards).
    pub tier: Arc<RwLock<crate::roles::ModelTier>>,
    pub scout_interval_secs: Arc<RwLock<u64>>,
    /// Project-level event bus for the panel's WS stream.
    pub events: Arc<crate::events::EventBus>,
    /// Inspector harness registry — kept in sync so teams created /
    /// replaced at runtime are visible in the observation UI (I2).
    pub registry: Arc<llm_harness_inspector::state::HarnessRegistry>,
    /// Live operator pump tasks, keyed by project id. Restarting a project
    /// must abort the pump bound to the old `SocialContext`.
    pub operator_pumps: Arc<Mutex<HashMap<String, AbortHandle>>>,
    /// Live per-member thinking pumps, keyed by `<project>/<agent>`.
    pub thought_pumps: Arc<Mutex<HashMap<String, AbortHandle>>>,
    /// Explicitly authorized workspace roots for host filesystem browsing.
    pub grants: Arc<RwLock<WorkspaceGrants>>,
    /// Startup restoration outcome so clients can distinguish healthy and degraded launches.
    pub startup_recovery: Arc<RwLock<crate::runtime::StartupRecovery>>,
}

/// Shared handle to the manager (kept for direct consumers/tests).
pub type SharedPm = Arc<RwLock<TeamManager>>;

/// Build the domain router. Mount with `InspectorBuilder::extra_routes`.
pub fn team_router(state: AppState) -> Router {
    team_routes("/api/team", state.clone()).merge(team_routes("/api/agent-team", state))
}

fn team_routes(prefix: &'static str, state: AppState) -> Router {
    Router::new()
        .route(&format!("{prefix}/startup"), get(startup_recovery))
        .route(
            &format!("{prefix}/projects"),
            get(list_projects)
                .post(register_project)
                .delete(delete_project),
        )
        .route(&format!("{prefix}/projects/restart"), post(restart_project))
        .route(&format!("{prefix}/chat"), post(chat))
        .route(&format!("{prefix}/chat"), get(chat_history))
        .route(
            &format!("{prefix}/members"),
            get(list_members).post(add_member).delete(remove_member),
        )
        .route(&format!("{prefix}/issues"), get(list_issues))
        .route(
            &format!("{prefix}/issue/{{id}}/confirm"),
            post(confirm_issue),
        )
        .route(&format!("{prefix}/issue/{{id}}/reject"), post(reject_issue))
        .route(&format!("{prefix}/events"), get(events_ws))
        .route(
            &format!("{prefix}/settings"),
            get(get_settings).post(put_settings),
        )
        .route(&format!("{prefix}/apikey"), post(post_apikey))
        .route(&format!("{prefix}/fs"), get(list_dir))
        .route(
            &format!("{prefix}/fs/grants"),
            get(list_workspace_grants)
                .post(add_workspace_grant)
                .delete(remove_workspace_grant),
        )
        .route(&format!("{prefix}/pulse"), get(pulse))
        .route(&format!("{prefix}/agent/inject"), post(agent_inject))
        .route(&format!("{prefix}/agent/abort"), post(agent_abort))
        .route(&format!("{prefix}/agent/session"), get(agent_session))
        .route(
            &format!("{prefix}/agent/config"),
            get(agent_config).put(put_agent_config),
        )
        .route(&format!("{prefix}/templates"), get(list_templates))
        .route(&format!("{prefix}/upgrade-check"), post(upgrade_check))
        .route(&format!("{prefix}/upgrade-apply"), post(upgrade_apply))
        .with_state(state)
}

async fn startup_recovery(State(state): State<AppState>) -> Response {
    let recovery = state.startup_recovery.read().await.clone();
    let execution_mode = state.pm.read().await.execution_mode();
    Json(serde_json::json!({ "recovery": recovery, "execution_mode": execution_mode })).into_response()
}

// ── GET /api/team/projects ───────────────────────────────────────────────────

async fn list_projects(State(state): State<AppState>) -> Response {
    let pm = state.pm.read().await;
    let projects: Vec<serde_json::Value> = pm
        .ids()
        .iter()
        .filter_map(|id| {
            pm.get(id).map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "repo_path": p.repo_path.as_ref().map(|p| p.to_string_lossy()),
                })
            })
        })
        .collect();
    Json(serde_json::json!({ "projects": projects })).into_response()
}
#[derive(Deserialize)]
struct RegisterBody {
    id: String,
    /// 团队显示名；缺省用 id。
    #[serde(default)]
    name: Option<String>,
    /// 从模板创建；缺省 = 空白团队（仅 operator 可见，需再配成员）。
    #[serde(default)]
    template_id: Option<String>,
    /// 工作仓库；空 = 数据目录兜底。
    #[serde(default)]
    repo_path: Option<String>,
}

async fn register_project(
    State(state): State<AppState>,
    Json(body): Json<RegisterBody>,
) -> Response {
    if !body
        .id
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        || !body
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        || body.id.len() > 64
    {
        return err_resp(
            "team id must start with a letter/digit and contain only letters, digits, '_' or '-' (max 64)".into(),
            StatusCode::BAD_REQUEST,
        );
    }
    let tier = state.tier.read().await.clone();
    // base_url 不在内存 tier 里（只有模型档位是 live 的）——从 settings.json
    // 读，保证 ⚙ 里配置的 base_url 真正到达装配路径（I1）。
    let base_url = crate::settings::Settings::load(&state.data_root)
        .ok()
        .flatten()
        .map(|s| s.base_url)
        .unwrap_or_default();
    let settings = crate::settings::Settings {
        models: tier,
        scout_interval_secs: *state.scout_interval_secs.read().await,
        base_url,
    };
    let api_key = crate::settings::load_api_key(&state.data_root).unwrap_or_default();

    // 组装 spec：有模板走模板展开（repo_path 注入 spec），无模板空白单成员占位。
    let mut spec = match &body.template_id {
        Some(tid) => {
            let store =
                crate::template_store::TemplateStore::new(Some(state.data_root.join("templates")));
            match store.load(tid) {
                Ok(mut t) => {
                    t.spec.id = body.id.clone();
                    t.spec.name = body.name.clone().unwrap_or(t.spec.name);
                    t.spec.repo_path = body.repo_path.clone();
                    t.spec.template = Some(crate::team_spec::TemplateRef {
                        template_id: t.id.clone(),
                        version: t.version.clone(),
                    });
                    t.spec.template_baseline = Some(crate::team_spec::TemplateBaseline {
                        version: t.version.clone(),
                        members: t.spec.members.clone(),
                    });
                    t.spec
                }
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
                    )
                        .into_response();
                }
            }
        }
        None => crate::team_spec::TeamSpec {
            id: body.id.clone(),
            template: None,
            template_baseline: None,
            name: body.name.clone().unwrap_or_else(|| body.id.clone()),
            repo_path: body.repo_path.clone(),
            sandbox_mode: None,
            members: vec![],
        },
    };
    if spec.members.is_empty() {
        spec.members.push(crate::team_spec::MemberSpec {
            id: "planner".into(),
            persona: "你是团队的 planner，直接执行操作员交给你的任务。".into(),
            role_label: "规划者".into(),
            model: crate::team_spec::ModelRef::Tier("main".into()),
            toolkits: vec!["fs".into()],
            behaviors: vec![],
            provider_override: None,
            persona_file: None,
        });
    }

    let repo = body
        .repo_path
        .as_ref()
        .and_then(|p| (!p.is_empty()).then(|| std::path::PathBuf::from(p)));
    let repo = match repo {
        Some(path) => match crate::workspace::canonicalize_client_workspace_path(&path) {
            Ok(path) => Some(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return err_resp(
                    "workspace is not authorized; grant it before registering".into(),
                    StatusCode::FORBIDDEN,
                );
            }
            Err(error) => {
                return err_resp(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
            }
        },
        None => None,
    };
    if let Some(repo) = &repo {
        match state.grants.read().await.contains(repo) {
            Ok(true) => {}
            Ok(false) => {
                return err_resp(
                    "workspace is not authorized; grant it before registering".into(),
                    StatusCode::FORBIDDEN,
                );
            }
            Err(error) => {
                return err_resp(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }
    if let Some(repo) = &repo {
        spec.repo_path = Some(repo.to_string_lossy().into_owned());
    }
    let _grants = state.grants.read().await;
    let mut pm = state.pm.write().await;
    match pm.create(spec, repo, &settings, &api_key).await {
        Ok(_) => {
            register_team_side_effects(&state, &pm, &body.id);
            (StatusCode::CREATED, Json(serde_json::json!({ "ok": true }))).into_response()
        }
        Err(TeamManagerError::NotAGitRepo(p)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": format!("not a git repo: {}", p.display()) })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// DELETE /api/team/projects/:id — stop the team and delete its isolated
/// data directory, including chat, issues, memory, sessions and timers.
#[derive(Deserialize)]
struct DeleteQuery {
    id: String,
}

async fn delete_project(State(state): State<AppState>, Query(q): Query<DeleteQuery>) -> Response {
    let mut pm = state.pm.write().await;
    match pm.delete(&q.id).await {
        Ok(_) => {
            remove_team_from_registry(&state.registry, &q.id);
            stop_operator_pump(&state.operator_pumps, &q.id);
            remove_team_thought_pumps(&state.thought_pumps, &q.id);
            state.events.clear_project(&q.id);
            state
                .events
                .publish(crate::events::team_deleted_event(&q.id));
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Err(e) => {
            let error = e.to_string();
            if !matches!(e, TeamManagerError::UnknownTeam(_)) {
                let _ = crate::diagnostics::append_lifecycle_event(
                    &state.data_root,
                    serde_json::json!({
                        "event": "team_delete_failed",
                        "project": q.id,
                        "error": error,
                    }),
                );
                state
                    .events
                    .publish(crate::events::team_delete_failed_event(&q.id, &error));
            }
            let status = match e {
                TeamManagerError::UnknownTeam(_) => StatusCode::NOT_FOUND,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (
                status,
                Json(serde_json::json!({ "ok": false, "error": error })),
            )
                .into_response()
        }
    }
}

// ── GET/POST /api/team/settings ──────────────────────────────────────────────

async fn get_settings(State(state): State<AppState>) -> Response {
    let settings = crate::settings::Settings::load(&state.data_root)
        .ok()
        .flatten()
        .unwrap_or_default();
    Json(serde_json::json!({
        "models": {
            "strong": settings.models.strong,
            "main": settings.models.main,
            "cheap": settings.models.cheap,
        },
        "scout_interval_secs": settings.scout_interval_secs,
        "base_url": settings.base_url,
        "api_key_set": crate::settings::load_api_key(&state.data_root).is_some(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct SettingsBody {
    strong: String,
    main: String,
    cheap: String,
    scout_interval_secs: u64,
    base_url: String,
    /// Some(非空) → 覆写全局 key（0600 文件）；None/空 → 不动。
    #[serde(default)]
    api_key: Option<String>,
}

async fn put_settings(State(state): State<AppState>, Json(body): Json<SettingsBody>) -> Response {
    if body.strong.trim().is_empty() || body.main.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "error": "strong and main model ids are required",
            })),
        )
            .into_response();
    }
    let settings = crate::settings::Settings {
        models: crate::roles::ModelTier {
            strong: body.strong.trim().to_string(),
            main: body.main.trim().to_string(),
            cheap: body.cheap.trim().to_string(),
        },
        scout_interval_secs: body.scout_interval_secs,
        // Normalize: "api.hyper-op.com" → "https://api.hyper-op.com".
        base_url: normalize_base_url(&body.base_url),
    };
    if let Err(e) = settings.save(&state.data_root) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response();
    }
    // Live-update the tier/interval so projects registered afterwards pick
    // them up. Model changes do NOT retroactively affect running teams.
    *state.tier.write().await = settings.models.clone();
    *state.scout_interval_secs.write().await = settings.scout_interval_secs;
    // Optional key update (single-form UX: settings + key in one save).
    if let Some(key) = body.api_key.filter(|k| !k.is_empty())
        && let Err(e) = crate::settings::save_api_key(&state.data_root, &key)
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response();
    }
    Json(serde_json::json!({
        "ok": true,
        "note": "model changes apply to projects registered from now on; restart existing projects to pick them up",
    }))
    .into_response()
}

/// Users paste hosts without a scheme; default to HTTPS unless http://
/// was explicit.
fn normalize_base_url(raw: &str) -> String {
    let v = raw.trim().trim_end_matches('/');
    if v.is_empty() {
        return String::new();
    }
    if v.starts_with("http://") || v.starts_with("https://") {
        v.to_string()
    } else {
        format!("https://{v}")
    }
}

#[derive(Deserialize)]
struct ApiKeyBody {
    api_key: String,
}

async fn post_apikey(State(state): State<AppState>, Json(body): Json<ApiKeyBody>) -> Response {
    let key = body.api_key.trim().to_string();
    if key.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": "api_key is required" })),
        )
            .into_response();
    }
    if let Err(e) = crate::settings::save_api_key(&state.data_root, &key) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response();
    }
    Json(serde_json::json!({ "ok": true })).into_response()
}

// ── GET /api/team/fs?path= — directory browser for the repo picker ──────────

#[derive(Deserialize)]
struct FsQuery {
    /// Directory to list. Empty = the first authorized workspace root.
    path: Option<String>,
}

/// List subdirectories of `path` for the panel's folder picker.
///
/// Read-only, directories only, hidden entries skipped. Only canonical
/// descendants of explicitly granted roots are exposed; symlink escapes are
/// resolved before access.
async fn list_dir(State(state): State<AppState>, Query(q): Query<FsQuery>) -> Response {
    let grants = state.grants.read().await;
    let dir: PathBuf = match q.path.as_deref() {
        None | Some("") => match grants.grants().first() {
            Some(grant) => grant.path.clone(),
            None => {
                return err_resp(
                    "no workspace grants; authorize a directory first".into(),
                    StatusCode::FORBIDDEN,
                );
            }
        },
        Some(p) => p.into(),
    };
    let dir = match grants.resolve_if_authorized(&dir) {
        Ok(Some(dir)) => dir,
        Ok(None) => {
            return err_resp(
                "directory is outside authorized workspace roots".into(),
                StatusCode::FORBIDDEN,
            )
            .into_response();
        }
        Err(error) => return err_resp(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    };
    let meta = match std::fs::metadata(&dir) {
        Ok(m) => m,
        Err(e) => return err_resp(format!("{}: {e}", dir.display()), StatusCode::NOT_FOUND),
    };
    if !meta.is_dir() {
        return err_resp("not a directory".into(), StatusCode::BAD_REQUEST);
    }
    let mut dirs: Vec<serde_json::Value> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return err_resp("cannot read directory".into(), StatusCode::FORBIDDEN);
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            dirs.push(serde_json::json!({ "name": name }));
        }
    }
    dirs.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Json(serde_json::json!({
        "path": dir.to_string_lossy(),
        "is_repo": dir.join(".git").exists(),
        "dirs": dirs,
        "grant_roots": grants.grants().iter().map(|g| g.path.to_string_lossy()).collect::<Vec<_>>(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct WorkspaceGrantBody {
    path: String,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Deserialize)]
struct WorkspaceGrantQuery {
    id: String,
}

async fn list_workspace_grants(State(state): State<AppState>) -> Response {
    let grants = state.grants.read().await;
    Json(serde_json::json!({ "grants": grants.grants() })).into_response()
}

async fn add_workspace_grant(
    State(state): State<AppState>,
    Json(body): Json<WorkspaceGrantBody>,
) -> Response {
    let mut grants = state.grants.write().await;
    match grants.add(&body.path, body.label) {
        Ok(grant) => (StatusCode::CREATED, Json(serde_json::json!(grant))).into_response(),
        Err(crate::workspace::WorkspaceError::InvalidPath(path)) => err_resp(
            format!("invalid workspace grant: {}", path.display()),
            StatusCode::BAD_REQUEST,
        ),
        Err(error) => err_resp(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn remove_workspace_grant(
    State(state): State<AppState>,
    Query(query): Query<WorkspaceGrantQuery>,
) -> Response {
    let mut grants = state.grants.write().await;
    let Some(pending_removal) = grants
        .grants()
        .iter()
        .find(|grant| grant.id == query.id)
        .cloned()
    else {
        return err_resp("unknown workspace grant".into(), StatusCode::NOT_FOUND);
    };
    let active = {
        let pm = state.pm.read().await;
        pm.ids().into_iter().any(|id| {
            pm.get(&id)
                .and_then(|entry| entry.repo_path.as_deref())
                .map(|repo| {
                    crate::workspace::WorkspaceGrants::contains_path_with_root(
                        repo,
                        &pending_removal.path,
                    )
                    .unwrap_or(true)
                })
                .unwrap_or(false)
        })
    };
    if active {
        return err_resp(
            "revoke the project before revoking its active workspace grant".into(),
            StatusCode::CONFLICT,
        );
    }
    let removed = grants.remove(&query.id).map_err(|error| match error {
        crate::workspace::WorkspaceError::UnknownGrant(_) => {
            err_resp("unknown workspace grant".into(), StatusCode::NOT_FOUND)
        }
        error => err_resp(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    });
    let _removed = match removed {
        Ok(grant) => grant,
        Err(response) => return response,
    };
    Json(serde_json::json!({ "ok": true })).into_response()
}

// ── GET /api/team/pulse?project= — situation snapshot ────────────────────────

#[derive(Deserialize)]
struct PulseQuery {
    project: String,
}

/// (agent, error) 已推送记录，按 team 索引——pulse 轮询去重用。
type SeenErrors = std::collections::HashMap<String, Vec<(String, String)>>;

static LAST_AGENT_ERRORS: std::sync::LazyLock<tokio::sync::RwLock<SeenErrors>> =
    std::sync::LazyLock::new(|| tokio::sync::RwLock::new(Default::default()));

async fn pulse(State(state): State<AppState>, Query(q): Query<PulseQuery>) -> Response {
    let pm = state.pm.read().await;
    let Some(project) = pm.get(&q.project) else {
        return err_resp(
            format!("unknown project: {}", q.project),
            StatusCode::NOT_FOUND,
        );
    };
    let member_ids = project
        .spec
        .members
        .iter()
        .map(|member| member.id.clone())
        .collect::<Vec<_>>();
    let mut snap = crate::pulse::snapshot(&project.team, &member_ids).await;
    snap["pending"] = crate::pulse::pending_traffic(&project.team, &member_ids)["pending"].clone();
    snap["timers"] = serde_json::json!({ "pending": crate::pulse::pending_timers(&project.team) });
    snap["issues"] = {
        let list = project.issues.list().unwrap_or_default();
        serde_json::json!({
            "pending": list.iter().filter(|i| i.status == crate::issue::IssueStatus::Pending).count(),
            "confirmed": list.iter().filter(|i| i.status == crate::issue::IssueStatus::Confirmed).count(),
        })
    };

    // 错误外显（面板不再让用户猜）：成员带 error 时推 agent_error 事件。
    // 去重：同一成员同一错误文本只推一次（内存小表）；恢复后清记录。
    {
        let mut seen = LAST_AGENT_ERRORS.write().await;
        let entry = seen.entry(q.project.clone()).or_default();
        for a in snap["agents"].as_array().unwrap() {
            let Some(err) = a["error"].as_str() else {
                continue;
            };
            let id = a["id"].as_str().unwrap_or_default();
            let key = (id.to_string(), err.to_string());
            if !entry.contains(&key) {
                if entry.len() > 32 {
                    entry.clear(); // 简单防涨
                }
                entry.push(key);
                state
                    .events
                    .publish(crate::events::agent_error_event(&q.project, id, err));
            }
        }
        entry.retain(|(id, _)| {
            snap["agents"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["id"].as_str() == Some(id.as_str()) && a["error"].is_string())
        });
    }
    Json(snap).into_response()
}

// ── POST /api/team/agent/inject|abort — per-agent control ───────────────────

#[derive(Deserialize)]
struct AgentCtlQuery {
    project: String,
    agent: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    before: Option<i64>,
}

#[derive(Deserialize)]
struct InjectBody {
    text: String,
}

/// Inject a message into a member's current/next turn (harness steer).
async fn agent_inject(
    State(state): State<AppState>,
    Query(q): Query<AgentCtlQuery>,
    Json(body): Json<InjectBody>,
) -> Response {
    let pm = state.pm.read().await;
    let Some(project) = pm.get(&q.project) else {
        return err_resp(
            format!("unknown project: {}", q.project),
            StatusCode::NOT_FOUND,
        );
    };
    if project.workflow.is_some() {
        return conflict("controlled teams accept work through /api/team/chat".into());
    }
    match project
        .team
        .harness(&llm_harness_agent_team::AgentId::new(&q.agent))
    {
        Some(h) => {
            h.steer(&body.text);
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        None => err_resp(format!("unknown agent: {}", q.agent), StatusCode::NOT_FOUND),
    }
}

/// Abort a member's current run. The runner re-queues any in-flight
/// messages, so the task is not lost — it will be retried by the team.
async fn agent_abort(State(state): State<AppState>, Query(q): Query<AgentCtlQuery>) -> Response {
    let pm = state.pm.read().await;
    let Some(project) = pm.get(&q.project) else {
        return err_resp(
            format!("unknown project: {}", q.project),
            StatusCode::NOT_FOUND,
        );
    };
    // Controlled teams own cancellation through the PipelineController:
    // a member-level harness.abort() would kill the member's harness without
    // cancelling the controller-owned task token, leaving the controller
    // waiter blocked until its timeout. Rejection mirrors agent_inject.
    // The bus-backed workflow sink is the authorized shutdown path instead.
    if project.workflow.is_some() || crate::pipeline::is_controlled(&project.spec) {
        return conflict(
            "controlled teams cancel work through the workflow owner, not agent abort".into(),
        );
    }
    if q.agent == TEAM_TARGET {
        let interrupted: Vec<&str> = project
            .spec
            .members
            .iter()
            .filter_map(|member| {
                let harness = project
                    .team
                    .harness(&llm_harness_agent_team::AgentId::new(&member.id))?;
                harness.abort();
                Some(member.id.as_str())
            })
            .collect();
        return Json(serde_json::json!({ "ok": true, "interrupted": interrupted })).into_response();
    }
    match project
        .team
        .harness(&llm_harness_agent_team::AgentId::new(&q.agent))
    {
        Some(h) => {
            h.abort();
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        None => err_resp(format!("unknown agent: {}", q.agent), StatusCode::NOT_FOUND),
    }
}

/// GET /api/team/agent/session?project=&agent= — the agent's run history
/// (active session path projected to user/assistant/tool lines).
async fn agent_session(State(state): State<AppState>, Query(q): Query<AgentCtlQuery>) -> Response {
    let pm = state.pm.read().await;
    let Some(project) = pm.get(&q.project) else {
        return err_resp(
            format!("unknown project: {}", q.project),
            StatusCode::NOT_FOUND,
        );
    };
    let Some(h) = project
        .team
        .harness(&llm_harness_agent_team::AgentId::new(&q.agent))
    else {
        return err_resp(format!("unknown agent: {}", q.agent), StatusCode::NOT_FOUND);
    };
    let entries = match h.read_active_path().await {
        Ok(e) => e,
        Err(e) => {
            return err_resp(
                format!("session read failed: {e}"),
                StatusCode::INTERNAL_SERVER_ERROR,
            );
        }
    };
    let lines: Vec<serde_json::Value> = entries
        .iter()
        .filter_map(|entry| match &entry.payload {
            llm_harness_agent::session::types::SessionEntryPayload::Message(msg) => {
                Some(project_message_line(msg))
            }
            _ => None,
        })
        .collect();
    Json(serde_json::json!({ "lines": lines })).into_response()
}

/// GET /api/team/agent/config?project=&agent= — tools + system prompt +
/// model (same projection as the inspector's config endpoint).
async fn agent_config(State(state): State<AppState>, Query(q): Query<AgentCtlQuery>) -> Response {
    let pm = state.pm.read().await;
    let Some(project) = pm.get(&q.project) else {
        return err_resp(
            format!("unknown project: {}", q.project),
            StatusCode::NOT_FOUND,
        );
    };
    let Some(h) = project
        .team
        .harness(&llm_harness_agent_team::AgentId::new(&q.agent))
    else {
        return err_resp(format!("unknown agent: {}", q.agent), StatusCode::NOT_FOUND);
    };
    let s = h.state();
    let tools: Vec<String> = s.tools.iter().map(|t| t.name().to_string()).collect();
    let member = project.spec.members.iter().find(|m| m.id == q.agent);
    let override_info = member.map(|m| {
        serde_json::json!({
            "model": m.provider_override.as_ref().and_then(|o| o.model.clone()),
            "base_url": m.provider_override.as_ref().and_then(|o| o.base_url.clone()),
            "api_key_set": member_api_key_set(&state.data_root, &project.id, &q.agent),
        })
    });
    Json(serde_json::json!({
        "model": s.model,
        "system_prompt": s.system_prompt,
        "tools": tools,
        "thinking_level": format!("{:?}", s.thinking_level),
        "temperature": s.temperature,
        "override": override_info,
    }))
    .into_response()
}

/// GET /api/team/templates — template gallery for team creation.
async fn list_templates(State(state): State<AppState>) -> Response {
    let store = crate::template_store::TemplateStore::new(Some(state.data_root.join("templates")));
    let metas: Vec<serde_json::Value> = store
        .list()
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "version": m.version,
                "name": m.name,
                "member_ids": m.member_ids,
            })
        })
        .collect();
    Json(serde_json::json!({ "templates": metas })).into_response()
}

/// Upgrade analysis for one team: which members would follow a template
/// upgrade (serialized field-equality against the template originals) and
/// which are locally overridden.
#[derive(Deserialize)]
struct UpgradeCheckBody {
    project: String,
}

async fn upgrade_check(
    State(state): State<AppState>,
    Json(body): Json<UpgradeCheckBody>,
) -> Response {
    let pm = state.pm.read().await;
    let Some(entry) = pm.get(&body.project) else {
        return err_resp(
            format!("unknown project: {}", body.project),
            StatusCode::NOT_FOUND,
        );
    };
    let Some(tref) = &entry.spec.template else {
        return err_resp(
            "team has no template reference".to_string(),
            StatusCode::BAD_REQUEST,
        );
    };
    let store = crate::template_store::TemplateStore::new(Some(state.data_root.join("templates")));
    let template = match store.load(&tref.template_id) {
        Ok(t) => t,
        Err(e) => return err_resp(e.to_string(), StatusCode::NOT_FOUND),
    };

    let (follow, overridden) = member_upgrade_plan(&entry.spec, &template);
    Json(serde_json::json!({
        "template": { "id": tref.template_id, "version": tref.version },
        "template_current_version": template.version,
        "would_follow": follow,
        "overridden": overridden,
    }))
    .into_response()
}

/// Decide which instance members would follow a template upgrade. Prefer the
/// creation-time baseline; specs saved before baselines existed fall back to
/// exact equality with the current template.
fn member_upgrade_plan(
    spec: &crate::team_spec::TeamSpec,
    template: &crate::template_store::LoadedTemplate,
) -> (Vec<String>, Vec<String>) {
    let mut follow = Vec::new();
    let mut overridden = Vec::new();
    for member in &spec.members {
        let matches_baseline = spec.template_baseline.as_ref().and_then(|baseline| {
            baseline
                .members
                .iter()
                .find(|old| old.id == member.id)
                .map(|old| old == member)
        });
        let same_as_template = template
            .spec
            .members
            .iter()
            .find(|original| original.id == member.id)
            .is_some_and(|original| original == member);
        if matches_baseline.unwrap_or(same_as_template) {
            follow.push(member.id.clone());
        } else {
            overridden.push(member.id.clone());
        }
    }
    (follow, overridden)
}

/// Apply the loaded template to members that still match the creation baseline.
async fn upgrade_apply(
    State(state): State<AppState>,
    Json(body): Json<UpgradeCheckBody>,
) -> Response {
    let template = {
        let pm = state.pm.read().await;
        let Some(entry) = pm.get(&body.project) else {
            return err_resp(
                format!("unknown project: {}", body.project),
                StatusCode::NOT_FOUND,
            );
        };
        let Some(tref) = &entry.spec.template else {
            return err_resp(
                "team has no template reference".to_string(),
                StatusCode::BAD_REQUEST,
            );
        };
        let store =
            crate::template_store::TemplateStore::new(Some(state.data_root.join("templates")));
        match store.load(&tref.template_id) {
            Ok(template) => template,
            Err(e) => return err_resp(e.to_string(), StatusCode::NOT_FOUND),
        }
    };

    let tier = state.tier.read().await.clone();
    let base_url = crate::settings::Settings::load(&state.data_root)
        .ok()
        .flatten()
        .map(|s| s.base_url)
        .unwrap_or_default();
    let settings = crate::settings::Settings {
        models: tier,
        scout_interval_secs: *state.scout_interval_secs.read().await,
        base_url,
    };
    let api_key = crate::settings::load_api_key(&state.data_root).unwrap_or_default();
    let mut pm = state.pm.write().await;
    match pm
        .apply_template_upgrade(&body.project, template.clone(), &settings, &api_key)
        .await
    {
        Ok(outcome) => {
            if !outcome.rebuilt.is_empty()
                && let Some(entry) = pm.get(&body.project)
            {
                for member in &entry.spec.members {
                    if outcome.rebuilt.contains(&member.id)
                        && let Some(h) = entry
                            .team
                            .harness(&llm_harness_agent_team::AgentId::new(&member.id))
                    {
                        state
                            .registry
                            .register(format!("{}/{}", body.project, member.id), h);
                    }
                }
            }
            Json(serde_json::json!({
                "ok": true,
                "template": {
                    "id": template.id,
                    "version": template.version,
                },
                "upgraded": outcome.upgraded,
                "overridden": outcome.overridden,
                "rebuilt": outcome.rebuilt,
            }))
            .into_response()
        }
        Err(e) => err_resp(e.to_string(), StatusCode::CONFLICT),
    }
}

/// Whether a member-scoped API key file exists (0600, never returned).
fn member_api_key_set(data_root: &std::path::Path, team_id: &str, agent: &str) -> bool {
    data_root
        .join("teams")
        .join(team_id)
        .join("secrets")
        .join(agent)
        .exists()
}

/// PUT /api/team/agent/config — per-member configuration.
///
/// persona/role_label → hot-apply; model/api_key/base_url/toolkits →
/// rebuild via `replace_member` (response carries `rebuilt: true`).
#[derive(Deserialize)]
struct AgentConfigBody {
    project: String,
    agent: String,
    #[serde(default)]
    persona: Option<String>,
    #[serde(default)]
    toolkits: Option<Vec<String>>,
    /// "strong"/"main"/"cheap" → tier ref; anything else → explicit model.
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    /// Written to the 0600 member secrets file; never echoed back.
    #[serde(default)]
    api_key: Option<String>,
}

async fn put_agent_config(
    State(state): State<AppState>,
    Json(body): Json<AgentConfigBody>,
) -> Response {
    let tier = state.tier.read().await.clone();
    // base_url 不在内存 tier 里（只有模型档位是 live 的）——从 settings.json
    // 读，保证 ⚙ 里配置的 base_url 真正到达装配路径（I1）。
    let base_url = crate::settings::Settings::load(&state.data_root)
        .ok()
        .flatten()
        .map(|s| s.base_url)
        .unwrap_or_default();
    let settings = crate::settings::Settings {
        models: tier,
        scout_interval_secs: *state.scout_interval_secs.read().await,
        base_url,
    };

    // Assemble the updated MemberSpec from the current instance.
    let mut pm = state.pm.write().await;
    let (current, _team_id) = {
        let Some(entry) = pm.get(&body.project) else {
            return err_resp(
                format!("unknown project: {}", body.project),
                StatusCode::NOT_FOUND,
            );
        };
        match entry.spec.members.iter().find(|m| m.id == body.agent) {
            Some(m) => (m.clone(), body.project.clone()),
            None => {
                return err_resp(
                    format!("unknown member: {}", body.agent),
                    StatusCode::NOT_FOUND,
                );
            }
        }
    };

    // Member-scoped api key → private file created atomically with 0600.
    // IDs are known-good above, and the storage helper rejects traversal.
    if let Some(key) = &body.api_key
        && let Err(error) =
            crate::settings::save_member_api_key(&state.data_root, &body.project, &body.agent, key)
    {
        return err_resp(
            format!("failed to store member API key: {error}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    }

    let mut member = current;
    if let Some(persona) = body.persona {
        member.persona = persona;
    }
    if let Some(toolkits) = body.toolkits {
        member.toolkits = toolkits;
    }
    if body.model.is_some() || body.base_url.is_some() || body.api_key.is_some() {
        let ov = member
            .provider_override
            .get_or_insert_with(Default::default);
        if let Some(model) = &body.model {
            match model.as_str() {
                "strong" | "main" | "cheap" => {
                    member.model = crate::team_spec::ModelRef::Tier(model.clone());
                    ov.model = None;
                }
                other => {
                    member.model = crate::team_spec::ModelRef::Explicit(other.to_string());
                    ov.model = Some(other.to_string());
                }
            }
        }
        if let Some(base_url) = &body.base_url {
            ov.base_url = Some(normalize_base_url(base_url));
        }
        // api_key 只存 0600 secrets 文件（上面已写入）；spec 不携带明文。
        // rebuild 路径（update_member）从 secrets 文件注入。
        if body.api_key.is_some() {
            member.provider_override.as_mut().unwrap().api_key = Some(String::new());
        }
    }

    let api_key = crate::settings::load_api_key(&state.data_root).unwrap_or_default();
    match pm
        .update_member(
            &body.project,
            member,
            &settings,
            &api_key,
            body.api_key.is_some(),
        )
        .await
    {
        Ok(update) => {
            // I2: replace_member 构建了新 harness——registry 同步到新 Arc。
            if update == crate::team_manager::MemberUpdate::Rebuilt
                && let Some(h) = pm.get(&body.project).and_then(|e| {
                    e.team
                        .harness(&llm_harness_agent_team::AgentId::new(&body.agent))
                })
            {
                state
                    .registry
                    .register(format!("{}/{}", body.project, body.agent), h.clone());
                spawn_agent_thought_pump(
                    state.events.clone(),
                    state.thought_pumps.clone(),
                    &body.project,
                    &body.agent,
                    h,
                );
            }
            Json(serde_json::json!({
                "ok": true,
                "rebuilt": update == crate::team_manager::MemberUpdate::Rebuilt,
            }))
            .into_response()
        }
        Err(e) => err_resp(e.to_string(), StatusCode::CONFLICT),
    }
}

/// Forward one team's operator messages to the WS event bus (I2: also
/// spawned for teams created at runtime, not just startup teams).
pub fn spawn_operator_pump(
    events: Arc<crate::events::EventBus>,
    pumps: Arc<Mutex<HashMap<String, AbortHandle>>>,
    team_id: String,
    ctx: llm_harness_agent_team::SocialContext,
) {
    let pump_team_id = team_id.clone();
    let handle = tokio::spawn(async move {
        let operator = llm_harness_agent_team::AgentId::new(reserved::OPERATOR);
        loop {
            ctx.wait_for_message(&operator).await;
            for msg in ctx.drain_inbox(&operator) {
                let message = msg.clone();
                let message_type = match msg.message_type {
                    llm_harness_agent_team::MessageType::Broadcast => "broadcast",
                    llm_harness_agent_team::MessageType::DelegationRequest => "delegation",
                    _ => "direct",
                };
                let view_kind = if msg.from.as_str() == reserved::OPERATOR {
                    ChatView::GroupMessage.as_str()
                } else if msg.broadcast_id.is_some() {
                    ChatView::GroupReply.as_str()
                } else {
                    ChatView::DirectMessage.as_str()
                };
                events.publish(crate::events::team_message_event(
                    &pump_team_id,
                    msg.from.as_str(),
                    msg.to.as_str(),
                    &msg.body,
                    message_type,
                    msg.message_id,
                    msg.broadcast_id,
                    view_kind,
                ));
                ctx.ack_inbox_messages(&[message]);
            }
        }
    });
    register_operator_pump(&pumps, team_id, handle.abort_handle());
}

fn register_operator_pump(
    pumps: &Arc<Mutex<HashMap<String, AbortHandle>>>,
    team_id: String,
    handle: AbortHandle,
) {
    let mut pumps = pumps.lock();
    if let Some(old) = pumps.insert(team_id, handle) {
        old.abort();
    }
}

fn stop_operator_pump(pumps: &Arc<Mutex<HashMap<String, AbortHandle>>>, team_id: &str) {
    if let Some(handle) = pumps.lock().remove(team_id) {
        handle.abort();
    }
}

/// Forward one member's thinking deltas to the WS event bus.
pub fn spawn_agent_thought_pump(
    events: Arc<crate::events::EventBus>,
    pumps: Arc<Mutex<HashMap<String, AbortHandle>>>,
    project: &str,
    agent: &str,
    harness: Arc<llm_harness_agent::AgentHarness>,
) {
    let mut event_rx = harness.subscribe();
    let pump_project = project.to_string();
    let pump_agent = agent.to_string();
    let key = format!("{project}/{agent}");
    let handle = tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    if let llm_harness_agent::AgentHarnessEvent::Agent(
                        llm_harness_types::AgentEvent::ThinkingDelta {
                            message_id,
                            thinking,
                            ..
                        },
                    ) = event.as_ref()
                    {
                        events.publish(crate::events::agent_thought_event(
                            &pump_project,
                            &pump_agent,
                            message_id,
                            thinking,
                        ));
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    let mut pumps = pumps.lock();
    if let Some(old) = pumps.insert(key, handle.abort_handle()) {
        old.abort();
    }
}

fn remove_team_thought_pumps(pumps: &Arc<Mutex<HashMap<String, AbortHandle>>>, team_id: &str) {
    let prefix = format!("{team_id}/");
    let keys: Vec<String> = pumps
        .lock()
        .keys()
        .filter(|key| key.starts_with(&prefix))
        .cloned()
        .collect();
    let mut pumps = pumps.lock();
    for key in keys {
        if let Some(handle) = pumps.remove(&key) {
            handle.abort();
        }
    }
}

fn stop_agent_thought_pump(
    pumps: &Arc<Mutex<HashMap<String, AbortHandle>>>,
    project: &str,
    agent: &str,
) {
    if let Some(handle) = pumps.lock().remove(&format!("{project}/{agent}")) {
        handle.abort();
    }
}

/// Project one AgentMessage to a display line (kind + text).
fn project_message_line(msg: &llm_harness_types::AgentMessage) -> serde_json::Value {
    use llm_harness_types::{AgentMessage, ContentBlock, DataBlock};
    match msg {
        AgentMessage::User(u) => {
            let text: String = u
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.trim().to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            serde_json::json!({ "kind": "user", "text": text })
        }
        AgentMessage::Assistant(a) => {
            let mut text = String::new();
            let mut tools: Vec<String> = Vec::new();
            for b in &a.content {
                match b {
                    ContentBlock::Text { text: t, .. } => text.push_str(t.trim()),
                    ContentBlock::ToolUse { name, .. } => tools.push(name.clone()),
                    _ => {}
                }
            }
            serde_json::json!({ "kind": "assistant", "text": text, "tools": tools })
        }
        AgentMessage::ToolResult(t) => {
            let text: String = t
                .content
                .iter()
                .filter_map(|b| match b {
                    DataBlock::Text { text, .. } => Some(text.trim().to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            serde_json::json!({
                "kind": "tool_result",
                "text": text,
                "is_error": t.is_error,
            })
        }
        _ => serde_json::json!({ "kind": "other" }),
    }
}

fn chat_message_json(
    project: &str,
    entry: &llm_harness_agent_team::ChatHistoryEntry,
    view_kind: ChatView,
) -> serde_json::Value {
    let message = &entry.message;
    let message_type = match message.message_type {
        llm_harness_agent_team::MessageType::Broadcast => "broadcast",
        llm_harness_agent_team::MessageType::DelegationRequest => "delegation",
        _ => "direct",
    };
    let conversation = match view_kind {
        ChatView::DirectMessage => {
            if message.from.as_str() == reserved::OPERATOR {
                message.to.as_str()
            } else {
                message.from.as_str()
            }
        }
        _ => TEAM_TARGET,
    };
    let to = if view_kind == ChatView::GroupMessage {
        TEAM_TARGET
    } else {
        message.to.as_str()
    };
    serde_json::json!({
        "type": "team_message",
        "project": project,
        "from": message.from.as_str(),
        "to": to,
        "text": message.body,
        "message_type": message_type,
        "message_id": message.message_id,
        "broadcast_id": message.broadcast_id,
        "view_kind": view_kind.as_str(),
        "conversation": conversation,
        "sequence": entry.sequence,
        "created_at": entry.created_at,
        "ts": entry.created_at,
    })
}

fn err_resp(message: String, status: StatusCode) -> Response {
    (
        status,
        Json(serde_json::json!({ "ok": false, "error": message })),
    )
        .into_response()
}

// ── POST /api/team/chat ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ChatBody {
    project: String,
    /// Routing target: any member of the team (panel offers the member
    /// list; the backend re-checks against the spec).
    target: String,
    text: String,
}

async fn chat(State(state): State<AppState>, Json(body): Json<ChatBody>) -> Response {
    let pm = state.pm.read().await;
    let Some(project) = pm.get(&body.project) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "ok": false, "error": format!("unknown project: {}", body.project) })),
        )
            .into_response();
    };
    // Controlled coding-team ingress: ordinary user chat — team broadcast or
    // direct member target — must reach the orchestrator workflow only, never
    // a worker inbox. Generic teams keep direct/broadcast chat below.
    if project.workflow.is_some() || crate::pipeline::is_controlled(&project.spec) {
        if body.target != TEAM_TARGET && !project.spec.members.iter().any(|m| m.id == body.target) {
            return err_resp(
                format!("unknown member: {}", body.target),
                StatusCode::BAD_REQUEST,
            );
        }
        if body.text.trim().is_empty() {
            return err_resp(
                "workflow objective must not be empty".into(),
                StatusCode::BAD_REQUEST,
            );
        }
        if project.team.social_context().is_none() {
            return conflict("team is not built".into());
        }
        let journal = match llm_harness_agent_team::ChatHistoryJournal::open(
            project.data_dir().join("chat.sqlite3"),
        ) {
            Ok(journal) => journal,
            Err(error) => return conflict(error.to_string()),
        };
        let issue = crate::issue::Issue {
            id: format!("chat-{}", uuid::Uuid::new_v4()),
            status: IssueStatus::Confirmed,
            source: "operator".into(),
            created: chrono::Utc::now().to_rfc3339(),
            title: "Operator request".into(),
            body: body.text.clone(),
        };
        return match project.start_workflow(issue, Some(state.events.clone())) {
            Ok(id) => {
                // Admission precedes representation: a rejected workflow is not
                // an accepted team message. The journal write never touches an
                // AgentRunner inbox or wakes a worker.
                let message_id = uuid::Uuid::now_v7().to_string();
                let broadcast_id = id.clone();
                let message = llm_harness_agent_team::InboxMessage {
                    journal_id: None,
                    from: llm_harness_agent_team::AgentId::new(reserved::OPERATOR),
                    to: llm_harness_agent_team::AgentId::new(TEAM_TARGET),
                    body: body.text.clone(),
                    message_type: llm_harness_agent_team::MessageType::Broadcast,
                    priority: llm_harness_agent_team::priority::OPERATOR,
                    task_id: None,
                    task_kind: None,
                    broadcast_id: Some(broadcast_id.clone()),
                    message_id: Some(message_id.clone()),
                };
                if let Err(error) = journal.append(&message) {
                    return conflict(error.to_string());
                }
                state.events.publish(crate::events::team_message_event(
                    &body.project,
                    reserved::OPERATOR,
                    TEAM_TARGET,
                    &body.text,
                    "broadcast",
                    Some(message_id),
                    Some(broadcast_id),
                    ChatView::GroupMessage.as_str(),
                ));
                (
                    StatusCode::ACCEPTED,
                    Json(serde_json::json!({"ok": true, "workflow_id": id})),
                )
                    .into_response()
            }
            Err(error) => conflict(error.to_string()),
        };
    }
    let effective_target = if body.target == TEAM_TARGET {
        let mention = body
            .text
            .split_whitespace()
            .find_map(|word| word.strip_prefix('@').filter(|id| !id.is_empty()))
            .unwrap_or(TEAM_TARGET);
        if mention != "team" && !project.spec.members.iter().any(|m| m.id == mention) {
            return err_resp(
                format!("unknown member: {mention}"),
                StatusCode::BAD_REQUEST,
            );
        }
        body.target.clone()
    } else {
        body.target.clone()
    };

    if effective_target == TEAM_TARGET {
        match project
            .team
            .submit_broadcast_with_message_id(&body.text, None)
        {
            Ok(message_ids) => {
                state.events.publish(crate::events::team_message_event(
                    &body.project,
                    reserved::OPERATOR,
                    TEAM_TARGET,
                    &body.text,
                    "broadcast",
                    Some(message_ids.message_id),
                    Some(message_ids.broadcast_id),
                    ChatView::GroupMessage.as_str(),
                ));
                (
                    StatusCode::ACCEPTED,
                    Json(serde_json::json!({ "ok": true })),
                )
                    .into_response()
            }
            Err(e) => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
            )
                .into_response(),
        }
    } else {
        if !project
            .spec
            .members
            .iter()
            .any(|m| m.id == effective_target)
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "ok": false, "error": format!("unknown member: {}", effective_target) })),
            )
                .into_response();
        }
        match project
            .team
            .submit_with_message_id(&effective_target, &body.text, None)
        {
            Ok(message_id) => {
                state.events.publish(crate::events::team_message_event(
                    &body.project,
                    reserved::OPERATOR,
                    &effective_target,
                    &body.text,
                    "direct",
                    Some(message_id),
                    None,
                    ChatView::DirectMessage.as_str(),
                ));
                (
                    StatusCode::ACCEPTED,
                    Json(serde_json::json!({ "ok": true })),
                )
                    .into_response()
            }
            Err(e) => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
            )
                .into_response(),
        }
    }
}

/// GET /api/team/chat?project=&agent=&limit=20
async fn chat_history(State(state): State<AppState>, Query(q): Query<AgentCtlQuery>) -> Response {
    let pm = state.pm.read().await;
    let Some(project) = pm.get(&q.project) else {
        return err_resp(
            format!("unknown project: {}", q.project),
            StatusCode::NOT_FOUND,
        );
    };
    let operator_id = llm_harness_agent_team::AgentId::new(reserved::OPERATOR);
    let agent_id = if q.agent == TEAM_TARGET {
        operator_id.clone()
    } else {
        llm_harness_agent_team::AgentId::new(&q.agent)
    };
    if q.agent != TEAM_TARGET
        && project
            .team
            .social_context()
            .is_none_or(|ctx| ctx.get_agent_info(&agent_id).is_none())
    {
        return err_resp(format!("unknown agent: {}", q.agent), StatusCode::NOT_FOUND);
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let context = match project.team.social_context() {
        Some(ctx) => ctx,
        None => return err_resp("team is not built".to_string(), StatusCode::CONFLICT),
    };
    let messages = if q.agent == TEAM_TARGET {
        match context.group_history_entries(limit, q.before) {
            Ok(entries) => entries
                .into_iter()
                .map(|entry| {
                    let view_kind = if entry.message.message_type
                        == llm_harness_agent_team::MessageType::Broadcast
                    {
                        ChatView::GroupMessage
                    } else {
                        ChatView::GroupReply
                    };
                    chat_message_json(&q.project, &entry, view_kind)
                })
                .collect::<Vec<_>>(),
            Err(error) => {
                return err_resp(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    } else {
        match context.private_history_entries(&operator_id, &agent_id, limit, q.before) {
            Ok(entries) => entries
                .into_iter()
                .map(|entry| chat_message_json(&q.project, &entry, ChatView::DirectMessage))
                .collect::<Vec<_>>(),
            Err(error) => {
                return err_resp(error.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    };
    Json(serde_json::json!({ "messages": messages })).into_response()
}

#[derive(Deserialize)]
struct MembersQuery {
    project: String,
}

async fn list_members(State(state): State<AppState>, Query(q): Query<MembersQuery>) -> Response {
    let pm = state.pm.read().await;
    let Some(project) = pm.get(&q.project) else {
        return err_resp(
            format!("unknown project: {}", q.project),
            StatusCode::NOT_FOUND,
        );
    };
    let members = project
        .spec
        .members
        .iter()
        .map(|member| {
            serde_json::json!({
                "id": member.id,
                "persona": member.persona,
                "role_label": member.role_label,
                "model": match &member.model {
                    crate::team_spec::ModelRef::Tier(model) => model,
                    crate::team_spec::ModelRef::Explicit(model) => model,
                },
                "toolkits": member.toolkits,
            })
        })
        .collect::<Vec<_>>();
    Json(serde_json::json!({ "members": members })).into_response()
}

#[derive(Deserialize)]
struct AddMemberBody {
    project: String,
    id: String,
    #[serde(default)]
    persona: String,
    #[serde(default)]
    role_label: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    toolkits: Vec<String>,
}

async fn add_member(State(state): State<AppState>, Json(body): Json<AddMemberBody>) -> Response {
    let id = body.id.trim();
    if id.is_empty()
        || id == TEAM_TARGET
        || reserved::is_reserved_id(id)
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return err_resp(
            "member id must contain only letters, digits, '_' or '-'".to_string(),
            StatusCode::BAD_REQUEST,
        );
    }
    if body.model.trim().is_empty() {
        return err_resp("model is required".to_string(), StatusCode::BAD_REQUEST);
    }
    let model = match body.model.trim() {
        "strong" | "main" | "cheap" => crate::team_spec::ModelRef::Tier(body.model.trim().into()),
        other => crate::team_spec::ModelRef::Explicit(other.into()),
    };
    let member = crate::team_spec::MemberSpec {
        id: id.to_string(),
        persona: body.persona,
        role_label: if body.role_label.trim().is_empty() {
            id.to_string()
        } else {
            body.role_label
        },
        model,
        toolkits: body.toolkits,
        behaviors: vec![],
        provider_override: None,
        persona_file: None,
    };

    let tier = state.tier.read().await.clone();
    let base_url = crate::settings::Settings::load(&state.data_root)
        .ok()
        .flatten()
        .map(|settings| settings.base_url)
        .unwrap_or_default();
    let settings = crate::settings::Settings {
        models: tier,
        scout_interval_secs: *state.scout_interval_secs.read().await,
        base_url,
    };
    let api_key = crate::settings::load_api_key(&state.data_root).unwrap_or_default();

    let mut pm = state.pm.write().await;
    let Some(entry) = pm.get_mut(&body.project) else {
        return err_resp(
            format!("unknown project: {}", body.project),
            StatusCode::NOT_FOUND,
        );
    };
    if entry.spec.members.iter().any(|member| member.id == id) {
        return err_resp(format!("member already exists: {id}"), StatusCode::CONFLICT);
    }
    entry.spec.members.push(member);
    if let Some(baseline) = entry.spec.template_baseline.as_mut()
        && !baseline.members.iter().any(|member| member.id == id)
    {
        baseline
            .members
            .push(entry.spec.members.last().unwrap().clone());
    }
    crate::team_manager::persist_spec(&state.data_root, &entry.spec);

    match pm.restart(&body.project, &settings, &api_key).await {
        Ok(()) => {
            register_team_side_effects(&state, &pm, &body.project);
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Err(e) => err_resp(e.to_string(), StatusCode::CONFLICT),
    }
}

#[derive(Deserialize)]
struct MemberMutationQuery {
    project: String,
    agent: String,
}

async fn remove_member(
    State(state): State<AppState>,
    Query(q): Query<MemberMutationQuery>,
) -> Response {
    let tier = state.tier.read().await.clone();
    let base_url = crate::settings::Settings::load(&state.data_root)
        .ok()
        .flatten()
        .map(|settings| settings.base_url)
        .unwrap_or_default();
    let settings = crate::settings::Settings {
        models: tier,
        scout_interval_secs: *state.scout_interval_secs.read().await,
        base_url,
    };
    let api_key = crate::settings::load_api_key(&state.data_root).unwrap_or_default();

    let mut pm = state.pm.write().await;
    stop_agent_thought_pump(&state.thought_pumps, &q.project, &q.agent);
    let Some(entry) = pm.get_mut(&q.project) else {
        return err_resp(
            format!("unknown project: {}", q.project),
            StatusCode::NOT_FOUND,
        );
    };
    if !entry.spec.members.iter().any(|member| member.id == q.agent) {
        return err_resp(
            format!("unknown member: {}", q.agent),
            StatusCode::NOT_FOUND,
        );
    }
    if entry.spec.members.len() <= 1 {
        return err_resp(
            "cannot remove the last member".to_string(),
            StatusCode::CONFLICT,
        );
    }
    entry.spec.members.retain(|member| member.id != q.agent);
    if let Some(baseline) = entry.spec.template_baseline.as_mut() {
        baseline.members.retain(|member| member.id != q.agent);
    }
    crate::team_manager::persist_spec(&state.data_root, &entry.spec);

    match pm.restart(&q.project, &settings, &api_key).await {
        Ok(()) => {
            register_team_side_effects(&state, &pm, &q.project);
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Err(e) => err_resp(e.to_string(), StatusCode::CONFLICT),
    }
}

// ── GET /api/team/issues ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct IssuesQuery {
    project: String,
}

async fn list_issues(State(state): State<AppState>, Query(q): Query<IssuesQuery>) -> Response {
    let pm = state.pm.read().await;
    let Some(project) = pm.get(&q.project) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "ok": false, "error": format!("unknown project: {}", q.project) })),
        )
            .into_response();
    };
    match project.issues.list() {
        Ok(issues) => {
            let items: Vec<serde_json::Value> = issues
                .iter()
                .map(|i| {
                    serde_json::json!({
                        "id": i.id,
                        "status": i.status.as_str(),
                        "source": i.source,
                        "created": i.created,
                        "title": i.title,
                    })
                })
                .collect();
            Json(serde_json::json!({ "issues": items })).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

// ── POST /api/team/issue/:id/confirm|reject ──────────────────────────────────

async fn confirm_issue(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    transition_issue(state, &id, IssueStatus::Confirmed).await
}

async fn reject_issue(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    transition_issue(state, &id, IssueStatus::Rejected).await
}

/// Shared confirm/reject logic: flip the status, publish the change, then
/// hand the confirmed issue to the project's planner via the operator
/// channel with the full issue body.
async fn transition_issue(state: AppState, id: &str, to: IssueStatus) -> Response {
    let pm = state.pm.read().await;
    // The issue lives in exactly one project; scan in registration order.
    let Some((project_id, project)) = pm
        .ids()
        .iter()
        .filter_map(|pid| pm.get(pid).map(|p| (pid.clone(), p)))
        .find(|(_, p)| p.issues.load(id).is_ok())
    else {
        return not_found(format!("issue not found: {id}"));
    };

    let issue = match project.issues.set_status(id, to) {
        Ok(i) => i,
        Err(crate::issue::IssueError::InvalidTransition { from, to }) => {
            return conflict(format!("invalid transition: {from} -> {to}"));
        }
        Err(e) => {
            return internal_error(e.to_string());
        }
    };
    state.events.publish(crate::events::issue_event(
        &project_id,
        &issue.id,
        issue.status.as_str(),
    ));

    // Confirmed issues: submit via the team's handoff routes (from spec
    // behaviors). No route → status change only.
    if to == IssueStatus::Confirmed
        && let Err(e) = pm.apply_issue_handoff(&project_id, &issue, Some(state.events.clone()))
    {
        tracing::error!("issue {id} confirmed but handoff failed: {e}");
        return conflict(format!("status changed but handoff failed: {e}"));
    }

    Json(serde_json::json!({
        "ok": true,
        "project": project_id,
        "id": issue.id,
        "status": issue.status.as_str(),
    }))
    .into_response()
}

// ── WS /api/team/events ─────────────────────────────────────────────────────

/// Stream project-level events (operator messages, issue changes) to the
/// panel. Same loopback-Origin auth model as the inspector's event WS.
async fn events_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<EventsWsQuery>,
) -> Response {
    if !is_localhost_origin(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |socket| events_ws_loop(socket, state, query.live_only))
}

#[derive(Default, Deserialize)]
struct EventsWsQuery {
    #[serde(default)]
    live_only: bool,
}

/// Localhost-browser-origin check shared with the inspector's event WS
/// (extracted for direct unit testing — full WS handshakes need a live
/// server, which lives in the smoke/verification step, not unit tests).
fn is_localhost_origin(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            let l = s.to_ascii_lowercase();
            l.starts_with("http://localhost")
                || l.starts_with("http://127.0.0.1")
                || l.starts_with("https://localhost")
                || l.starts_with("https://127.0.0.1")
        })
        .unwrap_or(false)
}

async fn events_ws_loop(socket: axum::extract::ws::WebSocket, state: AppState, live_only: bool) {
    use axum::extract::ws::Message;
    use futures::{SinkExt, StreamExt};
    let (mut sender, mut receiver) = socket.split();
    // Collector mode subscribes before acknowledging readiness, avoiding the
    // replay-to-subscription gap. The ordinary panel path is unchanged.
    let mut rx = if live_only {
        Some(state.events.subscribe())
    } else {
        None
    };
    if live_only {
        if sender
            .send(Message::Text(r#"{"type":"event_stream_ready"}"#.into()))
            .await
            .is_err()
        {
            return;
        }
    } else {
        for line in state.events.replay() {
            if sender.send(Message::Text(line.into())).await.is_err() {
                return;
            }
        }
    }
    let mut rx = rx.take().unwrap_or_else(|| state.events.subscribe());
    loop {
        tokio::select! {
            // Client pings/pongs keep the connection healthy; ignore input.
            Some(Ok(_)) = receiver.next() => {}
            result = rx.recv() => {
                match result {
                    Ok(line) => {
                        if sender.send(Message::Text(line.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) if live_only => {
                        let marker = serde_json::json!({"type": "event_stream_gap", "dropped": count}).to_string();
                        if sender.send(Message::Text(marker.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            else => break,
        }
    }
}

/// POST /api/team/projects/restart?id= — cleanly stop a project, then rebuild
/// it from its persisted spec. Issues, memory, inbox and timer journals stay.
async fn restart_project(State(state): State<AppState>, Query(q): Query<DeleteQuery>) -> Response {
    let tier = state.tier.read().await.clone();
    let base_url = crate::settings::Settings::load(&state.data_root)
        .ok()
        .flatten()
        .map(|s| s.base_url)
        .unwrap_or_default();
    let settings = crate::settings::Settings {
        models: tier,
        scout_interval_secs: *state.scout_interval_secs.read().await,
        base_url,
    };
    let api_key = crate::settings::load_api_key(&state.data_root).unwrap_or_default();

    let mut pm = state.pm.write().await;
    remove_team_from_registry(&state.registry, &q.id);
    stop_operator_pump(&state.operator_pumps, &q.id);
    remove_team_thought_pumps(&state.thought_pumps, &q.id);
    let result = pm.restart(&q.id, &settings, &api_key).await;
    register_team_side_effects(&state, &pm, &q.id);
    match result {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(TeamManagerError::UnknownTeam(id)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "ok": false, "error": format!("unknown project: {id}") })),
        )
            .into_response(),
        Err(TeamManagerError::NotAGitRepo(path)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": format!("not a git repo: {}", path.display()) })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

fn remove_team_from_registry(
    registry: &Arc<llm_harness_inspector::state::HarnessRegistry>,
    team_id: &str,
) {
    let prefix = format!("{team_id}/");
    for key in registry
        .keys()
        .into_iter()
        .filter(|k| k.starts_with(&prefix))
    {
        registry.remove(&key);
    }
}

pub(crate) fn register_team_side_effects(state: &AppState, pm: &TeamManager, team_id: &str) {
    if let Some(entry) = pm.get(team_id) {
        for member in &entry.spec.members {
            if let Some(harness) = entry
                .team
                .harness(&llm_harness_agent_team::AgentId::new(&member.id))
            {
                state
                    .registry
                    .register(format!("{team_id}/{}", member.id), harness.clone());
                spawn_agent_thought_pump(
                    state.events.clone(),
                    state.thought_pumps.clone(),
                    team_id,
                    &member.id,
                    harness,
                );
            }
        }
    }
    if let Some(ctx) = pm
        .get(team_id)
        .and_then(|entry| entry.team.social_context().cloned())
    {
        spawn_operator_pump(
            state.events.clone(),
            state.operator_pumps.clone(),
            team_id.to_string(),
            ctx,
        );
    }
}

fn not_found(message: String) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "ok": false, "error": message })),
    )
        .into_response()
}

fn conflict(message: String) -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({ "ok": false, "error": message })),
    )
        .into_response()
}

fn internal_error(message: String) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "ok": false, "error": message })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::ModelTier;
    use crate::team_manager::TeamManager;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use llm_harness_agent_team::{AgentId, TimeoutManager};
    use llm_harness_loop::test_utils::MockLlmClient;
    use std::time::Duration;
    use tower::ServiceExt;

    /// Dynamic mock echoes the actual runtime-generated task id. Every reply
    /// travels through the real model/tool/runner path, not direct inbox injection.
    struct WorkflowClient {
        replies: Mutex<std::collections::VecDeque<String>>,
        tasks: Mutex<Vec<String>>,
        attacks: Mutex<std::collections::VecDeque<(String, serde_json::Value)>>,
        hold_second: std::sync::atomic::AtomicBool,
        release: tokio::sync::Notify,
    }

    impl WorkflowClient {
        fn new(replies: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.iter().map(|s| s.to_string()).collect()),
                tasks: Mutex::new(vec![]),
                attacks: Mutex::new(std::collections::VecDeque::new()),
                hold_second: std::sync::atomic::AtomicBool::new(false),
                release: tokio::sync::Notify::new(),
            })
        }
    }

    impl llm_harness_loop::LlmClient for WorkflowClient {
        fn capabilities(&self) -> llm_harness_loop::ProviderCapabilities {
            llm_harness_loop::ProviderCapabilities::new(false, false, false)
        }

        fn chat<'a, 'b, 'f>(
            &'a self,
            req: &'b llm_harness_loop::ChatRequest,
        ) -> futures::future::BoxFuture<
            'f,
            Result<llm_harness_loop::ChatResponse, llm_harness_loop::LlmError>,
        >
        where
            'a: 'f,
            'b: 'f,
            Self: 'f,
        {
            Box::pin(async move { self.chat_stream(req).await?.collect().await })
        }

        fn chat_stream<'a, 'b, 'f>(
            &'a self,
            req: &'b llm_harness_loop::ChatRequest,
        ) -> futures::future::BoxFuture<
            'f,
            Result<llm_harness_loop::StreamHandle, llm_harness_loop::LlmError>,
        >
        where
            'a: 'f,
            'b: 'f,
            Self: 'f,
        {
            Box::pin(async move {
                use llm_harness_loop::test_utils::MockResponse;
                use llm_harness_loop::{LlmMessage, RequestContent};
                let text = req
                    .messages()
                    .iter()
                    .filter_map(|message| match message {
                        LlmMessage::User(content) => Some(
                            content
                                .iter()
                                .filter_map(|part| match part {
                                    RequestContent::Text(text) => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let task = text
                    .rsplit_once("task_id=\"")
                    .expect("runtime reply contract")
                    .1
                    .split('"')
                    .next()
                    .unwrap()
                    .to_string();
                {
                    let mut tasks = self.tasks.lock();
                    if !tasks.contains(&task) {
                        tasks.push(task.clone());
                    }
                }
                let attack = self.attacks.lock().pop_front();
                let response = if let Some((tool, args)) = attack {
                    MockResponse::tool_use("bypass", &tool, &args.to_string())
                } else if let Some(body) = ({
                    let mut replies = self.replies.lock();
                    replies.pop_front()
                }) {
                    if self.tasks.lock().len() == 2
                        && self.hold_second.load(std::sync::atomic::Ordering::SeqCst)
                    {
                        self.release.notified().await;
                    }
                    MockResponse::tool_use(
                        "reply",
                        "send_message",
                        &serde_json::json!({
                            "to": crate::pipeline::CONTROLLER, "task_id": task,
                            "task_kind": "result", "body": body,
                        })
                        .to_string(),
                    )
                } else {
                    MockResponse::text("reply delivered")
                };
                llm_harness_loop::test_utils::MockLlmClient::with_single(response)
                    .chat_stream(req)
                    .await
            })
        }
    }

    async fn controlled_fixture(
        replies: &[&str],
        missing: bool,
    ) -> (
        Router,
        SharedPm,
        Arc<WorkflowClient>,
        Arc<WorkflowClient>,
        Arc<MockLlmClient>,
        crate::env_cfg::TestProviderFactoryGuard,
    ) {
        let (router, pm, repo, factory) = setup().await;
        let orchestrator = WorkflowClient::new(replies);
        let coder = WorkflowClient::new(&["implementation evidence"]);
        let reviewer = WorkflowClient::new(&["review evidence: no findings"]);
        let idle = Arc::new(MockLlmClient::new(vec![]));
        let clients: Vec<(String, Arc<dyn llm_harness_loop::LlmClient>)> = vec![
            ("orchestrator".into(), orchestrator.clone()),
            ("coder".into(), coder.clone()),
            ("reviewer".into(), reviewer),
            ("idle".into(), idle.clone()),
        ];
        crate::env_cfg::install_provider_factory(Arc::new(move || clients.clone()));
        let members = ["orchestrator", "coder", "reviewer-performance", "judge"]
            .into_iter()
            .filter(|id| !missing || *id != "orchestrator")
            .map(|id| crate::team_spec::MemberSpec {
                id: id.into(),
                role_label: id.into(),
                persona: "test member".into(),
                model: crate::team_spec::ModelRef::Explicit(
                    match id {
                        "orchestrator" | "coder" => id,
                        "reviewer-performance" => "reviewer",
                        _ => "idle",
                    }
                    .into(),
                ),
                toolkits: vec![],
                behaviors: vec![],
                provider_override: None,
                persona_file: None,
            })
            .collect();
        let spec = crate::team_spec::TeamSpec {
            id: "controlled".into(),
            name: "controlled".into(),
            template: Some(crate::team_spec::TemplateRef {
                template_id: "coding-team".into(),
                version: "test".into(),
            }),
            template_baseline: None,
            repo_path: Some(repo.to_string_lossy().into()),
            sandbox_mode: None,
            members,
        };
        pm.write()
            .await
            .create(
                spec,
                Some(repo),
                &crate::settings::Settings {
                    models: tier(),
                    scout_interval_secs: 0,
                    base_url: String::new(),
                },
                "",
            )
            .await
            .unwrap();
        (router, pm, orchestrator, coder, idle, factory)
    }

    async fn wait_workflow_phase(pm: &SharedPm, id: &str, phase: &str) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let state = pm
                    .read()
                    .await
                    .get("controlled")
                    .unwrap()
                    .workflow
                    .as_ref()
                    .unwrap()
                    .state(id);
                if state.as_ref().is_some_and(|state| state.phase == phase)
                    && !pm
                        .read()
                        .await
                        .get("controlled")
                        .unwrap()
                        .workflow_running()
                {
                    break;
                }
                let _ = &state;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("workflow did not reach expected phase");
    }

    #[tokio::test]
    async fn controlled_chat_persists_and_publishes_once_and_activates_one_workflow() {
        let (_router, pm, orchestrator, coder, idle, _factory) = controlled_fixture(
            &["ACTION: FINISH\nFinal response.\nEvidence: done.\n"],
            false,
        )
        .await;
        // Reuse the route-test pattern with an inspectable event bus.
        let bus = Arc::new(crate::events::EventBus::new());
        let pumps = Arc::new(Mutex::new(HashMap::new()));
        let data_root = pm.read().await.data_root().to_path_buf();
        let grants = crate::workspace::WorkspaceGrants::load(&data_root).unwrap();
        let router = team_router(AppState {
            pm: pm.clone(),
            data_root,
            tier: Arc::new(RwLock::new(tier())),
            scout_interval_secs: Arc::new(RwLock::new(0)),
            events: bus.clone(),
            registry: llm_harness_inspector::state::HarnessRegistry::new("default"),
            operator_pumps: pumps.clone(),
            thought_pumps: Arc::new(Mutex::new(HashMap::new())),
            grants: Arc::new(RwLock::new(grants)),
            startup_recovery: Arc::new(RwLock::new(crate::runtime::StartupRecovery::default())),
        });
        let ctx = pm
            .read()
            .await
            .get("controlled")
            .unwrap()
            .team
            .social_context()
            .unwrap()
            .clone();
        super::spawn_operator_pump(bus.clone(), pumps, "controlled".into(), ctx);
        let (status, body) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "controlled", "target": "team", "text": "implement the feature",
            }),
        )
        .await;
        assert_eq!(status, 202, "{body}");
        let workflow_id = body["workflow_id"].as_str().unwrap();
        wait_workflow_phase(&pm, workflow_id, "done").await;
        let guard = pm.read().await;
        let project = guard.get("controlled").unwrap();
        let states = project.workflow.as_ref().unwrap().all_states();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].issue_id, workflow_id);
        assert_eq!(orchestrator.tasks.lock().len(), 1);
        assert!(coder.tasks.lock().is_empty());
        assert_eq!(idle.call_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        let ctx = project.team.social_context().unwrap().clone();
        drop(guard);
        assert_eq!(ctx.group_history_entries(100, None).unwrap().len(), 2);
        for member in ["orchestrator", "coder", "reviewer-performance", "judge"] {
            assert!(
                ctx.drain_inbox(&llm_harness_agent_team::AgentId::new(member))
                    .iter()
                    .all(|message| {
                        message.from.as_str() != reserved::OPERATOR
                            && message.body != "Final response.\nEvidence: done.\n"
                    })
            );
        }
        let (status, history) =
            get_json(&router, "/api/team/chat?project=controlled&agent=team").await;
        assert_eq!(status, 200);
        let user_messages: Vec<_> = history["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["from"] == reserved::OPERATOR)
            .collect();
        assert_eq!(user_messages.len(), 1);
        let message = user_messages[0];
        assert_eq!(message["to"], TEAM_TARGET);
        assert_eq!(message["text"], "implement the feature");
        assert_eq!(message["message_type"], "broadcast");
        assert_eq!(message["view_kind"], "group_message");
        assert_eq!(message["conversation"], TEAM_TARGET);
        assert!(message["sequence"].as_i64().is_some());
        assert!(message["broadcast_id"].as_str().is_some());
        assert_eq!(message["broadcast_id"], workflow_id);
        let final_messages: Vec<_> = history["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["from"] == crate::pipeline::ORCHESTRATOR_ID)
            .collect();
        assert_eq!(final_messages.len(), 1);
        let final_message = final_messages[0];
        let package = final_message["text"].as_str().unwrap();
        assert!(package.contains("**Outcome:** FINISH"));
        assert!(package.contains("Final response.\nEvidence: done.\n"));
        assert_eq!(package.matches("## Human review package").count(), 1);
        assert_eq!(final_message["to"], reserved::OPERATOR);
        assert_eq!(final_message["conversation"], TEAM_TARGET);
        assert_eq!(final_message["view_kind"], "group_reply");
        assert_eq!(final_message["broadcast_id"], message["broadcast_id"]);
        let (status, private_history) = get_json(
            &router,
            "/api/team/chat?project=controlled&agent=pipeline-controller",
        )
        .await;
        assert_eq!(status, 200);
        assert!(
            private_history["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|message| message["from"] != reserved::OPERATOR)
        );
        // Give the live operator pump a chance to expose any second terminal
        // message; controlled workflows must publish only the journaled one.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let events: Vec<serde_json::Value> = bus
            .replay()
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        // The two chat events plus the pipeline lifecycle fan-out (V0.6
        // sink → EventBus): this one-shot FINISH workflow emits
        // pipeline_started and pipeline_completed on top of the chat events.
        let chat_events: Vec<&serde_json::Value> = events
            .iter()
            .filter(|event| event["type"] == "team_message")
            .collect();
        assert_eq!(
            chat_events.len(),
            2,
            "exactly the journaled input and terminal chat messages may appear: {events:?}"
        );
        let lifecycle: Vec<&str> = events
            .iter()
            .filter_map(|event| match event["type"].as_str() {
                Some(name) if name.starts_with("pipeline_") => Some(name),
                _ => None,
            })
            .collect();
        assert_eq!(
            lifecycle,
            ["pipeline_started", "pipeline_completed"],
            "the workflow's lifecycle events must land on the project bus"
        );
        let input_event = chat_events
            .iter()
            .find(|event| event["from"] == reserved::OPERATOR)
            .unwrap();
        let final_event = chat_events
            .iter()
            .find(|event| event["from"] == crate::pipeline::ORCHESTRATOR_ID)
            .unwrap();
        assert_eq!(input_event["type"], "team_message");
        assert_eq!(input_event["message_id"], message["message_id"]);
        assert_eq!(input_event["conversation"], TEAM_TARGET);
        assert_eq!(input_event["view_kind"], "group_message");
        assert_eq!(input_event["broadcast_id"], message["broadcast_id"]);
        assert_eq!(final_event["text"], package);
        assert_eq!(final_event["message_id"], final_message["message_id"]);
        assert_eq!(final_event["conversation"], TEAM_TARGET);
        assert_eq!(final_event["view_kind"], "group_reply");
        assert_eq!(final_event["broadcast_id"], message["broadcast_id"]);
    }

    /// A correlated orchestrator reply whose body is not a valid ACTION is
    /// nonterminal progress: the workflow keeps waiting for a real ACTION and
    /// only the bounded decision timeout can end the round. Regression for
    /// chat-bb893129: an ACK used to be accepted as the decision and fatally
    /// escalated as "malformed ACTION first line".
    #[tokio::test]
    async fn controlled_chat_malformed_reply_is_progress_not_terminal() {
        let (router, pm, orchestrator, coder, _idle, _factory) =
            controlled_fixture(&["ACTION: FINISH"], false).await;
        let (status, body) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "controlled", "target": "team", "text": "implement the feature",
            }),
        )
        .await;
        assert_eq!(status, 202, "{body}");
        let workflow_id = body["workflow_id"].as_str().unwrap();
        // The malformed ACTION-shaped reply must NOT terminate the decision:
        // no escalation within a bounded window, no worker activation.
        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let state = pm
                    .read()
                    .await
                    .get("controlled")
                    .unwrap()
                    .workflow
                    .as_ref()
                    .unwrap()
                    .state(workflow_id);
                if state.is_some_and(|state| state.phase != "deciding") {
                    panic!("malformed reply must not terminalize the decision");
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_err();
        assert!(coder.tasks.lock().is_empty());
        assert_eq!(orchestrator.tasks.lock().len(), 1);
    }

    /// Regression for chat-bb893129: the orchestrator ACKs a decision
    /// (task_kind=ack, no ACTION) and then sends a valid ACTION in a later
    /// provider turn. The ACK must not terminalize the decision; the
    /// subsequent valid ACTION seals the run and completes the workflow.
    #[tokio::test]
    async fn controlled_ack_reply_does_not_terminalize_then_action_finishes() {
        let (router, pm, orchestrator, coder, _idle, _factory) = controlled_fixture(
            &[
                "ACK — decision in progress: closing the verification gap first.",
                "ACTION: FINISH\nFinal response after the ACK.",
            ],
            false,
        )
        .await;
        let (status, body) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "controlled", "target": "team", "text": "implement the feature",
            }),
        )
        .await;
        assert_eq!(status, 202, "{body}");
        let workflow_id = body["workflow_id"].as_str().unwrap();
        wait_workflow_phase(&pm, workflow_id, "done").await;
        let state = pm
            .read()
            .await
            .get("controlled")
            .unwrap()
            .workflow
            .as_ref()
            .unwrap()
            .state(workflow_id)
            .unwrap();
        assert_eq!(state.phase, "done");
        let terminal = state.terminal_user_outcome.expect("terminal package");
        assert_eq!(terminal.attribution, crate::pipeline::TerminalAttribution::Orchestrator);
        assert!(terminal.body.contains("**Outcome:** FINISH"));
        assert!(terminal.body.contains("Final response after the ACK."));
        assert_eq!(orchestrator.tasks.lock().len(), 1);
        assert!(coder.tasks.lock().is_empty());
    }

    /// The ACK must not dispatch a worker or cancel the decision task: only
    /// the subsequent valid ACTION advances the workflow.
    #[tokio::test]
    async fn controlled_ack_then_action_dispatches_coder_once() {
        let (router, pm, orchestrator, coder, _idle, _factory) = controlled_fixture(
            &[
                "ACK — decision in progress: closing the verification gap.",
                "ACTION: DISPATCH coder\nimplement the feature now.",
                "ACTION: FINISH\nAll work reviewed and complete.",
            ],
            false,
        )
        .await;
        let (status, body) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "controlled", "target": "team", "text": "implement",
            }),
        )
        .await;
        assert_eq!(status, 202, "{body}");
        let workflow_id = body["workflow_id"].as_str().unwrap();
        // Pre-fix, the ACK was accepted as the decision and escalated as
        // "malformed ACTION first line" before any coder dispatch. Reaching
        // done with exactly one coder assignment proves the ACK neither
        // terminalized the decision nor dispatched a worker.
        wait_workflow_phase(&pm, workflow_id, "done").await;
        assert_eq!(
            coder.tasks.lock().len(),
            1,
            "coder dispatched exactly once by the ACTION, never by the ACK"
        );
        let state = pm
            .read()
            .await
            .get("controlled")
            .unwrap()
            .workflow
            .as_ref()
            .unwrap()
            .state(workflow_id)
            .unwrap();
        assert_eq!(state.phase, "done");
    }

    #[tokio::test]
    async fn explicit_escalate_body_is_published_once_as_orchestrator_output() {
        let (router, pm, orchestrator, coder, _idle, _factory) = controlled_fixture(
            &["ACTION: ESCALATE\nA human must approve the unavailable credential."],
            false,
        )
        .await;
        let (status, body) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "controlled", "target": "team", "text": "perform guarded work",
            }),
        )
        .await;
        assert_eq!(status, 202, "{body}");
        let workflow_id = body["workflow_id"].as_str().unwrap();
        wait_workflow_phase(&pm, workflow_id, "escalated").await;

        let state = pm
            .read()
            .await
            .get("controlled")
            .unwrap()
            .workflow
            .as_ref()
            .unwrap()
            .state(workflow_id)
            .unwrap();
        let terminal = state.terminal_user_outcome.expect("terminal package");
        assert_eq!(terminal.attribution, crate::pipeline::TerminalAttribution::Orchestrator);
        assert!(terminal.body.contains("**Outcome:** ESCALATE"));
        assert!(terminal.body.contains("A human must approve the unavailable credential."));

        let (status, history) =
            get_json(&router, "/api/team/chat?project=controlled&agent=team").await;
        assert_eq!(status, 200);
        let terminals = history["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["from"] != reserved::OPERATOR)
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0]["from"], crate::pipeline::ORCHESTRATOR_ID);
        assert_eq!(terminals[0]["text"], terminal.body);
        assert_eq!(orchestrator.tasks.lock().len(), 1);
        assert!(coder.tasks.lock().is_empty());
    }

    #[tokio::test]
    async fn controlled_workflow_missing_orchestrator_runs_zero_workers() {
        let (router, pm, orchestrator, coder, idle, _factory) = controlled_fixture(&[], true).await;
        let (status, body) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "controlled", "target": "team", "text": "implement",
            }),
        )
        .await;
        assert_eq!(status, 202, "{body}");
        wait_workflow_phase(&pm, body["workflow_id"].as_str().unwrap(), "escalated").await;
        let (history_status, history) =
            get_json(&router, "/api/team/chat?project=controlled&agent=team").await;
        assert_eq!(history_status, 200);
        let terminals: Vec<_> = history["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["from"] == crate::pipeline::WORKFLOW_STATUS_ID)
            .collect();
        assert_eq!(terminals.len(), 1);
        assert!(terminals[0]["text"].as_str().unwrap().contains("requires orchestrator"));
        assert!(orchestrator.tasks.lock().is_empty());
        assert!(coder.tasks.lock().is_empty());
        assert_eq!(idle.call_count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// A: coding-team + target=team must enter the controlled workflow, never
    /// broadcast to member inboxes.
    #[tokio::test]
    async fn controlled_team_target_ingresses_workflow_not_broadcast() {
        let (router, pm, _orchestrator, _coder, idle, _factory) =
            controlled_fixture(&[], true).await;
        let (status, body) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "controlled", "target": "team", "text": "implement the feature",
            }),
        )
        .await;
        assert_eq!(status, 202, "{body}");
        // workflow_id proves the controlled ingress, not a broadcast reply.
        let workflow_id = body["workflow_id"].as_str().expect("controlled ingress");
        wait_workflow_phase(&pm, workflow_id, "escalated").await;
        let ctx = pm
            .read()
            .await
            .get("controlled")
            .unwrap()
            .team
            .social_context()
            .unwrap()
            .clone();
        for member in ["coder", "reviewer-performance", "judge"] {
            let drained = ctx.drain_inbox(&llm_harness_agent_team::AgentId::new(member));
            assert!(
                drained
                    .iter()
                    .all(|m| !m.body.contains("implement the feature")),
                "{member} inbox must not receive user chat"
            );
        }
        assert_eq!(idle.call_count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// B: coding-team + target=<worker> must enter the controlled workflow,
    /// never submit directly to that worker's inbox.
    #[tokio::test]
    async fn controlled_team_member_target_ingresses_workflow_not_worker_inbox() {
        let (router, pm, _orchestrator, _coder, idle, _factory) =
            controlled_fixture(&[], true).await;
        let (status, body) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "controlled", "target": "coder", "text": "fix the bug",
            }),
        )
        .await;
        assert_eq!(status, 202, "{body}");
        let workflow_id = body["workflow_id"].as_str().expect("controlled ingress");
        wait_workflow_phase(&pm, workflow_id, "escalated").await;
        let ctx = pm
            .read()
            .await
            .get("controlled")
            .unwrap()
            .team
            .social_context()
            .unwrap()
            .clone();
        let drained = ctx.drain_inbox(&llm_harness_agent_team::AgentId::new("coder"));
        assert!(
            drained.iter().all(|m| !m.body.contains("fix the bug")),
            "coder inbox must not receive direct user chat"
        );
        assert_eq!(idle.call_count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn controlled_workflow_malformed_action_runs_zero_workers() {
        let (router, pm, orchestrator, coder, idle, _factory) =
            controlled_fixture(&["prose\nACTION: DISPATCH coder\nimplement"], false).await;
        let (status, body) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "controlled", "target": "coder", "text": "implement",
            }),
        )
        .await;
        assert_eq!(status, 202, "{body}");
        let workflow_id = body["workflow_id"].as_str().unwrap();
        // A malformed first ACTION is nonterminal progress now: the workflow
        // must keep waiting (bounded decision timeout later), never escalate
        // as "malformed action" and never activate a worker.
        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let state = pm
                    .read()
                    .await
                    .get("controlled")
                    .unwrap()
                    .workflow
                    .as_ref()
                    .unwrap()
                    .state(workflow_id);
                if state.is_some_and(|state| state.phase != "deciding") {
                    panic!("malformed ACTION must not terminalize the decision");
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_err();
        assert_eq!(orchestrator.tasks.lock().len(), 1);
        assert!(coder.tasks.lock().is_empty());
        assert_eq!(idle.call_count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn controlled_workflow_coder_completion_is_reviewed_before_next_decision() {
        let (router, pm, orchestrator, coder, idle, _factory) = controlled_fixture(
            &[
                "ACTION: DISPATCH coder\nimplement",
                "ACTION: FINISH\nFinal response.",
            ],
            false,
        )
        .await;
        orchestrator
            .hold_second
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (status, body) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "controlled", "target": "team", "text": "implement",
            }),
        )
        .await;
        assert_eq!(status, 202, "{body}");
        let id = body["workflow_id"].as_str().unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while orchestrator.tasks.lock().len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(coder.tasks.lock().len(), 1, "exactly one coder assignment");
        assert_eq!(
            idle.call_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "judge remains idle"
        );
        let phase = pm
            .read()
            .await
            .get("controlled")
            .unwrap()
            .workflow
            .as_ref()
            .unwrap()
            .state(id)
            .unwrap()
            .phase;
        assert_eq!(phase, "deciding");
        let state = pm
            .read()
            .await
            .get("controlled")
            .unwrap()
            .workflow
            .as_ref()
            .unwrap()
            .state(id)
            .unwrap();
        assert!(state.snapshot_id.is_some());
        assert_eq!(state.reviewers_dispatched, ["reviewer-performance"]);
        assert_eq!(state.reviewer_findings.len(), 1);
        let (status, _) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "controlled", "target": "team", "text": "concurrent request",
            }),
        )
        .await;
        assert_eq!(status, 409, "one outstanding workflow per team");
        let (history_status, history) =
            get_json(&router, "/api/team/chat?project=controlled&agent=team").await;
        assert_eq!(history_status, 200);
        let user_messages: Vec<_> = history["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["from"] == reserved::OPERATOR)
            .collect();
        assert_eq!(user_messages.len(), 1);
        assert_eq!(user_messages[0]["text"], "implement");
        orchestrator.release.notify_one();
        wait_workflow_phase(&pm, id, "done").await;
        assert_eq!(coder.tasks.lock().len(), 1);
        assert_eq!(idle.call_count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn controlled_workflow_worker_tools_cannot_activate_peers() {
        let (router, pm, _, coder, idle, _factory) = controlled_fixture(
            &[
                "ACTION: DISPATCH coder\nimplement",
                "ACTION: FINISH\nFinal response.",
            ],
            false,
        )
        .await;
        coder.attacks.lock().extend([
            ("delegate_task".into(), serde_json::json!({"to": "reviewer-performance", "task": "review"})),
            ("broadcast".into(), serde_json::json!({"message": "everyone work"})),
            ("send_message".into(), serde_json::json!({"to": "judge", "body": "judge now"})),
            ("send_message".into(), serde_json::json!({"to": "operator", "body": "public reply", "broadcast_id": "old-broadcast"})),
        ]);
        let (status, body) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "controlled", "target": "team", "text": "implement",
            }),
        )
        .await;
        assert_eq!(status, 202);
        wait_workflow_phase(&pm, body["workflow_id"].as_str().unwrap(), "done").await;
        assert!(
            coder.attacks.lock().is_empty(),
            "all bypasses were attempted"
        );
        assert_eq!(coder.tasks.lock().len(), 1);
        assert_eq!(idle.call_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        let harness = pm
            .read()
            .await
            .get("controlled")
            .unwrap()
            .team
            .harness(&AgentId::new("coder"))
            .unwrap();
        let entries = harness.read_active_path().await.unwrap();
        let session = serde_json::to_string(&entries).unwrap();
        assert!(
            session
                .matches("Only execute the current assignment.")
                .count()
                >= 4,
            "tool calls must be denied, not merely ignored: {session}"
        );
    }

    #[tokio::test]
    async fn controlled_workflow_malformed_next_action_is_progress_after_coder() {
        let (router, pm, orchestrator, coder, idle, _factory) = controlled_fixture(
            &["ACTION: DISPATCH coder\nimplement", "VERDICT: PASS"],
            false,
        )
        .await;
        let (status, body) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "controlled", "target": "team", "text": "implement",
            }),
        )
        .await;
        assert_eq!(status, 202);
        // The malformed second decision ("VERDICT: PASS") is nonterminal
        // progress: no escalation/malformed-action failure, the coder has run
        // exactly once, and the workflow remains owned (bounded decision
        // timeout is the only way this round can still end).
        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let state = pm
                    .read()
                    .await
                    .get("controlled")
                    .unwrap()
                    .workflow
                    .as_ref()
                    .unwrap()
                    .state(body["workflow_id"].as_str().unwrap());
                if state.is_some_and(|state| {
                    matches!(
                        state.phase.as_str(),
                        "done" | "failed" | "escalated"
                    )
                }) {
                    panic!("malformed next action must not terminalize the decision");
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_err();
        assert_eq!(orchestrator.tasks.lock().len(), 2);
        assert_eq!(coder.tasks.lock().len(), 1);
        assert_eq!(idle.call_count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn controlled_workflow_confirmed_issue_uses_orchestrator() {
        let (router, pm, orchestrator, coder, idle, _factory) =
            controlled_fixture(&["ACTION: FINISH\nFinal response."], false).await;
        let issue = pm
            .read()
            .await
            .get("controlled")
            .unwrap()
            .issues
            .create("operator", "objective", "body")
            .unwrap();
        let (status, body) = json_post(
            &router,
            &format!("/api/team/issue/{}/confirm", issue.id),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        wait_workflow_phase(&pm, &issue.id, "done").await;
        assert_eq!(orchestrator.tasks.lock().len(), 1);
        assert!(coder.tasks.lock().is_empty());
        assert_eq!(idle.call_count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    fn tier() -> ModelTier {
        ModelTier {
            strong: "m".into(),
            main: "m".into(),
            cheap: "m".into(),
        }
    }

    /// Unique temp dir (tests run in parallel).
    fn unique_dir(prefix: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{n}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn make_repo() -> std::path::PathBuf {
        let dir = unique_dir("ct-routes-repo");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    #[tokio::test]
    async fn startup_recovery_is_exposed_to_panel() {
        let _factory = install_mock_factory();
        let data_root = unique_dir("ct-routes-recovery");
        let recovery = crate::runtime::StartupRecovery {
            status: crate::runtime::StartupStatus::Degraded,
            persisted_teams: 2,
            restored_teams: 1,
            failed_teams: vec![crate::runtime::RecoveryFailure {
                team_id: "blocked".into(),
                reason: crate::runtime::RecoveryFailureReason::WorkspaceUnauthorized,
                detail: "/repo".into(),
            }],
        };
        let state = AppState {
            pm: Arc::new(RwLock::new(TeamManager::new(&data_root))),
            data_root,
            tier: Arc::new(RwLock::new(tier())),
            scout_interval_secs: Arc::new(RwLock::new(0)),
            events: Arc::new(crate::events::EventBus::new()),
            registry: llm_harness_inspector::state::HarnessRegistry::new("default"),
            operator_pumps: Arc::new(Mutex::new(HashMap::new())),
            thought_pumps: Arc::new(Mutex::new(HashMap::new())),
            grants: Arc::new(RwLock::new(
                crate::workspace::WorkspaceGrants::load("unused-for-recovery").unwrap(),
            )),
            startup_recovery: Arc::new(RwLock::new(recovery)),
        };
        let router = team_router(state);
        let (status, json) = get_json(&router, "/api/team/startup").await;
        assert_eq!(status, 200);
        assert_eq!(json["recovery"]["status"], "degraded");
        assert_eq!(json["recovery"]["persisted_teams"], 2);
        assert_eq!(json["recovery"]["restored_teams"], 1);
        assert_eq!(
            json["recovery"]["failed_teams"][0]["reason"],
            "workspace_unauthorized"
        );
    }

    #[tokio::test]
    async fn agent_team_routes_are_exposed_under_app_prefix() {
        let (router, _pm, repo, _factory) = setup().await;

        let (legacy_status, legacy_json) = get_json(&router, "/api/team/startup").await;
        let (app_status, app_json) = get_json(&router, "/api/agent-team/startup").await;
        assert_eq!(legacy_status, 200);
        assert_eq!(app_status, 200);
        assert_eq!(legacy_json, app_json);

        let (status, json) = get_json(&router, "/api/agent-team/templates").await;
        assert_eq!(status, 200, "{json}");
        assert!(
            json["templates"]
                .as_array()
                .unwrap()
                .iter()
                .any(|template| template["id"] == "coding-team")
        );

        let (status, json) = json_post(
            &router,
            "/api/agent-team/projects",
            serde_json::json!({
                "id": "p1",
                "repo_path": repo.to_string_lossy(),
            }),
        )
        .await;
        assert_eq!(status, 201, "{json}");

        let (status, json) = get_json(&router, "/api/team/projects").await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(json["projects"][0]["id"], "p1");
    }

    /// Tests install a mock factory returning one "m" client and build the
    /// state directly (no env vars — the AppState carries tier/interval).
    #[must_use]
    fn install_mock_factory() -> crate::env_cfg::TestProviderFactoryGuard {
        crate::env_cfg::install_test_provider_factory(Arc::new(|| {
            vec![(
                "m".into(),
                Arc::new(MockLlmClient::new(vec![])) as Arc<dyn llm_harness_loop::LlmClient>,
            )]
        }))
    }

    async fn setup() -> (
        Router,
        SharedPm,
        std::path::PathBuf,
        crate::env_cfg::TestProviderFactoryGuard,
    ) {
        let _factory = install_mock_factory();
        let data_root = unique_dir("ct-routes");
        let pm = Arc::new(RwLock::new(TeamManager::new(&data_root)));
        let repo = make_repo();
        let mut grants = crate::workspace::WorkspaceGrants::load(&data_root).unwrap();
        grants.add(&repo, None).unwrap();
        let state = AppState {
            pm: pm.clone(),
            data_root: data_root.clone(),
            tier: Arc::new(RwLock::new(tier())),
            scout_interval_secs: Arc::new(RwLock::new(0)),
            events: Arc::new(crate::events::EventBus::new()),
            registry: llm_harness_inspector::state::HarnessRegistry::new("default"),
            operator_pumps: Arc::new(Mutex::new(HashMap::new())),
            thought_pumps: Arc::new(Mutex::new(HashMap::new())),
            grants: Arc::new(RwLock::new(grants)),
            startup_recovery: Arc::new(RwLock::new(crate::runtime::StartupRecovery::default())),
        };
        let router = team_router(state);
        (router, pm, repo, _factory)
    }

    async fn json_put(
        router: &Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (u16, serde_json::Value) {
        json_method(router, "PUT", uri, body).await
    }

    async fn json_post(
        router: &Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (u16, serde_json::Value) {
        json_method(router, "POST", uri, body).await
    }

    async fn json_method(
        router: &Router,
        method: &str,
        uri: &str,
        body: serde_json::Value,
    ) -> (u16, serde_json::Value) {
        let resp = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            serde_json::json!(null)
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::json!(null))
        };
        (status, json)
    }

    async fn get_json(router: &Router, uri: &str) -> (u16, serde_json::Value) {
        let resp = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::json!(null));
        (status, json)
    }

    #[tokio::test]
    async fn restarting_replaces_registered_operator_pump() {
        let events = Arc::new(crate::events::EventBus::new());
        let mut rx = events.subscribe();
        let pumps = Arc::new(Mutex::new(HashMap::new()));
        let old_ctx = llm_harness_agent_team::SocialContext::new(10);
        super::spawn_operator_pump(events.clone(), pumps.clone(), "p1".into(), old_ctx.clone());

        let message = |body: &'static str| llm_harness_agent_team::InboxMessage {
            journal_id: None,
            from: AgentId::new("scout"),
            to: AgentId::new("operator"),
            body: body.into(),
            message_type: llm_harness_agent_team::MessageType::Direct,
            priority: 0,
            task_id: None,
            task_kind: None,
            broadcast_id: None,
            message_id: None,
        };
        old_ctx.push_message(message("before restart")).unwrap();
        assert!(rx.recv().await.unwrap().contains("before restart"));

        super::spawn_operator_pump(
            events,
            pumps.clone(),
            "p1".into(),
            llm_harness_agent_team::SocialContext::new(10),
        );
        old_ctx.push_message(message("after restart")).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(rx.try_recv().is_err(), "old pump must be aborted");
        assert_eq!(pumps.lock().len(), 1);
    }

    #[tokio::test]
    async fn register_via_http_validates_repo() {
        let (router, _pm, repo, _factory) = setup().await;
        let bad_repo = repo.parent().unwrap().join("not-a-git-repo");
        std::fs::create_dir_all(&bad_repo).unwrap();
        let (status, json) = json_post(
            &router,
            "/api/team/fs/grants",
            serde_json::json!({ "path": bad_repo.to_string_lossy() }),
        )
        .await;
        assert_eq!(status, 201, "{json}");
        let (status, json) = json_post(
            &router,
            "/api/team/projects",
            serde_json::json!({ "id": "bad", "repo_path": bad_repo.to_string_lossy() }),
        )
        .await;
        assert_eq!(status, 400, "{json}");
        assert_eq!(json["ok"], false);
    }

    #[tokio::test]
    async fn register_and_list_projects() {
        let (router, _pm, repo, _factory) = setup().await;
        let (status, json) = json_post(
            &router,
            "/api/team/projects",
            serde_json::json!({ "id": "p1", "repo_path": repo.to_string_lossy() }),
        )
        .await;
        assert_eq!(status, 201, "{json}");
        let (status, json) = get_json(&router, "/api/team/projects").await;
        assert_eq!(status, 200);
        assert_eq!(json["projects"][0]["id"], "p1");

        let (status, pulse) = get_json(&router, "/api/team/pulse?project=p1").await;
        assert_eq!(status, 200, "{pulse}");
        assert_eq!(pulse["timers"]["pending"], 0, "{pulse}");
    }

    async fn create_generic_chat_team(pm: &SharedPm, repo: &std::path::Path) {
        // Direct chat/broadcast remain generic-team features. Coding-team
        // ingress now has the separate controlled_workflow_* contract tests.
        let mut spec = crate::template_store::TemplateStore::new(None)
            .load("coding-team")
            .unwrap()
            .spec;
        spec.id = "p1".into();
        spec.template = None;
        spec.members
            .retain(|m| matches!(m.id.as_str(), "coder" | "reviewer"));
        for member in &mut spec.members {
            member.model = crate::team_spec::ModelRef::Explicit("m".into());
        }
        pm.write()
            .await
            .create(
                spec,
                Some(repo.to_path_buf()),
                &crate::settings::Settings {
                    models: tier(),
                    scout_interval_secs: 0,
                    base_url: String::new(),
                },
                "",
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn chat_routes_by_explicit_target() {
        let (router, pm, repo, _factory) = setup().await;
        create_generic_chat_team(&pm, &repo).await;

        // coder route accepted.
        let (status, json) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({ "project": "p1", "target": "coder", "text": "fix the bug" }),
        )
        .await;
        assert_eq!(status, 202, "coder: {json}");

        // reviewer route accepted (second template member).
        let (status, _) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({ "project": "p1", "target": "reviewer", "text": "review when ready" }),
        )
        .await;
        assert_eq!(status, 202);

        // Non-member target rejected.
        let (status, _) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({ "project": "p1", "target": "nonexistent-role", "text": "no" }),
        )
        .await;
        assert_eq!(status, 400);

        // Unknown project → 404.
        let (status, _) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({ "project": "ghost", "target": "coder", "text": "x" }),
        )
        .await;
        assert_eq!(status, 404);

        // The coder actually received the chat text in its inbox.
        let project = pm
            .read()
            .await
            .get("p1")
            .unwrap()
            .team
            .social_context()
            .unwrap()
            .clone();
        let drained = project.drain_inbox(&llm_harness_agent_team::AgentId::new("coder"));
        assert!(
            drained.iter().any(|m| m.body.contains("fix the bug")),
            "coder inbox must carry the chat text"
        );
        // The reviewer received its message too.
        let drained = project.drain_inbox(&llm_harness_agent_team::AgentId::new("reviewer"));
        assert!(drained.iter().any(|m| m.body.contains("review when ready")));
    }

    #[tokio::test]
    async fn group_mention_remains_visible_to_team() {
        let (router, pm, repo, _factory) = setup().await;
        create_generic_chat_team(&pm, &repo).await;

        let (status, json) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "p1",
                "target": "team",
                "text": "@coder please investigate",
            }),
        )
        .await;
        assert_eq!(status, 202, "{json}");

        let (_, json) = get_json(&router, "/api/team/chat?project=p1&agent=team").await;
        let group_messages = json["messages"].as_array().unwrap();
        assert_eq!(group_messages.len(), 1, "{json}");
        assert_eq!(group_messages[0]["view_kind"], "group_message");
        assert_eq!(group_messages[0]["to"], "team");
        assert_eq!(group_messages[0]["text"], "@coder please investigate");

        let context = pm
            .read()
            .await
            .get("p1")
            .unwrap()
            .team
            .social_context()
            .unwrap()
            .clone();
        let coder_messages = context.drain_inbox(&llm_harness_agent_team::AgentId::new("coder"));
        assert!(
            coder_messages
                .iter()
                .any(|message| message.body == "@coder please investigate")
        );
    }

    #[tokio::test]
    async fn chat_history_exposes_stable_ids_and_server_views() {
        let (router, pm, repo, _factory) = setup().await;
        json_post(
            &router,
            "/api/team/projects",
            serde_json::json!({
                "id": "p1",
                "repo_path": repo.to_string_lossy(),
                "template_id": "coding-team",
            }),
        )
        .await;

        let ctx = pm
            .read()
            .await
            .get("p1")
            .unwrap()
            .team
            .social_context()
            .unwrap()
            .clone();
        let operator = llm_harness_agent_team::AgentId::new(reserved::OPERATOR);
        let coder = llm_harness_agent_team::AgentId::new("coder");
        let broadcast = ctx
            .broadcast_with_message_id(&operator, "team prompt", None)
            .unwrap();
        ctx.send_direct_with_message_id(
            &coder,
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
            &coder,
            "private note",
            llm_harness_agent_team::priority::OPERATOR,
            None,
            None,
            None,
        )
        .unwrap();

        let (_, json) = get_json(&router, "/api/team/chat?project=p1&agent=team").await;
        let group_messages = json["messages"].as_array().unwrap();
        assert_eq!(group_messages.len(), 2);
        assert!(group_messages.iter().all(|message| {
            !message["message_id"]
                .as_str()
                .unwrap_or_default()
                .is_empty()
        }));
        assert!(
            group_messages
                .iter()
                .any(|message| message["view_kind"] == "group_message")
        );
        assert!(
            group_messages
                .iter()
                .any(|message| message["view_kind"] == "group_reply")
        );
        let newest_sequence = group_messages
            .last()
            .expect("history must contain messages")["sequence"]
            .as_i64()
            .expect("durable history must expose a sequence");
        let (_, json) = get_json(
            &router,
            &format!("/api/team/chat?project=p1&agent=team&before={newest_sequence}"),
        )
        .await;
        let older_messages = json["messages"].as_array().expect("history response");
        assert_eq!(older_messages.len(), 1);
        assert_eq!(older_messages[0]["view_kind"], "group_message");

        let (_, json) = get_json(&router, "/api/team/chat?project=p1&agent=coder").await;
        let private_messages = json["messages"].as_array().unwrap();
        assert_eq!(private_messages.len(), 1);
        assert_eq!(private_messages[0]["view_kind"], "direct_message");
        assert!(
            !private_messages[0]["message_id"]
                .as_str()
                .unwrap_or_default()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn confirm_issue_changes_status_without_handoff() {
        let (router, pm, repo, _factory) = setup().await;
        json_post(
            &router,
            "/api/team/projects",
            serde_json::json!({
                "id": "p1",
                "repo_path": repo.to_string_lossy(),
                "template_id": "coding-team",
            }),
        )
        .await;
        // Seed a pending issue (as the scout would).
        let pm_guard = pm.read().await;
        let store = crate::issue::IssueStore::new(pm_guard.get("p1").unwrap().issues.root());
        drop(pm_guard);
        let issue = store
            .create("scout", "Fix executor io context", "现象：错误被吞")
            .unwrap();

        // Confirm via HTTP.
        let (status, json) = json_post(
            &router,
            &format!("/api/team/issue/{}/confirm", issue.id),
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(json["status"], "confirmed");

        // Status persisted.
        let listed = store.list().unwrap();
        assert_eq!(listed[0].status, IssueStatus::Confirmed);

        // V0 contract: confirm performs a status change only — the coding-team
        // template defines no issue handoff route (planner removed).
        let ctx = pm
            .read()
            .await
            .get("p1")
            .unwrap()
            .team
            .social_context()
            .unwrap()
            .clone();
        for member in ["coder", "reviewer", "judge"] {
            let drained = ctx.drain_inbox(&llm_harness_agent_team::AgentId::new(member));
            assert!(
                !drained
                    .iter()
                    .any(|m| m.body.contains("操作员确认") && m.body.contains("现象：错误被吞")),
                "no member may receive the confirmed issue body without a handoff route"
            );
        }
    }

    #[tokio::test]
    async fn reject_issue_flips_status_without_planner_handoff() {
        let (router, pm, repo, _factory) = setup().await;
        json_post(
            &router,
            "/api/team/projects",
            serde_json::json!({ "id": "p1", "repo_path": repo.to_string_lossy() }),
        )
        .await;
        let store = crate::issue::IssueStore::new(pm.read().await.get("p1").unwrap().issues.root());
        let issue = store.create("scout", "dup thing", "b").unwrap();

        let (status, _) = json_post(
            &router,
            &format!("/api/team/issue/{}/reject", issue.id),
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(store.list().unwrap()[0].status, IssueStatus::Rejected);

        // No planner handoff on rejection.
        let ctx = pm
            .read()
            .await
            .get("p1")
            .unwrap()
            .team
            .social_context()
            .unwrap()
            .clone();
        let drained = ctx.drain_inbox(&llm_harness_agent_team::AgentId::new("planner"));
        assert!(drained.is_empty(), "rejected issue must not reach planner");
    }

    #[tokio::test]
    async fn double_confirm_conflicts() {
        let (router, pm, repo, _factory) = setup().await;
        json_post(
            &router,
            "/api/team/projects",
            serde_json::json!({ "id": "p1", "repo_path": repo.to_string_lossy() }),
        )
        .await;
        let store = crate::issue::IssueStore::new(pm.read().await.get("p1").unwrap().issues.root());
        let issue = store.create("scout", "t", "b").unwrap();

        let (s1, _) = json_post(
            &router,
            &format!("/api/team/issue/{}/confirm", issue.id),
            serde_json::json!(null),
        )
        .await;
        assert_eq!(s1, 200);
        // confirmed → confirmed is not a legal transition.
        let (s2, json) = json_post(
            &router,
            &format!("/api/team/issue/{}/confirm", issue.id),
            serde_json::json!(null),
        )
        .await;
        assert_eq!(s2, 409, "{json}");
    }

    #[tokio::test]
    async fn unknown_issue_404s() {
        let (router, _pm, _repo, _factory) = setup().await;
        let (status, _) = json_post(
            &router,
            "/api/team/issue/I-999/confirm",
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, 404);
    }

    #[test]
    fn ws_origin_check_allows_localhost_only() {
        let mut headers = axum::http::HeaderMap::new();
        assert!(!super::is_localhost_origin(&headers), "no origin → deny");
        headers.insert("origin", "http://evil.com".parse().unwrap());
        assert!(
            !super::is_localhost_origin(&headers),
            "foreign origin → deny"
        );
        headers.insert("origin", "http://localhost:8080".parse().unwrap());
        assert!(super::is_localhost_origin(&headers), "localhost → allow");
        headers.insert("origin", "http://127.0.0.1:9000".parse().unwrap());
        assert!(super::is_localhost_origin(&headers), "loopback → allow");
    }

    #[tokio::test]
    async fn issue_transition_publishes_event() {
        let (_router, pm, repo, _factory) = setup().await;
        // Rebuild the router with a bus we can inspect directly.
        let bus = Arc::new(crate::events::EventBus::new());
        let mut rx = bus.subscribe();
        let data_root = pm.read().await.data_root().to_path_buf();
        let grants = crate::workspace::WorkspaceGrants::load(&data_root).unwrap();
        let state = AppState {
            pm: pm.clone(),
            data_root: data_root.clone(),
            tier: Arc::new(RwLock::new(tier())),
            scout_interval_secs: Arc::new(RwLock::new(0)),
            events: bus,
            registry: llm_harness_inspector::state::HarnessRegistry::new("default"),
            operator_pumps: Arc::new(Mutex::new(HashMap::new())),
            thought_pumps: Arc::new(Mutex::new(HashMap::new())),
            grants: Arc::new(RwLock::new(grants)),
            startup_recovery: Arc::new(RwLock::new(crate::runtime::StartupRecovery::default())),
        };
        let router2 = team_router(state);
        json_post(
            &router2,
            "/api/team/projects",
            serde_json::json!({ "id": "p1", "repo_path": repo.to_string_lossy() }),
        )
        .await;
        let store = crate::issue::IssueStore::new(pm.read().await.get("p1").unwrap().issues.root());
        let issue = store.create("scout", "t", "b").unwrap();
        let (status, _) = json_post(
            &router2,
            &format!("/api/team/issue/{}/confirm", issue.id),
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, 200);
        let got = rx.try_recv().unwrap();
        assert!(
            got.contains("issue_changed") && got.contains(&issue.id),
            "{got}"
        );
    }

    #[tokio::test]
    async fn settings_roundtrip_via_api() {
        let (router, _pm, _repo, _factory) = setup().await;
        // Initial state: nothing configured.
        let (status, json) = get_json(&router, "/api/team/settings").await;
        assert_eq!(status, 200);
        assert_eq!(json["api_key_set"], false);

        // Save settings.
        let (status, json) = json_post(
            &router,
            "/api/team/settings",
            serde_json::json!({
                "strong": "glm-5.2",
                "main": "glm-4.7",
                "cheap": "glm-4.5-air",
                "base_url": "https://example.com/v4",
                "scout_interval_secs": 600,
            }),
        )
        .await;
        assert_eq!(status, 200, "{json}");

        // Save an API key.
        let (status, _) = json_post(
            &router,
            "/api/team/apikey",
            serde_json::json!({ "api_key": "sk-test" }),
        )
        .await;
        assert_eq!(status, 200);

        // Read back.
        let (status, json) = get_json(&router, "/api/team/settings").await;
        assert_eq!(status, 200);
        assert_eq!(json["scout_interval_secs"], 600);
        assert_eq!(json["base_url"], "https://example.com/v4");
        assert_eq!(json["api_key_set"], true);
    }

    #[test]
    fn base_url_normalization() {
        assert_eq!(
            super::normalize_base_url("api.hyper-op.com"),
            "https://api.hyper-op.com"
        );
        assert_eq!(super::normalize_base_url("https://x.io/"), "https://x.io");
        assert_eq!(
            super::normalize_base_url("http://insecure.io"),
            "http://insecure.io"
        );
        assert_eq!(super::normalize_base_url("  "), "");
    }

    #[tokio::test]
    async fn settings_validation_rejects_empty_models() {
        let (router, _pm, _repo, _factory) = setup().await;
        let (status, json) = json_post(
            &router,
            "/api/team/settings",
            serde_json::json!({
                "strong": "",
                "main": "m",
                "cheap": "c",
                "base_url": "",
                "scout_interval_secs": 0,
            }),
        )
        .await;
        assert_eq!(status, 400, "{json}");
    }

    fn enc(s: &std::path::Path) -> String {
        s.to_string_lossy()
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                    (b as char).to_string()
                }
                other => format!("%{other:02X}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn fs_listing_marks_repos_and_skips_hidden() {
        let (router, _pm, repo, _factory) = setup().await;
        // The repo dir contains a .git (hidden) — listing must not show it.
        let (status, json) = get_json(&router, &format!("/api/team/fs?path={}", enc(&repo))).await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(json["is_repo"], true);
        let dirs = json["dirs"].as_array().unwrap();
        assert!(
            dirs.iter()
                .all(|d| !d["name"].as_str().unwrap().starts_with('.'))
        );
    }

    #[tokio::test]
    async fn fs_listing_rejects_missing_path() {
        let (router, _pm, _repo, _factory) = setup().await;
        let (status, _) = get_json(&router, "/api/team/fs?path=/no/such/dir").await;
        assert_eq!(status, 403);
    }

    #[tokio::test]
    async fn workspace_grant_lifecycle_controls_browsing() {
        let (router, _pm, repo, _factory) = setup().await;
        // canonicalize before comparing: the API returns canonical paths and on
        // macOS /var is a symlink to /private/var, so the raw temp path never
        // compares equal.
        let outside = repo.parent().unwrap().join("outside-workspace");
        std::fs::create_dir_all(&outside).unwrap();
        let outside = std::fs::canonicalize(&outside).unwrap();

        let (status, json) = get_json(&router, "/api/team/fs/grants").await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(json["grants"].as_array().unwrap().len(), 1);

        let (status, json) = json_post(
            &router,
            "/api/team/fs/grants",
            serde_json::json!({ "path": outside.to_string_lossy(), "label": "outside" }),
        )
        .await;
        assert_eq!(status, 201, "{json}");
        let grant_id = json["id"].as_str().unwrap();

        let (status, json) =
            get_json(&router, &format!("/api/team/fs?path={}", enc(&outside))).await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(json["path"], outside.to_string_lossy().to_string());

        let (status, json) = json_method(
            &router,
            "DELETE",
            &format!("/api/team/fs/grants?id={grant_id}"),
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, 200, "{json}");
        let (status, _) = get_json(&router, &format!("/api/team/fs?path={}", enc(&outside))).await;
        assert_eq!(status, 403);
        std::fs::remove_dir_all(&outside).unwrap();
    }

    #[tokio::test]
    async fn ungranted_paths_and_projects_are_rejected() {
        let (router, _pm, repo, _factory) = setup().await;
        let outside = repo.parent().unwrap().join("outside-workspace");
        std::fs::create_dir_all(&outside).unwrap();

        let (status, json) =
            get_json(&router, &format!("/api/team/fs?path={}", enc(&outside))).await;
        assert_eq!(status, 403, "{json}");
        let (status, json) = json_post(
            &router,
            "/api/team/projects",
            serde_json::json!({ "id": "outside", "repo_path": outside.to_string_lossy() }),
        )
        .await;
        assert_eq!(status, 403, "{json}");
        std::fs::remove_dir_all(&outside).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escape_is_rejected() {
        let (router, _pm, repo, _factory) = setup().await;
        let outside = repo.parent().unwrap().join("outside-symlink-target");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("escape")).unwrap();

        let (status, _) = get_json(
            &router,
            &format!("/api/team/fs?path={}", enc(&repo.join("escape"))),
        )
        .await;
        assert_eq!(status, 403);
        let (status, _) = json_post(
            &router,
            "/api/team/projects",
            serde_json::json!({ "id": "escape", "repo_path": repo.join("escape").to_string_lossy() }),
        )
        .await;
        assert_eq!(status, 403);
        std::fs::remove_file(repo.join("escape")).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }

    #[tokio::test]
    async fn raw_wsl_unc_path_grants_then_creates_team() {
        let (router, pm, _repo, _factory) = setup().await;
        // A fresh workspace NOT pre-granted by setup(), addressed only via a
        // raw WSL UNC spelling, never hard-coding runtime host values.
        let workspace = _repo.parent().unwrap().join("unc-workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        let canonical = std::fs::canonicalize(&workspace).unwrap();
        let tail = canonical
            .strip_prefix("/")
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let unc = format!(r"\\wsl.localhost\TestDistro\{}", tail.replace('/', "\\"));
        let unc_mixed = format!(r"\\WSL$/TestDistro/{}", tail.replace('/', "\\"));

        // Project creation with the raw UNC fails before the grant.
        let (status, json) = json_post(
            &router,
            "/api/team/projects",
            serde_json::json!({ "id": "unc-team", "repo_path": unc.clone() }),
        )
        .await;
        assert_eq!(status, 403, "{json}");

        // Grant via the raw UNC.
        let (status, json) = json_post(
            &router,
            "/api/team/fs/grants",
            serde_json::json!({ "path": unc.clone(), "label": "unc" }),
        )
        .await;
        assert_eq!(status, 201, "{json}");
        assert_eq!(json["path"], canonical.to_string_lossy().as_ref());

        // Case-insensitive / mixed-separator spelling is authorized too.
        let (status, json) = get_json(
            &router,
            &format!(
                "/api/team/fs?path={}",
                enc(std::path::Path::new(&unc_mixed))
            ),
        )
        .await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(json["path"], canonical.to_string_lossy().as_ref());

        // Project creation with the same raw UNC now succeeds, and the
        // manager stores the canonical Linux identity.
        let (status, json) = json_post(
            &router,
            "/api/team/projects",
            serde_json::json!({ "id": "unc-team", "repo_path": unc.clone() }),
        )
        .await;
        assert_eq!(status, 201, "{json}");
        assert_eq!(
            pm.read().await.get("unc-team").unwrap().repo_path,
            Some(canonical.clone())
        );
        std::fs::remove_dir_all(&workspace).unwrap();
    }

    #[tokio::test]
    async fn delete_project_removes_and_persists() {
        let (router, pm, repo, _factory) = setup().await;
        json_post(
            &router,
            "/api/team/projects",
            serde_json::json!({ "id": "p1", "repo_path": repo.to_string_lossy() }),
        )
        .await;
        assert!(pm.read().await.get("p1").is_some());

        // DELETE via query param.
        let resp = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/team/projects?id=p1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert!(pm.read().await.get("p1").is_none());

        // Second delete → 404.
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/team/projects?id=p1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);
    }

    #[tokio::test]
    async fn restart_project_restores_persisted_team_and_pending_timer() {
        let (router, pm, repo, _factory) = setup().await;
        json_post(
            &router,
            "/api/team/projects",
            serde_json::json!({ "id": "p1", "repo_path": repo.to_string_lossy() }),
        )
        .await;
        let member_count = pm.read().await.get("p1").unwrap().spec.members.len();

        let ctx = pm
            .read()
            .await
            .get("p1")
            .unwrap()
            .team
            .social_context()
            .unwrap()
            .clone();
        let timer = TimeoutManager::new(ctx);
        timer
            .set_self_reminder_durably(
                AgentId::new("a"),
                "check after restart",
                Duration::from_secs(60),
                Some("restart-check".into()),
            )
            .unwrap();

        let (status, json) = json_post(
            &router,
            "/api/team/projects/restart?id=p1",
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, 200, "{json}");
        assert!(pm.read().await.get("p1").is_some());
        assert_eq!(
            pm.read().await.get("p1").unwrap().spec.members.len(),
            member_count
        );
        assert_eq!(
            pm.read()
                .await
                .get("p1")
                .unwrap()
                .team
                .social_context()
                .unwrap()
                .pending_timer_count(),
            1
        );

        pm.write().await.shutdown("p1").await.unwrap();
        assert!(
            pm.read().await.get("p1").is_none(),
            "restart test should clean up the rebuilt team"
        );
    }

    // ── per-agent config PUT ─────────────────────────────────────────────

    async fn register_template_team(router: &Router, repo: &std::path::Path) {
        let (status, json) = json_post(
            router,
            "/api/team/projects",
            serde_json::json!({
                "id": "p1",
                "repo_path": repo.to_string_lossy(),
                "template_id": "coding-team",
            }),
        )
        .await;
        assert_eq!(status, 201, "{json}");
    }

    #[tokio::test]
    async fn agent_config_put_hot_applies_persona() {
        let (router, pm, repo, _factory) = setup().await;
        register_template_team(&router, &repo).await;

        let (status, json) = json_put(
            &router,
            "/api/team/agent/config",
            serde_json::json!({ "project": "p1", "agent": "coder", "persona": "new persona text" }),
        )
        .await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(json["rebuilt"], false, "persona-only change hot-applies");

        let (status, json) =
            get_json(&router, "/api/team/agent/config?project=p1&agent=coder").await;
        assert_eq!(status, 200);
        assert!(
            json["system_prompt"]
                .as_str()
                .unwrap()
                .contains("new persona text"),
            "persona hot-applied: {}",
            json["system_prompt"]
        );

        // 实例 spec 持久化已更新
        let spec_text =
            std::fs::read_to_string(pm.read().await.data_root().join("teams/p1/team.json"))
                .unwrap();
        assert!(spec_text.contains("new persona text"));
    }

    #[tokio::test]
    async fn agent_config_model_change_rebuilds_member() {
        let (router, _pm, repo, _factory) = setup().await;
        register_template_team(&router, &repo).await;

        let (status, json) = json_put(
            &router,
            "/api/team/agent/config",
            serde_json::json!({
                "project": "p1", "agent": "coder", "model": "explicit-model-x",
            }),
        )
        .await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(json["rebuilt"], true, "model change rebuilds");

        let (status, json) =
            get_json(&router, "/api/team/agent/config?project=p1&agent=coder").await;
        assert_eq!(status, 200);
        assert_eq!(json["model"], "explicit-model-x");
    }

    #[tokio::test]
    async fn agent_config_tier_change_resolves_via_settings() {
        let (router, _pm, repo, _factory) = setup().await;
        register_template_team(&router, &repo).await;

        // tier "cheap" resolves to the mock model "m" from settings.
        let (status, json) = json_put(
            &router,
            "/api/team/agent/config",
            serde_json::json!({ "project": "p1", "agent": "coder", "model": "cheap" }),
        )
        .await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(json["rebuilt"], true);

        let (_, json) = get_json(&router, "/api/team/agent/config?project=p1&agent=coder").await;
        assert_eq!(json["model"], "m");
        assert_eq!(json["override"]["model"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn agent_config_override_key_isolated() {
        let (router, pm, repo, _factory) = setup().await;
        register_template_team(&router, &repo).await;

        let (status, json) = json_put(
            &router,
            "/api/team/agent/config",
            serde_json::json!({
                "project": "p1", "agent": "coder",
                "api_key": "sk-secret-x", "base_url": "o.io/v4",
            }),
        )
        .await;
        assert_eq!(status, 200, "{json}");

        // secrets 文件 0600 (unix; Windows has no POSIX mode bits)
        let secrets = pm.read().await.data_root().join("teams/p1/secrets/coder");
        assert!(secrets.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&secrets).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "secrets file must be 0600");
        }

        let spec_text =
            std::fs::read_to_string(pm.read().await.data_root().join("teams/p1/team.json"))
                .unwrap();
        assert!(
            !spec_text.contains("sk-secret-x"),
            "provider secret must not leak into team.json"
        );

        // GET 不回显明文
        let body = get_json(&router, "/api/team/agent/config?project=p1&agent=coder")
            .await
            .1;
        let body_text = serde_json::to_string(&body).unwrap();
        assert!(
            !body_text.contains("sk-secret-x"),
            "key must never echo back"
        );
        assert_eq!(body["override"]["api_key_set"], true);
        assert_eq!(body["override"]["base_url"], "https://o.io/v4");
    }

    #[tokio::test]
    async fn agent_config_key_change_rebuilds_member() {
        let (router, _pm, repo, _factory) = setup().await;
        register_template_team(&router, &repo).await;

        for key in ["sk-first", "sk-second"] {
            let (status, json) = json_put(
                &router,
                "/api/team/agent/config",
                serde_json::json!({
                    "project": "p1", "agent": "coder", "api_key": key,
                }),
            )
            .await;
            assert_eq!(status, 200, "{json}");
            assert_eq!(json["rebuilt"], true, "key-only change must rebuild");
        }
    }

    #[tokio::test]
    async fn agent_config_unknown_member_404() {
        let (router, _pm, repo, _factory) = setup().await;
        register_template_team(&router, &repo).await;
        let (status, _) = json_put(
            &router,
            "/api/team/agent/config",
            serde_json::json!({ "project": "p1", "agent": "ghost", "persona": "x" }),
        )
        .await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn agent_config_does_not_write_secrets_for_unknown_ids() {
        let (router, pm, repo, _factory) = setup().await;
        register_template_team(&router, &repo).await;
        let (status, _) = json_put(
            &router,
            "/api/team/agent/config",
            serde_json::json!({
                "project": "../escape",
                "agent": "../../leaked-secret",
                "api_key": "sk-escape",
            }),
        )
        .await;
        assert_eq!(status, 404);

        let leaked = pm.read().await.data_root().join("leaked-secret");
        assert!(!leaked.exists(), "unknown IDs must not construct paths");
    }

    // ── templates / upgrade-check ────────────────────────────────────────

    #[tokio::test]
    async fn templates_endpoint_lists_builtin() {
        let (router, _pm, _repo, _factory) = setup().await;
        let (status, json) = get_json(&router, "/api/team/templates").await;
        assert_eq!(status, 200);
        let templates = json["templates"].as_array().unwrap();
        assert!(
            templates.iter().any(
                |t| t["id"] == "coding-team" && t["member_ids"].as_array().unwrap().len() == 3
            ),
            "{templates:?}"
        );
    }

    #[tokio::test]
    async fn upgrade_check_classifies_overridden_members() {
        let (router, _pm, repo, _factory) = setup().await;
        register_template_team(&router, &repo).await;

        // 初始：全部成员 = 模板原版（同版本）→ 无 overridden
        let (status, json) = json_post(
            &router,
            "/api/team/upgrade-check",
            serde_json::json!({ "project": "p1" }),
        )
        .await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(json["overridden"].as_array().unwrap().len(), 0);
        assert_eq!(json["would_follow"].as_array().unwrap().len(), 3);
        assert_eq!(json["template_current_version"], "6");

        // 改一个成员 persona → 该成员进入 overridden
        let (status, _) = json_put(
            &router,
            "/api/team/agent/config",
            serde_json::json!({ "project": "p1", "agent": "coder", "persona": "custom coder" }),
        )
        .await;
        assert_eq!(status, 200);
        let (_, json) = json_post(
            &router,
            "/api/team/upgrade-check",
            serde_json::json!({ "project": "p1" }),
        )
        .await;
        let overridden = json["overridden"].as_array().unwrap();
        assert_eq!(overridden.len(), 1);
        assert_eq!(overridden[0], "coder");
        assert_eq!(json["would_follow"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn upgrade_check_no_template_is_error() {
        let (router, _pm, repo, _factory) = setup().await;
        let (status, _) = json_post(
            &router,
            "/api/team/projects",
            serde_json::json!({ "id": "p1", "repo_path": repo.to_string_lossy() }),
        )
        .await;
        assert_eq!(status, 201);
        let (status, _) = json_post(
            &router,
            "/api/team/upgrade-check",
            serde_json::json!({ "project": "p1" }),
        )
        .await;
        assert_eq!(status, 400);
    }

    #[tokio::test]
    async fn upgrade_apply_updates_baseline_members_and_persists_version() {
        let (router, pm, repo, _factory) = setup().await;
        json_post(
            &router,
            "/api/team/projects",
            serde_json::json!({ "id": "p1", "repo_path": repo.to_string_lossy() }),
        )
        .await;

        let template_dir = pm
            .read()
            .await
            .data_root()
            .join("templates")
            .join("planner-team");
        std::fs::create_dir_all(&template_dir).unwrap();
        std::fs::write(
            template_dir.join("team.json"),
            r#"{"id":"planner-team","name":"Planner Team","version":"2","members":[{"id":"planner","persona":"template persona v2","role_label":"规划者","model":"main","toolkits":["fs"],"behaviors":[]}]}"#,
        )
        .unwrap();
        {
            let mut guard = pm.write().await;
            let entry = guard.get_mut("p1").unwrap();
            entry.spec.template = Some(crate::team_spec::TemplateRef {
                template_id: "planner-team".into(),
                version: "1".into(),
            });
            let mut baseline_member = entry.spec.members[0].clone();
            baseline_member.persona = "created persona v1".into();
            entry.spec.members[0] = baseline_member.clone();
            entry.spec.template_baseline = Some(crate::team_spec::TemplateBaseline {
                version: "1".into(),
                members: vec![baseline_member],
            });
        }

        let (status, json) = json_post(
            &router,
            "/api/team/upgrade-apply",
            serde_json::json!({ "project": "p1" }),
        )
        .await;
        assert_eq!(status, 200, "{json}");
        assert_eq!(json["upgraded"], serde_json::json!(["planner"]));
        assert_eq!(json["overridden"], serde_json::json!([]));
        assert_eq!(json["template"]["version"], "2");

        let entry = pm.read().await;
        let entry = entry.get("p1").unwrap();
        assert_eq!(entry.spec.template.as_ref().unwrap().version, "2");
        assert_eq!(entry.spec.members[0].persona, "template persona v2");
        let harness = entry
            .team
            .harness(&llm_harness_agent_team::AgentId::new("planner"))
            .unwrap();
        assert_eq!(
            harness.state().system_prompt,
            Some("template persona v2".into())
        );
        let persisted =
            std::fs::read_to_string(pm.read().await.data_root().join("teams/p1/team.json"))
                .unwrap();
        assert!(persisted.contains("template persona v2"));
        assert!(persisted.contains("\"version\": \"2\""));
    }

    #[tokio::test]
    async fn global_base_url_survives_registration() {
        // I1 regression: base_url saved in settings.json must reach the
        // team assembly path (previously hardcoded to empty).
        let (router, _pm, repo, _factory) = setup().await;

        // Save settings WITH base_url + key.
        let (status, json) = json_post(
            &router,
            "/api/team/settings",
            serde_json::json!({
                "strong": "m", "main": "m", "cheap": "m",
                "base_url": "https://custom.example.com/v4",
                "scout_interval_secs": 0,
                "api_key": "sk-global",
            }),
        )
        .await;
        assert_eq!(status, 200, "{json}");

        // Register from template — must succeed with the global key.
        let (status, json) = json_post(
            &router,
            "/api/team/projects",
            serde_json::json!({
                "id": "p1",
                "repo_path": repo.to_string_lossy(),
                "template_id": "coding-team",
            }),
        )
        .await;
        assert_eq!(status, 201, "{json}");
    }

    /// Regression: after a workflow reaches its terminal path (FINISH here)
    /// the spawned task handle must finish, so the next chat ingress starts a
    /// NEW workflow instead of returning 409 "a workflow is already running".
    #[tokio::test]
    async fn workflow_ownership_released_after_finish() {
        let (router, pm, _orchestrator, _coder, _idle, _factory) = controlled_fixture(
            &[
                "ACTION: FINISH\nFirst response.",
                "ACTION: FINISH\nSecond response.",
            ],
            false,
        )
        .await;

        let (status, body) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "controlled", "target": "team", "text": "first objective",
            }),
        )
        .await;
        assert_eq!(status, 202, "{body}");
        let workflow_id = body["workflow_id"].as_str().expect("controlled ingress");
        wait_workflow_phase(&pm, workflow_id, "done").await;

        // The spawned task finishes asynchronously after run() returns; poll
        // until the entry reports no live workflow task.
        tokio::time::timeout(Duration::from_secs(2), async {
            while pm
                .read()
                .await
                .get("controlled")
                .unwrap()
                .workflow_running()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("workflow ownership must release after FINISH");

        let (status, body) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "controlled", "target": "team", "text": "second objective",
            }),
        )
        .await;
        assert_eq!(status, 202, "second start must not 409: {body}");
        let workflow_id2 = body["workflow_id"].as_str().expect("controlled ingress");
        wait_workflow_phase(&pm, workflow_id2, "done").await;
    }

    /// Regression: an error terminal path (malformed orchestrator action →
    /// ESCALATE) must release workflow ownership exactly like FINISH.
    #[tokio::test]
    async fn workflow_ownership_released_after_error_path() {
        // A valid ACTION naming an unknown worker fails the controller fast
        // ("invalid worker target") — the deterministic error path that
        // releases workflow ownership. Malformed replies are progress now and
        // can no longer drive this path.
        let (router, pm, _orchestrator, _coder, _idle, _factory) = controlled_fixture(
            &["ACTION: DISPATCH ghost\nimplement", "ACTION: FINISH\nretry done."],
            false,
        )
        .await;
        let (status, body) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "controlled", "target": "team", "text": "will error",
            }),
        )
        .await;
        assert_eq!(status, 202, "{body}");
        let workflow_id = body["workflow_id"].as_str().expect("controlled ingress");
        wait_workflow_phase(&pm, workflow_id, "escalated").await;

        tokio::time::timeout(Duration::from_secs(2), async {
            while pm
                .read()
                .await
                .get("controlled")
                .unwrap()
                .workflow_running()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("error path must release workflow ownership");

        // Next ingress must start a fresh workflow, not 409.
        let (status, body) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "controlled", "target": "team", "text": "retry objective",
            }),
        )
        .await;
        assert_eq!(status, 202, "retry must not 409: {body}");
        let workflow_id2 = body["workflow_id"].as_str().expect("controlled ingress");
        wait_workflow_phase(&pm, workflow_id2, "done").await;
    }

    /// R3: member-level `agent/abort` must not bypass controller-owned
    /// cancellation for controlled teams. A rejected request must be a 409
    /// BEFORE any harness.abort(), leaving the active task and the
    /// controller waiter intact; the workflow then advances on a valid
    /// correlated reply.
    #[tokio::test]
    async fn controlled_team_rejects_agent_abort_for_active_member() {
        let (router, pm, orchestrator, coder, _idle, _factory) = controlled_fixture(
            &[
                "ACTION: DISPATCH coder\nimplement",
                "ACTION: FINISH\nFinal response.",
            ],
            false,
        )
        .await;
        // Park the orchestrator between its first (DISPATCH) and second
        // (FINISH) decision so the coder is actively working.
        orchestrator
            .hold_second
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (status, body) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({
                "project": "controlled", "target": "team", "text": "implement",
            }),
        )
        .await;
        assert_eq!(status, 202, "{body}");
        let workflow_id = body["workflow_id"].as_str().unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while coder.tasks.lock().len() < 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("coder dispatch never arrived");
        // Rejected for the active worker, the deciding orchestrator, the
        // team target, and an unknown member — all before any harness
        // abort runs.
        for agent in ["coder", "orchestrator", "reviewer-performance", TEAM_TARGET] {
            let (status, body) = json_post(
                &router,
                &format!("/api/team/agent/abort?project=controlled&agent={agent}"),
                serde_json::json!({}),
            )
            .await;
            assert_eq!(status, 409, "{agent}: {body}");
        }

        // The workflow is still live and controller-owned after every
        // rejection: no fabricated terminal, no cancelled waiter. (The
        // coder's single-assignment run may already have completed
        // naturally; its reply's acceptance is proven below by the
        // decision still being able to advance.)
        {
            let guard = pm.read().await;
            let project = guard.get("controlled").unwrap();
            let state = project
                .workflow
                .as_ref()
                .unwrap()
                .state(workflow_id)
                .expect("workflow state survives rejected aborts");
            assert!(
                state.phase == "executing" || state.phase == "deciding",
                "workflow must stay live, got {:?}",
                state.phase
            );
        }

        // The still-parked orchestrator decision (held across every
        // rejected abort) completes the workflow: rejected aborts never
        // sealed, cancelled, or terminalized the controller-owned run.
        orchestrator.release.notify_one();
        wait_workflow_phase(&pm, workflow_id, "done").await;
    }

    /// R3: `agent/abort` keeps working for ordinary (non-controlled) teams:
    /// member-level abort is accepted, and the runner re-queues in-flight
    /// work instead of losing it.
    #[tokio::test]
    async fn ordinary_team_agent_abort_still_works() {
        let (router, pm, repo, _factory) = setup().await;
        create_generic_chat_team(&pm, &repo).await;
        // The generic team has no workflow; abort must be accepted (200).
        let (status, body) = json_post(
            &router,
            "/api/team/agent/abort?project=p1&agent=coder",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["ok"], true);
    }
}
