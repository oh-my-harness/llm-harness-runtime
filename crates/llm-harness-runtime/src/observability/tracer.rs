use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use uuid::Uuid;

use anyhow::Result;

/// Span identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanId(pub Uuid);

/// Trace identifier (links spans across services).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceId(pub Uuid);

/// Span kind, aligned with OTel conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    Internal,
    Client,
    Server,
    Producer,
    Consumer,
}

/// Span status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpanStatus {
    Ok,
    Error(String),
    Unset,
}

/// Attribute value for span attributes.
#[derive(Debug, Clone)]
pub enum AttributeValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Array(Vec<AttributeValue>),
}

/// A single span event emitted by the tracing system.
#[derive(Debug, Clone)]
pub struct SpanEvent {
    /// Span identifier.
    pub span_id: SpanId,
    /// Trace identifier.
    pub trace_id: TraceId,
    /// Parent span identifier.
    pub parent_span_id: Option<SpanId>,
    /// Span name (e.g., `"llm_call"`, `"tool_exec"`, `"turn"`).
    pub name: String,
    /// Span kind.
    pub kind: SpanKind,
    /// Span start time.
    pub start_time: DateTime<Utc>,
    /// Span end time.
    pub end_time: DateTime<Utc>,
    /// Span attributes.
    pub attributes: HashMap<String, AttributeValue>,
    /// Span status.
    pub status: SpanStatus,
}

/// Trace exporter — backend-agnostic span export.
pub trait TraceExporter: Send + Sync {
    /// Export a batch of spans.
    fn export<'a>(&'a self, events: &'a [SpanEvent]) -> BoxFuture<'a, Result<()>>;

    /// Flush any buffered spans.
    fn flush(&self) -> BoxFuture<'_, Result<()>>;
}

/// Capture level per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureLevel {
    /// Metadata only: token counts, latency, stop_reason, errors.
    MetadataOnly,
    /// Metadata + full LLM prompt/response text.
    Full,
}

/// Span capture configuration.
pub struct SpanCaptureConfig {
    /// Capture level.
    pub level: CaptureLevel,
    /// Optional redaction hook (applied in `Full` level).
    pub redactor: Option<Arc<dyn SpanRedactor>>,
}

impl Default for SpanCaptureConfig {
    fn default() -> Self {
        Self {
            level: CaptureLevel::MetadataOnly,
            redactor: None,
        }
    }
}

/// Hook for redacting sensitive content from spans.
pub trait SpanRedactor: Send + Sync {
    /// Redact sensitive data from the span in-place.
    fn redact(&self, span: &mut SpanEvent);
}

/// Test utilities for `TraceExporter` — available in test builds only.
#[cfg(test)]
pub mod test_util {
    use std::sync::Mutex;

    use futures::future::BoxFuture;

    use super::{Result, SpanEvent, TraceExporter};

    /// `TraceExporter` that accumulates spans in memory for assertions.
    pub struct RecordingExporter {
        spans: Mutex<Vec<SpanEvent>>,
    }

    impl RecordingExporter {
        /// Create a new empty recording exporter.
        pub fn new() -> Self {
            Self {
                spans: Mutex::new(Vec::new()),
            }
        }

        /// Return all spans recorded so far.
        pub fn exported_spans(&self) -> Vec<SpanEvent> {
            self.spans.lock().unwrap().clone()
        }
    }

    impl Default for RecordingExporter {
        fn default() -> Self {
            Self::new()
        }
    }

    impl TraceExporter for RecordingExporter {
        fn export<'a>(&'a self, events: &'a [SpanEvent]) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.spans.lock().unwrap().extend(events.iter().cloned());
                Ok(())
            })
        }

        fn flush(&self) -> BoxFuture<'_, Result<()>> {
            Box::pin(async move { Ok(()) })
        }
    }

    /// No-op `TraceExporter` for compile-time interface checks.
    pub struct NoopExporter;

    impl TraceExporter for NoopExporter {
        fn export<'a>(&'a self, _events: &'a [SpanEvent]) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move { Ok(()) })
        }

        fn flush(&self) -> BoxFuture<'_, Result<()>> {
            Box::pin(async move { Ok(()) })
        }
    }
}
