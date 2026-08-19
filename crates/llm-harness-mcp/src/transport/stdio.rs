use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex as ParkingMutex;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{
    CloseHandler, McpTransport, McpTransportError, NotificationHandler, RequestHandler,
    TransportCallbacks, TransportResult, request_error_response, resolve_timeout_ms,
};
use crate::config::McpServerConfig;
use crate::json_rpc::{
    JsonRpcError, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, RequestId,
    error_codes,
};

type PendingResponse = oneshot::Sender<TransportResult<Value>>;

struct StdioInner {
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    pending: ParkingMutex<HashMap<RequestId, PendingResponse>>,
    next_id: AtomicU64,
    connected: AtomicBool,
    closing: AtomicBool,
    timeout_ms: u64,
    callbacks: TransportCallbacks,
    cancellation: CancellationToken,
    stdout_task: ParkingMutex<Option<JoinHandle<()>>>,
    stderr_task: ParkingMutex<Option<JoinHandle<()>>>,
}

impl StdioInner {
    async fn write_json(&self, value: &impl serde::Serialize) -> TransportResult<()> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(McpTransportError::Disconnected);
        }
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');

        let mut stdin = self.stdin.lock().await;
        let writer = stdin.as_mut().ok_or(McpTransportError::Disconnected)?;
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        Ok(())
    }

    fn fail_pending(&self, error: McpTransportError) {
        let pending = std::mem::take(&mut *self.pending.lock());
        for (_, sender) in pending {
            let _ = sender.send(Err(error.clone()));
        }
    }

    fn mark_closed(&self) {
        if self.connected.swap(false, Ordering::AcqRel) {
            self.fail_pending(McpTransportError::Closed);
            if !self.closing.load(Ordering::Acquire) {
                self.callbacks.close();
            }
        }
    }
    async fn cleanup_child(&self) {
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    async fn handle_message(self: &Arc<Self>, message: JsonRpcMessage) {
        match message {
            JsonRpcMessage::Response(response) => self.handle_response(response),
            JsonRpcMessage::Notification(notification) => self.callbacks.notify(notification),
            JsonRpcMessage::Request(request) => {
                let inner = Arc::clone(self);
                tokio::spawn(async move {
                    inner.handle_server_request(request).await;
                });
            }
        }
    }

    fn handle_response(&self, response: JsonRpcResponse) {
        let Some(ref id) = response.id else {
            tracing::warn!("ignoring MCP response without request id");
            return;
        };
        let Some(sender) = self.pending.lock().remove(id) else {
            tracing::debug!(?id, "ignoring MCP response for unknown request id");
            return;
        };
        let result = response.into_result().map_err(McpTransportError::from);
        let _ = sender.send(result);
    }

    async fn handle_server_request(self: &Arc<Self>, request: JsonRpcRequest) {
        let id = request.id.clone();
        let response = match self.callbacks.request() {
            Some(handler) => match handler(request).await {
                Ok(result) => JsonRpcResponse::success(id, result),
                Err(error) => request_error_response(id, error),
            },
            None => JsonRpcResponse::error(
                Some(id),
                JsonRpcError {
                    code: error_codes::METHOD_NOT_FOUND,
                    message: "Method not found".to_owned(),
                    data: None,
                },
            ),
        };
        if let Err(error) = self.write_json(&response).await {
            tracing::warn!(%error, "failed to reply to MCP server request");
        }
    }
}

/// Newline-delimited JSON-RPC over a child process's stdio.
pub struct StdioTransport {
    inner: Arc<StdioInner>,
}

