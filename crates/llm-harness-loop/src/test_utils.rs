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
use llm_harness_types::{
    AssistantMessage, AssistantMessageKind, ContentBlock, EnvError, ExecutionEnv, FileInfo,
    ShellOptions, ShellOutput,
};
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
    /// Stream events to emit in order. Ignored when `stream_error` is set or
    /// when `stream_impl` is set.
    pub events: Vec<Result<StreamEvent, LlmError>>,
    /// Model name to set on the `StreamHandle`. Ignored when `stream_error` is set.
    pub model: String,
    /// If set, `chat_stream` returns this error instead of a `StreamHandle`.
    pub stream_error: Option<LlmError>,
    /// Custom stream implementation; when set, takes precedence over `events`.
    pub stream_impl: Option<BoxStream<'static, Result<StreamEvent, LlmError>>>,
}

impl MockResponse {
    /// Simple text response ending with `EndTurn` and missing provider usage.
    ///
    /// This convenience path intentionally cannot serve as trusted pricing
    /// evidence. Use [`MockResponse::text_with_reported_zero`] or
    /// [`MockResponse::with_reported_usage`] when the mock provider is meant to
    pub fn text(text: &str) -> Self {
        let t = text.to_owned();
        Self {
            model: "mock-model".into(),
            stream_error: None,
            stream_impl: None,
            events: vec![
                Ok(StreamEvent::ContentStart {
                    index: 0,
                    kind: llm_adapter::types::ContentKind::Text,
                }),
                Ok(StreamEvent::TextDelta { index: 0, text: t }),
                Ok(StreamEvent::ContentStop {
                    index: 0,
                    signature: None,
                }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                }),
            ],
        }
    }

    /// Simple text response with an explicit, authoritative all-zero report.
    pub fn text_with_reported_zero(text: &str) -> Self {
        Self::text(text).with_reported_usage(Usage::default())
    }

    /// Alias that makes a deliberately missing report explicit at the call site.
    pub fn text_with_missing_usage(text: &str) -> Self {
        Self::text(text)
    }

    /// Replaces this response's terminal usage with authoritative provider
    /// values. Canonical numeric violations become `ReportedInvalid`.
    pub fn with_reported_usage(mut self, usage: Usage) -> Self {
        self.replace_usage(usage.reported());
        self
    }

    /// Replaces this response's terminal usage with a provider report known to
    /// violate a provider-specific invariant.
    pub fn with_reported_invalid_usage(mut self, usage: Usage) -> Self {
        self.replace_usage(usage.reported_invalid());
        self
    }

    fn replace_usage(&mut self, usage: Usage) {
        for event in &mut self.events {
            if let Ok(StreamEvent::MessageStop {
                usage: terminal, ..
            }) = event
            {
                *terminal = usage;
                return;
            }
        }
        panic!("mock response has no terminal MessageStop usage");
    }

    /// Replaces this response's terminal stop_reason. Used to simulate
    /// providers that return ToolUse blocks with EndTurn, or vice versa.
    pub fn with_stop_reason(mut self, stop_reason: StopReason) -> Self {
        for event in &mut self.events {
            if let Ok(StreamEvent::MessageStop {
                stop_reason: terminal,
                ..
            }) = event
            {
                *terminal = stop_reason;
                return self;
            }
        }
        panic!("mock response has no terminal MessageStop event");
    }

    /// Tool-use response: the model calls one tool.
    pub fn tool_use(tool_use_id: &str, tool_name: &str, args_json: &str) -> Self {
        let id = tool_use_id.to_owned();
        let name = tool_name.to_owned();
        let args = args_json.to_owned();
        Self {
            model: "mock-model".into(),
            stream_error: None,
            stream_impl: None,
            events: vec![
                Ok(StreamEvent::ContentStart {
                    index: 0,
                    kind: llm_adapter::types::ContentKind::ToolInvocation { id, name },
                }),
                Ok(StreamEvent::ToolDelta {
                    index: 0,
                    arguments: args,
                }),
                Ok(StreamEvent::ContentStop {
                    index: 0,
                    signature: None,
                }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                    usage: Usage::default(),
                }),
            ],
        }
    }

    /// Like [`MockResponse::tool_use`] but terminates with `EndTurn` instead of
    /// `ToolUse`. Used to simulate Anthropic-style providers that return a
    /// `structured_result` tool_use block as a terminal structured output
    /// (no tool execution expected).
    pub fn tool_use_end_turn(tool_use_id: &str, tool_name: &str, args_json: &str) -> Self {
        let id = tool_use_id.to_owned();
        let name = tool_name.to_owned();
        let args = args_json.to_owned();
        Self {
            model: "mock-model".into(),
            stream_error: None,
            stream_impl: None,
            events: vec![
                Ok(StreamEvent::ContentStart {
                    index: 0,
                    kind: llm_adapter::types::ContentKind::ToolInvocation { id, name },
                }),
                Ok(StreamEvent::ToolDelta {
                    index: 0,
                    arguments: args,
                }),
                Ok(StreamEvent::ContentStop {
                    index: 0,
                    signature: None,
                }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                }),
            ],
        }
    }

    /// Preset that causes `chat_stream` to return a `RateLimit` error.
    pub fn rate_limit_error() -> Self {
        Self {
            events: vec![],
            model: String::new(),
            stream_error: Some(LlmError::RateLimit { retry_after: None }),
            stream_impl: None,
        }
    }
    /// Preset that emits a couple of text deltas then terminates abnormally
    /// with `LlmError::StreamIncomplete` mid-stream (no `MessageStop`).
    ///
    /// Reproduces the issue #69 scenario: a dropped SSE connection surfaces
    /// as `Err(StreamIncomplete)` inside the event stream, which `loop_fn`
    /// converts to `AgentEvent::Error(Provider(...))`.
    pub fn stream_dropped_mid(partial_text: &str, received_chunks: u32) -> Self {
        let t = partial_text.to_owned();
        Self {
            model: "mock-model".into(),
            stream_error: None,
            stream_impl: None,
            events: vec![
                Ok(StreamEvent::ContentStart {
                    index: 0,
                    kind: llm_adapter::types::ContentKind::Text,
                }),
                Ok(StreamEvent::TextDelta { index: 0, text: t }),
                Err(LlmError::StreamIncomplete {
                    received_chunks,
                    finish_reason: None,
                }),
            ],
        }
    }

    /// Emits `n` initial events from a default text response, then stalls
    /// forever (channel sender stays open, receiver blocks). Used to test
    /// the stream idle watchdog.
    pub fn stall_after(n: usize) -> Self {
        let template = MockResponse::text("stalled");
        let initial: Vec<Result<StreamEvent, LlmError>> =
            template.events.into_iter().take(n).collect();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamEvent, LlmError>>(32);
        tokio::spawn(async move {
            for ev in initial {
                if tx.send(ev).await.is_err() {
                    break;
                }
            }
            // Do not close tx — receiver blocks forever, simulating stream stall.
            std::future::pending::<()>().await;
        });
        let inner: BoxStream<'static, Result<StreamEvent, LlmError>> =
            Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx));
        Self {
            events: vec![],
            model: "mock-model".into(),
            stream_error: None,
            stream_impl: Some(inner),
        }
    }

    /// Emits the given events one at a time, sleeping `interval_ms` between
    /// each. Used to verify the idle timer resets on each event arrival.
    pub fn slow_stream(events: Vec<Result<StreamEvent, LlmError>>, interval_ms: u64) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamEvent, LlmError>>(32);
        tokio::spawn(async move {
            for ev in events {
                tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
                if tx.send(ev).await.is_err() {
                    break;
                }
            }
        });
        let inner: BoxStream<'static, Result<StreamEvent, LlmError>> =
            Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx));
        Self {
            events: vec![],
            model: "mock-model".into(),
            stream_error: None,
            stream_impl: Some(inner),
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
    /// All `ChatRequest` values passed to `chat_stream`, captured for test assertions.
    pub captured_requests: Mutex<Vec<ChatRequest>>,
    /// Capabilities reported by [`Provider::capabilities`]. Defaults to all-false;
    /// override via [`MockLlmClient::with_capabilities`].
    capabilities: ProviderCapabilities,
}
impl MockLlmClient {
    /// Create from a list of preset responses.
    pub fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            call_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            captured_requests: Mutex::new(Vec::new()),
            capabilities: ProviderCapabilities::new(false, false, false),
        }
    }

    /// Convenience constructor for a single preset response.
    pub fn with_single(r: MockResponse) -> Self {
        Self::new(vec![r])
    }

    /// Set the capabilities this mock reports (e.g. `supports_json_schema`).
    /// Returns `self` for chaining. Used by integration tests that need the
    /// structured-output upgrade path to engage.
    pub fn with_capabilities(mut self, caps: ProviderCapabilities) -> Self {
        self.capabilities = caps;
        self
    }
}

