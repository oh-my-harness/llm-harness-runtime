#![cfg(target_os = "linux")]

use llm_harness_platform::trust::TrustProfile;
use llm_harness_sandbox_gateway::{
    AuditFailureMode, GatewayConfig, GatewayPrincipalConfig, SandboxGateway, validate_audit,
};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const TOKEN: &str = "gateway-audit-test-token-0123456789abcdef";

fn tempdir_0700() -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("audit tempdir");
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700))
        .expect("set tempdir permissions");
    temp
}

fn config(audit_path: &Path, failure_mode: AuditFailureMode) -> GatewayConfig {
    GatewayConfig::new(vec![GatewayPrincipalConfig {
        principal: "control-service".to_owned(),
        bearer_token: TOKEN.to_owned(),
        fs_allowlist: vec![],
        host_project_dirs: vec![],
        max_trust_profile: TrustProfile::Restricted,
        requests_per_minute: None,
        max_active_sandboxes: None,
    }])
    .expect("gateway config")
    .with_audit_failure_mode(failure_mode)
    .with_work_root(audit_path.parent().expect("audit parent").to_path_buf())
    .with_audit_path(audit_path.to_path_buf())
}

async fn start_gateway(audit_path: &Path, failure_mode: AuditFailureMode) -> String {
    let gateway = SandboxGateway::new(config(audit_path, failure_mode)).expect("gateway");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gateway");
    let address = format!("http://{}", listener.local_addr().expect("gateway address"));
    tokio::spawn(async move {
        axum::serve(listener, gateway.router())
            .await
            .expect("serve gateway");
    });
    address
}

#[tokio::test]
async fn gateway_audits_unauthenticated_and_authenticated_requests() {
    let temp = tempdir_0700();
    let audit_path = temp.path().join("audit.jsonl");
    let address = start_gateway(&audit_path, AuditFailureMode::default()).await;

    let client = reqwest::Client::new();
    let unauthenticated = client
        .get(format!("{address}/metrics"))
        .header("x-request-id", "0197e0f2-6a08-7000-0000-000000000001")
        .send()
        .await
        .expect("unauthenticated request");
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);

    let authenticated = client
        .get(format!("{address}/metrics"))
        .bearer_auth(TOKEN)
        .header("x-request-id", "0197e0f2-6a08-7000-0000-000000000002")
        .send()
        .await
        .expect("authenticated request");
    assert_eq!(authenticated.status(), reqwest::StatusCode::OK);

    let audit = std::fs::read_to_string(&audit_path).expect("read audit log");
    assert!(audit.contains(r#""event":"request_started""#));
    assert!(audit.contains(r#""event":"authentication_denied""#));
    assert!(audit.contains(r#""event":"request_completed""#));
    assert!(audit.contains("0197e0f2-6a08-7000-0000-000000000001"));
    assert!(audit.contains("0197e0f2-6a08-7000-0000-000000000002"));
    let validation = validate_audit(&audit_path).expect("validate audit chain");
    assert!(validation.entries >= 5);
}

#[tokio::test]
async fn gateway_fails_closed_when_audit_write_fails() {
    let temp = tempdir_0700();
    let audit_path = temp.path().join("audit.jsonl");
    std::fs::File::create(&audit_path).expect("create audit file");
    std::fs::set_permissions(&audit_path, std::fs::Permissions::from_mode(0o600))
        .expect("set audit file permissions");
    let address = start_gateway(&audit_path, AuditFailureMode::FailClosed).await;
    std::fs::remove_file(&audit_path).expect("remove audit file");

    let response = reqwest::Client::new()
        .get(format!("{address}/metrics"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("metrics request");
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    let body = response
        .json::<serde_json::Value>()
        .await
        .expect("error body");
    assert_eq!(body["code"], "audit_unavailable");
}

#[tokio::test]
async fn gateway_can_fail_open_when_audit_write_fails() {
    let temp = tempdir_0700();
    let audit_path = temp.path().join("audit.jsonl");
    std::fs::File::create(&audit_path).expect("create audit file");
    std::fs::set_permissions(&audit_path, std::fs::Permissions::from_mode(0o600))
        .expect("set audit file permissions");
    let address = start_gateway(&audit_path, AuditFailureMode::FailOpen).await;
    std::fs::remove_file(&audit_path).expect("remove audit file");

    let response = reqwest::Client::new()
        .get(format!("{address}/metrics"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("metrics request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
}
