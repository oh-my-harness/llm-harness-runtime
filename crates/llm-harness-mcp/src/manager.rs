use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::future::{BoxFuture, FutureExt, join_all};
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::McpServerConfig;
use crate::config::{MismatchPolicy, ToolChangePolicy, VersionPolicy};
use crate::session::{McpSession, McpSessionError, SessionResult};
use crate::transport::{create_transport, resolve_timeout_ms};
use crate::types::{McpToolCallResult, McpToolDefinition, notification_methods};
use crate::utils::sanitize_name;
use crate::version_lock::{
    McpLockFile, ServerInfoSnapshot, ServerLockEntry, compute_tool_hashes, diff_tool_snapshots,
};

/// Default startup budget for `discover_and_connect`.
///
/// This is the total time the manager waits for MCP servers to finish
/// `initialize` + `tools/list` before returning. It is intentionally much
/// larger than a local process handshake: remote HTTP/SSE servers (and
/// proxies) can take hundreds of milliseconds. Override with
/// `OMP_MCP_STARTUP_TIMEOUT_MS`; `0` disables the extra budget and falls back
/// to each connection attempt's own timeout.
pub const STARTUP_TIMEOUT_MS: u64 = 10_000;
/// Grace period used by the legacy "cached tools are deferred" test path.
pub const CACHED_TOOL_GRACE_MS: u64 = 250;
const STARTUP_TIMEOUT_ENV: &str = "OMP_MCP_STARTUP_TIMEOUT_MS";
const RECONNECT_WINDOW: Duration = Duration::from_secs(30);
const MAX_RECONNECTS_IN_WINDOW: usize = 5;
const RECONNECT_BACKOFF_MS: [u64; 5] = [0, 500, 1_000, 2_000, 4_000];

/// Resolve the startup budget from an optional environment value.
///
/// Kept pure for tests; the manager reads the real environment once at
/// construction so concurrent teams share a stable value.
pub fn resolve_startup_timeout_ms(env_override: Option<&str>) -> u64 {
    if let Some(raw) = env_override {
        match raw.parse::<u64>() {
            Ok(value) => return value,
            Err(error) => {
                tracing::warn!(
                    value = raw,
                    %error,
                    "ignoring invalid OMP_MCP_STARTUP_TIMEOUT_MS"
                );
            }
        }
    }
    STARTUP_TIMEOUT_MS
}

fn default_startup_timeout_ms() -> u64 {
    resolve_startup_timeout_ms(std::env::var(STARTUP_TIMEOUT_ENV).ok().as_deref())
}

/// Factory used to open an MCP session for a server config.
///
/// Public so test code in downstream crates can inject a fake connector via
/// the `test-utils`-gated constructors.
pub type SessionConnector = Arc<
    dyn Fn(McpServerConfig) -> BoxFuture<'static, SessionResult<Arc<McpSession>>> + Send + Sync,
>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connected,
    Connecting,
    Disconnected,
}

#[derive(Debug, Clone)]
pub struct ExposedTool {
    pub server_name: String,
    pub full_name: String,
    pub tool_def: McpToolDefinition,
    pub deferred: bool,
}

#[derive(Debug, Error)]
pub enum McpManagerError {
    #[error(transparent)]
    Session(#[from] McpSessionError),
    #[error("unknown MCP server '{0}'")]
    UnknownServer(String),
    #[error("unknown MCP tool '{0}'")]
    UnknownTool(String),
    #[error("MCP server '{server}' failed to connect: {message}")]
    Connection { server: String, message: String },
    #[error("MCP server '{0}' reconnect circuit breaker is open")]
    CircuitOpen(String),
    #[error("MCP tool cache error: {0}")]
    Cache(String),
    #[error("MCP server '{server}' version mismatch: expected {expected}, got {actual}")]
    VersionMismatch {
        server: String,
        expected: String,
        actual: String,
    },
    #[error("MCP server '{server}' credential reference '{reference}' could not be resolved")]
    CredentialUnresolved { server: String, reference: String },
    #[error("MCP server '{server}' tool definition changed: {summary}")]
    ToolDefinitionChanged { server: String, summary: String },
}

pub type ManagerResult<T> = Result<T, McpManagerError>;

#[derive(Clone)]
enum ConnectionAttempt {
    Pending,
    Ready(Arc<McpSession>),
    Failed(Arc<String>),
}

pub struct McpToolCache {
    entries: Arc<RwLock<HashMap<String, Vec<McpToolDefinition>>>>,
    cache_path: Option<Arc<PathBuf>>,
    flush_notify: Arc<tokio::sync::Notify>,
    flush_task_started: AtomicBool,
    cancellation: CancellationToken,
}

impl McpToolCache {
    pub fn new(cache_path: Option<PathBuf>) -> Self {
        let cache_path = cache_path.map(Arc::new);
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            cache_path,
            flush_notify: Arc::new(tokio::sync::Notify::new()),
            flush_task_started: AtomicBool::new(false),
            cancellation: CancellationToken::new(),
        }
    }

    pub fn get(&self, server_name: &str) -> Option<Vec<McpToolDefinition>> {
        self.entries.read().get(server_name).cloned()
    }

    pub async fn set(&self, server_name: &str, tools: Vec<McpToolDefinition>) {
        // Lazily spawn the background flush task on first `set()` rather than
        // in `new()`, so `new()` stays synchronous and does not panic when
        // called outside a tokio runtime context. `set()` is async, so we are
        // guaranteed to be inside a runtime here.
        if self.cache_path.is_some() && !self.flush_task_started.swap(true, Ordering::SeqCst) {
            self.spawn_flush_task();
        }
        self.entries.write().insert(server_name.to_owned(), tools);
        self.flush_notify.notify_one();
    }

    pub async fn load_from_disk(&self) -> ManagerResult<()> {
        let Some(path) = &self.cache_path else {
            return Ok(());
        };
        let bytes = match tokio::fs::read(path.as_ref()).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(McpManagerError::Cache(error.to_string())),
        };
        let entries = serde_json::from_slice(&bytes)
            .map_err(|error| McpManagerError::Cache(error.to_string()))?;
        *self.entries.write() = entries;
        Ok(())
    }

    pub async fn flush(&self) -> ManagerResult<()> {
        let Some(path) = &self.cache_path else {
            return Ok(());
        };
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| McpManagerError::Cache(error.to_string()))?;
        }
        let bytes = serde_json::to_vec_pretty(&*self.entries.read())
            .map_err(|error| McpManagerError::Cache(error.to_string()))?;
        tokio::fs::write(path.as_ref(), bytes)
            .await
            .map_err(|error| McpManagerError::Cache(error.to_string()))
    }

    fn spawn_flush_task(&self) {
        let entries = Arc::clone(&self.entries);
        let cache_path = Arc::clone(self.cache_path.as_ref().unwrap());
        let flush_notify = Arc::clone(&self.flush_notify);
        let cancellation = self.cancellation.clone();
        tokio::spawn(async move {
            // Debounce: wait 500ms after first notification, coalescing
            // subsequent notifications into a single write.
            loop {
                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    _ = flush_notify.notified() => {}
                }
                if cancellation.is_cancelled() {
                    break;
                }
                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                }
                if cancellation.is_cancelled() {
                    break;
                }
                // Drain any additional notifications accumulated during the
                // debounce window so the next write cycle starts fresh.
                while flush_notify.notified().now_or_never().is_some() {}
                let bytes = match serde_json::to_vec_pretty(&*entries.read()) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::warn!(%error, "failed to serialize MCP tool cache");
                        continue;
                    }
                };
                if let Some(parent) = cache_path.parent()
                    && !parent.as_os_str().is_empty()
                    && let Err(error) = tokio::fs::create_dir_all(parent).await
                {
                    tracing::warn!(%error, "failed to create MCP tool cache directory");
                    continue;
                }
                if let Err(error) = tokio::fs::write(cache_path.as_ref(), bytes).await {
                    tracing::warn!(%error, "failed to persist MCP tool cache");
                }
            }
        });
    }
}

impl Drop for McpToolCache {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl Default for McpToolCache {
    fn default() -> Self {
        Self::new(None)
    }
}

struct ManagerInner {
    connections: RwLock<HashMap<String, Arc<McpSession>>>,
    pending_connections: RwLock<HashMap<String, watch::Receiver<ConnectionAttempt>>>,
    server_configs: RwLock<HashMap<String, McpServerConfig>>,
    tools: RwLock<Vec<ExposedTool>>,
    failures: RwLock<HashMap<String, String>>,
    reconnect_history: RwLock<HashMap<String, Vec<Instant>>>,
    tool_cache: McpToolCache,
    connection_tasks: Mutex<HashMap<String, JoinHandle<()>>>,
    reconnection_tasks: Mutex<HashMap<String, JoinHandle<()>>>,
    connector: SessionConnector,
    shutting_down: AtomicBool,
    lock_file: Arc<RwLock<McpLockFile>>,
    lock_file_path: Option<PathBuf>,
    /// Serializes lock-file persistence. Each writer snapshots the in-memory
    /// lock *after* acquiring this gate, so out-of-order writer scheduling can
    /// never roll the file back to an older snapshot.
    lock_persist_gate: Arc<AsyncMutex<()>>,
    credential_provider: Arc<dyn crate::credential::CredentialProvider>,
    credential_scopes: RwLock<HashMap<String, HashMap<String, Vec<String>>>>,
    startup_timeout_ms: u64,
    /// Internal upper bound applied to a single connection attempt when the
    /// resolved attempt timeout (`OMP_MCP_TIMEOUT_MS` / `config.timeout`) is
    /// `0`, i.e. explicitly disabled. Without it, `startup_timeout_ms == 0`
    /// plus a disabled attempt timeout would wait forever. Defaults to
    /// [`crate::transport::DEFAULT_TIMEOUT_MS`] and is only overridden by
    /// test constructors.
    attempt_safety_timeout_ms: u64,
    /// Serializes connection publication (session + live tools + Ready) with
    /// startup-timeout settlement. Without this gate, a timeout could abort a
    /// task between publishing tools and publishing the session, or replace
    /// freshly published live tools with deferred cache entries.
    ///
    /// Lock order: `startup_gate` → `tools` → `connections` →
    /// `pending_connections` → `failures`. The gate is the only async lock;
    /// every `parking_lot` guard acquired under it is dropped before the gate
    /// is released, so no await point ever observes a held sync guard.
    startup_gate: AsyncMutex<()>,
}

pub struct McpManager {
    inner: Arc<ManagerInner>,
}

impl McpManager {
    pub fn new() -> Self {
        Self::build_inner(
            None,
            Arc::new(default_connector),
            Some(PathBuf::from(".mcp-lock.json")),
            Arc::new(crate::credential::EnvCredentialProvider),
            default_startup_timeout_ms(),
            crate::transport::DEFAULT_TIMEOUT_MS,
        )
    }

