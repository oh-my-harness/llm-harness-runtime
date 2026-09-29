use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response, Sse};
use axum::routing::{get, post};
use futures::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::auth::{AuthenticatedPrincipal, bearer_token};
use crate::service::{
    ControlEvent, ControlServiceError, CreateAgentSessionRequest, PromptRequest, SessionEvent,
    SharedAgentControlManager, SteerRequest,
};

pub fn build_router(state: SharedAgentControlManager) -> Router {
    let protected_routes = Router::new()
        .route("/v1/agent-sessions", post(create_session))
        .route(
            "/v1/agent-sessions/{id}",
            get(get_session).delete(delete_session),
        )
        .route("/v1/agent-sessions/{id}/prompt", post(prompt_session))
        .route("/v1/agent-sessions/{id}/steer", post(steer_session))
        .route("/v1/agent-sessions/{id}/abort", post(abort_session))
        .route(
            "/v1/agent-sessions/{id}/files/upload",
            post(upload_session_files),
        )
        .route(
            "/v1/agent-sessions/{id}/files/read",
            post(read_session_file),
        )
        .route("/v1/agent-sessions/{id}/events", get(session_events))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            authenticate,
        ))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(state.clone());

    let health_routes = Router::new()
        .route("/healthz", get(health))
        .with_state(state);
    protected_routes.merge(health_routes)
}

async fn authenticate(
    State(state): State<SharedAgentControlManager>,
    headers: HeaderMap,
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    let Some(token) = bearer_token(&headers) else {
        return crate::auth::unauthorized();
    };
    let Some(principal) = state.authenticate_token(&token) else {
        return crate::auth::unauthorized();
    };
    let mut request = request;
    request.extensions_mut().insert(principal);
    next.run(request).await
}

async fn create_session(
    State(state): State<SharedAgentControlManager>,
    principal: axum::Extension<AuthenticatedPrincipal>,
    axum::Json(request): axum::Json<CreateAgentSessionRequest>,
) -> Response {
    match state.create_session(principal.0, request).await {
        Ok(status) => (StatusCode::CREATED, axum::Json(status)).into_response(),
        Err(error) => error_response(error),
    }
}

async fn get_session(
    State(state): State<SharedAgentControlManager>,
    principal: axum::Extension<AuthenticatedPrincipal>,
    Path(session_id): Path<String>,
) -> Response {
    match state.get_session(&principal.0, &session_id) {
        Ok(status) => axum::Json(status).into_response(),
        Err(error) => error_response(error),
    }
}

async fn prompt_session(
    State(state): State<SharedAgentControlManager>,
    principal: axum::Extension<AuthenticatedPrincipal>,
    Path(session_id): Path<String>,
    axum::Json(request): axum::Json<PromptRequest>,
) -> Response {
    match state.prompt(&principal.0, &session_id, request) {
        Ok((prompt_id, status)) => (
            StatusCode::ACCEPTED,
            axum::Json(serde_json::json!({
                "prompt_id": prompt_id,
                "session": status,
            })),
        )
            .into_response(),
        Err(error) => error_response(error),
    }
}

async fn steer_session(
    State(state): State<SharedAgentControlManager>,
    principal: axum::Extension<AuthenticatedPrincipal>,
    Path(session_id): Path<String>,
    axum::Json(request): axum::Json<SteerRequest>,
) -> Response {
    match state.steer(&principal.0, &session_id, request) {
        Ok(status) => (StatusCode::ACCEPTED, axum::Json(status)).into_response(),
        Err(error) => error_response(error),
    }
}

async fn abort_session(
    State(state): State<SharedAgentControlManager>,
    principal: axum::Extension<AuthenticatedPrincipal>,
    Path(session_id): Path<String>,
) -> Response {
    match state.abort(&principal.0, &session_id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error_response(error),
    }
}

