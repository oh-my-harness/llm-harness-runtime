//! The agent-team-studio daemon: zero-config cold start.
//!
//! All configuration lives in the web panel (model tiers, API key,
//! team creation from templates) and persists under the data root
//! (`~/.harness/agent-team-studio` by default). Startup restores
//! persisted team specs and rebuilds every team; a fresh install just shows the setup page.

use llm_harness_agent_team_studio::diagnostics::install_panic_reporter;
use llm_harness_agent_team_studio::runtime::{StudioConfig, StudioRuntime, default_data_root};

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
    install_panic_reporter(default_data_root());

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let runtime = match StudioRuntime::start(StudioConfig::from_env()).await {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("agent-team-studio: startup failed: {e}");
            std::process::exit(1);
        }
    };

    println!(
        "STUDIO_PANEL_DESCRIPTOR={}",
        runtime.panel_descriptor_path().display()
    );

    wait_for_shutdown().await;
    tracing::info!("shutting down");
    runtime.shutdown().await;
}
