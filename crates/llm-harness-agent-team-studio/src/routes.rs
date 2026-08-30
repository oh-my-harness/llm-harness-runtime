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
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::issue::IssueStatus;
use crate::team_manager::{TeamManager, TeamManagerError, unpersist_spec};

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
}

/// Shared handle to the manager (kept for direct consumers/tests).
pub type SharedPm = Arc<RwLock<TeamManager>>;

/// Build the domain router. Mount with `InspectorBuilder::extra_routes`.
pub fn team_router(state: AppState) -> Router {
    Router::new()
        .route(
            "/api/team/projects",
            get(list_projects)
                .post(register_project)
                .delete(delete_project),
        )
        .route("/api/team/chat", post(chat))
        .route("/api/team/issues", get(list_issues))
        .route("/api/team/issue/{id}/confirm", post(confirm_issue))
        .route("/api/team/issue/{id}/reject", post(reject_issue))
        .route("/api/team/events", get(events_ws))
        .route("/api/team/settings", get(get_settings).post(put_settings))
        .route("/api/team/apikey", post(post_apikey))
        .route("/api/team/fs", get(list_dir))
        .route("/api/team/pulse", get(pulse))
        .route("/api/team/agent/inject", post(agent_inject))
        .route("/api/team/agent/abort", post(agent_abort))
        .route("/api/team/agent/session", get(agent_session))
        .route(
            "/api/team/agent/config",
            get(agent_config).put(put_agent_config),
        )
        .route("/api/team/templates", get(list_templates))
        .route("/api/team/upgrade-check", post(upgrade_check))
        .with_state(state)
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
            name: body.name.clone().unwrap_or_else(|| body.id.clone()),
            repo_path: body.repo_path.clone(),
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
    let mut pm = state.pm.write().await;
    match pm.create(spec, repo, &settings, &api_key).await {
        Ok(_) => {
            // I2: sync inspector registry for the new team's members.
            {
                let entry = pm.get(&body.id).expect("just inserted");
                for m in &entry.spec.members {
                    if let Some(h) = entry.team.harness(&llm_harness_agent_team::AgentId::new(&m.id)) {
                        state.registry.register(format!("{}/{}", body.id, m.id), h);
                    }
                }
            }
            // I2: operator pump for the new team (WS live feed).
            if let Some(ctx) = pm
                .get(&body.id)
                .and_then(|e| e.team.social_context().cloned())
            {
                spawn_operator_pump(state.events.clone(), body.id.clone(), ctx);
            }
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

/// DELETE /api/team/projects/:id — shut down a project's team. Issues and
/// memory persist on disk; the project is removed from the registry.
#[derive(Deserialize)]
struct DeleteQuery {
    id: String,
}

async fn delete_project(State(state): State<AppState>, Query(q): Query<DeleteQuery>) -> Response {
    let mut pm = state.pm.write().await;
    match pm.shutdown(&q.id).await {
        Ok(()) => {
            // I2: drop the team's harness entries from the observation UI.
            let keys: Vec<String> = state
                .registry
                .keys()
                .into_iter()
                .filter(|k| k.starts_with(&format!("{}/", q.id)))
                .collect();
            for k in keys {
                state.registry.remove(&k);
            }
            unpersist_spec(&state.data_root, &q.id);
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
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
    /// Directory to list. Empty = the caller's home directory.
    path: Option<String>,
}

/// List subdirectories of `path` for the panel's folder picker.
///
/// Read-only, directories only, hidden entries skipped. This endpoint
/// exists so the browser can fill in an absolute repo path (the File
/// System Access API cannot expose absolute paths); it rides the same
/// loopback-bind security model as every other route.
async fn list_dir(Query(q): Query<FsQuery>) -> Response {
    let dir: PathBuf = match q.path.as_deref() {
        None | Some("") => std::env::var("HOME").unwrap_or_default().into(),
        Some(p) => p.into(),
    };
    if dir.as_os_str().is_empty() {
        return err_resp(
            "cannot determine home directory".into(),
            StatusCode::BAD_REQUEST,
        );
    }
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
    }))
    .into_response()
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
    let mut snap = crate::pulse::snapshot(&project.team).await;
    snap["pending"] = crate::pulse::pending_traffic(&project.team)["pending"].clone();
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

    // 逐字段相等判定（序列化投影，跳过 id/repo_path 等实例字段）。
    let same_as_template = |m: &crate::team_spec::MemberSpec| -> Option<bool> {
        let original = template.spec.members.iter().find(|t| t.id == m.id)?;
        Some(serde_json::to_value(m).ok()? == serde_json::to_value(original).ok()?)
    };
    let overridden: Vec<String> = entry
        .spec
        .members
        .iter()
        .filter(|m| !same_as_template(m).unwrap_or(false))
        .map(|m| m.id.clone())
        .collect();
    let follow: Vec<String> = entry
        .spec
        .members
        .iter()
        .filter(|m| same_as_template(m).unwrap_or(false))
        .map(|m| m.id.clone())
        .collect();
    Json(serde_json::json!({
        "template": { "id": tref.template_id, "version": tref.version },
        "template_current_version": template.version,
        "would_follow": follow,
        "overridden": overridden,
    }))
    .into_response()
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

    // Member-scoped api key → 0600 file. This happens only after both IDs
    // are known-good, so they can never construct a secrets path.
    if let Some(key) = &body.api_key {
        let secrets = state
            .data_root
            .join("teams")
            .join(&body.project)
            .join("secrets");
        if std::fs::create_dir_all(&secrets).is_ok() {
            let path = secrets.join(&body.agent);
            #[cfg(unix)]
            {
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                let _ = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&path)
                    .and_then(|mut f| f.write_all(key.as_bytes()));
            }
            #[cfg(not(unix))]
            let _ = std::fs::write(&path, key);
        }
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
        .update_member(&body.project, member, &settings, &api_key)
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
                    .register(format!("{}/{}", body.project, body.agent), h);
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
    team_id: String,
    ctx: llm_harness_agent_team::SocialContext,
) {
    tokio::spawn(async move {
        let operator = llm_harness_agent_team::AgentId::new("operator");
        loop {
            ctx.wait_for_message(&operator).await;
            for msg in ctx.drain_inbox(&operator) {
                events.publish(crate::events::operator_event(
                    &team_id,
                    msg.from.as_str(),
                    &msg.body,
                ));
            }
        }
    });
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
    if !project.spec.members.iter().any(|m| m.id == body.target) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": format!("unknown member: {}", body.target) })),
        )
            .into_response();
    }
    match project.team.submit(&body.target, &body.text) {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "ok": true })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
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
        && let Err(e) = pm.apply_issue_handoff(&project_id, &issue)
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
) -> Response {
    if !is_localhost_origin(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |socket| events_ws_loop(socket, state))
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

async fn events_ws_loop(socket: axum::extract::ws::WebSocket, state: AppState) {
    use axum::extract::ws::Message;
    use futures::{SinkExt, StreamExt};
    let (mut sender, mut receiver) = socket.split();
    // Replay recent events first, then forward live ones.
    for line in state.events.replay() {
        if sender.send(Message::Text(line.into())).await.is_err() {
            return;
        }
    }
    let mut rx = state.events.subscribe();
    loop {
        tokio::select! {
            // Client pings/pongs keep the connection healthy; ignore input.
            Some(Ok(_)) = receiver.next() => {}
            Ok(line) = rx.recv() => {
                if sender.send(Message::Text(line.into())).await.is_err() {
                    break;
                }
            }
            else => break,
        }
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
    use llm_harness_loop::test_utils::MockLlmClient;
    use tower::ServiceExt;

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

    /// Tests install a mock factory returning one "m" client and build the
    /// state directly (no env vars — the AppState carries tier/interval).
    fn install_mock_factory() {
        crate::env_cfg::install_provider_factory(Arc::new(|| {
            vec![(
                "m".into(),
                Arc::new(MockLlmClient::new(vec![])) as Arc<dyn llm_harness_loop::LlmClient>,
            )]
        }));
    }

    async fn setup() -> (Router, SharedPm, std::path::PathBuf) {
        install_mock_factory();
        let data_root = unique_dir("ct-routes");
        let pm = Arc::new(RwLock::new(TeamManager::new(&data_root)));
        let state = AppState {
            pm: pm.clone(),
            data_root: data_root.clone(),
            tier: Arc::new(RwLock::new(tier())),
            scout_interval_secs: Arc::new(RwLock::new(0)),
            events: Arc::new(crate::events::EventBus::new()),
            registry: llm_harness_inspector::state::HarnessRegistry::new("default"),
        };
        let router = team_router(state);
        (router, pm, make_repo())
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
    async fn register_via_http_validates_repo() {
        let (router, _pm, _repo) = setup().await;
        let (status, json) = json_post(
            &router,
            "/api/team/projects",
            serde_json::json!({ "id": "bad", "repo_path": "/no/such/repo" }),
        )
        .await;
        assert_eq!(status, 400, "{json}");
        assert_eq!(json["ok"], false);
    }

    #[tokio::test]
    async fn register_and_list_projects() {
        let (router, _pm, repo) = setup().await;
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
    }

    #[tokio::test]
    async fn chat_routes_by_explicit_target() {
        let (router, pm, repo) = setup().await;
        // Register from the built-in template: members include planner and
        // brainstormer (dynamic target whitelist is exercised against them).
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
        eprintln!("DEBUG register: {status} {json}");
        assert_eq!(status, 201, "register: {json}");

        // planner route accepted.
        let (status, json) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({ "project": "p1", "target": "planner", "text": "fix the bug" }),
        )
        .await;
        assert_eq!(status, 202, "planner: {json}");

        // brainstormer route accepted.
        let (status, _) = json_post(
            &router,
            "/api/team/chat",
            serde_json::json!({ "project": "p1", "target": "brainstormer", "text": "I want a feature" }),
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
            serde_json::json!({ "project": "ghost", "target": "planner", "text": "x" }),
        )
        .await;
        assert_eq!(status, 404);

        // The planner actually received the chat text in its inbox.
        let project = pm
            .read()
            .await
            .get("p1")
            .unwrap()
            .team
            .social_context()
            .unwrap()
            .clone();
        let drained = project.drain_inbox(&llm_harness_agent_team::AgentId::new("planner"));
        assert!(
            drained.iter().any(|m| m.body.contains("fix the bug")),
            "planner inbox must carry the chat text"
        );
        // The brainstormer received its message too.
        let drained = project.drain_inbox(&llm_harness_agent_team::AgentId::new("brainstormer"));
        assert!(drained.iter().any(|m| m.body.contains("I want a feature")));
    }

    #[tokio::test]
    async fn confirm_issue_submits_to_planner() {
        let (router, pm, repo) = setup().await;
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

        // Planner received the handoff with the full issue body.
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
        assert!(
            drained
                .iter()
                .any(|m| m.body.contains("操作员确认") && m.body.contains("现象：错误被吞")),
            "planner must receive the confirmed issue body"
        );
    }

    #[tokio::test]
    async fn reject_issue_flips_status_without_planner_handoff() {
        let (router, pm, repo) = setup().await;
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
        let (router, pm, repo) = setup().await;
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
        let (router, _pm, _repo) = setup().await;
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
        let (_router, pm, repo) = setup().await;
        // Rebuild the router with a bus we can inspect directly.
        let bus = Arc::new(crate::events::EventBus::new());
        let mut rx = bus.subscribe();
        let state = AppState {
            pm: pm.clone(),
            data_root: unique_dir("ct-routes-bus"),
            tier: Arc::new(RwLock::new(tier())),
            scout_interval_secs: Arc::new(RwLock::new(0)),
            events: bus,
            registry: llm_harness_inspector::state::HarnessRegistry::new("default"),
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
        let (router, _pm, _repo) = setup().await;
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
        let (router, _pm, _repo) = setup().await;
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
        let (router, _pm, repo) = setup().await;
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
        let (router, _pm, _repo) = setup().await;
        let (status, _) = get_json(&router, "/api/team/fs?path=/no/such/dir").await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn delete_project_removes_and_persists() {
        let (router, pm, repo) = setup().await;
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
        let (router, pm, repo) = setup().await;
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
        let (router, _pm, repo) = setup().await;
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
        let (router, _pm, repo) = setup().await;
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
        let (router, pm, repo) = setup().await;
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

        // secrets 文件 0600
        let secrets = pm.read().await.data_root().join("teams/p1/secrets/coder");
        assert!(secrets.exists());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&secrets).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "secrets file must be 0600");

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
    async fn agent_config_unknown_member_404() {
        let (router, _pm, repo) = setup().await;
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
        let (router, pm, repo) = setup().await;
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
        let (router, _pm, _repo) = setup().await;
        let (status, json) = get_json(&router, "/api/team/templates").await;
        assert_eq!(status, 200);
        let templates = json["templates"].as_array().unwrap();
        assert!(
            templates.iter().any(
                |t| t["id"] == "coding-team" && t["member_ids"].as_array().unwrap().len() == 11
            ),
            "{templates:?}"
        );
    }

    #[tokio::test]
    async fn upgrade_check_classifies_overridden_members() {
        let (router, _pm, repo) = setup().await;
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
        assert_eq!(json["would_follow"].as_array().unwrap().len(), 11);
        assert_eq!(json["template_current_version"], "1");

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
        assert_eq!(json["would_follow"].as_array().unwrap().len(), 10);
    }

    #[tokio::test]
    async fn upgrade_check_no_template_is_error() {
        let (router, _pm, repo) = setup().await;
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
    async fn global_base_url_survives_registration() {
        // I1 regression: base_url saved in settings.json must reach the
        // team assembly path (previously hardcoded to empty).
        let (router, _pm, repo) = setup().await;
        install_mock_factory();

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
}