async fn upload_session_files(
    State(state): State<SharedAgentControlManager>,
    principal: axum::Extension<AuthenticatedPrincipal>,
    Path(session_id): Path<String>,
    axum::Json(request): axum::Json<llm_harness_sandbox_remote::protocol::UploadFilesRequest>,
) -> Response {
    match state.upload_files(&principal.0, &session_id, request).await {
        Ok(response) => (StatusCode::OK, axum::Json(response)).into_response(),
        Err(error) => error_response(error),
    }
}

async fn read_session_file(
    State(state): State<SharedAgentControlManager>,
    principal: axum::Extension<AuthenticatedPrincipal>,
    Path(session_id): Path<String>,
    axum::Json(request): axum::Json<llm_harness_sandbox_remote::protocol::ReadFileRequest>,
) -> Response {
    match state.read_file(&principal.0, &session_id, request).await {
        Ok(response) => (StatusCode::OK, axum::Json(response)).into_response(),
        Err(error) => error_response(error),
    }
}

async fn delete_session(
    State(state): State<SharedAgentControlManager>,
    principal: axum::Extension<AuthenticatedPrincipal>,
    Path(session_id): Path<String>,
) -> Response {
    match state.delete_session(&principal.0, &session_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error_response(error),
    }
}

async fn session_events(
    State(state): State<SharedAgentControlManager>,
    principal: axum::Extension<AuthenticatedPrincipal>,
    Path(session_id): Path<String>,
) -> Response {
    match state.subscribe_session(&principal.0, &session_id) {
        Ok(events) => {
            let sid = session_id.clone();
            let stream = BroadcastStream::new(events).map(move |event| match event {
                Ok(event) => Ok::<_, std::convert::Infallible>(sse_event(event, &sid)),
                Err(error) => Ok(sse_event(
                    SessionEvent {
                        prompt_id: None,
                        turn_id: None,
                        event: ControlEvent::Failed {
                            message: format!("event stream error: {error}"),
                        },
                    },
                    &sid,
                )),
            });
            Sse::new(stream).into_response()
        }
        Err(error) => error_response(error),
    }
}

async fn health() -> &'static str {
    "ok"
}

const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

fn error_response(error: ControlServiceError) -> Response {
    (
        error.status_code(),
        axum::Json(serde_json::json!({
            "code": error.code(),
            "message": error.to_string(),
        })),
    )
        .into_response()
}

fn sse_event(event: SessionEvent, session_id: &str) -> axum::response::sse::Event {
    let event_value = serde_json::to_value(&event.event).expect("serialize control event");
    let event_type = event.event.event_type();
    let json = serde_json::json!({
        "type": event_type,
        "session_id": session_id,
        "prompt_id": event.prompt_id,
        "turn_id": event.turn_id,
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "data": event_value,
    });
    axum::response::sse::Event::default().data(json.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ControlServiceConfig, GatewayConfig, LlmConfig, PrincipalConfig};
    use crate::service::AgentSessionFactory;
    use axum::body::Body;
    use tower::ServiceExt;

    struct FailingFactory;

    #[async_trait::async_trait]
    impl AgentSessionFactory for FailingFactory {
        async fn create(
            &self,
            _request: &CreateAgentSessionRequest,
        ) -> anyhow::Result<crate::service::AgentSessionComponents> {
            anyhow::bail!("not used by route tests")
        }
    }

    fn test_state() -> SharedAgentControlManager {
        let config = ControlServiceConfig {
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
                token_env: "unused".to_owned(),
                max_trust_profile: llm_harness_platform::trust::TrustProfile::Restricted,
                max_sessions: Some(1),
            }],
            persistence_path: None,
        };
        std::sync::Arc::new(
            crate::service::AgentControlManager::new(config, std::sync::Arc::new(FailingFactory))
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn healthz_does_not_require_authentication() {
        let response = build_router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .method(axum::http::Method::GET)
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn protected_routes_require_authentication() {
        let response = build_router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .method(axum::http::Method::POST)
                    .uri("/v1/agent-sessions")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
