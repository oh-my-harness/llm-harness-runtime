use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use futures::future::BoxFuture;
use llm_harness_types::{
    AfterProviderResponseCtx, AfterProviderResponseHook, AfterToolCallCtx, AfterToolCallDecision,
    AfterToolCallHook, AfterTurnCtx, AfterTurnHook, BeforeProviderRequestCtx,
    BeforeProviderRequestHook,
};

use crate::tracer::{
    AttributeValue, CaptureLevel, SpanCaptureConfig, SpanEvent, SpanId, SpanKind, SpanStatus,
    TraceExporter, TraceId,
};

struct SpanState {
    span_id: SpanId,
    start_time: chrono::DateTime<Utc>,
    parent_span_id: Option<SpanId>,
}

struct SpanContextMap {
    task_span: Option<SpanState>,
    task_id: Option<String>,
    session_span: Option<SpanState>,
    session_id: Option<String>,
    model: Option<String>,
    /// Active turn spans keyed by turn_index; finalized by AfterTurnHook.
    turn_spans: HashMap<u32, SpanState>,
    /// LLM call spans started by BeforeProviderRequestHook; finalized by AfterProviderResponseHook.
    llm_spans: HashMap<u32, SpanState>,
    /// Completed spans awaiting the next export drain.
    completed: Vec<SpanEvent>,
    /// Trace ID shared across all spans in this context.
    trace_id: TraceId,
    /// Set by begin_turn(); read by BeforeProviderRequestHook and AfterProviderResponseHook.
    current_turn_index: Option<u32>,
}

impl SpanContextMap {
    fn new() -> Self {
        Self {
            task_span: None,
            task_id: None,
            session_span: None,
            session_id: None,
            model: None,
            turn_spans: HashMap::new(),
            llm_spans: HashMap::new(),
            completed: Vec::new(),
            trace_id: TraceId(uuid::Uuid::new_v4()),
            current_turn_index: None,
        }
    }

    fn complete_span(
        &mut self,
        state: SpanState,
        name: String,
        kind: SpanKind,
        attributes: HashMap<String, AttributeValue>,
        status: SpanStatus,
    ) {
        self.completed.push(SpanEvent {
            span_id: state.span_id,
            trace_id: self.trace_id,
            parent_span_id: state.parent_span_id,
            name,
            kind,
            start_time: state.start_time,
            end_time: Utc::now(),
            attributes,
            status,
        });
    }

    fn drain_completed(&mut self) -> Vec<SpanEvent> {
        std::mem::take(&mut self.completed)
    }
}

/// Converts core hook callbacks into `SpanEvent`s and writes them to a `TraceExporter`.
///
/// Implements four hooks:
/// - `BeforeProviderRequestHook` — records LLM span start time
/// - `AfterProviderResponseHook` — finalizes the LLM span (usage, latency, headers)
/// - `AfterToolCallHook`         — emits a tool span per tool execution
/// - `AfterTurnHook`             — finalizes the turn span
///
/// Task and session spans are driven by `begin_task`/`end_task` and
/// `begin_session`/`end_session`, called from `WorkflowEngine` (not from core hooks).
pub struct TracingHookAdapter {
    exporter: Arc<dyn TraceExporter>,
    capture_config: SpanCaptureConfig,
    active_spans: Mutex<SpanContextMap>,
}

impl TracingHookAdapter {
    /// Create a new adapter with the given exporter and capture configuration.
    pub fn new(exporter: Arc<dyn TraceExporter>, capture_config: SpanCaptureConfig) -> Self {
        Self {
            exporter,
            capture_config,
            active_spans: Mutex::new(SpanContextMap::new()),
        }
    }

    /// Begin a task span. Does not export until `end_task()` fires.
    pub fn begin_task(&self, task_id: &str) {
        let mut spans = self.active_spans.lock().unwrap();
        spans.task_id = Some(task_id.to_owned());
        spans.task_span = Some(SpanState {
            span_id: SpanId(uuid::Uuid::new_v4()),
            start_time: Utc::now(),
            parent_span_id: None,
        });
    }

    /// End the task span. The span moves to `completed` and will be exported on
    /// the next hook callback that calls `drain_completed()`.
    pub fn end_task(&self, task_id: &str, status: SpanStatus) {
        let mut spans = self.active_spans.lock().unwrap();
        if let Some(state) = spans.task_span.take() {
            let mut attrs = HashMap::new();
            attrs.insert(
                "agent.task_id".into(),
                AttributeValue::String(task_id.to_owned()),
            );
            spans.complete_span(state, "task".into(), SpanKind::Internal, attrs, status);
        }
    }

