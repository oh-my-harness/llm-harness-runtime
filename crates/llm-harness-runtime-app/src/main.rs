use llm_harness_agent_team_studio::diagnostics::append_lifecycle_event;
use llm_harness_agent_team_studio::diagnostics::install_panic_reporter;
use llm_harness_runtime_app::app::{RuntimeApp, RuntimeAppConfig};
use llm_harness_runtime_app::lifecycle::{
    LifecycleConfig, ShutdownOutcome, ShutdownSignalHandler, shutdown_with_timeout,
};
use llm_harness_runtime_app::migration::{parse_data_command, run_data_command};
use std::path::Path;

fn record_lifecycle_event(data_root: &Path, data: serde_json::Value) {
    let event = data
        .get("event")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    if let Err(error) = append_lifecycle_event(data_root, data) {
        tracing::error!(
            event = %event,
            %error,
            "failed to persist lifecycle event"
        );
    }
}

#[tokio::main]
async fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if let Some(command) = parse_data_command(&arguments).unwrap_or_else(|error| {
        eprintln!("llm-harness-app: {error}");
        std::process::exit(2);
    }) {
        match run_data_command(command) {
            Ok(result) => {
                println!("{}", serde_json::to_string_pretty(&result).unwrap());
            }
            Err(error) => {
                eprintln!("llm-harness-app: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    let lifecycle_config = match LifecycleConfig::from_env() {
        Ok(lifecycle_config) => lifecycle_config,
        Err(error) => {
            eprintln!("llm-harness-app: invalid lifecycle configuration: {error}");
            std::process::exit(2);
        }
    };
    let config = RuntimeAppConfig::from_env();
    let shutdown_data_root = config.data_root.clone();
    install_panic_reporter(&config.data_root);

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut shutdown_signal_handler = match ShutdownSignalHandler::install() {
        Ok(shutdown_signal_handler) => shutdown_signal_handler,
        Err(error) => {
            eprintln!("llm-harness-app: shutdown signal handler failed: {error}");
            record_lifecycle_event(
                &shutdown_data_root,
                serde_json::json!({
                    "event": "shutdown_signal_error",
                    "error": error.to_string(),
                }),
            );
            std::process::exit(1);
        }
    };

    let started_app = tokio::select! {
        app = RuntimeApp::start(config) => match app {
            Ok(app) => Some(app),
            Err(error) => {
                eprintln!("llm-harness-app: startup failed: {error}");
                std::process::exit(1);
            }
        },
        shutdown_source = shutdown_signal_handler.wait() => {
            tracing::info!(
                source = shutdown_source.as_str(),
                "shutting down during startup"
            );
            record_lifecycle_event(
                &shutdown_data_root,
                serde_json::json!({
                    "event": "shutdown_signal",
                    "source": shutdown_source.as_str(),
                    "startup_aborted": true,
                }),
            );
            None
        }
    };
    let Some(app) = started_app else {
        std::process::exit(130);
    };

    println!(
        "RUNTIME_APP_PANEL_DESCRIPTOR={}",
        app.panel_descriptor_path().display()
    );

    let shutdown_source = shutdown_signal_handler.wait().await;

    tracing::info!(source = shutdown_source.as_str(), "shutting down");
    record_lifecycle_event(
        &shutdown_data_root,
        serde_json::json!({
            "event": "shutdown_signal",
            "source": shutdown_source.as_str(),
        }),
    );
    let shutdown_timeout =
        shutdown_source.effective_shutdown_timeout(lifecycle_config.shutdown_timeout());
    let outcome = shutdown_with_timeout(app.shutdown(), shutdown_timeout).await;
    if outcome == ShutdownOutcome::TimedOut {
        record_lifecycle_event(
            &shutdown_data_root,
            serde_json::json!({
                "event": "shutdown_timeout",
                "source": shutdown_source.as_str(),
                "configured_timeout_secs": lifecycle_config.shutdown_timeout().as_secs(),
                "effective_timeout_secs": shutdown_timeout.as_secs(),
            }),
        );
        eprintln!("llm-harness-app: graceful shutdown timed out");
        std::process::exit(124);
    }
}
