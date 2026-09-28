use llm_harness_agent_control::reconcile_orphaned_sandboxes;
use llm_harness_agent_control::{ControlServiceConfig, build_router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "agent-control.json".to_owned());
    let config_bytes = std::fs::read(&config_path)
        .map_err(|error| anyhow::anyhow!("failed to read config {config_path}: {error}"))?;
    let config = ControlServiceConfig::from_slice(&config_bytes)?;
    if let Some(persistence_path) = &config.persistence_path {
        let persistence = llm_harness_agent_control::SessionPersistence::new(persistence_path);
        let gateway_token = config.gateway_token()?;
        let cleaned = reconcile_orphaned_sandboxes(
            &persistence,
            &config.gateway.base_url,
            &gateway_token,
            config.gateway.allow_insecure_http,
        )
        .await?;
        if !cleaned.is_empty() {
            tracing::info!("reconciled {} orphaned sandboxes on startup", cleaned.len());
        }
    }
    let bind_addr = format!("{}:{}", config.bind_addr(), config.port());
    let factory = std::sync::Arc::new(llm_harness_agent_control::RemoteSandboxSessionFactory::new(
        config.clone(),
    ));
    let state = std::sync::Arc::new(llm_harness_agent_control::AgentControlManager::new(
        config, factory,
    )?);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!(%bind_addr, "agent control service listening");

    axum::serve(listener, build_router(std::sync::Arc::clone(&state)))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("agent control service shutting down");
        })
        .await?;

    state.shutdown_all().await
}
