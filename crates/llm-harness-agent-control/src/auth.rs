use axum::http::{HeaderMap, StatusCode};
use llm_harness_platform::trust::TrustProfile;

#[derive(Debug, Clone)]
pub struct AuthenticatedPrincipal {
    pub id: String,
    pub max_trust_profile: TrustProfile,
    pub max_sessions: usize,
}

pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(axum::http::header::AUTHORIZATION)?;
    let value = value.to_str().ok()?;
    value
        .strip_prefix("Bearer ")
        .map(ToOwned::to_owned)
        .filter(|token| !token.is_empty())
}

pub(crate) fn unauthorized() -> axum::response::Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({
            "code": "unauthorized",
            "message": "missing or invalid bearer token"
        })),
    )
        .into_response()
}

use axum::response::IntoResponse;
