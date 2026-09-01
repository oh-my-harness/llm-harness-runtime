//! The agent-team-studio daemon: zero-config cold start.
//!
//! All configuration lives in the web panel (model tiers, API key,
//! team creation from templates) and persists under the data root
//! (`~/.harness/agent-team-studio` by default). Startup restores
//! persisted team specs and rebuilds every team; a fresh install just shows the setup page.

use llm_harness_agent_team::AgentId;
use llm_harness_agent_team_studio::routes::{AppState, team_router};
use llm_harness_agent_team_studio::settings::{Settings, load_api_key};
use llm_harness_agent_team_studio::team_manager::{TeamManager, list_persisted};
use llm_harness_inspector::Inspector;
use llm_harness_inspector::state::HarnessRegistry;
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let data_root = std::env::var("STUDIO_DATA_ROOT")
        .or_else(|_| std::env::var("CT_DATA_ROOT"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_data_root());
    let port: u16 = std::env::var("STUDIO_PORT")
        .or_else(|_| std::env::var("CT_PORT"))
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    std::fs::create_dir_all(&data_root).expect("data root must be creatable");

    // Restore saved settings (or defaults on a fresh install — the panel
    // walks the user through configuration).
    let saved = Settings::load(&data_root).expect("settings must be readable");
    let settings = saved.unwrap_or_default();

    // Provider factory from the stored API key + model ids. Empty key →
    // empty providers; the panel's setup page collects both.
    let key = load_api_key(&data_root).unwrap_or_default();
    let base_url = std::env::var("OPENAI_API_BASE").unwrap_or_default();
    let models = settings.models.clone();
    let key_for_factory = key.clone();
    let base_for_factory = base_url.clone();
    llm_harness_agent_team_studio::env_cfg::install_provider_factory(Arc::new(move || {
        llm_harness_agent_team_studio::config::build_providers(
            &key_for_factory,
            &base_for_factory,
            &models,
        )
    }));

    let mut tm = TeamManager::new(&data_root);

    // Restore persisted team specs. Registration failures are logged,
    // not fatal (e.g. an API key was cleared since last run).
    for spec in list_persisted(&data_root) {
        let repo_path = spec.repo_hint();
        match tm.create(spec.clone(), repo_path, &settings, &key).await {
            Ok(_) => tracing::info!("team `{}` restored", spec.id),
            Err(e) => tracing::error!("team `{}` restore failed: {e}", spec.id),
        }
    }

    // Register every member harness into the inspector registry, keyed
    // "<team>/<agent>" so cross-team names never collide. Members come
    // from each team's spec.
    let registry: std::sync::Arc<HarnessRegistry> = HarnessRegistry::new("default");
    for id in tm.ids() {
        if let Some(entry) = tm.get(&id) {
            for member in &entry.spec.members {
                if let Some(h) = entry.team.harness(&AgentId::new(&member.id)) {
                    registry.register(format!("{id}/{}", member.id), h);
                }
            }
        }
    }

    let tm_shared = Arc::new(tokio::sync::RwLock::new(tm));
    let events = Arc::new(llm_harness_agent_team_studio::events::EventBus::new());
    let operator_pumps = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));

    // Pump every team's operator stream into the event bus so the
    // panel's WS clients see agent→operator traffic live.
    {
        let tm = tm_shared.read().await;
        for id in tm.ids() {
            let Some(entry) = tm.get(&id) else {
                continue;
            };
            let Some(ctx) = entry.team.social_context().cloned() else {
                continue;
            };
            llm_harness_agent_team_studio::routes::spawn_operator_pump(
                events.clone(),
                operator_pumps.clone(),
                id.clone(),
                ctx,
            );
        }
    }

    let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static");
    let state = AppState {
        pm: tm_shared.clone(),
        data_root: data_root.clone(),
        tier: Arc::new(tokio::sync::RwLock::new(settings.models.clone())),
        scout_interval_secs: Arc::new(tokio::sync::RwLock::new(settings.scout_interval_secs)),
        events: events.clone(),
        registry: registry.clone(),
        operator_pumps,
    };
    let inspector = match Inspector::registry(registry)
        .extra_routes(team_router(state))
        .extra_static("app", static_dir)
        .port(port)
        .mount()
        .await
    {
        Ok(i) => i,
        Err(e) => {
            eprintln!("agent-team-studio: inspector mount failed: {e}");
            std::process::exit(1);
        }
    };

    if let Some(addr) = inspector.bound_addr() {
        tracing::info!("agent-team-studio panel: http://{addr}/app/team.html");
    }

    // Hold until Ctrl+C, then shut everything down.
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
    inspector.shutdown().await;
}

fn default_data_root() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".harness/agent-team-studio"))
        .unwrap_or_else(|| PathBuf::from(".agent-team-studio"))
}
