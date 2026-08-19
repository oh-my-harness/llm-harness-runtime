mod convert;
mod otlp;

pub use otlp::{OtlpConfig, OtlpTraceExporter};

use std::sync::Mutex;

use anyhow::Result;
use futures::future::BoxFuture;
use llm_harness_runtime_platform::tracer::{SpanEvent, TraceExporter};

/// In-memory `TraceExporter` for unit and integration testing.
///
/// Accumulates `SpanEvent` values in memory for assertions. Does NOT connect
/// to any external OTel collector. For production use, implement a real
/// `TraceExporter` using the opentelemetry-otlp crate.
#[doc(hidden)]
pub struct InMemoryTraceExporter {
    exported_spans: Mutex<Vec<SpanEvent>>,
}

impl InMemoryTraceExporter {
    /// Create a new empty exporter.
    pub fn new() -> Self {
        Self {
            exported_spans: Mutex::new(Vec::new()),
        }
    }

    /// Return all spans exported so far (for assertions in tests).
    pub fn exported_spans(&self) -> Vec<SpanEvent> {
        self.exported_spans.lock().unwrap().clone()
    }
}

impl Default for InMemoryTraceExporter {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceExporter for InMemoryTraceExporter {
    fn export<'a>(&'a self, events: &'a [SpanEvent]) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.exported_spans
                .lock()
                .unwrap()
                .extend(events.iter().cloned());
            Ok(())
        })
    }

    fn flush(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_runtime_platform::tracer::{SpanId, SpanKind, SpanStatus, TraceId};
    use std::collections::HashMap;

    fn make_span(name: &str) -> SpanEvent {
        SpanEvent {
            span_id: SpanId(uuid::Uuid::new_v4()),
            trace_id: TraceId(uuid::Uuid::new_v4()),
            parent_span_id: None,
            name: name.to_string(),
            kind: SpanKind::Internal,
            start_time: chrono::Utc::now(),
            end_time: chrono::Utc::now(),
            attributes: HashMap::new(),
            status: SpanStatus::Ok,
        }
    }

    #[tokio::test]
    async fn export_spans_accumulates() {
        let exporter = InMemoryTraceExporter::new();

        exporter
            .export(&[make_span("llm_call"), make_span("tool_exec")])
            .await
            .unwrap();
        assert_eq!(exporter.exported_spans().len(), 2);

        exporter.export(&[make_span("turn")]).await.unwrap();
        assert_eq!(exporter.exported_spans().len(), 3);
    }

    #[tokio::test]
    async fn empty_export_works() {
        let exporter = InMemoryTraceExporter::new();
        exporter.export(&[]).await.unwrap();
        assert!(exporter.exported_spans().is_empty());
    }
}