#[async_trait]
impl Provider for MockLlmClient {
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone()
    }

    async fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.chat_stream(req).await?.collect().await
    }

    async fn chat_stream(&self, req: &ChatRequest) -> Result<StreamHandle, LlmError> {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.captured_requests.lock().unwrap().push(req.clone());
        let response = {
            let mut guard = self.responses.lock().unwrap();
            if guard.is_empty() {
                MockResponse::text("(no more mock responses)")
            } else {
                guard.remove(0)
            }
        };
        // Return an error directly if the preset is an error preset.
        if let Some(e) = response.stream_error {
            return Err(e);
        }
        let inner: BoxStream<'static, Result<StreamEvent, LlmError>> =
            if let Some(stream) = response.stream_impl {
                stream
            } else {
                Box::pin(stream::iter(response.events))
            };
        Ok(StreamHandle::from_raw_stream(response.model, inner))
    }
}

// ── AssistantMessage test helper ─────────────────────────────────────────────

/// Construct an `AssistantMessage` for tests with sensible defaults.
/// ID/turn_id are filled with stable test values; caller provides content.
/// For variants needing different `kind` or `stop_reason`, modify the
/// returned struct's fields at the call site.
pub fn test_assistant_message(content: Vec<ContentBlock>) -> AssistantMessage {
    use chrono::Utc;

    AssistantMessage {
        kind: AssistantMessageKind::FinalAnswer,
        message_id: "test_msg".into(),
        turn_id: "test_turn".into(),
        content,
        stop_reason: None,
        timestamp: Utc::now(),
        provider: None,
        api: None,
        model: None,
        usage: None,
        error_message: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal_usage(response: &MockResponse) -> &Usage {
        response
            .events
            .iter()
            .find_map(|event| match event {
                Ok(StreamEvent::MessageStop { usage, .. }) => Some(usage),
                _ => None,
            })
            .expect("mock response has terminal usage")
    }

    #[test]
    fn mock_usage_requires_explicit_reported_provenance() {
        let missing = MockResponse::text("missing");
        assert_eq!(
            terminal_usage(&missing).provenance,
            llm_adapter::types::UsageProvenance::Missing
        );

        let reported_zero = MockResponse::text_with_reported_zero("reported");
        let usage = terminal_usage(&reported_zero);
        assert_eq!(
            usage.provenance,
            llm_adapter::types::UsageProvenance::ReportedValid
        );
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
    }

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
    fn with_stop_reason_replaces_terminal_stop_reason() {
        use llm_adapter::types::StopReason as AdapterStop;

        let resp = MockResponse::text("hello").with_stop_reason(AdapterStop::ToolUse);
        let stop = resp.events.iter().rev().find_map(|e| match e {
            Ok(StreamEvent::MessageStop { stop_reason, .. }) => Some(stop_reason),
            _ => None,
        });
        assert!(matches!(stop, Some(AdapterStop::ToolUse)));
    }

    #[test]
    #[should_panic(expected = "no terminal MessageStop event")]
    fn with_stop_reason_panics_without_message_stop() {
        use llm_adapter::types::StopReason as AdapterStop;

        // rate_limit_error has no events — no MessageStop to mutate.
        let _ = MockResponse::rate_limit_error().with_stop_reason(AdapterStop::ToolUse);
    }

    #[tokio::test]
    async fn mock_client_returns_error_for_error_preset() {
        let client = MockLlmClient::with_single(MockResponse::rate_limit_error());
        let req = ChatRequest::builder("mock-model", 1024).build();
        let result = client.chat_stream(&req).await;
        assert!(matches!(result, Err(LlmError::RateLimit { .. })));
    }

    #[test]
    fn no_op_env_working_dir() {
        let env = NoOpEnv;
        assert_eq!(env.working_dir(), Path::new("/tmp"));
    }
}

#[cfg(test)]
mod stream_mock_tests {
    use super::*;
    use futures::StreamExt;

    #[tokio::test]
    async fn stall_after_emits_n_events_then_blocks() {
        let resp = MockResponse::stall_after(2);
        let client = MockLlmClient::with_single(resp);
        let req = ChatRequest::builder("m", 1).build();
        let mut handle = client.chat_stream(&req).await.unwrap();
        // First two events arrive
        let ev1 = handle.events().next().await;
        let ev2 = handle.events().next().await;
        assert!(ev1.is_some());
        assert!(ev2.is_some());
        // Third should block — wrap in timeout to verify
        let ev3 =
            tokio::time::timeout(std::time::Duration::from_millis(50), handle.events().next())
                .await;
        assert!(ev3.is_err(), "third event should block (stream stalled)");
    }

    #[tokio::test]
    async fn slow_stream_emits_events_at_interval() {
        let events = MockResponse::text("hi").events;
        let resp = MockResponse::slow_stream(events, 5);
        let client = MockLlmClient::with_single(resp);
        let req = ChatRequest::builder("m", 1).build();
        let mut handle = client.chat_stream(&req).await.unwrap();
        // Collect all events — they arrive at 5ms intervals
        let collected: Vec<_> = handle.events().collect().await;
        assert!(!collected.is_empty(), "should receive all events");
        assert!(
            collected.iter().any(|r| r.is_ok()),
            "should have at least one ok event"
        );
    }
}
