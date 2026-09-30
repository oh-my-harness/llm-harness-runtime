#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use llm_harness_platform::trust::TrustProfile;
use llm_harness_sandbox_gateway::{GatewayConfig, GatewayPrincipalConfig, SandboxGateway};

#[tokio::test]
async fn health_endpoints_are_public_and_protected_routes_require_auth() {
    let work_root = tempfile::tempdir().expect("create work root");
    std::fs::set_permissions(work_root.path(), std::fs::Permissions::from_mode(0o700))
        .expect("set work root permissions");
    let config = GatewayConfig::new(vec![GatewayPrincipalConfig {
        principal: "test".to_owned(),
        bearer_token: "test-bearer-token-at-least-32-bytes".to_owned(),
        fs_allowlist: vec![PathBuf::from("/workspace")],
        host_project_dirs: Vec::new(),
        max_trust_profile: TrustProfile::Restricted,
        requests_per_minute: None,
        max_active_sandboxes: None,
    }])
    .expect("valid config")
    .with_work_root(work_root.path().to_path_buf());

    let gateway = SandboxGateway::new(config).expect("build gateway");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gateway");
    let address = listener.local_addr().expect("gateway address");
    let server = tokio::spawn(async move {
        axum::serve(listener, gateway.router())
            .await
            .expect("serve gateway");
    });

    let client = reqwest::Client::new();
    let health = client
        .get(format!("http://{address}/healthz"))
        .send()
        .await
        .expect("health request");
    assert_eq!(health.status(), reqwest::StatusCode::OK);
    assert_eq!(
        health.text().await.expect("health body"),
        r#"{"status":"ok","backend":"linux-bwrap"}"#
    );

    let readiness = client
        .get(format!("http://{address}/readyz"))
        .send()
        .await
        .expect("readiness request");
    assert_eq!(readiness.status(), reqwest::StatusCode::OK);

    let unauthorized = client
        .post(format!("http://{address}/v1/sandboxes"))
        .json(&serde_json::json!({"workspace": "/workspace", "trust_profile": "restricted"}))
        .send()
        .await
        .expect("unauthenticated create request");
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

    server.abort();
}