    /// Begin a session span (call when AgentHarness starts).
    pub fn begin_session(&self, session_id: &str, model: &str) {
        let mut spans = self.active_spans.lock().unwrap();
        let parent = spans.task_span.as_ref().map(|s| s.span_id);
        spans.session_id = Some(session_id.to_owned());
        spans.model = Some(model.to_owned());
        spans.session_span = Some(SpanState {
            span_id: SpanId(uuid::Uuid::new_v4()),
            start_time: Utc::now(),
            parent_span_id: parent,
        });
    }

    /// End the session span.
    pub fn end_session(&self, session_id: &str, status: SpanStatus) {
        let mut spans = self.active_spans.lock().unwrap();
        if let Some(state) = spans.session_span.take() {
            let mut attrs = HashMap::new();
            attrs.insert(
                "agent.session_id".into(),
                AttributeValue::String(session_id.to_owned()),
            );
            if let Some(ref m) = spans.model.clone() {
                attrs.insert("gen_ai.model".into(), AttributeValue::String(m.clone()));
            }
            spans.complete_span(state, "session".into(), SpanKind::Internal, attrs, status);
        }
    }

    /// Register the start of a turn (call before each turn from BeforeTurnHook or WorkflowEngine).
    /// `AfterTurnHook` finalizes it using the stored start time.
    pub fn begin_turn(&self, turn_index: u32) {
        let mut spans = self.active_spans.lock().unwrap();
        spans.current_turn_index = Some(turn_index);
        let parent = spans.session_span.as_ref().map(|s| s.span_id);
        spans.turn_spans.insert(
            turn_index,
            SpanState {
                span_id: SpanId(uuid::Uuid::new_v4()),
                start_time: Utc::now(),
                parent_span_id: parent,
            },
        );
    }
}

impl BeforeProviderRequestHook for TracingHookAdapter {
    fn before_request<'a>(&'a self, _ctx: BeforeProviderRequestCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let mut spans = self.active_spans.lock().unwrap();
            if let Some(turn_idx) = spans.current_turn_index {
                let parent = spans.turn_spans.get(&turn_idx).map(|s| s.span_id);
                // Record request dispatch time; AfterProviderResponseHook uses this start_time.
                spans.llm_spans.insert(
                    turn_idx,
                    SpanState {
                        span_id: SpanId(uuid::Uuid::new_v4()),
                        start_time: Utc::now(),
                        parent_span_id: parent,
                    },
                );
            }
        })
    }
}

impl AfterProviderResponseHook for TracingHookAdapter {
    fn after_response<'a>(&'a self, ctx: AfterProviderResponseCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let info = ctx.info;
            let mut attrs = HashMap::new();
            attrs.insert(
                "gen_ai.usage.input_tokens".into(),
                AttributeValue::Int(
                    info.usage
                        .as_ref()
                        .map(|u| u.input_tokens as i64)
                        .unwrap_or(0),
                ),
            );
            attrs.insert(
                "gen_ai.usage.output_tokens".into(),
                AttributeValue::Int(
                    info.usage
                        .as_ref()
                        .map(|u| u.output_tokens as i64)
                        .unwrap_or(0),
                ),
            );
            attrs.insert(
                "agent.latency_ms".into(),
                AttributeValue::Int(info.latency_ms as i64),
            );
            if let Some(ref usage) = info.usage {
                attrs.insert(
                    "gen_ai.usage.cache_read_tokens".into(),
                    AttributeValue::Int(usage.cache_read_tokens as i64),
                );
                attrs.insert(
                    "gen_ai.usage.cache_creation_tokens".into(),
                    AttributeValue::Int(usage.cache_creation_tokens as i64),
                );
            }
            if let Some(code) = info.status_code {
                attrs.insert("http.status_code".into(), AttributeValue::Int(code as i64));
            }
            if self.capture_config.level == CaptureLevel::Full {
                for (k, v) in &info.response_headers {
                    attrs.insert(
                        format!("http.response.header.{k}"),
                        AttributeValue::String(v.clone()),
                    );
                }
            }

            let completed = {
                let mut spans = self.active_spans.lock().unwrap();
                let turn_idx = spans.current_turn_index;

                // Pop the LLM span recorded by BeforeProviderRequestHook to preserve its start time.
                let llm_state = turn_idx
                    .and_then(|i| spans.llm_spans.remove(&i))
                    .unwrap_or_else(|| {
                        let parent = turn_idx
                            .and_then(|i| spans.turn_spans.get(&i))
                            .map(|s| s.span_id);
                        SpanState {
                            span_id: SpanId(uuid::Uuid::new_v4()),
                            start_time: Utc::now(),
                            parent_span_id: parent,
                        }
                    });

                spans.complete_span(
                    llm_state,
                    "llm_call".into(),
                    SpanKind::Client,
                    attrs,
                    SpanStatus::Ok,
                );
                spans.drain_completed()
            };

