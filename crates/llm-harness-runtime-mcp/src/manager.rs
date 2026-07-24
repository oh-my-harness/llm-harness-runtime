use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::future::{BoxFuture, join_all};
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::McpServerConfig;
use crate::session::{McpSession, McpSessionError, SessionResult};
use crate::transport::{create_transport, resolve_timeout_ms};
use crate::types::{McpToolCallResult, McpToolDefinition, notification_methods};

pub const STARTUP_TIMEOUT_MS: u64 = 250;
const RECONNECT_WINDOW: Duration = Duration::from_secs(30);
const MAX_RECONNECTS_IN_WINDOW: usize = 5;
const RECONNECT_BACKOFF_MS: [u64; 5] = [0, 500, 1_000, 2_000, 4_000];

pub(crate) type SessionConnector = Arc<
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
}

pub type ManagerResult<T> = Result<T, McpManagerError>;

#[derive(Clone)]
enum ConnectionAttempt {
    Pending,
    Ready(Arc<McpSession>),
    Failed(Arc<String>),
}

pub struct McpToolCache {
    entries: RwLock<HashMap<String, Vec<McpToolDefinition>>>,
    cache_path: Option<PathBuf>,
}

impl McpToolCache {
    pub fn new(cache_path: Option<PathBuf>) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            cache_path,
        }
    }

    pub fn get(&self, server_name: &str) -> Option<Vec<McpToolDefinition>> {
        self.entries.read().get(server_name).cloned()
    }

    pub async fn set(&self, server_name: &str, tools: Vec<McpToolDefinition>) {
        self.entries.write().insert(server_name.to_owned(), tools);
        if let Err(error) = self.flush().await {
            tracing::warn!(%error, "failed to persist MCP tool cache");
        }
    }

    pub async fn load_from_disk(&self) -> ManagerResult<()> {
        let Some(path) = &self.cache_path else {
            return Ok(());
        };
        let bytes = match tokio::fs::read(path).await {
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
        tokio::fs::write(path, bytes)
            .await
            .map_err(|error| McpManagerError::Cache(error.to_string()))
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
}

pub struct McpManager {
    inner: Arc<ManagerInner>,
}

impl McpManager {
    pub fn new() -> Self {
        Self::with_connector(None, Arc::new(default_connector))
    }

    pub fn with_cache_path(cache_path: impl Into<PathBuf>) -> Self {
        Self::with_connector(Some(cache_path.into()), Arc::new(default_connector))
    }

    pub(crate) fn with_connector(cache_path: Option<PathBuf>, connector: SessionConnector) -> Self {
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
        let _ =
            tokio::time::timeout(Duration::from_millis(STARTUP_TIMEOUT_MS), join_all(settle)).await;

        for (name, receiver) in waiters {
            if matches!(*receiver.borrow(), ConnectionAttempt::Pending)
                && let Some(tools) = self.inner.tool_cache.get(&name)
            {
                replace_server_tools(&self.inner, &name, tools, true);
            }
        }
        Ok(())
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

    pub fn get_tools(&self) -> Vec<ExposedTool> {
        self.inner.tools.read().clone()
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

fn default_connector(
    config: McpServerConfig,
) -> BoxFuture<'static, SessionResult<Arc<McpSession>>> {
    Box::pin(async move {
        let transport = create_transport(&config).await?;
        Ok(Arc::new(McpSession::connect(transport).await?))
    })
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
    let result = if timeout_ms == 0 {
        connect.await.map_err(|error| error.to_string())
    } else {
        tokio::time::timeout(Duration::from_millis(timeout_ms), connect)
            .await
            .map_err(|_| format!("connection timed out after {timeout_ms}ms"))
            .and_then(|result| result.map_err(|error| error.to_string()))
    };

    match result {
        Ok((session, tools)) => {
            wire_session(&inner, &name, &session);
            inner
                .connections
                .write()
                .insert(name.clone(), Arc::clone(&session));
            inner.failures.write().remove(&name);
            replace_server_tools(&inner, &name, tools.clone(), false);
            inner.tool_cache.set(&name, tools).await;
            let _ = sender.send(ConnectionAttempt::Ready(session));
        }
        Err(message) => {
            inner.failures.write().insert(name.clone(), message.clone());
            let _ = sender.send(ConnectionAttempt::Failed(Arc::new(message)));
        }
    }
    inner.pending_connections.write().remove(&name);
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
            if let Some(session) = session {
                match session.list_tools().await {
                    Ok(tools) => {
                        replace_server_tools(&inner, &name, tools.clone(), false);
                        inner.tool_cache.set(&name, tools).await;
                    }
                    Err(error) => {
                        tracing::warn!(server = name, %error, "failed to refresh MCP tools");
                    }
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

fn sanitize_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "unnamed".to_owned()
    } else {
        sanitized
    }
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
        let manager = McpManager::with_connector(None, connector);
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

        let loaded = McpToolCache::new(Some(path));
        loaded.load_from_disk().await.unwrap();

        assert_eq!(loaded.get("server").unwrap(), vec![tool]);
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
        };

        let _ = manager
            .start_connection("test".to_owned(), config)
            .unwrap();
        // Wait for the timeout to fire and record the failure.
        tokio::time::sleep(Duration::from_millis(500)).await;

        assert!(
            manager.inner.failures.read().contains_key("test"),
            "timed-out connection must record a failure"
        );
    }
}
