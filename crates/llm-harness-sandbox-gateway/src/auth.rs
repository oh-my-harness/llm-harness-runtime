use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::http::{StatusCode, header::AUTHORIZATION};
use axum::response::{IntoResponse, Response};
use llm_harness_sandbox_remote::protocol::GatewayErrorResponse;
use sha2::{Digest, Sha256};

/// One authenticated principal and its permitted guest filesystem roots.
#[derive(Clone)]
pub struct GatewayPrincipalConfig {
    pub principal: String,
    pub bearer_token: String,
    /// Guest-coordinate allowlist (restricted profile). Must be `/workspace`
    /// or a subpath.
    pub fs_allowlist: Vec<PathBuf>,
    /// Host directories bound read-write into trusted-profile sandboxes,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub host_project_dirs: Vec<PathBuf>,
    /// Highest profile this principal may request. Requests above the cap
    /// are rejected. Default `restricted`.
    pub max_trust_profile: llm_harness_platform::trust::TrustProfile,
    /// Optional fixed-window request limit. `None` disables rate limiting.
    pub requests_per_minute: Option<usize>,
}

/// Firecracker VM artifacts and VM defaults used by the gateway backend.
#[derive(Clone)]
pub struct FirecrackerBackendConfig {
    pub binary_path: PathBuf,
    pub kernel_image_path: PathBuf,
    pub base_image_path: PathBuf,
    pub max_vcpu_count: u32,
    pub max_memory_mib: u32,
    pub max_disk_mib: Option<u32>,
    pub vsock_guest_cid_base: u32,
    pub guest_agent_port: u32,
    pub readiness_timeout: Duration,
}

/// Execution backend selected for every sandbox in this gateway process.
#[derive(Clone)]
pub enum GatewayBackend {
    Bwrap,
    /// macOS `sandbox-exec` backend (Seatbelt). Only available on macOS.
    #[cfg(target_os = "macos")]
    Seatbelt,
    Firecracker(FirecrackerBackendConfig),
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
    audit_path: Option<PathBuf>,
    audit_failure_mode: AuditFailureMode,
    backend: GatewayBackend,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditFailureMode {
    #[default]
    FailClosed,
    FailOpen,
}

#[derive(Clone)]
struct PrincipalSecret {
    id: String,
    token_hash: [u8; 32],
    fs_allowlist: Vec<PathBuf>,
    host_project_dirs: Vec<PathBuf>,
    max_trust_profile: llm_harness_platform::trust::TrustProfile,
    requests_per_minute: Option<usize>,
}

/// An authenticated principal attached to a request by the auth middleware.
#[derive(Clone)]
pub(crate) struct AuthenticatedPrincipal {
    pub(crate) id: Arc<str>,
    pub(crate) fs_allowlist: Arc<[PathBuf]>,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) host_project_dirs: Arc<[PathBuf]>,
    pub(crate) max_trust_profile: llm_harness_platform::trust::TrustProfile,
    pub(crate) requests_per_minute: Option<usize>,
}