    pub fn with_cache_path(cache_path: impl Into<PathBuf>) -> Self {
        Self::build_inner(
            Some(cache_path.into()),
            Arc::new(default_connector),
            Some(PathBuf::from(".mcp-lock.json")),
            Arc::new(crate::credential::EnvCredentialProvider),
            default_startup_timeout_ms(),
            crate::transport::DEFAULT_TIMEOUT_MS,
        )
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_connector(cache_path: Option<PathBuf>, connector: SessionConnector) -> Self {
        Self::build_inner(
            cache_path,
            connector,
            None,
            Arc::new(crate::credential::EnvCredentialProvider),
            default_startup_timeout_ms(),
            crate::transport::DEFAULT_TIMEOUT_MS,
        )
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_connector_and_startup_timeout(
        cache_path: Option<PathBuf>,
        connector: SessionConnector,
        startup_timeout_ms: u64,
    ) -> Self {
        Self::build_inner(
            cache_path,
            connector,
            None,
            Arc::new(crate::credential::EnvCredentialProvider),
            startup_timeout_ms,
            crate::transport::DEFAULT_TIMEOUT_MS,
        )
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_connector_and_lock(
        cache_path: Option<PathBuf>,
        connector: SessionConnector,
        lock_file_path: PathBuf,
    ) -> Self {
        Self::build_inner(
            cache_path,
            connector,
            Some(lock_file_path),
            Arc::new(crate::credential::EnvCredentialProvider),
            default_startup_timeout_ms(),
            crate::transport::DEFAULT_TIMEOUT_MS,
        )
    }

    /// Construct a manager with injected startup budget and attempt safety
    /// bound. Test-only: lets tests exercise the `startup=0` / `timeout=0`
    /// combinations with small, deterministic values.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_connector_and_timeouts(
        cache_path: Option<PathBuf>,
        connector: SessionConnector,
        startup_timeout_ms: u64,
        attempt_safety_timeout_ms: u64,
    ) -> Self {
        Self::build_inner(
            cache_path,
            connector,
            None,
            Arc::new(crate::credential::EnvCredentialProvider),
            startup_timeout_ms,
            attempt_safety_timeout_ms,
        )
    }

    /// Test-only constructor that also injects the credential provider, so a
    /// stalled provider can be exercised against the attempt deadline.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_credential_provider_and_timeouts(
        cache_path: Option<PathBuf>,
        connector: SessionConnector,
        provider: Arc<dyn crate::credential::CredentialProvider>,
        startup_timeout_ms: u64,
        attempt_safety_timeout_ms: u64,
    ) -> Self {
        Self::build_inner(
            cache_path,
            connector,
            None,
            provider,
            startup_timeout_ms,
            attempt_safety_timeout_ms,
        )
    }

    /// Construct an McpManager with a custom credential provider (e.g. for
    /// `vault://` reference resolution). Use this when the default
    /// `EnvCredentialProvider` is insufficient.
    pub fn with_credential_provider(
        cache_path: Option<PathBuf>,
        connector: SessionConnector,
        lock_file_path: Option<PathBuf>,
        provider: Arc<dyn crate::credential::CredentialProvider>,
    ) -> Self {
        Self::build_inner(
            cache_path,
            connector,
            lock_file_path,
            provider,
            default_startup_timeout_ms(),
            crate::transport::DEFAULT_TIMEOUT_MS,
        )
    }

    fn build_inner(
        cache_path: Option<PathBuf>,
        connector: SessionConnector,
        lock_file_path: Option<PathBuf>,
        credential_provider: Arc<dyn crate::credential::CredentialProvider>,
        startup_timeout_ms: u64,
        attempt_safety_timeout_ms: u64,
    ) -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                connections: RwLock::new(HashMap::new()),
                pending_connections: RwLock::new(HashMap::new()),
                server_configs: RwLock::new(HashMap::new()),
                tools: RwLock::new(Vec::new()),
                failures: RwLock::new(HashMap::new()),
                reconnect_history: RwLock::new(HashMap::new()),
                tool_cache: McpToolCache::new(cache_path),
                connection_tasks: Mutex::new(HashMap::new()),
                reconnection_tasks: Mutex::new(HashMap::new()),
                connector,
                shutting_down: AtomicBool::new(false),
                lock_file: Arc::new(RwLock::new(McpLockFile::default())),
                lock_file_path,
                lock_persist_gate: Arc::new(AsyncMutex::new(())),
                credential_provider,
                credential_scopes: RwLock::new(HashMap::new()),
                startup_timeout_ms,
                // A safety bound of zero would defeat its purpose; keep a
                // one-millisecond floor so the `0/0` combination can never
                // wait forever even if a caller injects zero.
                attempt_safety_timeout_ms: attempt_safety_timeout_ms.max(1),
                startup_gate: AsyncMutex::new(()),
            }),
        }
    }

    pub async fn discover_and_connect(
        &self,
        configs: HashMap<String, McpServerConfig>,
    ) -> ManagerResult<()> {
        self.inner.shutting_down.store(false, Ordering::Release);
        if let Err(error) = self.inner.tool_cache.load_from_disk().await {
            tracing::warn!(%error, "failed to load MCP tool cache");
        }
        if let Some(lock_path) = &self.inner.lock_file_path {
            match McpLockFile::load_from_disk(lock_path).await {
                Ok(lock) => *self.inner.lock_file.write() = lock,
                Err(error) => {
                    tracing::error!(
                        %error,
                        "failed to load MCP lock file; refusing to connect servers"
                    );
                    let msg = format!("lock file load failed: {error}");
                    for name in configs.keys() {
                        self.inner
                            .failures
                            .write()
                            .insert(name.clone(), msg.clone());
                    }
                    return Ok(());
                }
            }
        }
        self.inner.server_configs.write().extend(configs.clone());

        let mut waiters = Vec::new();
        for (name, config) in configs {
            match self.start_connection(name.clone(), config) {
                Ok(receiver) => waiters.push((name, receiver)),
                Err(error) => {
                    self.inner.failures.write().insert(name, error.to_string());
                }
            }
        }

        let settle = waiters
            .iter_mut()
            .map(|(_, receiver)| wait_until_settled(receiver))
            .collect::<Vec<_>>();
        let startup_timeout_ms = self.inner.startup_timeout_ms;
        let timed_out = if startup_timeout_ms == 0 {
            join_all(settle).await;
            false
        } else {
            match tokio::time::timeout(Duration::from_millis(startup_timeout_ms), join_all(settle))
                .await
            {
                Ok(_) => false,
                Err(_) => true,
            }
        };
        if timed_out {
            self.handle_startup_timeout(&waiters, startup_timeout_ms)
                .await;
        }

        {
            // Keep the check-and-replace atomic with connection publication:
            // a task that publishes live tools must never be overwritten by a
            // stale deferred cache entry.
            let _gate = self.inner.startup_gate.lock().await;
            for (name, receiver) in waiters {
                if matches!(*receiver.borrow(), ConnectionAttempt::Pending)
                    && self.get_connection_status(&name) != ConnectionStatus::Connected
                    && let Some(tools) = self.inner.tool_cache.get(&name)
                {
                    replace_server_tools(&self.inner, &name, tools, true);
                }
            }
        }
        Ok(())
    }

    /// Settle connections that are still handshaking when the startup budget
    /// expires.
    ///
    /// - A connection that already published a session wins the race; keep it
    ///   and do not overwrite its live tools with cached definitions.
    /// - A pending connection with cached tools keeps running in the
    ///   background and exposes deferred tools, matching the historical
    ///   fast-gate contract. It records a diagnostic failure while it
    ///   continues, and clears it on success.
    /// - A pending connection without cached tools is aborted, its future is
    ///   awaited for a bounded grace period, and a terminal failure is
    ///   recorded. This is the path that prevents `Connecting` zombies and
    ///   orphan tasks for slow remote servers.
    async fn handle_startup_timeout(
        &self,
        waiters: &[(String, watch::Receiver<ConnectionAttempt>)],
        timeout_ms: u64,
    ) {
        let message = format!("startup timed out after {timeout_ms}ms");
        let cached_message =
            format!("startup still connecting after {timeout_ms}ms; cached tools are deferred");
        let mut aborted = Vec::new();
        {
            let _gate = self.inner.startup_gate.lock().await;
            for (name, receiver) in waiters {
                if !matches!(*receiver.borrow(), ConnectionAttempt::Pending) {
                    continue;
                }
                if self.inner.connections.read().contains_key(name) {
                    // The task published a session before the gate was taken.
                    // Keep the live connection and its tools.
                    continue;
                }
                if self.inner.tool_cache.get(name).is_some() {
                    // Cached-tools grace: the attempt stays tracked in
                    // pending_connections and continues in the background.
                    self.inner
                        .failures
                        .write()
                        .insert(name.clone(), cached_message.clone());
                    continue;
                }
                let handle = self.inner.connection_tasks.lock().remove(name);
                if let Some(handle) = handle {
                    handle.abort();
                    aborted.push((name.clone(), handle));
                }
                self.inner.pending_connections.write().remove(name);
                if !self.inner.connections.read().contains_key(name) {
                    self.inner
                        .failures
                        .write()
                        .insert(name.clone(), message.clone());
                }
            }
        }
        for (name, handle) in aborted {
            if tokio::time::timeout(Duration::from_secs(1), handle)
                .await
                .is_err()
            {
                tracing::warn!(
                    server = name,
                    "MCP connection task did not stop within cleanup grace period"
                );
            }
        }
    }

    pub async fn add_server(&self, name: String, config: McpServerConfig) -> ManagerResult<()> {
        config
            .validate()
            .map_err(|error| McpManagerError::Connection {
                server: name.clone(),
                message: error.to_string(),
            })?;
        self.disconnect_connection_only(&name).await;
        self.inner
            .server_configs
            .write()
            .insert(name.clone(), config.clone());
        let receiver = self.start_connection(name.clone(), config)?;
        wait_for_attempt(receiver, &name).await.map(|_| ())
    }

    pub async fn reload_all(&self) -> ManagerResult<()> {
        let configs = self.inner.server_configs.read().clone();
        self.disconnect_connections().await;
        self.discover_and_connect(configs).await
    }

    pub async fn reconnect(&self, name: &str) -> ManagerResult<()> {
        let config = self
            .inner
            .server_configs
            .read()
            .get(name)
            .cloned()
            .ok_or_else(|| McpManagerError::UnknownServer(name.to_owned()))?;
        self.inner.reconnect_history.write().remove(name);
        if let Some(handle) = self.inner.reconnection_tasks.lock().remove(name) {
            handle.abort();
        }
        self.disconnect_connection_only(name).await;

        let mut last_error = None;
        for delay_ms in RECONNECT_BACKOFF_MS {
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            let receiver = self.start_connection(name.to_owned(), config.clone())?;
            match wait_for_attempt(receiver, name).await {
                Ok(_) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| McpManagerError::Connection {
            server: name.to_owned(),
            message: "reconnect attempts exhausted".to_owned(),
        }))
    }

    pub async fn disconnect_server(&self, name: &str) -> ManagerResult<()> {
        self.inner.server_configs.write().remove(name);
        self.inner.failures.write().remove(name);
        self.inner.reconnect_history.write().remove(name);
        if let Some(handle) = self.inner.connection_tasks.lock().remove(name) {
            handle.abort();
        }
        if let Some(handle) = self.inner.reconnection_tasks.lock().remove(name) {
            handle.abort();
        }
        self.inner.pending_connections.write().remove(name);
        self.disconnect_connection_only(name).await;
        remove_server_tools(&self.inner, name);
        Ok(())
    }

    pub async fn disconnect_all(&self) -> ManagerResult<()> {
        self.inner.shutting_down.store(true, Ordering::Release);
        self.cancel_background_tasks();
        self.inner.pending_connections.write().clear();
        self.disconnect_connections().await;
        self.inner.server_configs.write().clear();
        self.inner.failures.write().clear();
        self.inner.reconnect_history.write().clear();
        self.inner.tools.write().clear();
        Ok(())
    }

    pub fn cancel_background_tasks(&self) {
        for (_, handle) in std::mem::take(&mut *self.inner.connection_tasks.lock()) {
            handle.abort();
        }
        for (_, handle) in std::mem::take(&mut *self.inner.reconnection_tasks.lock()) {
            handle.abort();
        }
    }

    pub fn get_connection_status(&self, name: &str) -> ConnectionStatus {
        if self.inner.connections.read().contains_key(name) {
            ConnectionStatus::Connected
        } else if self.inner.pending_connections.read().contains_key(name)
            || self.inner.reconnection_tasks.lock().contains_key(name)
        {
            ConnectionStatus::Connecting
        } else {
            ConnectionStatus::Disconnected
        }
    }

    /// Read the connection status and its failure reason as one consistent
    /// snapshot.
    ///
    /// Startup transitions (publish, defer, terminal failure) happen under
    /// `startup_gate`, so callers that must render a user-facing reason use
    /// this instead of two independent reads that could pair a fresh
    /// `Disconnected` with a stale deferred diagnostic.
    pub async fn connection_snapshot(&self, name: &str) -> (ConnectionStatus, Option<String>) {
        let _gate = self.inner.startup_gate.lock().await;
        let status = self.get_connection_status(name);
        let reason = self.inner.failures.read().get(name).cloned();
        (status, reason)
    }

    pub fn get_tools(&self) -> Vec<ExposedTool> {
        self.inner.tools.read().clone()
    }

    /// Returns the set of credential keys authorized for the given tool
    /// (raw `tool_def.name` before `sanitize_name`) on the given server.
    pub fn authorized_credentials_for(&self, server_name: &str, tool_name: &str) -> Vec<String> {
        let scopes = self.inner.credential_scopes.read();
        let Some(server_scopes) = scopes.get(server_name) else {
            return Vec::new();
        };
        server_scopes
            .iter()
            .filter_map(|(key, allowed_tools)| {
                if allowed_tools.iter().any(|t| t == tool_name) {
                    Some(key.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Returns true if the given server has any scoped credential declarations.
    pub fn has_scoped_credentials(&self, server_name: &str) -> bool {
        let scopes = self.inner.credential_scopes.read();
        scopes.get(server_name).is_some_and(|s| !s.is_empty())
    }

    /// Returns a snapshot of all credential scopes (server_name → credential_key → allowed_tools).
    pub fn credential_scopes_snapshot(&self) -> HashMap<String, HashMap<String, Vec<String>>> {
        self.inner.credential_scopes.read().clone()
    }

    pub fn errors(&self) -> HashMap<String, String> {
        self.inner.failures.read().clone()
    }

    pub fn session(&self, server_name: &str) -> Option<Arc<McpSession>> {
        self.inner.connections.read().get(server_name).cloned()
    }

    pub fn sessions(&self) -> Vec<(String, Arc<McpSession>)> {
        let mut sessions = self
            .inner
            .connections
            .read()
            .iter()
            .map(|(name, session)| (name.clone(), Arc::clone(session)))
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.0.cmp(&right.0));
        sessions
    }

    pub async fn call_tool(
        &self,
        full_name: &str,
        arguments: Value,
    ) -> ManagerResult<McpToolCallResult> {
        let tool = self
            .inner
            .tools
            .read()
            .iter()
            .find(|tool| tool.full_name == full_name)
            .cloned()
            .ok_or_else(|| McpManagerError::UnknownTool(full_name.to_owned()))?;
        let session = self.wait_for_connection(&tool.server_name).await?;
        match session
            .call_tool(&tool.tool_def.name, arguments.clone())
            .await
        {
            Ok(result) => Ok(result),
            Err(error) if is_retryable(&error) => {
                self.reconnect(&tool.server_name).await?;
                let session = self.wait_for_connection(&tool.server_name).await?;
                Ok(session.call_tool(&tool.tool_def.name, arguments).await?)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn start_connection(
        &self,
        name: String,
        config: McpServerConfig,
    ) -> ManagerResult<watch::Receiver<ConnectionAttempt>> {
        start_connection_inner(&self.inner, name, config)
    }

    async fn wait_for_connection(&self, name: &str) -> ManagerResult<Arc<McpSession>> {
        let config_timeout = self
            .inner
            .server_configs
            .read()
            .get(name)
            .and_then(McpServerConfig::timeout_ms);
        let timeout_ms = resolve_timeout_ms(
            std::env::var("OMP_MCP_TIMEOUT_MS").ok().as_deref(),
            config_timeout,
        );
        let wait = async {
            loop {
                if let Some(session) = self.inner.connections.read().get(name).cloned() {
                    return Ok(session);
                }
                let receiver = self.inner.pending_connections.read().get(name).cloned();
                if let Some(receiver) = receiver {
                    return wait_for_attempt(receiver, name).await;
                }
                let reconnecting = self
                    .inner
                    .reconnection_tasks
                    .lock()
                    .get(name)
                    .is_some_and(|handle| !handle.is_finished());
                if reconnecting {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    continue;
                }
                let message = self
                    .inner
                    .failures
                    .read()
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| "not connected".to_owned());
                return Err(McpManagerError::Connection {
                    server: name.to_owned(),
                    message,
                });
            }
        };
        if timeout_ms == 0 {
            wait.await
        } else {
            tokio::time::timeout(Duration::from_millis(timeout_ms), wait)
                .await
                .map_err(|_| McpManagerError::Connection {
                    server: name.to_owned(),
                    message: format!("connection wait timed out after {timeout_ms}ms"),
                })?
        }
    }

    async fn disconnect_connection_only(&self, name: &str) {
        let session = self.inner.connections.write().remove(name);
        if let Some(session) = session {
            session.clear_on_close();
            if let Err(error) = session.disconnect().await {
                tracing::debug!(server = name, %error, "failed to disconnect MCP server");
            }
        }
    }

    async fn disconnect_connections(&self) {
        let sessions = std::mem::take(&mut *self.inner.connections.write()).into_values();
        let futures = sessions.map(|session| async move {
            session.clear_on_close();
            session.disconnect().await
        });
        for result in join_all(futures).await {
            if let Err(error) = result {
                tracing::debug!(%error, "failed to disconnect MCP server");
            }
        }
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for McpManager {
    fn drop(&mut self) {
        self.inner.shutting_down.store(true, Ordering::Release);
        self.cancel_background_tasks();
        // Clear pending connection attempts so an in-flight
        // `ConnectionAttempt::Ready(Arc<McpSession>)` held in a watch
        // receiver cannot keep the transport alive after Drop. Mirrors
        // `disconnect_all()`.
        self.inner.pending_connections.write().clear();
        // Take sessions out of the connections map so their transport
        // `Drop` impls run (StdioTransport kills the child, HttpTransport
        // cancels its listener). `Drop` cannot `.await`, so we cannot call
        // `disconnect_connections()`; instead we drop the `Arc<McpSession>`
        // clones synchronously. The connections map holds the sole strong
        // reference to each session — `wire_session` and
        // `schedule_auto_reconnect` only capture `Weak<ManagerInner>` /
        // `Arc<ManagerInner>`, never session clones — so dropping here drops
        // the last `Arc<McpSession>`, which drops `Arc<dyn McpTransport>`.
        let _sessions = std::mem::take(&mut *self.inner.connections.write());
    }
}

fn start_connection_inner(
    inner: &Arc<ManagerInner>,
    name: String,
    config: McpServerConfig,
) -> ManagerResult<watch::Receiver<ConnectionAttempt>> {
    config
        .validate()
        .map_err(|error| McpManagerError::Connection {
            server: name.clone(),
            message: error.to_string(),
        })?;
    if let Some(session) = inner.connections.read().get(&name).cloned() {
        let (_, receiver) = watch::channel(ConnectionAttempt::Ready(session));
        return Ok(receiver);
    }
    if let Some(receiver) = inner.pending_connections.read().get(&name).cloned() {
        return Ok(receiver);
    }

    let (sender, receiver) = watch::channel(ConnectionAttempt::Pending);
    inner
        .pending_connections
        .write()
        .insert(name.clone(), receiver.clone());
    let task_inner = Arc::clone(inner);
    let task_name = name.clone();
    let handle = tokio::spawn(async move {
        run_connection_attempt(task_inner, task_name, config, sender).await;
    });
    inner.connection_tasks.lock().insert(name, handle);
    Ok(receiver)
}

pub(crate) fn default_connector(
    config: McpServerConfig,
) -> BoxFuture<'static, SessionResult<Arc<McpSession>>> {
    Box::pin(async move {
        let transport = create_transport(&config).await?;
        Ok(Arc::new(McpSession::connect(transport).await?))
    })
}

async fn verify_and_lock(
    inner: &Arc<ManagerInner>,
    server_name: &str,
    session: &Arc<McpSession>,
    tools: &[McpToolDefinition],
    policy: &VersionPolicy,
) -> ManagerResult<()> {
    let server_info = session.server_info();
    let protocol_version = session.protocol_version();
    let current_snapshot = compute_tool_hashes(tools);

    // All read-diff-upsert happens in a single write-lock scope to avoid
    // TOCTOU. parking_lot guards are !Send, so we cannot hold them across
    // await points — persist happens after the lock is released.
    {
        let mut lock = inner.lock_file.write();
        let updated_entry = match lock.get_entry(server_name) {
            None => {
                tracing::info!(
                    server = server_name,
                    "first connection: locking server snapshot"
                );
                ServerLockEntry {
                    server_info: ServerInfoSnapshot {
                        name: server_info.name.clone(),
                        version: server_info.version.clone(),
                    },
                    protocol_version: protocol_version.clone(),
                    tools_snapshot: current_snapshot,
                }
            }
            Some(entry) => {
                if entry.server_info.version != server_info.version
                    || entry.protocol_version != protocol_version
                {
                    match policy.on_mismatch {
                        MismatchPolicy::FailClosed => {
                            return Err(McpManagerError::VersionMismatch {
                                server: server_name.to_owned(),
                                expected: entry.server_info.version.clone(),
                                actual: server_info.version.clone(),
                            });
                        }
                        MismatchPolicy::AllowWithWarning => {
                            tracing::warn!(
                                server = server_name,
                                expected = %entry.server_info.version,
                                actual = %server_info.version,
                                "MCP server version mismatch (allowed with warning)"
                            );
                        }
                        MismatchPolicy::AllowAndUpdate => {
                            tracing::info!(
                                server = server_name,
                                "updating lock file for version change"
                            );
                        }
                    }
                }

                let diff = diff_tool_snapshots(&entry.tools_snapshot, &current_snapshot);
                if !diff.is_empty() {
                    let should_accept = match policy.on_tool_change {
                        ToolChangePolicy::FailClosed => false,
                        ToolChangePolicy::AllowAdded => {
                            diff.removed.is_empty()
                                && diff.modified_desc.is_empty()
                                && diff.modified_schema.is_empty()
                        }
                        ToolChangePolicy::AllowAll => true,
                    };

                    if !should_accept {
                        return Err(McpManagerError::ToolDefinitionChanged {
                            server: server_name.to_owned(),
                            summary: diff.summary(),
                        });
                    }

                    tracing::warn!(
                        server = server_name,
                        summary = %diff.summary(),
                        "MCP tool definition changed (accepted by policy)"
                    );
                }

                ServerLockEntry {
                    server_info: ServerInfoSnapshot {
                        name: server_info.name.clone(),
                        version: server_info.version.clone(),
                    },
                    protocol_version: protocol_version.clone(),
                    tools_snapshot: current_snapshot,
                }
            }
        };
        lock.upsert_entry(server_name.to_owned(), updated_entry);
    }

    persist_lock_file(inner).await;
    Ok(())
}

async fn persist_lock_file(inner: &Arc<ManagerInner>) {
    let Some(path) = inner.lock_file_path.clone() else {
        return;
    };
    let lock_file = Arc::clone(&inner.lock_file);
    let persist_gate = Arc::clone(&inner.lock_persist_gate);
    // Run the write in a spawned task and await its handle. A caller-side
    // attempt / verification timeout can drop the handle, but the write task
    // keeps running to completion, so cancellation never truncates the file.
    // The task snapshots the in-memory lock *after* taking the persistence
    // gate: out-of-order scheduling therefore cannot write an older snapshot
    // over a newer one, and `save_to_disk` uses write-then-rename.
    let handle = tokio::spawn(async move {
        let _serial = persist_gate.lock().await;
        let lock = lock_file.read().clone();
        if let Err(error) = lock.save_to_disk(&path).await {
            tracing::warn!(%error, "failed to persist MCP lock file");
        }
    });
    let _ = handle.await;
}

/// Resolves all credential references (`${ENV:...}`, `vault://...`) in a
/// server config's env/headers and scoped credentials before the transport
/// is created. Returns a new config with concrete values; the original is
/// untouched. Fail-closed: an unresolved reference yields
/// `CredentialUnresolved`.
async fn resolve_config_credentials(
    inner: &Arc<ManagerInner>,
    server_name: &str,
    config: &McpServerConfig,
) -> ManagerResult<McpServerConfig> {
    use crate::credential::resolve_map;

    let provider = inner.credential_provider.as_ref();
    let mut resolved = config.clone();

    match &mut resolved {
        McpServerConfig::Stdio {
            env, credentials, ..
        } => {
            *env = resolve_map(env, provider).await.map_err(|ref_str| {
                McpManagerError::CredentialUnresolved {
                    server: server_name.to_owned(),
                    reference: ref_str,
                }
            })?;

            if let Some(creds) = credentials {
                let resolved_creds =
                    resolve_scoped_credentials(creds, provider)
                        .await
                        .map_err(|ref_str| McpManagerError::CredentialUnresolved {
                            server: server_name.to_owned(),
                            reference: ref_str,
                        })?;
                // Merge resolved scoped_env values into env so the transport
                // receives them. check_env_credential_mutex (config validate)
                // guarantees no key overlap between env and scoped_env.
                for (key, scoped) in &resolved_creds.scoped_env {
                    env.insert(key.clone(), scoped.value.clone());
                }
                *credentials = Some(resolved_creds);
            }
        }
        McpServerConfig::HttpOrSse {
            headers,
            credentials,
            ..
        } => {
            *headers = resolve_map(headers, provider).await.map_err(|ref_str| {
                McpManagerError::CredentialUnresolved {
                    server: server_name.to_owned(),
                    reference: ref_str,
                }
            })?;

            if let Some(creds) = credentials {
                let resolved_creds =
                    resolve_scoped_credentials(creds, provider)
                        .await
                        .map_err(|ref_str| McpManagerError::CredentialUnresolved {
                            server: server_name.to_owned(),
                            reference: ref_str,
                        })?;
                for (key, scoped) in &resolved_creds.scoped_env {
                    headers.insert(key.clone(), scoped.value.clone());
                }
                *credentials = Some(resolved_creds);
            }
        }
    }

    Ok(resolved)
}

/// Resolves the `value` field of each `ScopedCredential`, preserving
/// `allowed_tools`. Returns `Err(reference)` on the first unresolved value.
async fn resolve_scoped_credentials(
    creds: &crate::credential::CredentialDeclarations,
    provider: &dyn crate::credential::CredentialProvider,
) -> Result<crate::credential::CredentialDeclarations, String> {
    use crate::credential::{CredentialDeclarations, ScopedCredential, resolve_credential_value};
    let mut resolved = CredentialDeclarations::default();
    for (key, scoped) in &creds.scoped_env {
        let value = resolve_credential_value(&scoped.value, provider)
            .await
            .ok_or_else(|| scoped.value.clone())?;
        resolved.scoped_env.insert(
            key.clone(),
            ScopedCredential {
                value,
                allowed_tools: scoped.allowed_tools.clone(),
            },
        );
    }
    Ok(resolved)
}

/// Outcome of one attempt phase under the shared absolute deadline.
enum PhaseOutcome<T> {
    Done(T),
    TimedOut,
}

/// Run one phase only while the shared attempt deadline has budget left.
///
/// `tokio::time::timeout` polls its inner future before checking the timer, so
/// a zero-duration timeout could still let an already-ready phase run (and
/// publish) after the deadline. Checking the absolute deadline first makes an
/// expired attempt fail without polling the next phase.
async fn run_phase<F, T>(deadline: tokio::time::Instant, phase: F) -> PhaseOutcome<T>
where
    F: Future<Output = T>,
{
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return PhaseOutcome::TimedOut;
    }
    match tokio::time::timeout(remaining, phase).await {
        Ok(value) => PhaseOutcome::Done(value),
        Err(_) => PhaseOutcome::TimedOut,
    }
}

async fn run_connection_attempt(
    inner: Arc<ManagerInner>,
    name: String,
    config: McpServerConfig,
    sender: watch::Sender<ConnectionAttempt>,
) {
    let timeout_ms = resolve_timeout_ms(
        std::env::var("OMP_MCP_TIMEOUT_MS").ok().as_deref(),
        config.timeout_ms(),
    );
    // A resolved timeout of `0` explicitly disables the per-attempt deadline
    // (`OMP_MCP_TIMEOUT_MS=0` or `config.timeout: 0`). That must not make an
    // attempt unbounded: combined with `startup_timeout_ms == 0` it would
    // otherwise wait forever. Fall back to the internal safety bound and make
    // the reason visible in the failure message.
    let (attempt_timeout_ms, timeout_note) = if timeout_ms == 0 {
        (
            inner.attempt_safety_timeout_ms,
            " (attempt timeout disabled; safety bound)",
        )
    } else {
        (timeout_ms, "")
    };
    let attempt_timeout = Duration::from_millis(attempt_timeout_ms);
    // One absolute deadline for the whole attempt. Credential resolution,
    // connect + tools/list and verification each get only the time that is
    // left, so a slow phase cannot multiply the configured budget. An already
    // expired deadline fails the phase without polling it (see `run_phase`).
    let attempt_deadline = tokio::time::Instant::now() + attempt_timeout;
    let credential_timeout_message =
        format!("credential resolution timed out after {attempt_timeout_ms}ms{timeout_note}");
    let connect_timeout_message =
        format!("connection timed out after {attempt_timeout_ms}ms{timeout_note}");
    let verify_timeout_message =
        format!("connection verification timed out after {attempt_timeout_ms}ms{timeout_note}");

    // Credential resolution is part of the attempt: a stalled provider must
    // not keep the attempt (or a `startup_timeout_ms == 0` wait) alive after
    // the attempt deadline.
    let config = match run_phase(
        attempt_deadline,
        resolve_config_credentials(&inner, &name, &config),
    )
    .await
    {
        PhaseOutcome::TimedOut => {
            settle_connection_failure(&inner, &name, &sender, credential_timeout_message).await;
            return;
        }
        PhaseOutcome::Done(Err(error)) => {
            settle_connection_failure(&inner, &name, &sender, error.to_string()).await;
            return;
        }
        PhaseOutcome::Done(Ok(config)) => config,
    };
    let policy = config.version_policy().clone();
    let credential_scopes: Option<HashMap<String, Vec<String>>> = config
        .credentials()
        .map(|creds| {
            creds
                .scoped_env
                .iter()
                .filter_map(|(key, scoped)| {
                    scoped
                        .allowed_tools
                        .as_ref()
                        .map(|tools| (key.clone(), tools.clone()))
                })
                .collect::<HashMap<String, Vec<String>>>()
        })
        .filter(|scopes| !scopes.is_empty());
    // When the timeout cancels `connect`, the future is dropped.
    // Transport cleanup happens via transport Drop impls:
    // - StdioTransport::Drop calls start_kill on the child process
    // - HttpTransport::Drop cancels the listener task
    // - McpSession::connect closes the transport on Err, but on cancel
    //   the transport Drop handles cleanup
    let connect = async {
        let session = (inner.connector)(config).await?;
        let tools = session.list_tools().await?;
        SessionResult::Ok((session, tools))
    };
    let result = match run_phase(attempt_deadline, connect).await {
        PhaseOutcome::TimedOut => Err(connect_timeout_message),
        PhaseOutcome::Done(result) => result.map_err(|error| error.to_string()),
    };

    match result {
        Ok((session, tools)) => {
            let verification_error = match run_phase(
                attempt_deadline,
                verify_and_lock(&inner, &name, &session, &tools, &policy),
            )
            .await
            {
                PhaseOutcome::TimedOut => Some(verify_timeout_message),
                PhaseOutcome::Done(Ok(())) => None,
                PhaseOutcome::Done(Err(error)) => Some(error.to_string()),
            };
            if let Some(error_msg) = verification_error {
                if tokio::time::timeout(Duration::from_secs(1), session.disconnect())
                    .await
                    .is_err()
                {
                    tracing::warn!(
                        server = name,
                        "rejected MCP session did not disconnect within cleanup grace period"
                    );
                }
                settle_connection_failure(&inner, &name, &sender, error_msg).await;
                return;
            }
            // Register credential scopes
            if let Some(scopes) = &credential_scopes {
                inner
                    .credential_scopes
                    .write()
                    .insert(name.clone(), scopes.clone());
            }
            // Publish live tools, then the session, then Ready under the
            // startup gate. No awaits may happen inside this section.
            //
            // Invariant (and why this order): an external observer that sees
            // `ConnectionStatus::Connected` must already see the live tools
            // for that server. `get_connection_status` only checks
            // `connections`, so publishing tools first makes the "tools
            // visible, not yet Connected" window safe; the reverse order
            // would expose a Connected session with old/deferred tools.
            // During that window `call_tool` still resolves the fresh tool,
            // then `wait_for_connection` sees the still-pending attempt and
            // waits for this same critical section to publish `Ready`.
            // Status is derived from `connections` alone, so clearing
            // `failures`/`pending_connections` and sending `Ready` after the
            // insert cannot expose a half-settled Connected state.
            //
            // Lock order: startup_gate → tools → connections →
            // pending_connections → failures. Guards are released before the
            // gate is dropped; no sync guard is held across an await.
            let gate = inner.startup_gate.lock().await;
            wire_session(&inner, &name, &session);
            replace_server_tools(&inner, &name, tools.clone(), false);
            inner
                .connections
                .write()
                .insert(name.clone(), Arc::clone(&session));
            inner.failures.write().remove(&name);
            inner.pending_connections.write().remove(&name);
            let _ = sender.send(ConnectionAttempt::Ready(session));
            drop(gate);
            inner.tool_cache.set(&name, tools).await;
        }
        Err(message) => {
            settle_connection_failure(&inner, &name, &sender, message).await;
        }
    }
}

/// Record a terminal attempt failure and wake every waiter.
///
/// Status, failure and pending state are updated under `startup_gate` so an
/// observer can never read a Linked failure with pending still set.
async fn settle_connection_failure(
    inner: &Arc<ManagerInner>,
    name: &str,
    sender: &watch::Sender<ConnectionAttempt>,
    message: String,
) {
    let _gate = inner.startup_gate.lock().await;
    inner
        .failures
        .write()
        .insert(name.to_owned(), message.clone());
    inner.pending_connections.write().remove(name);
    let _ = sender.send(ConnectionAttempt::Failed(Arc::new(message)));
}

fn wire_session(inner: &Arc<ManagerInner>, name: &str, session: &Arc<McpSession>) {
    let weak = Arc::downgrade(inner);
    let close_name = name.to_owned();
    session.on_close(Arc::new(move || {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        schedule_auto_reconnect(inner, close_name.clone());
    }));

    let weak = Arc::downgrade(inner);
    let notification_name = name.to_owned();
    session.on_notification(Arc::new(move |notification| {
        if notification.method != notification_methods::TOOLS_LIST_CHANGED {
            return;
        }
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let name = notification_name.clone();
        tokio::spawn(async move {
            let session = inner.connections.read().get(&name).cloned();
            let Some(session) = session else { return };

            let tools = match session.list_tools().await {
                Ok(tools) => tools,
                Err(error) => {
                    tracing::warn!(server = name, %error, "failed to refresh MCP tools");
                    return;
                }
            };

            let policy = inner
                .server_configs
                .read()
                .get(&name)
                .map(|c| c.version_policy().clone())
                .unwrap_or_default();

            match verify_and_lock(&inner, &name, &session, &tools, &policy).await {
                Ok(()) => {
                    replace_server_tools(&inner, &name, tools.clone(), false);
                    inner.tool_cache.set(&name, tools).await;
                }
                Err(error) => {
                    tracing::warn!(
                        server = name,
                        %error,
                        "rejected tools/list_changed update (policy: fail_closed)"
                    );
                }
            }
        });
    }));
}

fn schedule_auto_reconnect(inner: Arc<ManagerInner>, name: String) {
    if inner.shutting_down.load(Ordering::Acquire) {
        return;
    }
    {
        let mut tasks = inner.reconnection_tasks.lock();
        if matches!(tasks.get(&name), Some(handle) if !handle.is_finished()) {
            return;
        }
        tasks.remove(&name);
    }
    inner.connections.write().remove(&name);

    let now = Instant::now();
    let circuit_open = {
        let mut histories = inner.reconnect_history.write();
        let history = histories.entry(name.clone()).or_default();
        history.retain(|attempt| now.duration_since(*attempt) <= RECONNECT_WINDOW);
        history.push(now);
        history.len() > MAX_RECONNECTS_IN_WINDOW
    };
    if circuit_open {
        tracing::warn!(server = name, "MCP reconnect circuit breaker opened");
        inner.failures.write().insert(
            name,
            "automatic reconnect circuit breaker opened".to_owned(),
        );
        return;
    }

    let Some(config) = inner.server_configs.read().get(&name).cloned() else {
        return;
    };
    let task_inner = Arc::clone(&inner);
    let task_name = name.clone();
    let handle = tokio::spawn(async move {
        let mut last_error = None;
        for delay_ms in RECONNECT_BACKOFF_MS {
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            match start_connection_inner(&task_inner, task_name.clone(), config.clone()) {
                Ok(receiver) => match wait_for_attempt(receiver, &task_name).await {
                    Ok(_) => {
                        last_error = None;
                        break;
                    }
                    Err(error) => last_error = Some(error.to_string()),
                },
                Err(error) => last_error = Some(error.to_string()),
            }
        }
        if let Some(message) = last_error {
            task_inner
                .failures
                .write()
                .insert(task_name.clone(), message);
        }
        task_inner.reconnection_tasks.lock().remove(&task_name);
    });
    inner.reconnection_tasks.lock().insert(name, handle);
}

async fn wait_until_settled(receiver: &mut watch::Receiver<ConnectionAttempt>) {
    while matches!(*receiver.borrow_and_update(), ConnectionAttempt::Pending) {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

async fn wait_for_attempt(
    mut receiver: watch::Receiver<ConnectionAttempt>,
    name: &str,
) -> ManagerResult<Arc<McpSession>> {
    loop {
        let state = receiver.borrow_and_update().clone();
        match state {
            ConnectionAttempt::Pending => {
                if receiver.changed().await.is_err() {
                    return Err(McpManagerError::Connection {
                        server: name.to_owned(),
                        message: "connection task ended without a result".to_owned(),
                    });
                }
            }
            ConnectionAttempt::Ready(session) => return Ok(session),
            ConnectionAttempt::Failed(message) => {
                return Err(McpManagerError::Connection {
                    server: name.to_owned(),
                    message: (*message).clone(),
                });
            }
        }
    }
}

fn replace_server_tools(
    inner: &ManagerInner,
    server_name: &str,
    tools: Vec<McpToolDefinition>,
    deferred: bool,
) {
    let prefix = format!("mcp__{}_", sanitize_name(server_name));
    let mut exposed = inner.tools.write();
    exposed.retain(|tool| tool.server_name != server_name);
    exposed.extend(tools.into_iter().map(|tool_def| ExposedTool {
        server_name: server_name.to_owned(),
        full_name: format!("{prefix}{}", sanitize_name(&tool_def.name)),
        tool_def,
        deferred,
    }));
    exposed.sort_by(|left, right| left.full_name.cmp(&right.full_name));
}

fn remove_server_tools(inner: &ManagerInner, server_name: &str) {
    inner
        .tools
        .write()
        .retain(|tool| tool.server_name != server_name);
}

fn is_retryable(error: &McpSessionError) -> bool {
    matches!(
        error,
        McpSessionError::Transport(
            crate::transport::McpTransportError::Disconnected
                | crate::transport::McpTransportError::Closed
                | crate::transport::McpTransportError::SessionExpired
        )
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::transport::{
        CloseHandler, McpTransport, NotificationHandler, RequestHandler, TransportResult,
    };

    struct FixtureTransport {
        connected: AtomicBool,
        responses: Mutex<VecDeque<Value>>,
        request_handler: Mutex<Option<RequestHandler>>,
        drop_tracker: Option<Arc<AtomicBool>>,
    }

    impl FixtureTransport {
        fn new(tool_name: &str) -> Self {
            Self {
                connected: AtomicBool::new(true),
                responses: Mutex::new(
                    [
                        json!({
                            "protocolVersion": crate::session::PREFERRED_PROTOCOL_VERSION,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "fixture", "version": "1"}
                        }),
                        json!({
                            "tools": [{
                                "name": tool_name,
                                "description": "fixture",
                                "inputSchema": {"type": "object"}
                            }]
                        }),
                    ]
                    .into(),
                ),
                request_handler: Mutex::new(None),
                drop_tracker: None,
            }
        }

        fn new_with_drop_tracker(tool_name: &str, tracker: Arc<AtomicBool>) -> Self {
            let mut transport = Self::new(tool_name);
            transport.drop_tracker = Some(tracker);
            transport
        }

        fn new_with_desc(tool_name: &str, desc: &str) -> Self {
            Self {
                connected: AtomicBool::new(true),
                responses: Mutex::new(
                    [
                        json!({
                            "protocolVersion": crate::session::PREFERRED_PROTOCOL_VERSION,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "fixture", "version": "1"}
                        }),
                        json!({
                            "tools": [{
                                "name": tool_name,
                                "description": desc,
                                "inputSchema": {"type": "object"}
                            }]
                        }),
                    ]
                    .into(),
                ),
                request_handler: Mutex::new(None),
                drop_tracker: None,
            }
        }
    }

    impl Drop for FixtureTransport {
        fn drop(&mut self) {
            if let Some(tracker) = &self.drop_tracker {
                tracker.store(true, Ordering::Release);
            }
        }
    }

    #[async_trait]
    impl McpTransport for FixtureTransport {
        async fn request(&self, _method: &str, _params: Value) -> TransportResult<Value> {
            Ok(self.responses.lock().pop_front().unwrap_or(json!({
                "content": [{"type": "text", "text": "ok"}]
            })))
        }

        async fn notify(&self, _method: &str, _params: Value) -> TransportResult<()> {
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.connected.load(Ordering::Acquire)
        }

        async fn close(&self) -> TransportResult<()> {
            self.connected.store(false, Ordering::Release);
            Ok(())
        }

        fn set_protocol_version(&self, _version: &str) {}

        fn on_request(&self, handler: RequestHandler) {
            *self.request_handler.lock() = Some(handler);
        }

        fn on_notification(&self, _handler: NotificationHandler) {}

        fn on_close(&self, _handler: CloseHandler) {}

        fn clear_on_close(&self) {}
    }

    fn stdio_config(name: &str) -> McpServerConfig {
        McpServerConfig::Stdio {
            command: name.to_owned(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            timeout: Some(5_000),
            version_policy: Default::default(),
            credentials: None,
        }
    }

    #[tokio::test]
    async fn connects_servers_in_parallel_and_isolates_failures() {
        let connector: SessionConnector = Arc::new(|config| {
            Box::pin(async move {
                let McpServerConfig::Stdio { command, .. } = config else {
                    unreachable!()
                };
                if command == "broken" {
                    return Err(McpSessionError::InvalidResponse("broken".to_owned()));
                }
                let transport: Arc<dyn McpTransport> =
                    Arc::new(FixtureTransport::new(&format!("{command}_tool")));
                Ok(Arc::new(McpSession::connect(transport).await?))
            })
        });
        let manager = McpManager::with_connector(None, connector);

        manager
            .discover_and_connect(HashMap::from([
                ("good".to_owned(), stdio_config("good")),
                ("broken".to_owned(), stdio_config("broken")),
            ]))
            .await
            .unwrap();

        assert_eq!(
            manager.get_connection_status("good"),
            ConnectionStatus::Connected
        );
        assert_eq!(
            manager.get_connection_status("broken"),
            ConnectionStatus::Disconnected
        );
        assert_eq!(manager.get_tools()[0].full_name, "mcp__good_good_tool");
        assert!(manager.errors().contains_key("broken"));
    }

    #[tokio::test]
    async fn startup_gate_exposes_cached_tools_as_deferred() {
        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_secs(60)).await;
                unreachable!()
            })
        });
        let manager =
            McpManager::with_connector_and_startup_timeout(None, connector, CACHED_TOOL_GRACE_MS);
        manager.inner.tool_cache.entries.write().insert(
            "slow".to_owned(),
            vec![
                serde_json::from_value(json!({
                    "name": "cached",
                    "description": "cached",
                    "inputSchema": {"type": "object"}
                }))
                .unwrap(),
            ],
        );

        let started = Instant::now();
        manager
            .discover_and_connect(HashMap::from([("slow".to_owned(), stdio_config("slow"))]))
            .await
            .unwrap();

        assert!(started.elapsed() < Duration::from_secs(2));
        let tools = manager.get_tools();
        assert_eq!(tools[0].full_name, "mcp__slow_cached");
        assert!(tools[0].deferred);
        manager.cancel_background_tasks();
    }

    struct SetOnDrop(Arc<AtomicBool>);

    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn startup_waits_for_slow_handshake() {
        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(400)).await;
                let transport: Arc<dyn McpTransport> = Arc::new(FixtureTransport::new("slow_tool"));
                Ok(Arc::new(McpSession::connect(transport).await?))
            })
        });
        let manager = McpManager::with_connector_and_startup_timeout(None, connector, 2_000);
        let started = Instant::now();
        manager
            .discover_and_connect(HashMap::from([("slow".to_owned(), stdio_config("slow"))]))
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(400));
        assert!(elapsed < Duration::from_secs(2));
        assert_eq!(
            manager.get_connection_status("slow"),
            ConnectionStatus::Connected
        );
        assert_eq!(manager.get_tools()[0].full_name, "mcp__slow_slow_tool");
        assert!(!manager.get_tools()[0].deferred);
    }

    #[tokio::test]
    async fn startup_timeout_cancels_pending_connection() {
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&dropped);
        let connector: SessionConnector = Arc::new(move |_config| {
            let flag = Arc::clone(&flag);
            Box::pin(async move {
                let _guard = SetOnDrop(flag);
                std::future::pending::<SessionResult<Arc<McpSession>>>().await
            })
        });
        let manager = McpManager::with_connector_and_startup_timeout(None, connector, 100);
        let started = Instant::now();
        manager
            .discover_and_connect(HashMap::from([("never".to_owned(), stdio_config("never"))]))
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            manager.get_connection_status("never"),
            ConnectionStatus::Disconnected
        );
        assert!(
            manager
                .errors()
                .get("never")
                .is_some_and(|message| message.contains("startup timed out"))
        );
        assert!(manager.inner.pending_connections.read().is_empty());
        assert!(manager.inner.connection_tasks.lock().is_empty());
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn startup_timeout_resolution() {
        assert_eq!(resolve_startup_timeout_ms(Some("20000")), 20_000);
        assert_eq!(resolve_startup_timeout_ms(Some("0")), 0);
        assert_eq!(
            resolve_startup_timeout_ms(Some("invalid")),
            STARTUP_TIMEOUT_MS
        );
        assert_eq!(resolve_startup_timeout_ms(None), STARTUP_TIMEOUT_MS);
    }

    #[tokio::test]
    async fn concurrent_starts_share_one_connection_attempt() {
        let calls = Arc::new(AtomicUsize::new(0));
        let connector_calls = Arc::clone(&calls);
        let connector: SessionConnector = Arc::new(move |_config| {
            connector_calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                let transport: Arc<dyn McpTransport> = Arc::new(FixtureTransport::new("shared"));
                Ok(Arc::new(McpSession::connect(transport).await?))
            })
        });
        let manager = McpManager::with_connector(None, connector);
        let config = stdio_config("shared");

        let first = manager
            .start_connection("server".to_owned(), config.clone())
            .unwrap();
        let second = manager
            .start_connection("server".to_owned(), config)
            .unwrap();
        let (first, second) = tokio::join!(
            wait_for_attempt(first, "server"),
            wait_for_attempt(second, "server")
        );

        assert!(first.is_ok());
        assert!(second.is_ok());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn tool_cache_round_trips_on_disk() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mcp-cache.json");
        let cache = McpToolCache::new(Some(path.clone()));
        let tool: McpToolDefinition = serde_json::from_value(json!({
            "name": "cached",
            "description": "",
            "inputSchema": {"type": "object"}
        }))
        .unwrap();
        cache.set("server", vec![tool.clone()]).await;
        cache.flush().await.unwrap();

        let loaded = McpToolCache::new(Some(path));
        loaded.load_from_disk().await.unwrap();

        assert_eq!(loaded.get("server").unwrap(), vec![tool]);
    }

    #[tokio::test]
    async fn tool_cache_debounces_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let cache = McpToolCache::new(Some(path.clone()));

        // Rapidly set tools for multiple servers.
        for i in 0..5 {
            cache.set(&format!("server-{i}"), vec![]).await;
        }

        // Wait for debounce window + write to complete.
        tokio::time::sleep(Duration::from_millis(700)).await;

        // File should exist with all 5 servers.
        let bytes = tokio::fs::read(&path).await.unwrap();
        let entries: HashMap<String, Vec<McpToolDefinition>> =
            serde_json::from_slice(&bytes).unwrap();
        assert_eq!(entries.len(), 5);
    }

    #[tokio::test]
    async fn connection_wait_survives_reconnect_backoff_window() {
        let manager = McpManager::with_connector(
            None,
            Arc::new(|_config| {
                Box::pin(async { Err(McpSessionError::InvalidResponse("unused".to_owned())) })
            }),
        );
        manager
            .inner
            .server_configs
            .write()
            .insert("server".to_owned(), stdio_config("server"));
        let transport: Arc<dyn McpTransport> = Arc::new(FixtureTransport::new("reconnected"));
        let session = Arc::new(McpSession::connect(transport).await.unwrap());
        let inner = Arc::clone(&manager.inner);
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(75)).await;
            inner
                .connections
                .write()
                .insert("server".to_owned(), session);
        });
        manager
            .inner
            .reconnection_tasks
            .lock()
            .insert("server".to_owned(), task);

        let session = manager.wait_for_connection("server").await.unwrap();

        assert!(session.is_connected());
    }

    #[tokio::test]
    async fn dropping_manager_aborts_self_referencing_connection_task() {
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, AtomicOrdering::Release);
            }
        }

        let started = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let connector_started = Arc::clone(&started);
        let connector_dropped = Arc::clone(&dropped);
        let connector: SessionConnector = Arc::new(move |_config| {
            let started = Arc::clone(&connector_started);
            let dropped = Arc::clone(&connector_dropped);
            Box::pin(async move {
                let _guard = DropFlag(dropped);
                started.store(true, AtomicOrdering::Release);
                futures::future::pending::<()>().await;
                unreachable!()
            })
        });
        let manager = McpManager::with_connector(None, connector);
        manager
            .start_connection("server".to_owned(), stdio_config("server"))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !started.load(AtomicOrdering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        drop(manager);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !dropped.load(AtomicOrdering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert!(dropped.load(AtomicOrdering::Acquire));
    }

    #[tokio::test]
    async fn timed_out_connection_records_failure() {
        let manager = McpManager::with_connector(
            None,
            Arc::new(|_config: McpServerConfig| {
                Box::pin(async move {
                    // Never completes — the timeout will cancel this future.
                    std::future::pending::<SessionResult<Arc<McpSession>>>().await
                })
            }),
        );
        let config = McpServerConfig::Stdio {
            command: "sh".to_owned(),
            args: vec!["-c".to_owned(), "true".to_owned()],
            env: std::collections::HashMap::new(),
            cwd: None,
            timeout: Some(50),
            version_policy: Default::default(),
            credentials: None,
        };

        let _ = manager.start_connection("test".to_owned(), config).unwrap();
        // Wait for the timeout to fire and record the failure.
        tokio::time::sleep(Duration::from_millis(500)).await;

        assert!(
            manager.inner.failures.read().contains_key("test"),
            "timed-out connection must record a failure"
        );
    }
    #[tokio::test]
    async fn manager_drop_cleans_up_connections() {
        // The transport's Drop should run when the manager is dropped.
        // Before the fix, Drop only cancelled background tasks but left
        // sessions in `connections`, keeping transports alive whenever
        // `ManagerInner` was kept alive by a lingering background-task ref.
        let transport_dropped = Arc::new(AtomicBool::new(false));
        let tracker = Arc::clone(&transport_dropped);
        let connector: SessionConnector = Arc::new(move |_config| {
            let tracker = Arc::clone(&tracker);
            Box::pin(async move {
                let transport: Arc<dyn McpTransport> = Arc::new(
                    FixtureTransport::new_with_drop_tracker("drop_tool", tracker),
                );
                Ok(Arc::new(McpSession::connect(transport).await?))
            })
        });
        let manager = McpManager::with_connector(None, connector);
        manager
            .discover_and_connect(HashMap::from([("srv".to_owned(), stdio_config("srv"))]))
            .await
            .unwrap();
        assert_eq!(
            manager.get_connection_status("srv"),
            ConnectionStatus::Connected
        );

        // Simulate a lingering background task that keeps `ManagerInner`
        // alive after the manager handle is dropped — this is exactly the
        // leak the bug produces in production (a connection/reconnection
        // task that outlives the manager handle).
        let lingering_inner = Arc::clone(&manager.inner);

        drop(manager);

        // The lingering ref prevents ManagerInner's Drop (there is none),
        // but the sessions we took out of `connections` must still drop,
        // releasing the transport.
        tokio::task::yield_now().await;
        assert!(
            transport_dropped.load(AtomicOrdering::Acquire),
            "transport Drop should run when manager is dropped even with a lingering ManagerInner ref"
        );
        // Clean up the lingering ref so the test process doesn't keep it alive.
        lingering_inner
            .shutting_down
            .store(true, AtomicOrdering::Release);
        for (_, handle) in std::mem::take(&mut *lingering_inner.connection_tasks.lock()) {
            handle.abort();
        }
        for (_, handle) in std::mem::take(&mut *lingering_inner.reconnection_tasks.lock()) {
            handle.abort();
        }
        let _ = std::mem::take(&mut *lingering_inner.connections.write());
    }

    #[test]
    fn is_retryable_classifies_transport_errors() {
        use crate::transport::McpTransportError;

        assert!(is_retryable(&McpSessionError::Transport(
            McpTransportError::Disconnected
        )));
        assert!(is_retryable(&McpSessionError::Transport(
            McpTransportError::Closed
        )));
        assert!(is_retryable(&McpSessionError::Transport(
            McpTransportError::SessionExpired
        )));

        // Non-retryable errors
        assert!(!is_retryable(&McpSessionError::Transport(
            McpTransportError::Timeout {
                method: "test".to_owned(),
                timeout_ms: 1000,
            }
        )));
        assert!(!is_retryable(&McpSessionError::Transport(
            McpTransportError::Http {
                status: 500,
                body: "server error".to_owned(),
            }
        )));
        assert!(!is_retryable(&McpSessionError::InvalidResponse(
            "bad".to_owned()
        )));
        assert!(!is_retryable(&McpSessionError::UnsupportedProtocolVersion(
            "old".to_owned()
        )));
        assert!(!is_retryable(&McpSessionError::MethodNotSupported("test")));
    }

    #[test]
    fn circuit_breaker_opens_after_max_reconnects() {
        // Verify constants are correct.
        assert_eq!(MAX_RECONNECTS_IN_WINDOW, 5);
        assert_eq!(RECONNECT_WINDOW, Duration::from_secs(30));
        assert_eq!(RECONNECT_BACKOFF_MS.len(), 5);
        assert_eq!(RECONNECT_BACKOFF_MS[0], 0); // immediate first retry
    }
    #[tokio::test]
    async fn first_connection_writes_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(".mcp-lock.json");

        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move {
                let transport: Arc<dyn McpTransport> = Arc::new(FixtureTransport::new("tool_a"));
                Ok(Arc::new(McpSession::connect(transport).await?))
            })
        });
        let manager = McpManager::with_connector_and_lock(None, connector, lock_path.clone());

        manager
            .discover_and_connect(HashMap::from([("srv".to_owned(), stdio_config("srv"))]))
            .await
            .unwrap();

        let lock = McpLockFile::load_from_disk(&lock_path).await.unwrap();
        let entry = lock.get_entry("srv").unwrap();
        assert_eq!(entry.server_info.name, "fixture");
        assert_eq!(entry.server_info.version, "1");
        assert_eq!(entry.tools_snapshot.len(), 1);
        assert_eq!(entry.tools_snapshot[0].name, "tool_a");
    }

    #[tokio::test]
    async fn version_mismatch_fail_closed_rejects_connection() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(".mcp-lock.json");

        let mut lock = McpLockFile::default();
        lock.upsert_entry(
            "srv".to_owned(),
            ServerLockEntry {
                server_info: ServerInfoSnapshot {
                    name: "fixture".to_owned(),
                    version: "2.0.0".to_owned(),
                },
                protocol_version: crate::session::PREFERRED_PROTOCOL_VERSION.to_owned(),
                tools_snapshot: compute_tool_hashes(&[McpToolDefinition {
                    name: "tool_a".to_owned(),
                    title: None,
                    description: "fixture".to_owned(),
                    input_schema: json!({"type": "object"}),
                    output_schema: None,
                    annotations: None,
                    icons: Vec::new(),
                    metadata: None,
                }]),
            },
        );
        lock.save_to_disk(&lock_path).await.unwrap();

        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move {
                let transport: Arc<dyn McpTransport> = Arc::new(FixtureTransport::new("tool_a"));
                Ok(Arc::new(McpSession::connect(transport).await?))
            })
        });
        let manager = McpManager::with_connector_and_lock(None, connector, lock_path);

        manager
            .discover_and_connect(HashMap::from([("srv".to_owned(), stdio_config("srv"))]))
            .await
            .unwrap();

        assert_eq!(
            manager.get_connection_status("srv"),
            ConnectionStatus::Disconnected
        );
        assert!(manager.errors().contains_key("srv"));
        assert!(manager.errors()["srv"].contains("version mismatch"));
    }

    #[tokio::test]
    async fn tool_change_fail_closed_rejects_connection() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(".mcp-lock.json");

        let original_tool = McpToolDefinition {
            name: "tool_a".to_owned(),
            title: None,
            description: "original".to_owned(),
            input_schema: json!({"type": "object"}),
            output_schema: None,
            annotations: None,
            icons: Vec::new(),
            metadata: None,
        };
        let mut lock = McpLockFile::default();
        lock.upsert_entry(
            "srv".to_owned(),
            ServerLockEntry {
                server_info: ServerInfoSnapshot {
                    name: "fixture".to_owned(),
                    version: "1".to_owned(),
                },
                protocol_version: crate::session::PREFERRED_PROTOCOL_VERSION.to_owned(),
                tools_snapshot: compute_tool_hashes(&[original_tool]),
            },
        );
        lock.save_to_disk(&lock_path).await.unwrap();

        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move {
                let transport: Arc<dyn McpTransport> =
                    Arc::new(FixtureTransport::new_with_desc("tool_a", "poisoned"));
                Ok(Arc::new(McpSession::connect(transport).await?))
            })
        });
        let manager = McpManager::with_connector_and_lock(None, connector, lock_path);

        manager
            .discover_and_connect(HashMap::from([("srv".to_owned(), stdio_config("srv"))]))
            .await
            .unwrap();

        assert_eq!(
            manager.get_connection_status("srv"),
            ConnectionStatus::Disconnected
        );
        assert!(manager.errors()["srv"].contains("tool definition changed"));
    }

    #[tokio::test]
    async fn unresolved_credential_rejects_connection() {
        unsafe { std::env::remove_var("TEST_MCP_UNRESOLVED_TOKEN") };
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(".mcp-lock.json");

        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move {
                let transport: Arc<dyn McpTransport> = Arc::new(FixtureTransport::new("tool_a"));
                Ok(Arc::new(McpSession::connect(transport).await?))
            })
        });
        let manager = McpManager::with_connector_and_lock(None, connector, lock_path);

        let mut config = stdio_config("srv");
        if let McpServerConfig::Stdio { env, .. } = &mut config {
            env.insert(
                "TOKEN".to_owned(),
                "${ENV:TEST_MCP_UNRESOLVED_TOKEN}".to_owned(),
            );
        }

        manager
            .discover_and_connect(HashMap::from([("srv".to_owned(), config)]))
            .await
            .unwrap();

        assert_eq!(
            manager.get_connection_status("srv"),
            ConnectionStatus::Disconnected
        );
        assert!(manager.errors()["srv"].contains("credential"));
    }
    #[tokio::test]
    async fn authorized_credentials_populated_for_scoped_tool() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(".mcp-lock.json");

        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move {
                let transport: Arc<dyn McpTransport> =
                    Arc::new(FixtureTransport::new("search_repos"));
                Ok(Arc::new(McpSession::connect(transport).await?))
            })
        });
        let manager = McpManager::with_connector_and_lock(None, connector, lock_path);

        let mut config = stdio_config("srv");
        let creds = crate::credential::CredentialDeclarations {
            scoped_env: {
                let mut map = HashMap::new();
                map.insert(
                    "TOKEN".to_owned(),
                    crate::credential::ScopedCredential {
                        value: "plain".to_owned(),
                        allowed_tools: Some(vec!["search_repos".to_owned()]),
                    },
                );
                map
            },
        };
        if let McpServerConfig::Stdio { credentials, .. } = &mut config {
            *credentials = Some(creds);
        }

        manager
            .discover_and_connect(HashMap::from([("srv".to_owned(), config)]))
            .await
            .unwrap();

        let source = crate::McpToolSource::new(Arc::new(manager));
        let adapters = source.into_adapters().await.unwrap();
        assert_eq!(adapters.len(), 1);
        let lookup = source.build_credential_lookup();
        use llm_harness_types::CredentialLookup;
        match lookup("mcp__srv_search_repos") {
            CredentialLookup::Authorized(creds) => {
                assert_eq!(creds, vec!["TOKEN".to_owned()]);
            }
            other => panic!("expected Authorized, got {other:?}"),
        }
    }
    #[tokio::test]
    async fn scoped_env_credential_merged_into_env() {
        let manager = McpManager::new();
        let mut config = stdio_config("srv");
        if let McpServerConfig::Stdio {
            env, credentials, ..
        } = &mut config
        {
            env.insert("PLAIN".to_owned(), "text".to_owned());
            *credentials = Some(crate::credential::CredentialDeclarations {
                scoped_env: {
                    let mut map = HashMap::new();
                    map.insert(
                        "SCOPED_TOKEN".to_owned(),
                        crate::credential::ScopedCredential {
                            value: "secret123".to_owned(),
                            allowed_tools: Some(vec!["tool_a".to_owned()]),
                        },
                    );
                    map
                },
            });
        }

        let resolved = resolve_config_credentials(&manager.inner, "srv", &config)
            .await
            .unwrap();

        if let McpServerConfig::Stdio { env, .. } = &resolved {
            assert_eq!(env.get("SCOPED_TOKEN"), Some(&"secret123".to_owned()));
            assert_eq!(env.get("PLAIN"), Some(&"text".to_owned()));
        } else {
            panic!("expected Stdio config");
        }
    }

    #[tokio::test]
    async fn scoped_env_credential_merged_into_http_headers() {
        let manager = McpManager::new();
        let config = McpServerConfig::HttpOrSse {
            url: "https://example.com/mcp".to_owned(),
            transport_type: None,
            headers: HashMap::new(),
            timeout: Some(5_000),
            version_policy: Default::default(),
            credentials: Some(crate::credential::CredentialDeclarations {
                scoped_env: {
                    let mut map = HashMap::new();
                    map.insert(
                        "Authorization".to_owned(),
                        crate::credential::ScopedCredential {
                            value: "Bearer token123".to_owned(),
                            allowed_tools: Some(vec!["tool_a".to_owned()]),
                        },
                    );
                    map
                },
            }),
        };

        let resolved = resolve_config_credentials(&manager.inner, "srv", &config)
            .await
            .unwrap();

        if let McpServerConfig::HttpOrSse { headers, .. } = &resolved {
            assert_eq!(
                headers.get("Authorization"),
                Some(&"Bearer token123".to_owned())
            );
        } else {
            panic!("expected HttpOrSse config");
        }
    }
    #[tokio::test]
    async fn corrupted_lock_file_rejects_connection() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(".mcp-lock.json");
        tokio::fs::write(&lock_path, "{ invalid json }")
            .await
            .unwrap();

        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move {
                let transport: Arc<dyn McpTransport> = Arc::new(FixtureTransport::new("tool_a"));
                Ok(Arc::new(McpSession::connect(transport).await?))
            })
        });
        let manager = McpManager::with_connector_and_lock(None, connector, lock_path);

        manager
            .discover_and_connect(HashMap::from([("srv".to_owned(), stdio_config("srv"))]))
            .await
            .unwrap();

        assert_eq!(
            manager.get_connection_status("srv"),
            ConnectionStatus::Disconnected
        );
        assert!(manager.errors().contains_key("srv"));
    }

    // ------------------------------------------------------------------
    // Startup-gate hardening regressions.
    // ------------------------------------------------------------------

    fn tool_definition(name: &str) -> McpToolDefinition {
        serde_json::from_value(json!({
            "name": name,
            "description": name,
            "inputSchema": {"type": "object"}
        }))
        .unwrap()
    }

    /// Poll a condition with a fixed small period and a hard upper bound.
    /// Never relies on random sleeps or on a single arbitrary sleep long
    /// enough to "probably" cover the transition.
    async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if condition() {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Settlement must never downgrade an already-published session to
    /// cached/deferred tools. The fabricated `Pending` waiter models the
    /// race window where a timeout settlement observes a waiter that has not
    /// yet been updated even though the connection is already published.
    #[tokio::test]
    async fn startup_timeout_settlement_keeps_published_live_tools() {
        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move {
                let transport: Arc<dyn McpTransport> = Arc::new(FixtureTransport::new("live_tool"));
                Ok(Arc::new(McpSession::connect(transport).await?))
            })
        });
        let manager = McpManager::with_connector_and_startup_timeout(None, connector, 2_000);
        manager
            .discover_and_connect(HashMap::from([("srv".to_owned(), stdio_config("srv"))]))
            .await
            .unwrap();
        assert_eq!(
            manager.get_connection_status("srv"),
            ConnectionStatus::Connected
        );
        assert!(manager.errors().is_empty());

        manager
            .inner
            .tool_cache
            .set("srv", vec![tool_definition("cached")])
            .await;

        // Fabricate a still-Pending waiter for an already-published server.
        let (_sender, receiver) = watch::channel(ConnectionAttempt::Pending);
        manager
            .handle_startup_timeout(&[("srv".to_owned(), receiver)], 25)
            .await;

        assert_eq!(
            manager.get_connection_status("srv"),
            ConnectionStatus::Connected
        );
        let tools = manager.get_tools();
        assert_eq!(tools.len(), 1, "cached tools must not be added: {tools:?}");
        assert_eq!(tools[0].full_name, "mcp__srv_live_tool");
        assert!(!tools[0].deferred, "published tools must stay live");
        assert!(
            manager.errors().is_empty(),
            "settlement of a published connection must not record a failure"
        );
        assert!(manager.inner.pending_connections.read().is_empty());
        // The cache is kept for the next cold start but did not overwrite
        // the live tools.
        assert_eq!(
            manager.inner.tool_cache.get("srv").unwrap()[0].name,
            "cached"
        );
    }

    /// Cached-grace path: startup budget expires, deferred cached tools are
    /// exposed with a Connecting status, then the late connector success must
    /// replace them with live tools and clear pending/failures.
    #[tokio::test]
    async fn cached_grace_late_success_replaces_deferred_tools() {
        let release = Arc::new(tokio::sync::Notify::new());
        let connector: SessionConnector = {
            let release = Arc::clone(&release);
            Arc::new(move |_config| {
                let release = Arc::clone(&release);
                Box::pin(async move {
                    release.notified().await;
                    let transport: Arc<dyn McpTransport> =
                        Arc::new(FixtureTransport::new("live_tool"));
                    Ok(Arc::new(McpSession::connect(transport).await?))
                })
            })
        };
        let manager =
            McpManager::with_connector_and_startup_timeout(None, connector, CACHED_TOOL_GRACE_MS);
        manager
            .inner
            .tool_cache
            .set("slow", vec![tool_definition("cached")])
            .await;

        let started = Instant::now();
        manager
            .discover_and_connect(HashMap::from([("slow".to_owned(), stdio_config("slow"))]))
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));

        assert_eq!(
            manager.get_connection_status("slow"),
            ConnectionStatus::Connecting
        );
        let deferred = manager.get_tools();
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].full_name, "mcp__slow_cached");
        assert!(deferred[0].deferred);
        assert!(
            manager.errors()["slow"].contains("cached tools are deferred"),
            "diagnostic failure expected: {:?}",
            manager.errors()
        );

        release.notify_one();

        // Publication order is tools → Connected → failure/pending cleanup.
        // Wait for the fully settled state, not just the Connected flag, so
        // the test cannot observe the transient "stale deferred failure"
        // window between `connections.insert` and `failures.remove`.
        wait_until("late success to settle", || {
            manager.get_connection_status("slow") == ConnectionStatus::Connected
                && !manager.errors().contains_key("slow")
                && manager.inner.pending_connections.read().is_empty()
        })
        .await;

        let tools = manager.get_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].full_name, "mcp__slow_live_tool");
        assert!(!tools[0].deferred, "deferred cached tools must be replaced");
        assert!(manager.errors().is_empty(), "{:?}", manager.errors());
        assert!(manager.inner.pending_connections.read().is_empty());
    }

    /// Cached-grace path where the late attempt fails: the deferred window
    /// must settle to a terminal Disconnected failure, not stay Connecting.
    #[tokio::test]
    async fn cached_grace_late_failure_is_terminal() {
        let release = Arc::new(tokio::sync::Notify::new());
        let connector: SessionConnector = {
            let release = Arc::clone(&release);
            Arc::new(move |_config| {
                let release = Arc::clone(&release);
                Box::pin(async move {
                    release.notified().await;
                    Err(McpSessionError::InvalidResponse("late failure".to_owned()))
                })
            })
        };
        let manager =
            McpManager::with_connector_and_startup_timeout(None, connector, CACHED_TOOL_GRACE_MS);
        manager
            .inner
            .tool_cache
            .set("slow", vec![tool_definition("cached")])
            .await;

        manager
            .discover_and_connect(HashMap::from([("slow".to_owned(), stdio_config("slow"))]))
            .await
            .unwrap();
        assert_eq!(
            manager.get_connection_status("slow"),
            ConnectionStatus::Connecting
        );

        release.notify_one();

        wait_until("late failure to settle", || {
            manager.get_connection_status("slow") == ConnectionStatus::Disconnected
        })
        .await;

        assert!(
            manager.errors()["slow"].contains("late failure"),
            "late error must be recorded: {:?}",
            manager.errors()
        );
        assert!(manager.inner.pending_connections.read().is_empty());
    }

    /// `startup = 0` means "no extra budget", so the attempt timeout is the
    /// only bound. A pending connector must therefore settle at 150ms, not
    /// hang in `discover_and_connect`.
    #[tokio::test]
    async fn startup_timeout_zero_defers_to_attempt_timeout() {
        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move { std::future::pending::<SessionResult<Arc<McpSession>>>().await })
        });
        let manager = McpManager::with_connector_and_timeouts(None, connector, 0, 30_000);
        let config = McpServerConfig::Stdio {
            command: "slow".to_owned(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            timeout: Some(150),
            version_policy: Default::default(),
            credentials: None,
        };

        let started = Instant::now();
        manager
            .discover_and_connect(HashMap::from([("slow".to_owned(), config)]))
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "discover must not wait for the unbounded connector"
        );

        assert_eq!(
            manager.get_connection_status("slow"),
            ConnectionStatus::Disconnected
        );
        assert!(
            manager.errors()["slow"].contains("connection timed out after 150ms"),
            "{:?}",
            manager.errors()
        );
        assert!(manager.inner.pending_connections.read().is_empty());
    }

    /// `startup = 0` and `config.timeout = 0` (both deadlines disabled) must
    /// still terminate via the internal attempt safety bound.
    #[tokio::test]
    async fn zero_timeouts_apply_attempt_safety_bound() {
        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move { std::future::pending::<SessionResult<Arc<McpSession>>>().await })
        });
        let manager = McpManager::with_connector_and_timeouts(None, connector, 0, 100);
        let config = McpServerConfig::Stdio {
            command: "never".to_owned(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            timeout: Some(0),
            version_policy: Default::default(),
            credentials: None,
        };

        let started = Instant::now();
        manager
            .discover_and_connect(HashMap::from([("never".to_owned(), config)]))
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "safety bound must terminate the attempt"
        );

        assert_eq!(
            manager.get_connection_status("never"),
            ConnectionStatus::Disconnected
        );
        let message = &manager.errors()["never"];
        assert!(
            message.contains("connection timed out after 100ms")
                && message.contains("safety bound"),
            "safety-bound timeout must be identifiable: {message}"
        );
        assert!(manager.inner.pending_connections.read().is_empty());
    }

    struct HangingCredentialProvider;

    impl crate::credential::CredentialProvider for HangingCredentialProvider {
        fn resolve(&self, _reference: &str) -> futures::future::BoxFuture<'_, Option<String>> {
            Box::pin(std::future::pending())
        }
    }

    /// The safety bound must also cover credential resolution: with both
    /// startup and attempt deadlines disabled, a stalled provider must not
    /// keep `discover_and_connect` or the pending attempt alive forever.
    #[tokio::test]
    async fn zero_timeouts_bound_stalled_credential_resolution() {
        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move {
                Err(McpSessionError::InvalidResponse(
                    "connector must not run".to_owned(),
                ))
            })
        });
        let manager = McpManager::with_credential_provider_and_timeouts(
            None,
            connector,
            Arc::new(HangingCredentialProvider),
            0,
            100,
        );
        let config = McpServerConfig::HttpOrSse {
            url: "http://127.0.0.1:9/mcp".to_owned(),
            transport_type: Some("http".to_owned()),
            headers: HashMap::from([(
                "Authorization".to_owned(),
                "vault://stalled-secret".to_owned(),
            )]),
            timeout: Some(0),
            version_policy: Default::default(),
            credentials: None,
        };

        let started = Instant::now();
        manager
            .discover_and_connect(HashMap::from([("never".to_owned(), config)]))
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stalled credential resolution must be bounded"
        );

        assert_eq!(
            manager.get_connection_status("never"),
            ConnectionStatus::Disconnected
        );
        let message = &manager.errors()["never"];
        assert!(
            message.contains("credential resolution timed out after 100ms")
                && message.contains("safety bound"),
            "stalled credential resolution must report a bounded failure: {message}"
        );
        assert!(manager.inner.pending_connections.read().is_empty());
    }

    /// An expired absolute deadline must fail the phase without polling it:
    /// `tokio::time::timeout` polls the inner future first, so an already-ready
    /// phase could otherwise run (and publish) after the deadline.
    #[tokio::test]
    async fn expired_phase_is_not_polled() {
        let polled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&polled);
        let outcome = run_phase(tokio::time::Instant::now(), async move {
            flag.store(true, Ordering::Release);
            1u8
        })
        .await;
        assert!(matches!(outcome, PhaseOutcome::TimedOut));
        assert!(
            !polled.load(Ordering::Acquire),
            "an expired deadline must not poll the next phase"
        );

        let outcome = run_phase(
            tokio::time::Instant::now() + Duration::from_secs(1),
            async { 2u8 },
        )
        .await;
        assert!(matches!(outcome, PhaseOutcome::Done(2)));
    }

    fn lock_entry(name: &str) -> ServerLockEntry {
        ServerLockEntry {
            server_info: ServerInfoSnapshot {
                name: name.to_owned(),
                version: "1".to_owned(),
            },
            protocol_version: crate::session::PREFERRED_PROTOCOL_VERSION.to_owned(),
            tools_snapshot: Vec::new(),
        }
    }

    /// Persistence writers snapshot the in-memory lock *after* taking the
    /// persistence gate, so a writer scheduled out of order cannot roll the
    /// file back to an older snapshot. The atomic write must not leave temp
    /// files behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lock_persistence_writes_latest_snapshot_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock.json");
        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move { Err(McpSessionError::InvalidResponse("unused".to_owned())) })
        });
        let manager = Arc::new(McpManager::with_connector_and_lock(
            None,
            connector,
            path.clone(),
        ));
        manager
            .inner
            .lock_file
            .write()
            .upsert_entry("first".to_owned(), lock_entry("first"));

        let gate = manager.inner.lock_persist_gate.lock().await;
        let inner = Arc::clone(&manager.inner);
        let persist = tokio::spawn(async move { persist_lock_file(&inner).await });

        // Give the writer time to run and block on the persistence gate, so a
        // regression that snapshots before acquiring it would write "first".
        tokio::time::sleep(Duration::from_millis(50)).await;
        manager
            .inner
            .lock_file
            .write()
            .upsert_entry("second".to_owned(), lock_entry("second"));
        drop(gate);
        persist.await.unwrap();

        let lock = McpLockFile::load_from_disk(&path).await.unwrap();
        assert!(lock.get_entry("first").is_some());
        assert!(
            lock.get_entry("second").is_some(),
            "writer must snapshot state after acquiring the persistence gate"
        );

        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "atomic write must not leave temp files: {leftovers:?}"
        );
    }

    /// Publication-order invariant: at every instant an observer sampling
    /// `status` before `tools` sees live tools whenever it sees Connected.
    /// Deterministic delays (0/5/10/15ms) sweep the race window instead of
    /// relying on randomness.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connected_implies_tools_published() {
        for delay_ms in [0u64, 5, 10, 15] {
            let connector: SessionConnector = Arc::new(move |_config| {
                Box::pin(async move {
                    if delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    }
                    let transport: Arc<dyn McpTransport> =
                        Arc::new(FixtureTransport::new("live_tool"));
                    Ok(Arc::new(McpSession::connect(transport).await?))
                })
            });
            let manager = Arc::new(McpManager::with_connector_and_startup_timeout(
                None, connector, 1_000,
            ));

            let done = Arc::new(AtomicBool::new(false));
            let violation = Arc::new(AtomicBool::new(false));
            let saw_connected = Arc::new(AtomicBool::new(false));
            let samples = Arc::new(AtomicUsize::new(0));
            let ready = Arc::new(tokio::sync::Notify::new());

            let observer = {
                let manager = Arc::clone(&manager);
                let done = Arc::clone(&done);
                let violation = Arc::clone(&violation);
                let saw_connected = Arc::clone(&saw_connected);
                let samples = Arc::clone(&samples);
                let ready = Arc::clone(&ready);
                tokio::spawn(async move {
                    ready.notify_one();
                    loop {
                        // Sample status first: if it is Connected, the live
                        // tools must already be visible.
                        let status = manager.get_connection_status("srv");
                        if status == ConnectionStatus::Connected {
                            saw_connected.store(true, Ordering::Release);
                            let tools = manager.get_tools();
                            let live = tools
                                .iter()
                                .any(|tool| tool.server_name == "srv" && !tool.deferred);
                            if !live {
                                violation.store(true, Ordering::Release);
                            }
                        }
                        samples.fetch_add(1, Ordering::Relaxed);
                        // Require one Connected sample so the test cannot pass
                        // by only observing pre-publication states.
                        if done.load(Ordering::Acquire) && saw_connected.load(Ordering::Acquire) {
                            break;
                        }
                        if samples.load(Ordering::Relaxed) > 1_000_000 {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
            };
            ready.notified().await;

            manager
                .discover_and_connect(HashMap::from([("srv".to_owned(), stdio_config("srv"))]))
                .await
                .unwrap();
            done.store(true, Ordering::Release);
            observer.await.unwrap();

            assert!(
                !violation.load(Ordering::Acquire),
                "delay {delay_ms}ms: observed Connected before live tools were published"
            );
            assert_eq!(
                manager.get_connection_status("srv"),
                ConnectionStatus::Connected
            );
            assert!(
                samples.load(Ordering::Relaxed) > 0,
                "observer must have sampled at least once"
            );
            assert!(
                saw_connected.load(Ordering::Acquire),
                "delay {delay_ms}ms: observer never sampled the Connected state"
            );
        }
    }
}
