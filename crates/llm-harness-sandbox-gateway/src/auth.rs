use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::http::{StatusCode, header::AUTHORIZATION};
use axum::response::{IntoResponse, Response};
use llm_harness_sandbox_remote::protocol::GatewayErrorResponse;
use sha2::{Digest, Sha256};

/// One authenticated principal and its permitted host filesystem roots.
#[derive(Clone)]
pub struct GatewayPrincipalConfig {
    pub principal: String,
    pub bearer_token: String,
    pub fs_allowlist: Vec<PathBuf>,
}

/// Static gateway configuration.
#[derive(Clone)]
pub struct GatewayConfig {
    principals: Vec<PrincipalSecret>,
    pub max_sandboxes: usize,
    pub default_exec_timeout_ms: u64,
    pub max_exec_timeout_ms: u64,
    pub max_output_bytes: usize,
    pub max_request_bytes: usize,
    work_root: Option<PathBuf>,
}

#[derive(Clone)]
struct PrincipalSecret {
    id: String,
    token_hash: [u8; 32],
    fs_allowlist: Vec<PathBuf>,
}

/// An authenticated principal attached to a request by the auth middleware.
#[derive(Clone)]
pub(crate) struct AuthenticatedPrincipal {
    pub(crate) id: Arc<str>,
    pub(crate) fs_allowlist: Arc<[PathBuf]>,
}

impl GatewayConfig {
    /// Create a validated gateway configuration.
    pub fn new(principals: Vec<GatewayPrincipalConfig>) -> anyhow::Result<Self> {
        if principals.is_empty() {
            anyhow::bail!("at least one gateway principal is required");
        }

        let mut secrets = Vec::with_capacity(principals.len());
        for principal in principals {
            if principal.principal.trim().is_empty() {
                anyhow::bail!("gateway principal id must not be empty");
            }
            if principal.bearer_token.len() < 32 {
                anyhow::bail!(
                    "gateway bearer token for principal {:?} must be at least 32 bytes",
                    principal.principal
                );
            }
            let mut allowlist = Vec::with_capacity(principal.fs_allowlist.len());
            for root in principal.fs_allowlist {
                let canonical = std::fs::canonicalize(&root).map_err(|error| {
                    anyhow::anyhow!("canonicalize fs_allowlist {root:?}: {error}")
                })?;
                if !canonical.is_dir() {
                    anyhow::bail!("fs_allowlist entry {canonical:?} is not a directory");
                }
                allowlist.push(canonical);
            }
            secrets.push(PrincipalSecret {
                id: principal.principal,
                token_hash: Sha256::digest(principal.bearer_token.as_bytes()).into(),
                fs_allowlist: allowlist,
            });
        }

        let config = Self {
            principals: secrets,
            max_sandboxes: 128,
            default_exec_timeout_ms: 60_000,
            max_exec_timeout_ms: 3_600_000,
            max_output_bytes: 1024 * 1024,
            max_request_bytes: 64 * 1024 * 1024,
            work_root: None,
        };
        config.validate()?;
        Ok(config)
    }

    /// Set the maximum number of concurrently tracked sandboxes.
    pub fn with_max_sandboxes(mut self, value: usize) -> Self {
        self.max_sandboxes = value;
        self
    }

    /// Set shell timeout defaults and the hard timeout ceiling.
    pub fn with_exec_timeout_ms(mut self, default_ms: u64, max_ms: u64) -> Self {
        self.default_exec_timeout_ms = default_ms;
        self.max_exec_timeout_ms = max_ms;
        self
    }

    /// Set the hard per-stream output cap.
    pub fn with_max_output_bytes(mut self, value: usize) -> Self {
        self.max_output_bytes = value;
        self
    }

    /// Set the maximum JSON request body size.
    pub fn with_max_request_bytes(mut self, value: usize) -> Self {
        self.max_request_bytes = value;
        self
    }

    /// Set the gateway-owned root used for sandbox work directories.
    pub fn with_work_root(mut self, root: PathBuf) -> Self {
        self.work_root = Some(root);
        self
    }

    pub(crate) fn prepare_work_root(&mut self) -> anyhow::Result<PathBuf> {
        let candidate = self
            .work_root
            .clone()
            .unwrap_or_else(|| PathBuf::from("/tmp/llm-harness-sandbox-gateway"));
        if !candidate.is_absolute() {
            anyhow::bail!("gateway work_root must be absolute");
        }
        match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    anyhow::bail!("gateway work_root {candidate:?} must be a directory");
                }
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o777 != 0o700 {
                    anyhow::bail!("gateway work_root {candidate:?} must have 0700 permissions");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&candidate)?;
            }
            Err(error) => return Err(error.into()),
        }
        let canonical = std::fs::canonicalize(&candidate)?;
        if canonical != candidate {
            anyhow::bail!("gateway work_root must be an absolute canonical path");
        }
        self.work_root = Some(canonical.clone());
        Ok(canonical)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.max_sandboxes == 0 {
            anyhow::bail!("max_sandboxes must be positive");
        }
        if self.default_exec_timeout_ms == 0 || self.max_exec_timeout_ms == 0 {
            anyhow::bail!("execution timeouts must be positive");
        }
        if self.default_exec_timeout_ms > self.max_exec_timeout_ms {
            anyhow::bail!("default execution timeout exceeds the maximum execution timeout");
        }
        if self.max_output_bytes == 0 {
            anyhow::bail!("max_output_bytes must be positive");
        }
        if self.max_request_bytes == 0 {
            anyhow::bail!("max_request_bytes must be positive");
        }
        Ok(())
    }

    pub(crate) fn authenticate(&self, bearer_token: &str) -> Option<AuthenticatedPrincipal> {
        let token_hash: [u8; 32] = Sha256::digest(bearer_token.as_bytes()).into();
        self.principals
            .iter()
            .find(|principal| constant_time_eq(&principal.token_hash, &token_hash))
            .map(|principal| AuthenticatedPrincipal {
                id: Arc::from(principal.id.as_str()),
                fs_allowlist: Arc::from(principal.fs_allowlist.as_slice()),
            })
    }

    pub(crate) fn principal(&self, principal_id: &str) -> Option<AuthenticatedPrincipal> {
        self.principals
            .iter()
            .find(|principal| principal.id == principal_id)
            .map(|principal| AuthenticatedPrincipal {
                id: Arc::from(principal.id.as_str()),
                fs_allowlist: Arc::from(principal.fs_allowlist.as_slice()),
            })
    }

    pub(crate) fn work_root(&self) -> &std::path::Path {
        self.work_root
            .as_deref()
            .expect("gateway work root is prepared")
    }
}

impl std::fmt::Debug for GatewayConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GatewayConfig")
            .field("principal_count", &self.principals.len())
            .field("max_sandboxes", &self.max_sandboxes)
            .field("default_exec_timeout_ms", &self.default_exec_timeout_ms)
            .field("max_exec_timeout_ms", &self.max_exec_timeout_ms)
            .field("max_output_bytes", &self.max_output_bytes)
            .finish()
    }
}

pub(crate) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

pub(crate) fn bearer_token(request: &axum::http::Request<axum::body::Body>) -> Option<&str> {
    request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

pub(crate) fn unauthorized() -> Response {
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(GatewayErrorResponse {
            code: "unauthorized".to_owned(),
            message: "invalid bearer token".to_owned(),
            path: None,
        }),
    )
        .into_response();
    response.headers_mut().insert(
        "WWW-Authenticate",
        axum::http::HeaderValue::from_static("Bearer"),
    );
    response
}
