//! Test utilities — only available with the `test-utils` feature.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures::{stream, stream::BoxStream};
use llm_adapter::{
    LlmError,
    provider::{Provider, ProviderCapabilities},
    stream_handle::StreamHandle,
    types::{ChatRequest, ChatResponse, StopReason, StreamEvent, Usage},
};
use llm_harness_types::{EnvError, ExecutionEnv, FileInfo, ShellOptions, ShellOutput};
use tokio_util::sync::CancellationToken;

// ── NoOpEnv ──────────────────────────────────────────────────────────────────

/// `ExecutionEnv` that returns errors for all operations. Used in tests.
pub struct NoOpEnv;

impl ExecutionEnv for NoOpEnv {
    fn working_dir(&self) -> &Path {
        Path::new("/tmp")
    }

    fn read_text_file<'a>(
        &'a self,
        p: &'a Path,
        _: CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<String, EnvError>> {
        let p = p.to_path_buf();
        Box::pin(async move { Err(EnvError::NotFound(p)) })
    }

    fn read_text_lines<'a>(
        &'a self,
        p: &'a Path,
        _: Option<usize>,
        _: CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<Vec<String>, EnvError>> {
        let p = p.to_path_buf();
        Box::pin(async move { Err(EnvError::NotFound(p)) })
    }

    fn read_binary_file<'a>(
        &'a self,
        p: &'a Path,
        _: CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<Vec<u8>, EnvError>> {
        let p = p.to_path_buf();
        Box::pin(async move { Err(EnvError::NotFound(p)) })
    }

    fn write_file<'a>(
        &'a self,
        p: &'a Path,
        _: &'a [u8],
        _: CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<(), EnvError>> {
        let p = p.to_path_buf();
        Box::pin(async move { Err(EnvError::PermissionDenied(p)) })
    }

    fn append_file<'a>(
        &'a self,
        p: &'a Path,
        _: &'a [u8],
        _: CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<(), EnvError>> {
        let p = p.to_path_buf();
        Box::pin(async move { Err(EnvError::PermissionDenied(p)) })
    }

    fn file_info<'a>(
        &'a self,
        p: &'a Path,
        _: CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<FileInfo, EnvError>> {
        let p = p.to_path_buf();
        Box::pin(async move { Err(EnvError::NotFound(p)) })
    }

    fn list_dir<'a>(
        &'a self,
        p: &'a Path,
        _: CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<Vec<FileInfo>, EnvError>> {
        let p = p.to_path_buf();
        Box::pin(async move { Err(EnvError::NotFound(p)) })
    }

    fn exists<'a>(
        &'a self,
        _: &'a Path,
        _: CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<bool, EnvError>> {
        Box::pin(async { Ok(false) })
    }

    fn create_dir<'a>(
        &'a self,
        p: &'a Path,
        _: bool,
        _: CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<(), EnvError>> {
        let p = p.to_path_buf();
        Box::pin(async move { Err(EnvError::PermissionDenied(p)) })
    }

    fn remove<'a>(
        &'a self,
        p: &'a Path,
        _: bool,
        _: bool,
        _: CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<(), EnvError>> {
        let p = p.to_path_buf();
        Box::pin(async move { Err(EnvError::PermissionDenied(p)) })
    }

    fn create_temp_dir<'a>(
        &'a self,
        _: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<PathBuf, EnvError>> {
        Box::pin(async { Err(EnvError::Other("no-op env".into())) })
    }

    fn execute_shell<'a>(
        &'a self,
        _: &'a str,
        _: ShellOptions<'a>,
    ) -> futures::future::BoxFuture<'a, Result<ShellOutput, EnvError>> {
        Box::pin(async { Err(EnvError::Other("no-op env".into())) })
    }

    fn cleanup<'a>(&'a self) -> futures::future::BoxFuture<'a, Result<(), EnvError>> {
        Box::pin(async { Ok(()) })
    }
}

// ── MockLlmClient ─────────────────────────────────────────────────────────────

/// Preset response for `MockLlmClient`.
pub struct MockResponse {
    /// Stream events to emit in order.
    pub events: Vec<Result<StreamEvent, LlmError>>,
    /// Model name to set on the `StreamHandle`.
    pub model: String,
}

impl MockResponse {
    /// Simple text response ending with `EndTurn`.
    pub fn text(text: &str) -> Self {
        let t = text.to_owned();
        Self {
            model: "mock-model".into(),
            events: vec![
                Ok(StreamEvent::ContentStart {
                    index: 0,
                    kind: llm_adapter::types::ContentKind::Text,
                }),
                Ok(StreamEvent::TextDelta { index: 0, text: t }),
                Ok(StreamEvent::ContentStop { index: 0 }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                }),
            ],
        }
    }

    /// Tool-use response: the model calls one tool.
    pub fn tool_use(tool_use_id: &str, tool_name: &str, args_json: &str) -> Self {
        let id = tool_use_id.to_owned();
        let name = tool_name.to_owned();
        let args = args_json.to_owned();
        Self {
            model: "mock-model".into(),
            events: vec![
                Ok(StreamEvent::ContentStart {
                    index: 0,
                    kind: llm_adapter::types::ContentKind::ToolInvocation { id, name },
                }),
                Ok(StreamEvent::ToolDelta {
                    index: 0,
                    arguments: args,
                }),
                Ok(StreamEvent::ContentStop { index: 0 }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                    usage: Usage::default(),
                }),
            ],
        }
    }
}

/// LLM client mock that returns pre-configured responses in sequence.
///
/// After all preset responses are consumed, subsequent calls return a simple
/// `EndTurn` text response.
pub struct MockLlmClient {
    responses: Mutex<Vec<MockResponse>>,
    /// Tracks how many times `chat_stream` was called.
    pub call_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl MockLlmClient {
    /// Create from a list of preset responses.
    pub fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            call_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Convenience constructor for a single preset response.
    pub fn with_single(r: MockResponse) -> Self {
        Self::new(vec![r])
    }
}

#[async_trait]
impl Provider for MockLlmClient {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::new(false, false, false)
    }

    async fn chat(&self, _req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        Err(LlmError::InvalidRequest(
            "MockLlmClient: use chat_stream instead".into(),
        ))
    }

    async fn chat_stream(&self, _req: &ChatRequest) -> Result<StreamHandle, LlmError> {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let response = {
            let mut guard = self.responses.lock().unwrap();
            if guard.is_empty() {
                MockResponse::text("(no more mock responses)")
            } else {
                guard.remove(0)
            }
        };
        let inner: BoxStream<'static, Result<StreamEvent, LlmError>> =
            Box::pin(stream::iter(response.events));
        Ok(StreamHandle::from_raw_stream(response.model, inner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_client_returns_text_response() {
        use futures::StreamExt;

        let client = MockLlmClient::with_single(MockResponse::text("hello"));
        let req = ChatRequest::builder("mock-model", 1024).build();
        let mut handle = client.chat_stream(&req).await.unwrap();
        let events: Vec<_> = handle.events().collect().await;
        assert!(!events.is_empty());
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Ok(StreamEvent::TextDelta { .. })))
        );
    }

    #[test]
    fn no_op_env_working_dir() {
        let env = NoOpEnv;
        assert_eq!(env.working_dir(), Path::new("/tmp"));
    }
}
