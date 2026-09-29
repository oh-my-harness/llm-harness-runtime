//! HTTP contract tests for the mobile agent control service.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use http_body_util::BodyExt;
use llm_harness_agent::harness::{AgentHarness, AgentHarnessOptions};
use llm_harness_loop::LlmClient;
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse, NoOpEnv};
use llm_harness_types::ExecutionEnv;
use tower::ServiceExt;

use llm_harness_agent_control::{
    AgentControlManager, AgentSandbox, AgentSessionComponents, AgentSessionFactory,
    CreateAgentSessionRequest, GatewayConfig, LlmConfig, PrincipalConfig,
};

struct FakeSandbox {
    shutdown: Arc<AtomicBool>,
}

#[async_trait]
impl AgentSandbox for FakeSandbox {
    fn id(&self) -> String {
        "test-sandbox".to_owned()
    }

    fn backend(&self) -> String {
        "test".to_owned()
    }

    fn env(&self) -> Arc<dyn ExecutionEnv> {
        Arc::new(NoOpEnv)
    }

    async fn shutdown(self: Box<Self>) -> anyhow::Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        Ok(())
    }
}

struct TextFactory {
    shutdown: Arc<AtomicBool>,
}

#[async_trait]
impl AgentSessionFactory for TextFactory {
    async fn create(
        &self,
        _request: &CreateAgentSessionRequest,
    ) -> anyhow::Result<AgentSessionComponents> {
        let client: Arc<dyn LlmClient> =
            Arc::new(MockLlmClient::new(vec![MockResponse::text("done")]));
        let env: Arc<dyn ExecutionEnv> = Arc::new(NoOpEnv);
        let harness =
            AgentHarness::new_in_memory(client, env, AgentHarnessOptions::new("test-model")).await;
        Ok(AgentSessionComponents {
            harness: Arc::new(harness),
            sandbox: Box::new(FakeSandbox {
                shutdown: Arc::clone(&self.shutdown),
            }),
        })
    }
}

fn build_test_router() -> axum::Router {
    unsafe {
        std::env::set_var("AGENT_CONTROL_HTTP_TEST_TOKEN", "test");
    }
    let config = llm_harness_agent_control::ControlServiceConfig {
        bind: None,
        port: None,
        default_model: "test-model".to_owned(),
        gateway: GatewayConfig {
            base_url: "http://127.0.0.1:1".to_owned(),
            token_env: "unused".to_owned(),
            allow_insecure_http: true,
        },
        llm: LlmConfig {
            api_key_env: "unused".to_owned(),
            provider: "test".to_owned(),
            base_url: None,
        },
        principals: vec![PrincipalConfig {
            principal: "phone".to_owned(),
            token_env: "AGENT_CONTROL_HTTP_TEST_TOKEN".to_owned(),
            max_trust_profile: llm_harness_platform::trust::TrustProfile::Restricted,
            max_sessions: Some(4),
        }],
        persistence_path: None,
    };
    let manager = AgentControlManager::new(
        config,
        Arc::new(TextFactory {
            shutdown: Arc::new(AtomicBool::new(false)),
        }),
    )
    .unwrap();
    llm_harness_agent_control::build_router(Arc::new(manager))
}

async fn json_body(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn full_http_flow_create_prompt_get_delete() {
    let app = build_test_router();
    let auth = ("authorization", "Bearer test");

    // POST /v1/agent-sessions → 201
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/agent-sessions")
                .header("content-type", "application/json")
                .header(auth.0, auth.1)
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 201, "create should return 201");
    let created = json_body(response).await;
    let session_id = created["id"].as_str().expect("id").to_owned();
    assert_eq!(created["state"], "idle");
    assert!(!session_id.is_empty());

    // GET /v1/agent-sessions/{id} → 200
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
    assert_eq!(response.status(), 200, "get should return 200");
    let status = json_body(response).await;
    assert_eq!(status["id"], session_id.as_str());
    assert_eq!(status["state"], "idle");

    // POST /v1/agent-sessions/{id}/prompt → 202
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/v1/agent-sessions/{session_id}/prompt"))
                .header("content-type", "application/json")
                .header(auth.0, auth.1)
                .body(Body::from(r#"{"prompt": "hello"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 202, "prompt should return 202");
    let prompt_response = json_body(response).await;
    assert!(prompt_response["prompt_id"].as_str().is_some());
    assert_eq!(prompt_response["session"]["state"], "running");

    // Wait for prompt to finish
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // GET → idle again
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
    let status = json_body(response).await;
    assert_eq!(status["state"], "idle");

    // DELETE /v1/agent-sessions/{id} → 204
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
    assert_eq!(response.status(), 204, "delete should return 204");
}

#[tokio::test]
async fn create_rejects_unknown_fields() {
    let app = build_test_router();
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/agent-sessions")
                .header("content-type", "application/json")
                .header("authorization", "Bearer test")
                .body(Body::from(r#"{"unknown": true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        422,
        "unknown field should be rejected as unprocessable entity"
    );
}

#[tokio::test]
async fn delete_nonexistent_returns_404() {
    let app = build_test_router();
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("DELETE")
                .uri("/v1/agent-sessions/nonexistent-id")
                .header("authorization", "Bearer test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn prompt_nonexistent_returns_404() {
    let app = build_test_router();
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/agent-sessions/nonexistent/prompt")
                .header("content-type", "application/json")
                .header("authorization", "Bearer test")
                .body(Body::from(r#"{"prompt": "hi"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn steer_returns_202() {
    let app = build_test_router();
    let auth = ("authorization", "Bearer test");

    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/agent-sessions")
                .header("content-type", "application/json")
                .header(auth.0, auth.1)
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let created = json_body(response).await;
    let session_id = created["id"].as_str().unwrap().to_owned();

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/v1/agent-sessions/{session_id}/steer"))
                .header("content-type", "application/json")
                .header(auth.0, auth.1)
                .body(Body::from(r#"{"message": "do something else"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        202,
        "steer on idle session should be queued for the next turn"
    );
}
