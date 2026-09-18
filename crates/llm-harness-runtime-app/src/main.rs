use llm_harness_agent_team_studio::diagnostics::install_panic_reporter;
use llm_harness_runtime_app::app::{RuntimeApp, RuntimeAppConfig};

async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[tokio::main]
async fn main() {
    let config = RuntimeAppConfig::from_env();
    install_panic_reporter(&config.data_root);

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let app = match RuntimeApp::start(config).await {
        Ok(app) => app,
        Err(error) => {
            eprintln!("llm-harness-app: startup failed: {error}");
            std::process::exit(1);
        }
    };

    println!(
        "RUNTIME_APP_PANEL_DESCRIPTOR={}",
        app.panel_descriptor_path().display()
    );

    wait_for_shutdown().await;
    tracing::info!("shutting down");
    app.shutdown().await;
}
