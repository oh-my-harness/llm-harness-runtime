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

use async_trait::async_trait;
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
use llm_harness_platform::sandbox::Sandbox;
use llm_harness_sandbox_gateway::{GatewayConfig, GatewayPrincipalConfig, SandboxGateway};
use llm_harness_sandbox_remote::protocol::{FileContent, UploadFile, UploadFilesRequest};
use llm_harness_sandbox_remote::{RemoteSandbox, RemoteSandboxClient, RemoteSandboxClientConfig};
use llm_harness_types::ExecutionEnv;

fn gateway_config(work_root: PathBuf) -> GatewayConfig {
    GatewayConfig::new(vec![GatewayPrincipalConfig {
        principal: "control-service".to_owned(),
        bearer_token: "gateway-e2e-test-token-0123456789abcdef".to_owned(),
        fs_allowlist: vec![PathBuf::from("/workspace")],
        host_project_dirs: vec![],
        max_trust_profile: llm_harness_platform::trust::TrustProfile::TrustedLan,
        requests_per_minute: None,
    }])
    .unwrap()
    .with_work_root(work_root)
}

#[tokio::test]
async fn upload_session_files_through_control_service() {
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

    let gateway = SandboxGateway::new(gateway_config(work_root.path().to_path_buf())).unwrap();
    let gateway_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_port = gateway_listener.local_addr().unwrap().port();
    let gateway_task = tokio::spawn(async move {
        axum::serve(gateway_listener, gateway.router())
            .await
            .unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let config = control_config(gateway_port);
    let llm_client = Arc::new(MockLlmClient::with_single(MockResponse::text("ok")));
    let factory = Arc::new(RemoteMockLlmFactory {
        config: config.clone(),
        llm_client,
    });
    let manager =
        Arc::new(llm_harness_agent_control::AgentControlManager::new(config, factory).unwrap());
    let app = llm_harness_agent_control::build_router(Arc::clone(&manager));
    let auth = (
        "authorization",
        "Bearer phone-e2e-test-token-0123456789abcdef",
    );

    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/agent-sessions")
                .header("content-type", "application/json")
                .header(auth.0, auth.1)
                .body(Body::from(r#"{"trust_profile": "restricted"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let created = read_json(response).await;
    let session_id = created["id"].as_str().unwrap().to_owned();

    let request = UploadFilesRequest {
        files: vec![
            UploadFile {
                path: "/workspace/notes.txt".to_owned(),
                content: FileContent::Text {
                    value: "uploaded by phone\n".to_owned(),
                },
            },
            UploadFile {
                path: "/workspace/payload.bin".to_owned(),
                content: FileContent::Binary {
                    base64: "AAEC".to_owned(),
                },
            },
        ],
    };
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/v1/agent-sessions/{session_id}/files/upload"))
                .header("content-type", "application/json")
                .header(auth.0, auth.1)
                .body(Body::from(serde_json::to_vec(&request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let uploaded = read_json(response).await;
    assert_eq!(
        uploaded["files"][0]["path"],
        serde_json::json!("/workspace/notes.txt")
    );
    assert_eq!(uploaded["files"][0]["bytes"], 18);
    assert_eq!(
        uploaded["files"][1]["path"],
        serde_json::json!("/workspace/payload.bin")
    );
    assert_eq!(uploaded["files"][1]["bytes"], 3);

    let response = app
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
    assert_eq!(response.status(), 204);
    gateway_task.abort();
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

struct RemoteMockLlmFactory {
    config: llm_harness_agent_control::ControlServiceConfig,
    llm_client: Arc<MockLlmClient>,
}

fn sandbox_config() -> llm_harness_platform::sandbox::SandboxConfig {
    llm_harness_platform::sandbox::SandboxConfig {
        fs_allowlist: Vec::new(),
        fs_denylist: Vec::new(),
        net_allowlist: Vec::new(),
        resource_limits: llm_harness_platform::sandbox::ResourceLimits::default(),
        work_dir: None,
        trust_profile: llm_harness_platform::trust::TrustProfile::Restricted,
        host_binds: Vec::new(),
    }
}

#[async_trait]
impl llm_harness_agent_control::AgentSessionFactory for RemoteMockLlmFactory {
    async fn create(
        &self,
        request: &llm_harness_agent_control::CreateAgentSessionRequest,
    ) -> anyhow::Result<llm_harness_agent_control::AgentSessionComponents> {
        let client_config = RemoteSandboxClientConfig::new_insecure(
            self.config.gateway.base_url.clone(),
            self.config.gateway_token()?,
        )?;
        let client = Arc::new(RemoteSandboxClient::new(client_config)?);
        let sandbox = RemoteSandbox::connect(sandbox_config(), client).await?;
        let env: Arc<dyn ExecutionEnv> = Sandbox::env(&sandbox);
        let model = request
            .model
            .as_deref()
            .unwrap_or(&self.config.default_model);
        let mut options = llm_harness_agent::harness::AgentHarnessOptions::new(model);
        options.tools =
            vec![Arc::new(llm_harness_tools::BashTool::new()) as Arc<dyn llm_harness_types::Tool>];
        let harness = llm_harness_agent::harness::AgentHarness::new_in_memory(
            self.llm_client.clone(),
            env,
            options,
        )
        .await;
        Ok(llm_harness_agent_control::AgentSessionComponents {
            harness: Arc::new(harness),
            sandbox: Box::new(llm_harness_agent_control::RemoteSandboxResource::new(
                sandbox,
            )),
        })
    }
}

#[tokio::test]
async fn full_e2e_create_prompt_events_steer_abort_delete() {
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
    let llm_client = Arc::new(MockLlmClient::with_single(MockResponse::tool_use(
        "call_e2e_bash",
        "bash",
        r#"{"command": "echo e2e-started; sleep 5; echo e2e-finished", "timeout": 10}"#,
    )));
    let factory = Arc::new(RemoteMockLlmFactory {
        config: config.clone(),
        llm_client,
    });
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

    // 5. Subscribe before submitting the prompt, then start a real remote
    //    shell through the Bwrap gateway.
    let principal = llm_harness_agent_control::AuthenticatedPrincipal {
        id: "phone".to_owned(),
        max_trust_profile: llm_harness_platform::trust::TrustProfile::TrustedLan,
        max_sessions: 2,
    };
    let mut events = manager
        .subscribe_session(&principal, &session_id)
        .expect("authenticated principal should subscribe");

    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/v1/agent-sessions/{session_id}/prompt"))
                .header("content-type", "application/json")
                .header(auth.0, auth.1)
                .body(Body::from(r#"{"prompt": "run the remote command"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 202, "prompt should return 202");
    let prompt_response = read_json(response).await;
    let prompt_id = prompt_response["prompt_id"]
        .as_str()
        .expect("prompt id")
        .to_owned();

    let mut saw_tool_started = false;
    while let Ok(event) = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
        .await
        .expect("tool start should arrive")
    {
        assert_eq!(event.prompt_id.as_deref(), Some(prompt_id.as_str()));
        if matches!(
            event.event,
            llm_harness_agent_control::ControlEvent::ToolStarted { .. }
        ) {
            saw_tool_started = true;
            break;
        }
    }
    assert!(saw_tool_started, "remote bash should start");

    // 6. Steering remains accepted while the remote tool is active.
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/v1/agent-sessions/{session_id}/steer"))
                .header("content-type", "application/json")
                .header(auth.0, auth.1)
                .body(Body::from(r#"{"message": "stop early"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 202, "active steer should return 202");

    // 7. Abort the active turn without deleting the session.
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/v1/agent-sessions/{session_id}/abort"))
                .header(auth.0, auth.1)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 204, "abort should return 204");

    let mut saw_aborted = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, events.recv()).await {
        if matches!(
            event.event,
            llm_harness_agent_control::ControlEvent::Aborted
        ) {
            saw_aborted = true;
            break;
        }
    }
    assert!(saw_aborted, "abort should emit an event");

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let status = manager.get_session(&principal, &session_id).unwrap();
    assert_eq!(status.state, "idle");

    // 8. Delete session (shuts down the real Bwrap sandbox)
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

    // 9. Verify session is gone
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
