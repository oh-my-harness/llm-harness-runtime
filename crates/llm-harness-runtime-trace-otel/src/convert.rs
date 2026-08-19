use std::collections::HashMap;

use llm_harness_runtime_platform::tracer::{AttributeValue, SpanEvent, SpanKind, SpanStatus};
use opentelemetry::trace::{
    SpanContext, SpanId, SpanKind as OtelSpanKind, Status as OtelStatus, TraceFlags, TraceId,
    TraceState,
};
use opentelemetry::{Array, KeyValue, Value};
use opentelemetry_sdk::trace::SpanData;

pub(crate) fn convert_span(event: &SpanEvent) -> SpanData {
    let trace_id = TraceId::from_bytes(*event.trace_id.0.as_bytes());
    let span_id = SpanId::from_bytes(event.span_id.0.as_bytes()[..8].try_into().unwrap());
    let parent_span_id = match event.parent_span_id {
        Some(p) => SpanId::from_bytes(p.0.as_bytes()[..8].try_into().unwrap()),
        None => SpanId::from_bytes([0u8; 8]),
    };
    let span_context = SpanContext::new(
        trace_id,
        span_id,
        TraceFlags::new(1).with_sampled(true),
        false,
        TraceState::default(),
    );
    let kind = match event.kind {
        SpanKind::Internal => OtelSpanKind::Internal,
        SpanKind::Client => OtelSpanKind::Client,
        SpanKind::Server => OtelSpanKind::Server,
        SpanKind::Producer => OtelSpanKind::Producer,
        SpanKind::Consumer => OtelSpanKind::Consumer,
    };
    let status = match &event.status {
        SpanStatus::Ok => OtelStatus::Unset,
        SpanStatus::Unset => OtelStatus::Unset,
        SpanStatus::Error(msg) => OtelStatus::Error {
            description: msg.clone().into(),
        },
    };
    SpanData {
        span_context,
        parent_span_id,
        parent_span_is_remote: false,
        span_kind: kind,
        name: event.name.clone().into(),
        start_time: event.start_time.into(),
        end_time: event.end_time.into(),
        attributes: convert_attributes(&event.attributes),
        dropped_attributes_count: 0,
        events: Default::default(),
        links: Default::default(),
        status,
        instrumentation_scope: Default::default(),
    }
}

pub(crate) fn convert_attributes(attrs: &HashMap<String, AttributeValue>) -> Vec<KeyValue> {
    attrs
        .iter()
        .map(|(k, v)| KeyValue::new(k.clone(), convert_value(v)))
        .collect()
}

fn convert_value(v: &AttributeValue) -> Value {
    match v {
        AttributeValue::String(s) => Value::String(s.clone().into()),
        AttributeValue::Int(i) => Value::I64(*i),
        AttributeValue::Float(f) => Value::F64(*f),
        AttributeValue::Bool(b) => Value::Bool(*b),
        AttributeValue::Array(arr) => {
            // OTel Array is homogeneous (bool/i64/f64/String). Convert all
            // elements to string representation to preserve mixed-type data.
            let strings: Vec<opentelemetry::StringValue> = arr
                .iter()
                .map(|v| convert_value(v).as_str().to_string().into())
                .collect();
            Value::Array(Array::String(strings))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_runtime_platform::tracer::{
        SpanId as PlatformSpanId, TraceId as PlatformTraceId,
    };
    use opentelemetry::trace::{SpanId as OtelSpanId, TraceId as OtelTraceId};

    fn make_span() -> SpanEvent {
        SpanEvent {
            span_id: PlatformSpanId(uuid::Uuid::nil()),
            trace_id: PlatformTraceId(uuid::Uuid::nil()),
            parent_span_id: None,
            name: "test_span".to_string(),
            kind: SpanKind::Client,
            start_time: chrono::DateTime::from_timestamp(1700000000, 0).unwrap(),
            end_time: chrono::DateTime::from_timestamp(1700000001, 0).unwrap(),
            attributes: HashMap::new(),
            status: SpanStatus::Ok,
        }
    }

    #[test]
    fn convert_span_maps_name_and_kind() {
        let event = make_span();
        let data = convert_span(&event);
        assert_eq!(data.name, "test_span");
        assert_eq!(data.span_kind, OtelSpanKind::Client);
    }

    #[test]
    fn convert_span_maps_ids_from_uuid_bytes() {
        let uuid = uuid::Uuid::new_v4();
        let mut event = make_span();
        event.trace_id = PlatformTraceId(uuid);
        event.span_id = PlatformSpanId(uuid);
        let data = convert_span(&event);
        assert_eq!(
            data.span_context.trace_id(),
            OtelTraceId::from_bytes(*uuid.as_bytes())
        );
        assert_eq!(
            data.span_context.span_id(),
            OtelSpanId::from_bytes(uuid.as_bytes()[..8].try_into().unwrap())
        );
    }

    #[test]
    fn convert_span_parent_none_is_invalid_span_id() {
        let event = make_span();
        let data = convert_span(&event);
        assert_eq!(data.parent_span_id, OtelSpanId::from_bytes([0u8; 8]));
    }

    #[test]
    fn convert_span_parent_some_maps_bytes() {
        let parent_uuid = uuid::Uuid::new_v4();
        let mut event = make_span();
        event.parent_span_id = Some(PlatformSpanId(parent_uuid));
        let data = convert_span(&event);
        assert_eq!(
            data.parent_span_id,
            OtelSpanId::from_bytes(parent_uuid.as_bytes()[..8].try_into().unwrap())
        );
    }

    #[test]
    fn convert_span_status_ok_maps_to_unset() {
        let event = make_span();
        let data = convert_span(&event);
        assert_eq!(data.status, OtelStatus::Unset);
    }

    #[test]
    fn convert_span_status_error_maps_to_error() {
        let mut event = make_span();
        event.status = SpanStatus::Error("something failed".to_string());
        let data = convert_span(&event);
        assert!(matches!(data.status, OtelStatus::Error { .. }));
    }

    #[test]
    fn convert_span_sampled_flag_is_true() {
        let event = make_span();
        let data = convert_span(&event);
        assert!(data.span_context.trace_flags().is_sampled());
    }

    #[test]
    fn convert_attributes_maps_all_variants() {
        let mut attrs = HashMap::new();
        attrs.insert("s".to_string(), AttributeValue::String("hello".to_string()));
        attrs.insert("i".to_string(), AttributeValue::Int(42));
        attrs.insert("f".to_string(), AttributeValue::Float(3.5));
        attrs.insert("b".to_string(), AttributeValue::Bool(true));
        attrs.insert(
            "arr".to_string(),
            AttributeValue::Array(vec![AttributeValue::Int(1), AttributeValue::Int(2)]),
        );

        let kvs = convert_attributes(&attrs);
        assert_eq!(kvs.len(), 5);

        let map: HashMap<&str, &Value> =
            kvs.iter().map(|kv| (kv.key.as_str(), &kv.value)).collect();

        assert!(matches!(map["s"], Value::String(_)));
        assert!(matches!(map["i"], Value::I64(42)));
        assert!(matches!(map["f"], Value::F64(_)));
        assert!(matches!(map["b"], Value::Bool(true)));
        assert!(matches!(map["arr"], Value::Array(_)));
    }

    #[test]
    fn convert_attributes_empty_returns_empty_vec() {
        let attrs = HashMap::new();
        let kvs = convert_attributes(&attrs);
        assert!(kvs.is_empty());
    }
}
