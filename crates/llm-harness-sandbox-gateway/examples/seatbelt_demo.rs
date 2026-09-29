//! Run a Seatbelt-backed sandbox gateway on macOS loopback for MVP smoke
//! testing. Prints the endpoint and bearer token; Ctrl-C to stop.
//!
//! ```sh
//! cargo run -p llm-harness-sandbox-gateway --example seatbelt_demo
//! ```

#[cfg(target_os = "macos")]
use std::time::Duration;

#[cfg(target_os = "macos")]
use llm_harness_sandbox_gateway::{
    GatewayBackend, GatewayConfig, GatewayPrincipalConfig, SandboxGateway,
};

#[cfg(target_os = "macos")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let token = std::env::var("GATEWAY_TOKEN")
        .ok()
        .filter(|t| t.len() >= 32)
        .unwrap_or_else(|| "mvp-smoke-token-0123456789abcdef".to_owned());
    let port: u16 = std::env::var("GATEWAY_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7080);

    let work_root = std::env::var("GATEWAY_WORK_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("llm-harness-mobile-mvp-gateway"));
    std::fs::create_dir_all(&work_root)?;
    let work_root = std::fs::canonicalize(&work_root)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&work_root, std::fs::Permissions::from_mode(0o700))?;
    }

    let config = GatewayConfig::new(vec![
        GatewayPrincipalConfig {
            principal: "phone".to_owned(),
            bearer_token: token.clone(),
            fs_allowlist: Vec::new(),
            host_project_dirs: Vec::new(),
            max_trust_profile: llm_harness_platform::trust::TrustProfile::TrustedFull,
            requests_per_minute: None,
        },
        GatewayPrincipalConfig {
            principal: "kiosk".to_owned(),
            bearer_token: std::env::var("GATEWAY_KIOSK_TOKEN")
                .unwrap_or_else(|_| "kiosk-readonly-token-0123456789abcdef".to_owned()),
            fs_allowlist: Vec::new(),
            host_project_dirs: Vec::new(),
            max_trust_profile: llm_harness_platform::trust::TrustProfile::Restricted,
            requests_per_minute: None,
        },
    ])?
    .with_work_root(work_root)
    .with_backend(GatewayBackend::Seatbelt);

    let gateway = SandboxGateway::new(config)?;
    let bind_addr = std::env::var("GATEWAY_BIND").unwrap_or_else(|_| "127.0.0.1".to_owned());
    let listener = tokio::net::TcpListener::bind((bind_addr.as_str(), port)).await?;
    println!("gateway listening on http://{bind_addr}:{port}");
    println!("LAN devices connect via http://<this-mac-LAN-ip>:{port}");
    println!("bearer token: {token}");
    println!("(emulator: 10.0.2.2:{port} + adb reverse; phone: this Mac's LAN IP)");

    let server = tokio::spawn({
        let router = gateway.router();
        async move { axum::serve(listener, router).await.expect("gateway server") }
    });

    tokio::signal::ctrl_c().await?;
    println!("shutting down; deleting all sandboxes…");
    gateway.shutdown_all().await?;
    server.abort();
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {}
