#![cfg(target_os = "linux")]

use llm_harness_sandbox_gateway::{GatewayConfig, GatewayPrincipalConfig, SandboxGateway};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

const OLD_TOKEN: &str = "gateway-old-credential-token-0123456789";
const NEW_TOKEN: &str = "gateway-new-credential-token-0123456789";
const OTHER_TOKEN: &str = "gateway-other-credential-token-012345678";

fn tempdir_0700() -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("credential tempdir");
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700))
        .expect("set tempdir permissions");
    temp
}

fn config(work_root: &Path, bearer_token: &str) -> GatewayConfig {
    GatewayConfig::new(vec![principal("control-service", bearer_token)])
        .expect("gateway config")
        .with_work_root(work_root.to_path_buf())
}

fn principal(id: &str, bearer_token: &str) -> GatewayPrincipalConfig {
    GatewayPrincipalConfig {
        principal: id.to_owned(),
        bearer_token: bearer_token.to_owned(),
        fs_allowlist: vec![],
        host_project_dirs: vec![],
        max_trust_profile: llm_harness_platform::trust::TrustProfile::Restricted,
        requests_per_minute: None,
        max_active_sandboxes: None,
    }
}

async fn start_gateway(config: GatewayConfig) -> (String, tokio::task::JoinHandle<()>) {
    let gateway = SandboxGateway::new(config).expect("build gateway");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gateway");
    let address = format!("http://{}", listener.local_addr().expect("gateway address"));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, gateway.router()).await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    (address, server)
}

#[tokio::test]
async fn gateway_rotates_a_credential_without_restart() {
    let temp = tempdir_0700();
    let (address, server) = start_gateway(config(temp.path(), OLD_TOKEN)).await;
    let client = reqwest::Client::new();

    let before = client
        .get(format!("{address}/metrics"))
        .bearer_auth(OLD_TOKEN)
        .send()
        .await
        .expect("metrics request");
    assert_eq!(before.status(), reqwest::StatusCode::OK);

    let rotated = client
        .post(format!("{address}/v1/credentials/rotate"))
        .bearer_auth(OLD_TOKEN)
        .json(&serde_json::json!({
            "new_token": NEW_TOKEN
        }))
        .send()
        .await
        .expect("rotate request");
    assert_eq!(rotated.status(), reqwest::StatusCode::NO_CONTENT);

    let old = client
        .get(format!("{address}/metrics"))
        .bearer_auth(OLD_TOKEN)
        .send()
        .await
        .expect("old credential request");
    assert_eq!(old.status(), reqwest::StatusCode::UNAUTHORIZED);
    let new = client
        .get(format!("{address}/metrics"))
        .bearer_auth(NEW_TOKEN)
        .send()
        .await
        .expect("new metrics request");
    assert_eq!(new.status(), reqwest::StatusCode::OK);

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn gateway_revokes_a_credential_without_restart() {
    let temp = tempdir_0700();
    let (address, server) = start_gateway(config(temp.path(), OLD_TOKEN)).await;
    let client = reqwest::Client::new();

    let revoked = client
        .post(format!("{address}/v1/credentials/revoke"))
        .bearer_auth(OLD_TOKEN)
        .send()
        .await
        .expect("revoke request");
    assert_eq!(revoked.status(), reqwest::StatusCode::NO_CONTENT);

    let rejected = client
        .get(format!("{address}/metrics"))
        .bearer_auth(OLD_TOKEN)
        .send()
        .await
        .expect("revoked metrics request");
    assert_eq!(rejected.status(), reqwest::StatusCode::UNAUTHORIZED);

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn credential_lifecycle_is_audited_without_plaintext_tokens() {
    let temp = tempdir_0700();
    let (address, server) = start_gateway(config(temp.path(), OLD_TOKEN)).await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{address}/v1/credentials/rotate"))
        .bearer_auth(OLD_TOKEN)
        .json(&serde_json::json!({
            "new_token": NEW_TOKEN
        }))
        .send()
        .await
        .expect("rotate request");
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    server.abort();
    let _ = server.await;

    let audit = std::fs::read_to_string(temp.path().join("audit.jsonl")).expect("read audit log");
    assert!(audit.contains(r#""event":"credential_rotated""#));
    assert!(!audit.contains(OLD_TOKEN));
    assert!(!audit.contains(NEW_TOKEN));
}

#[tokio::test]
async fn rotated_credentials_survive_gateway_restart() {
    let temp = tempdir_0700();
    let config = config(temp.path(), OLD_TOKEN);
    let (address, server) = start_gateway(config.clone()).await;
    reqwest::Client::new()
        .post(format!("{address}/v1/credentials/rotate"))
        .bearer_auth(OLD_TOKEN)
        .json(&serde_json::json!({
            "new_token": NEW_TOKEN
        }))
        .send()
        .await
        .expect("rotate request");
    server.abort();
    let _ = server.await;

    let (address, server) = start_gateway(config).await;
    let client = reqwest::Client::new();
    let old = client
        .get(format!("{address}/metrics"))
        .bearer_auth(OLD_TOKEN)
        .send()
        .await
        .expect("old metrics request");
    assert_eq!(old.status(), reqwest::StatusCode::UNAUTHORIZED);
    let new = client
        .get(format!("{address}/metrics"))
        .bearer_auth(NEW_TOKEN)
        .send()
        .await
        .expect("new metrics request");
    assert_eq!(new.status(), reqwest::StatusCode::OK);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn rotation_keeps_other_valid_principals_authenticated() {
    let temp = tempdir_0700();
    let config = GatewayConfig::new(vec![
        principal("control-service", OLD_TOKEN),
        principal("observer", OTHER_TOKEN),
    ])
    .expect("gateway config")
    .with_work_root(temp.path().to_path_buf());
    let (address, server) = start_gateway(config).await;
    let client = reqwest::Client::new();

    let rotated = client
        .post(format!("{address}/v1/credentials/rotate"))
        .bearer_auth(OLD_TOKEN)
        .json(&serde_json::json!({ "new_token": NEW_TOKEN }))
        .send()
        .await
        .expect("rotate request");
    assert_eq!(rotated.status(), reqwest::StatusCode::NO_CONTENT);

    let old = client
        .get(format!("{address}/metrics"))
        .bearer_auth(OLD_TOKEN)
        .send()
        .await
        .expect("old metrics request");
    assert_eq!(old.status(), reqwest::StatusCode::UNAUTHORIZED);
    let other = client
        .get(format!("{address}/metrics"))
        .bearer_auth(OTHER_TOKEN)
        .send()
        .await
        .expect("other metrics request");
    assert_eq!(other.status(), reqwest::StatusCode::OK);
    server.abort();
    let _ = server.await;
}
