#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use llm_harness_platform::trust::TrustProfile;
use llm_harness_sandbox_gateway::{GatewayConfig, GatewayPrincipalConfig, SandboxGateway};

#[tokio::test]
async fn metrics_are_protected_and_rate_limited_per_principal() {
    let work_root = tempfile::tempdir().expect("create work root");
    std::fs::set_permissions(work_root.path(), std::fs::Permissions::from_mode(0o700))
        .expect("set work root permissions");
    let config = GatewayConfig::new(vec![GatewayPrincipalConfig {
        principal: "phone".to_owned(),
        bearer_token: "gateway-e2e-test-token-0123456789abcdef".to_owned(),
        fs_allowlist: vec![PathBuf::from("/workspace")],
        host_project_dirs: Vec::new(),
        max_trust_profile: TrustProfile::Restricted,
        requests_per_minute: Some(1),
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
    let unauthorized = client
        .post(format!("http://{address}/v1/sandboxes"))
        .json(&serde_json::json!({
            "workspace": "/workspace",
            "trust_profile": "restricted"
        }))
        .send()
        .await
        .expect("unauthenticated create request");
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

    let metrics = client
        .get(format!("http://{address}/metrics"))
        .bearer_auth("gateway-e2e-test-token-0123456789abcdef")
        .send()
        .await
        .expect("authorized metrics request");
    assert_eq!(metrics.status(), reqwest::StatusCode::OK);
    let body = metrics.text().await.expect("metrics body");
    assert!(
        body.contains(r#"method="POST",route="/v1/sandboxes",principal="anonymous",status="401""#)
    );
    assert!(body.contains(r#"sandbox_gateway_active_sandboxes{backend="linux-bwrap"} 0"#));

    let limited = client
        .get(format!("http://{address}/metrics"))
        .bearer_auth("gateway-e2e-test-token-0123456789abcdef")
        .send()
        .await
        .expect("rate limited metrics request");
    assert_eq!(limited.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    assert!(limited.headers().get("retry-after").is_some());

    let health = client
        .get(format!("http://{address}/healthz"))
        .send()
        .await
        .expect("public health request");
    assert_eq!(health.status(), reqwest::StatusCode::OK);

    server.abort();
    let _ = server.await;
}
