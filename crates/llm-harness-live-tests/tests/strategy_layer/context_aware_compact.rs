//! Tests for context-aware compaction (Spec §7).
//!
//! Verifies that `context_aware_prompt_spec()` — injected via
//! `HarnessBuilder::compaction_prompt()` — produces a valid compaction
//! summary when `compact()` is called manually.
//!
//! - `context_aware_prompt_spec()` is set as the compaction prompt spec.
//! - A compaction query intent is provided via `set_compaction_query`.
//! - After 3 turns of conversation, manual `compact()` succeeds.
//! - `CompactionStart` and `CompactionEnd` events are emitted.

use std::sync::Arc;

use llm_harness_agent::ModelInfo;
use llm_harness_live_tests::skip_if_none;
use llm_harness_live_tests::{
    CapturedEvent, EventCapture, LiveProvider, MULTI_TURN, SINGLE_TURN, make_harness_with_session,
    with_timeout,
};
use llm_harness_strategy::context_aware_prompt_spec;

/// Build a Jsonl-backed harness with `context_aware_prompt_spec()` set as
/// the compaction prompt and a small context window so manual `compact()`
/// has content to compress.
async fn make_context_aware_harness(
    entry: &llm_harness_live_tests::LiveProviderEntry,
) -> (Arc<llm_harness_agent::AgentHarness>, tempfile::TempDir) {
    let (harness, _inspector, tmp) = make_harness_with_session(entry, |b| {
        b.max_tokens(1024)
            .model_info(Some(ModelInfo {
                context_window: 800,
                max_tokens: 1024,
            }))
            .compaction_model(
                entry.model.clone(),
                ModelInfo {
                    context_window: 200_000,
                    max_tokens: 4096,
                },
            )
            .compaction_keep_recent_tokens(Some(100))
            .compaction_reserve_tokens(Some(50))
            .compaction_prompt(Some(context_aware_prompt_spec()))
    })
    .await;

    // Set a task query intent so the context-aware template's {query}
    // placeholder is rendered with meaningful content.
    harness.set_compaction_query(Some(
        "User is collecting facts about space for a science report".to_string(),
    ));

    (harness, tmp)
}

/// Manual `compact()` with `context_aware_prompt_spec()` succeeds: the
/// compaction summary is generated without error, and both
/// `CompactionStart` and `CompactionEnd` events are emitted.
#[tokio::test]
async fn context_aware_compact_generates_summary() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let (harness, _tmp) = make_context_aware_harness(entry).await;

    // 3 turns to build up history with long-ish prompts.
    for i in 1..=3 {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(
            SINGLE_TURN,
            harness.prompt(format!(
                "Tell me fact #{i} about space. Give a detailed explanation with at least 3 sentences."
            )),
        )
        .await
        .expect("prompt failed");
        cap.settle().await;
    }

    let entries_before = harness.read_active_path().await.unwrap().len();
    assert!(entries_before >= 2, "should have entries before compaction");

    // Manually trigger compaction with the context-aware prompt spec.
    let cap = EventCapture::subscribe(&harness);
    let stats = with_timeout(MULTI_TURN, harness.compact())
        .await
        .expect("compaction with context_aware_prompt_spec should succeed");
    cap.settle().await;

    // Compaction reduced tokens.
    assert!(
        stats.tokens_after < stats.tokens_before,
        "tokens_after ({}) should be < tokens_before ({})",
        stats.tokens_after,
        stats.tokens_before
    );

    // Both compaction events were emitted.
    let events = cap.events();
    let has_start = events
        .iter()
        .any(|e| matches!(e, CapturedEvent::CompactionStart { .. }));
    let has_end = events
        .iter()
        .any(|e| matches!(e, CapturedEvent::CompactionEnd { .. }));
    assert!(has_start, "CompactionStart event was not emitted");
    assert!(has_end, "CompactionEnd event was not emitted");
}
