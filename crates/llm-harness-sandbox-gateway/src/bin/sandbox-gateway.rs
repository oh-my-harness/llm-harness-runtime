use llm_harness_sandbox_gateway::{GatewayServiceConfig, SandboxGateway};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        anyhow::bail!("usage: sandbox-gateway <config.json>");
    }

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config_bytes = std::fs::read(&args[1])?;
    let service_config = GatewayServiceConfig::from_slice(&config_bytes)?;
    let bind_addr = service_config.bind_addr();
    let port = service_config.port();
    let gateway_config = service_config.into_gateway_config()?;

    let gateway = SandboxGateway::new(gateway_config)?;
    let listener = tokio::net::TcpListener::bind((bind_addr.as_str(), port)).await?;
    tracing::info!(bind_addr = %bind_addr, port = %port, "sandbox gateway listening");

    let server = axum::serve(listener, gateway.router()).with_graceful_shutdown(shutdown_signal());
    server.await?;
    gateway.shutdown_all().await?;
    tracing::info!("sandbox gateway stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    let sigterm = {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        async move {
            sigterm.recv().await;
        }
    };

    #[cfg(unix)]
    tokio::select! {
        _ = ctrl_c => {},
        _ = sigterm => {},
    }

    #[cfg(not(unix))]
    ctrl_c.await.expect("wait for ctrl-c");
}
