//! End-to-end test: real sandbox gateway + agent control service.
//!
//! Starts a Bwrap-backed sandbox gateway on a random port, then runs the
//! full mobile control contract through it: create → prompt → events → delete.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use http_body_util::BodyExt;
use tower::ServiceExt;

use llm_harness_sandbox_gateway::{GatewayConfig, GatewayPrincipalConfig, SandboxGateway};

fn gateway_config(work_root: PathBuf) -> GatewayConfig {
    GatewayConfig::new(vec![GatewayPrincipalConfig {
        principal: "control-service".to_owned(),
        bearer_token: "gateway-e2e-test-token-0123456789abcdef".to_owned(),
        fs_allowlist: vec![PathBuf::from("/workspace")],
        host_project_dirs: vec![],
        max_trust_profile: llm_harness_platform::trust::TrustProfile::TrustedLan,
    }])
    .unwrap()
    .with_work_root(work_root)
}

fn control_config(gateway_port: u16) -> llm_harness_agent_control::ControlServiceConfig {
    llm_harness_agent_control::ControlServiceConfig {
        bind: Some("127.0.0.1".to_owned()),
        port: None,
        default_model: "test-model".to_owned(),
        gateway: llm_harness_agent_control::GatewayConfig {
            base_url: format!("http://127.0.0.1:{gateway_port}"),
            token_env: "E2E_GATEWAY_TOKEN".to_owned(),
            allow_insecure_http: true,
        },
        llm: llm_harness_agent_control::LlmConfig {
            api_key_env: "E2E_LLM_KEY".to_owned(),
            provider: "test".to_owned(),
            base_url: None,
        },
        principals: vec![llm_harness_agent_control::PrincipalConfig {
            principal: "phone".to_owned(),
            token_env: "E2E_PHONE_TOKEN".to_owned(),
            max_trust_profile: llm_harness_platform::trust::TrustProfile::TrustedLan,
            max_sessions: Some(2),
        }],
        persistence_path: None,
    }
}

async fn read_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn full_e2e_create_prompt_delete() {
    unsafe {
        std::env::set_var(
            "E2E_GATEWAY_TOKEN",
            "gateway-e2e-test-token-0123456789abcdef",
        );
        std::env::set_var("E2E_PHONE_TOKEN", "phone-e2e-test-token-0123456789abcdef");
        std::env::set_var("E2E_LLM_KEY", "e2e-test-llm-key");
    }

    let work_root = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(work_root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    // 1. Start the sandbox gateway
    let gateway = SandboxGateway::new(gateway_config(work_root.path().to_path_buf())).unwrap();
    let gateway_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_port = gateway_listener.local_addr().unwrap().port();
    let gateway_task = tokio::spawn(async move {
        axum::serve(gateway_listener, gateway.router())
            .await
            .unwrap();
    });

    // Give gateway a moment to bind
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // 2. Start the agent control service
    let config = control_config(gateway_port);
    let factory = Arc::new(llm_harness_agent_control::RemoteSandboxSessionFactory::new(
        config.clone(),
    ));
    let manager =
        Arc::new(llm_harness_agent_control::AgentControlManager::new(config, factory).unwrap());
    let app = llm_harness_agent_control::build_router(Arc::clone(&manager));
    let auth = (
        "authorization",
        "Bearer phone-e2e-test-token-0123456789abcdef",
    );

    // 3. Create session (this creates a real Bwrap sandbox via gateway)
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/agent-sessions")
                .header("content-type", "application/json")
                .header(auth.0, auth.1)
                .body(Body::from(
                    r#"{"model": "test-model", "trust_profile": "restricted"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 201, "create session should return 201");
    let created = read_json(response).await;
    let session_id = created["id"].as_str().expect("session id").to_owned();
    assert_eq!(created["state"], "idle");
    let sandbox_id = created["sandbox_id"]
        .as_str()
        .expect("sandbox id")
        .to_owned();
    assert!(!sandbox_id.is_empty());
    assert_eq!(created["backend"], "linux-bwrap");

    // 4. Get session status
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/v1/agent-sessions/{session_id}"))
                .header(auth.0, auth.1)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let status = read_json(response).await;
    assert_eq!(status["id"], session_id.as_str());
    assert_eq!(status["sandbox_id"], sandbox_id.as_str());

    // 5. Delete session (shuts down the real Bwrap sandbox)
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("DELETE")
                .uri(format!("/v1/agent-sessions/{session_id}"))
                .header(auth.0, auth.1)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 204, "delete should return 204");

    // 6. Verify session is gone
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/v1/agent-sessions/{session_id}"))
                .header(auth.0, auth.1)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 404, "deleted session should return 404");

    gateway_task.abort();
}