            if !completed.is_empty() {
                let _ = self.exporter.export(&completed).await;
            }
        })
    }
}

impl AfterToolCallHook for TracingHookAdapter {
    fn on_complete<'a>(
        &'a self,
        ctx: AfterToolCallCtx<'a>,
    ) -> BoxFuture<'a, AfterToolCallDecision> {
        Box::pin(async move {
            let is_error = ctx.result.is_err();
            let mut attrs = HashMap::from([
                (
                    "agent.tool_name".into(),
                    AttributeValue::String(ctx.tool_name.to_string()),
                ),
                (
                    "agent.tool_use_id".into(),
                    AttributeValue::String(ctx.tool_use_id.to_string()),
                ),
                ("agent.tool_is_error".into(), AttributeValue::Bool(is_error)),
                (
                    "agent.turn_index".into(),
                    AttributeValue::Int(ctx.turn_index as i64),
                ),
            ]);
            if let Err(e) = ctx.result {
                attrs.insert(
                    "error.message".into(),
                    AttributeValue::String(e.to_string()),
                );
            }

            let completed = {
                let mut spans = self.active_spans.lock().unwrap();
                let parent = spans.turn_spans.get(&ctx.turn_index).map(|s| s.span_id);
                let state = SpanState {
                    span_id: SpanId(uuid::Uuid::new_v4()),
                    start_time: Utc::now(),
                    parent_span_id: parent,
                };
                let status = if is_error {
                    SpanStatus::Error(ctx.result.as_ref().err().unwrap().to_string())
                } else {
                    SpanStatus::Ok
                };
                spans.complete_span(
                    state,
                    format!("tool.{}", ctx.tool_name),
                    SpanKind::Internal,
                    attrs,
                    status,
                );
                spans.drain_completed()
            };

            if !completed.is_empty() {
                let _ = self.exporter.export(&completed).await;
            }

            AfterToolCallDecision::Passthrough
        })
    }
}

impl AfterTurnHook for TracingHookAdapter {
    fn after_turn<'a>(&'a self, ctx: AfterTurnCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let attrs = HashMap::from([
                (
                    "agent.turn_index".into(),
                    AttributeValue::Int(ctx.turn_index as i64),
                ),
                (
                    "agent.new_message_count".into(),
                    AttributeValue::Int(ctx.new_messages.len() as i64),
                ),
            ]);

            let completed = {
                let mut spans = self.active_spans.lock().unwrap();
                // Pop the turn span created by begin_turn() to preserve its start time.
                let turn_state = spans.turn_spans.remove(&ctx.turn_index).unwrap_or_else(|| {
                    let parent = spans.session_span.as_ref().map(|s| s.span_id);
                    SpanState {
                        span_id: SpanId(uuid::Uuid::new_v4()),
                        start_time: Utc::now(),
                        parent_span_id: parent,
                    }
                });
                spans.complete_span(
                    turn_state,
                    format!("turn.{}", ctx.turn_index),
                    SpanKind::Internal,
                    attrs,
                    SpanStatus::Ok,
                );
                spans.drain_completed()
            };

            if !completed.is_empty() {
                let _ = self.exporter.export(&completed).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracer::test_util::{NoopExporter, RecordingExporter};

    #[test]
    fn adapter_construction() {
        let exporter = Arc::new(NoopExporter);
        let _adapter = TracingHookAdapter::new(exporter, SpanCaptureConfig::default());
    }

    #[test]
    fn begin_task_does_not_export_prematurely() {
        let exporter = Arc::new(RecordingExporter::new());
        let adapter = TracingHookAdapter::new(exporter.clone(), SpanCaptureConfig::default());

        adapter.begin_task("task-1");
        // begin_task only stores state — nothing is exported yet.
        assert!(exporter.exported_spans().is_empty());
    }

    #[test]
    fn end_task_moves_span_to_completed() {
        let exporter = Arc::new(RecordingExporter::new());
        let adapter = TracingHookAdapter::new(exporter.clone(), SpanCaptureConfig::default());

        adapter.begin_task("task-1");
        adapter.end_task("task-1", SpanStatus::Ok);
        // Span is in completed; exported on the next hook drain, not immediately.
        assert!(exporter.exported_spans().is_empty());
        assert_eq!(adapter.active_spans.lock().unwrap().completed.len(), 1);
    }
}
