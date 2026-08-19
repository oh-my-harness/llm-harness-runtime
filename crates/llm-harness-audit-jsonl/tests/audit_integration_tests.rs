//! Audit log integration tests — write → validate → tamper chain.
//!
//! Covers three scenarios:
//!   1. Full 5-event chain: write events, flush, validate hash chain.
//!   2. Sequential prev_hash: verify second entry's prev_hash equals first entry's hash.
//!   3. Tamper detection: modifying content after write fails validation.

use std::sync::Arc;

use chrono::Utc;
use llm_harness_audit_jsonl::JsonlAuditSink;
use llm_harness_types::EntryId;
use llm_harness_types::audit::{AuditDecision, AuditEntry, AuditEventType, AuditSink};

fn make_entry(trace_id: &str, event_type: AuditEventType, decision: AuditDecision) -> AuditEntry {
    AuditEntry {
        timestamp: Utc::now(),
        trace_id: trace_id.into(),
        session_id: EntryId::new(),
        task_id: Some("task-integration".into()),
        principal: "agent".into(),
        event_type,
        payload: serde_json::json!({"note": "integration test"}),
        decision: Some(decision),
        cost_delta: None,
        duration_ms: None,
        prev_hash: None,
        hash: String::new(),
    }
}

/// Full audit chain: 5 events across 2 trace IDs, flush, then validate.
#[tokio::test]
async fn full_audit_chain_write_and_validate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("full_chain.jsonl");
    let sink = JsonlAuditSink::new(&path);

    sink.record(make_entry(
        "trace-1",
        AuditEventType::Auth,
        AuditDecision::Allow,
    ))
    .await
    .unwrap();
    sink.record(make_entry(
        "trace-1",
        AuditEventType::StateTransition,
        AuditDecision::Allow,
    ))
    .await
    .unwrap();
    sink.record(make_entry(
        "trace-1",
        AuditEventType::ToolCall,
        AuditDecision::Allow,
    ))
    .await
    .unwrap();
    sink.record(make_entry(
        "trace-2",
        AuditEventType::HumanApproval,
        AuditDecision::Deny {
            reason: "user rejected".into(),
        },
    ))
    .await
    .unwrap();
    sink.record(make_entry(
        "trace-2",
        AuditEventType::BudgetExceeded,
        AuditDecision::Deny {
            reason: "limit reached".into(),
        },
    ))
    .await
    .unwrap();
    sink.flush().await.unwrap();

    let count = JsonlAuditSink::validate(&path).await.unwrap();
    assert_eq!(count, 5);

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 5);

    for (i, line) in lines.iter().enumerate() {
        let parsed: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("line {i} not valid JSON: {e}"));
        assert!(parsed.get("hash").is_some(), "line {i} missing hash");
        assert!(
            parsed.get("timestamp").is_some(),
            "line {i} missing timestamp"
        );
        assert!(
            parsed.get("event_type").is_some(),
            "line {i} missing event_type"
        );
    }
}

/// Each entry's prev_hash must equal the previous entry's hash.
#[tokio::test]
async fn hash_chain_is_sequential() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chain.jsonl");
    let sink = JsonlAuditSink::new(&path);

    sink.record(make_entry("t1", AuditEventType::Auth, AuditDecision::Allow))
        .await
        .unwrap();
    sink.record(make_entry(
        "t1",
        AuditEventType::ToolCall,
        AuditDecision::Allow,
    ))
    .await
    .unwrap();
    sink.flush().await.unwrap();

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    let lines: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert_eq!(lines.len(), 2);

    // First entry: prev_hash is null or absent.
    let prev0 = lines[0].get("prev_hash").and_then(|v| v.as_str());
    assert!(
        prev0.is_none() || prev0 == Some(""),
        "first entry prev_hash should be empty, got {prev0:?}"
    );

    // Second entry: prev_hash equals first entry's hash.
    let hash0 = lines[0]["hash"].as_str().expect("first entry has hash");
    let prev1 = lines[1]["prev_hash"]
        .as_str()
        .expect("second entry has prev_hash");
    assert_eq!(
        hash0, prev1,
        "prev_hash of entry 1 should equal hash of entry 0"
    );
}

/// Modified content after write must fail validation.
#[tokio::test]
async fn corrupted_content_breaks_hash_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt.jsonl");
    let sink = JsonlAuditSink::new(&path);

    sink.record(make_entry("t1", AuditEventType::Auth, AuditDecision::Allow))
        .await
        .unwrap();
    sink.record(make_entry(
        "t1",
        AuditEventType::ToolCall,
        AuditDecision::Allow,
    ))
    .await
    .unwrap();
    sink.flush().await.unwrap();

    // Corrupt the second line without recomputing its hash.
    let content = tokio::fs::read_to_string(&path).await.unwrap();
    let corrupted = content.replace("\"ToolCall\"", "\"Auth\"");
    tokio::fs::write(&path, corrupted).await.unwrap();

    let result = JsonlAuditSink::validate(&path).await;
    assert!(result.is_err(), "tampered log must fail validation");
}

/// Verify that AuditSink is object-safe (can be held as Arc<dyn AuditSink>).
#[test]
fn audit_sink_is_object_safe() {
    let sink = JsonlAuditSink::new("/tmp/unused_object_safe_test.jsonl");
    let _: Arc<dyn AuditSink> = Arc::new(sink);
}