impl StdioTransport {
    pub async fn connect(config: &McpServerConfig) -> TransportResult<Self> {
        let McpServerConfig::Stdio {
            command,
            args,
            env,
            cwd,
            timeout,
            ..
        } = config
        else {
            return Err(McpTransportError::InvalidConfig(
                "expected stdio server config".to_owned(),
            ));
        };

        config
            .validate()
            .map_err(|error| McpTransportError::InvalidConfig(error.to_string()))?;

        let mut process = Command::new(command);
        process
            .args(args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = cwd {
            process.current_dir(cwd);
        }

        let mut child = process.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpTransportError::Io("spawned MCP process has no stdin".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpTransportError::Io("spawned MCP process has no stdout".to_owned()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| McpTransportError::Io("spawned MCP process has no stderr".to_owned()))?;

        let inner = Arc::new(StdioInner {
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(Some(stdin)),
            pending: ParkingMutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            connected: AtomicBool::new(true),
            closing: AtomicBool::new(false),
            timeout_ms: resolve_timeout_ms(
                std::env::var("OMP_MCP_TIMEOUT_MS").ok().as_deref(),
                *timeout,
            ),
            callbacks: TransportCallbacks::default(),
            cancellation: CancellationToken::new(),
            stdout_task: ParkingMutex::new(None),
            stderr_task: ParkingMutex::new(None),
        });

        let stdout_task = tokio::spawn(read_stdout(Arc::downgrade(&inner), stdout));
        let stderr_task = tokio::spawn(read_stderr(Arc::downgrade(&inner), stderr));
        *inner.stdout_task.lock() = Some(stdout_task);
        *inner.stderr_task.lock() = Some(stderr_task);

        Ok(Self { inner })
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn request(&self, method: &str, params: Value) -> TransportResult<Value> {
        if !self.is_connected() {
            return Err(McpTransportError::Disconnected);
        }

        let id = RequestId::from(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let request = JsonRpcRequest::new(id.clone(), method, params);
        let (sender, receiver) = oneshot::channel();
        self.inner.pending.lock().insert(id.clone(), sender);

        if let Err(error) = self.inner.write_json(&request).await {
            self.inner.pending.lock().remove(&id);
            return Err(error);
        }

        let receive = async { receiver.await.unwrap_or(Err(McpTransportError::Closed)) };
        if self.inner.timeout_ms == 0 {
            receive.await
        } else {
            tokio::time::timeout(Duration::from_millis(self.inner.timeout_ms), receive)
                .await
                .map_err(|_| {
                    self.inner.pending.lock().remove(&id);
                    McpTransportError::Timeout {
                        method: method.to_owned(),
                        timeout_ms: self.inner.timeout_ms,
                    }
                })?
        }
    }

    async fn notify(&self, method: &str, params: Value) -> TransportResult<()> {
        self.inner
            .write_json(&JsonRpcNotification::new(method, params))
            .await
    }

    fn is_connected(&self) -> bool {
        self.inner.connected.load(Ordering::Acquire)
    }

    async fn close(&self) -> TransportResult<()> {
        let was_connected = self.inner.connected.swap(false, Ordering::AcqRel);
        self.inner.closing.store(true, Ordering::Release);
        self.inner.cancellation.cancel();
        self.inner.fail_pending(McpTransportError::Closed);
        self.inner.stdin.lock().await.take();
        self.inner.cleanup_child().await;
        if let Some(handle) = self.inner.stdout_task.lock().take() {
            handle.abort();
        }
        if let Some(handle) = self.inner.stderr_task.lock().take() {
            handle.abort();
        }
        if was_connected {
            self.inner.callbacks.close();
        }
        Ok(())
    }

    fn set_protocol_version(&self, _version: &str) {}

    fn on_request(&self, handler: RequestHandler) {
        self.inner.callbacks.set_request(handler);
    }

    fn on_notification(&self, handler: NotificationHandler) {
        self.inner.callbacks.set_notification(handler);
    }

    fn on_close(&self, handler: CloseHandler) {
        self.inner.callbacks.set_close(handler);
    }

    fn clear_on_close(&self) {
        self.inner.callbacks.clear_close();
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.inner.closing.store(true, Ordering::Release);
        self.inner.cancellation.cancel();
        // Best-effort synchronous kill: the child mutex is a tokio async
        // mutex that cannot be locked synchronously in Drop, so we use
        // try_lock. If it is contended, the close() path or kill_on_drop
        if let Ok(mut guard) = self.inner.child.try_lock()
            && let Some(mut child) = guard.take()
        {
            let _ = child.start_kill();
        }
        if let Some(handle) = self.inner.stdout_task.lock().take() {
            handle.abort();
        }
        if let Some(handle) = self.inner.stderr_task.lock().take() {
            handle.abort();
        }
    }
}

async fn read_stdout(weak: Weak<StdioInner>, stdout: tokio::process::ChildStdout) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        tokio::select! {
            _ = inner.cancellation.cancelled() => return,
            line = lines.next_line() => {
                drop(inner);
                match line {
                    Ok(Some(line)) => {
                        let Some(inner) = weak.upgrade() else {
                            return;
                        };
                        match serde_json::from_str::<JsonRpcMessage>(&line) {
                            Ok(message) => inner.handle_message(message).await,
                            Err(error) => {
                                tracing::warn!(%error, line, "ignoring malformed MCP stdio message");
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        tracing::warn!(%error, "MCP stdio stdout read failed");
                        break;
                    }
                }
            }
        }
    }
    if let Some(inner) = weak.upgrade() {
        inner.mark_closed();
    }
}

async fn read_stderr(weak: Weak<StdioInner>, stderr: tokio::process::ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        tokio::select! {
            _ = inner.cancellation.cancelled() => return,
            line = lines.next_line() => {
                drop(inner);
                match line {
                    Ok(Some(line)) => tracing::debug!(message = line, "MCP server stderr"),
                    Ok(None) => return,
                    Err(error) => {
                        tracing::debug!(%error, "MCP server stderr read failed");
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn fixture_config(script: String) -> McpServerConfig {
        #[cfg(windows)]
        {
            McpServerConfig::Stdio {
                command: "powershell".to_owned(),
                args: vec!["-NoProfile".to_owned(), "-Command".to_owned(), script],
                env: HashMap::new(),
                cwd: None,
                timeout: Some(5_000),
                version_policy: Default::default(),
                credentials: None,
            }
        }
        #[cfg(not(windows))]
        {
            McpServerConfig::Stdio {
                command: "sh".to_owned(),
                args: vec!["-c".to_owned(), script],
                env: HashMap::new(),
                cwd: None,
                timeout: Some(5_000),
                version_policy: Default::default(),
                credentials: None,
            }
        }
    }

    #[tokio::test]
    async fn stdio_round_trip_and_close_are_functional() {
        #[cfg(windows)]
        let script = concat!(
            "$input | ForEach-Object { ",
            "'{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}'; ",
            "[Console]::Out.Flush() }"
        )
        .to_owned();
        #[cfg(not(windows))]
        let script = concat!(
            "while IFS= read -r line; do ",
            "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}'; ",
            "done"
        )
        .to_owned();

        let transport = StdioTransport::connect(&fixture_config(script))
            .await
            .unwrap();
        let result = transport.request("fixture", json!({})).await.unwrap();
        assert_eq!(result, json!({"ok": true}));
        transport.close().await.unwrap();
        assert!(!transport.is_connected());
        transport.close().await.unwrap();
    }

    #[tokio::test]
    async fn stdio_timeout_cleans_pending_request() {
        #[cfg(windows)]
        let script = "$input | ForEach-Object { Start-Sleep -Seconds 5 }".to_owned();
        #[cfg(not(windows))]
        let script = "while IFS= read -r line; do sleep 5; done".to_owned();
        let mut config = fixture_config(script);
        if let McpServerConfig::Stdio { timeout, .. } = &mut config {
            *timeout = Some(30);
        }

        let transport = StdioTransport::connect(&config).await.unwrap();
        let error = transport.request("slow", Value::Null).await.unwrap_err();
        assert!(matches!(error, McpTransportError::Timeout { .. }));
        assert!(transport.inner.pending.lock().is_empty());
        transport.close().await.unwrap();
    }

    #[tokio::test]
    async fn stdio_child_killed_when_close_called_after_mark_closed() {
        // A script that stays alive until killed.
        #[cfg(not(windows))]
        let script = "sleep 30".to_owned();
        #[cfg(windows)]
        let script = "Start-Sleep -Seconds 30".to_owned();

        let transport = StdioTransport::connect(&fixture_config(script))
            .await
            .unwrap();
        // Simulate the read loop detecting EOF and calling mark_closed.
        transport.inner.mark_closed();
        // Now call close() — this used to skip kill+wait.
        transport.close().await.unwrap();
        // If we reach here without hanging, the child was cleaned up.
        // Verify child is None.
        assert!(transport.inner.child.lock().await.is_none());
    }
}