impl GatewayConfig {
    /// Create a validated gateway configuration.
    pub fn new(principals: Vec<GatewayPrincipalConfig>) -> anyhow::Result<Self> {
        if principals.is_empty() {
            anyhow::bail!("at least one gateway principal is required");
        }
        let mut principal_ids = std::collections::HashSet::new();
        let mut token_hashes = std::collections::HashSet::new();

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
                let root = root.to_str().ok_or_else(|| {
                    anyhow::anyhow!("gateway principal fs_allowlist must be valid UTF-8")
                })?;
                if !is_guest_workspace_path(root) {
                    anyhow::bail!(
                        "gateway principal fs_allowlist entry must be /workspace or a subpath"
                    );
                }
                allowlist.push(std::path::PathBuf::from(root));
            }
            let mut project_dirs = Vec::with_capacity(principal.host_project_dirs.len());
            for dir in &principal.host_project_dirs {
                if !dir.is_absolute() {
                    anyhow::bail!(
                        "gateway principal host_project_dirs entry {dir:?} must be absolute"
                    );
                }
                if !dir.is_dir() {
                    anyhow::bail!(
                        "gateway principal host_project_dirs entry {dir:?} must exist and be a directory"
                    );
                }
                project_dirs.push(dir.clone());
            }
            if principal.requests_per_minute == Some(0) {
                anyhow::bail!(
                    "gateway principal requests_per_minute for {:?} must be positive",
                    principal.principal
                );
            }
            let token_hash = Sha256::digest(principal.bearer_token.as_bytes()).into();
            if !principal_ids.insert(principal.principal.clone()) {
                anyhow::bail!(
                    "gateway principal {:?} is defined more than once",
                    principal.principal
                );
            }
            if !token_hashes.insert(token_hash) {
                anyhow::bail!("gateway bearer token hashes must be unique");
            }
            secrets.push(PrincipalSecret {
                id: principal.principal,
                token_hash,
                fs_allowlist: allowlist,
                host_project_dirs: project_dirs,
                max_trust_profile: principal.max_trust_profile,
                requests_per_minute: principal.requests_per_minute,
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
            audit_path: None,
            audit_failure_mode: AuditFailureMode::FailClosed,
            backend: GatewayBackend::Bwrap,
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

    /// Set an explicit gateway audit log path.
    pub fn with_audit_path(mut self, path: PathBuf) -> Self {
        self.audit_path = Some(path);
        self
    }

    /// Set whether failed audit writes reject requests or continue with a warning.
    pub fn with_audit_failure_mode(mut self, mode: AuditFailureMode) -> Self {
        self.audit_failure_mode = mode;
        self
    }

    /// Select an explicit execution backend for all sandboxes.
    pub fn with_backend(mut self, backend: GatewayBackend) -> Self {
        self.backend = backend;
        self
    }

    pub fn backend(&self) -> &GatewayBackend {
        &self.backend
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

    pub(crate) fn validate(&self) -> anyhow::Result<()> {
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
        if let Some(audit_path) = &self.audit_path {
            if !audit_path.is_absolute() {
                anyhow::bail!("audit_path must be absolute");
            }
            let parent = audit_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("audit_path must have a parent directory"))?;
            anyhow::ensure!(
                parent.is_dir(),
                "audit_path parent directory {parent:?} must exist"
            );
        }
        match &self.backend {
            GatewayBackend::Bwrap => {}
            #[cfg(target_os = "macos")]
            GatewayBackend::Seatbelt => {
                if !self.work_root.is_some() {
                    anyhow::bail!("seatbelt backend requires an explicit work root");
                }
            }
            GatewayBackend::Firecracker(config) => {
                validate_firecracker_backend(config, self.max_sandboxes)?
            }
        }
        Ok(())
    }

    pub(crate) fn credential_seeds(&self) -> Vec<(String, [u8; 32])> {
        self.principals
            .iter()
            .map(|principal| (principal.id.clone(), principal.token_hash))
            .collect()
    }

    pub(crate) fn principal(&self, principal_id: &str) -> Option<AuthenticatedPrincipal> {
        self.principals
            .iter()
            .find(|principal| principal.id == principal_id)
            .map(|principal| AuthenticatedPrincipal {
                id: Arc::from(principal.id.as_str()),
                fs_allowlist: Arc::from(principal.fs_allowlist.as_slice()),
                host_project_dirs: Arc::from(principal.host_project_dirs.as_slice()),
                max_trust_profile: principal.max_trust_profile,
                requests_per_minute: principal.requests_per_minute,
            })
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn work_root(&self) -> &std::path::Path {
        self.work_root
            .as_deref()
            .expect("gateway work root is prepared")
    }

    pub(crate) fn audit_path(&self) -> Option<&std::path::Path> {
        self.audit_path.as_deref()
    }

    pub(crate) fn audit_failure_mode(&self) -> AuditFailureMode {
        self.audit_failure_mode
    }
}

fn validate_firecracker_backend(
    config: &FirecrackerBackendConfig,
    max_sandboxes: usize,
) -> anyhow::Result<()> {
    let paths = [
        ("binary", &config.binary_path),
        ("kernel image", &config.kernel_image_path),
        ("base image", &config.base_image_path),
    ];
    for (label, path) in paths {
        if path.as_os_str().is_empty() {
            anyhow::bail!("firecracker {label} path must not be empty");
        }
        if !path.is_absolute() {
            anyhow::bail!("firecracker {label} path must be absolute");
        }
        if path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        }) {
            anyhow::bail!("firecracker {label} path must not contain . or ..");
        }
    }
    for (left_index, (left_label, left_path)) in paths.iter().enumerate() {
        for (right_label, right_path) in paths.iter().skip(left_index + 1) {
            if left_path == right_path {
                anyhow::bail!("firecracker {left_label} and {right_label} paths must differ");
            }
        }
    }

    let binary = std::fs::metadata(&config.binary_path)
        .map_err(|error| anyhow::anyhow!("invalid firecracker binary path: {error}"))?;
    use std::os::unix::fs::PermissionsExt;
    if !binary.is_file() || binary.permissions().mode() & 0o111 == 0 {
        anyhow::bail!("firecracker binary must be an executable regular file");
    }
    for (label, path) in [
        ("kernel image", &config.kernel_image_path),
        ("base image", &config.base_image_path),
    ] {
        let metadata = std::fs::metadata(path)
            .map_err(|error| anyhow::anyhow!("invalid firecracker {label} path: {error}"))?;
        if !metadata.is_file() {
            anyhow::bail!("firecracker {label} must be a regular file");
        }
        if metadata.len() == 0 {
            anyhow::bail!("firecracker {label} must not be empty");
        }
    }

    if config.max_vcpu_count == 0 {
        anyhow::bail!("firecracker max_vcpu_count must be positive");
    }
    if config.max_memory_mib == 0 {
        anyhow::bail!("firecracker max_memory_mib must be positive");
    }
    if let Some(max_disk_mib) = config.max_disk_mib {
        if max_disk_mib == 0 {
            anyhow::bail!("firecracker max_disk_mib must be positive");
        }
        let max_disk_bytes = u64::from(max_disk_mib) * 1024 * 1024;
        let base_image_size = std::fs::metadata(&config.base_image_path)
            .map_err(|error| anyhow::anyhow!("invalid firecracker base image path: {error}"))?
            .len();
        if base_image_size > max_disk_bytes {
            anyhow::bail!(
                "firecracker base image size {base_image_size} exceeds max_disk_mib {max_disk_mib}"
            );
        }
    }
    if config.vsock_guest_cid_base < 3 {
        anyhow::bail!("firecracker vsock_guest_cid_base must be at least 3");
    }
    let cid_capacity = u32::MAX - config.vsock_guest_cid_base + 1;
    if max_sandboxes > usize::try_from(cid_capacity).unwrap_or(usize::MAX) {
        anyhow::bail!("max_sandboxes exceeds vsock CID capacity");
    }
    if config.guest_agent_port == 0 {
        anyhow::bail!("firecracker guest_agent_port must be nonzero");
    }
    if config.readiness_timeout.is_zero() {
        anyhow::bail!("firecracker readiness_timeout must be positive");
    }
    Ok(())
}

fn is_guest_workspace_path(value: &str) -> bool {
    value == "/workspace"
        || (value.starts_with("/workspace/")
            && !value.contains('\0')
            && !value.contains('\\')
            && !value
                .split('/')
                .any(|component| component == "." || component == ".."))
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
            .field(
                "backend",
                &match self.backend {
                    GatewayBackend::Bwrap => "bwrap",
                    #[cfg(target_os = "macos")]
                    GatewayBackend::Seatbelt => "macos-seatbelt",
                    GatewayBackend::Firecracker(_) => "firecracker",
                },
            )
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
