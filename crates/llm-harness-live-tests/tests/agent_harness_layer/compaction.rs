#![cfg(feature = "live-llm")]

//! Tests: AgentHarness compaction layer.
//!
//! Verifies:
//! - Manual `compact()` reduces tokens and emits CompactionStart/End events
//! - Auto-compact triggers at turn start when the token threshold is exceeded
//! - Compaction preserves recall of facts established before compaction
//! - Circuit breaker reset + compaction query APIs work without error

use llm_harness_agent::ModelInfo;
use llm_harness_live_tests::{
    CapturedEvent, EventCapture, LiveProvider, MULTI_TURN, SINGLE_TURN, make_harness_with,
    with_timeout,
};

/// Manual `compact()` on a 3-turn conversation reduces tokens and emits
/// both `CompactionStart` and `CompactionEnd` events.
#[tokio::test]
async fn manual_compact_reduces_tokens() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        b.model_info(Some(ModelInfo {
            context_window: 500,
            max_tokens: 1024,
        }))
        .compaction_model(
            &entry.model,
            ModelInfo {
                context_window: 200_000,
                max_tokens: 4096,
            },
        )
        .compaction_keep_recent_tokens(Some(100))
        .compaction_reserve_tokens(Some(50))
    })
    .await;

    // 3 turns to build up history.
    for i in 1..=3 {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(
            SINGLE_TURN,
            harness.prompt(format!("Tell me fact #{i} about space.")),
        )
        .await
        .expect("prompt failed");
        cap.settle().await;
    }

    let entries_before = harness.read_active_path().await.unwrap().len();
    assert!(entries_before >= 2, "should have entries before compaction");

    // Manually trigger compaction, capturing events.
    let cap = EventCapture::subscribe(&harness);
    let stats = with_timeout(MULTI_TURN, harness.compact())
        .await
        .expect("compaction failed");
    cap.settle().await;

    assert!(
        stats.tokens_after < stats.tokens_before,
        "tokens_after ({}) should be < tokens_before ({})",
        stats.tokens_after,
        stats.tokens_before
    );

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

/// With `auto_compact(true)` and a small context window, at least one
/// `CompactionEnd` event is emitted during 10 turns of conversation.
#[tokio::test]
async fn auto_compact_triggers_on_threshold() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    // context_window=1200: large enough for the compactor to find a valid
    // cut boundary (800 was too small — LLM single-turn responses exceeded
    // the usable window, causing NoValidBoundary on every turn).
    // max_tokens=256: short responses so compaction triggers within 5 turns
    // while leaving enough context for a valid cut point.
    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        b.model_info(Some(ModelInfo {
            context_window: 1200,
            max_tokens: 256,
        }))
        .compaction_model(
            &entry.model,
            ModelInfo {
                context_window: 200_000,
                max_tokens: 4096,
            },
        )
        .auto_compact(true)
        .compaction_keep_recent_tokens(Some(100))
        .compaction_reserve_tokens(Some(50))
    })
    .await;

    let mut compaction_count = 0usize;
    for i in 1..=5 {
        let cap = EventCapture::subscribe(&harness);
        match with_timeout(
            MULTI_TURN,
            harness.prompt(format!(
                "Tell me fact #{i} about oceans. One sentence only."
            )),
        )
        .await
        {
            Ok(_) => {}
            Err(e) => {
                eprintln!("Turn {i}: prompt error (continuing): {e}");
                continue;
            }
        }
        cap.settle().await;

        let count_this_turn = cap
            .events()
            .iter()
            .filter(|e| matches!(e, CapturedEvent::CompactionEnd { .. }))
            .count();
        compaction_count += count_this_turn;
    }

    assert!(
        compaction_count > 0,
        "expected at least one auto-compact during 5 turns, got {compaction_count}"
    );
}

/// After 3 turns establishing a specific fact, compaction preserves the
/// fact in the summarized history — the LLM still recalls it.
#[tokio::test]
async fn compaction_preserves_recall() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        b.model_info(Some(ModelInfo {
            context_window: 500,
            max_tokens: 1024,
        }))
        .compaction_model(
            &entry.model,
            ModelInfo {
                context_window: 200_000,
                max_tokens: 4096,
            },
        )
        .compaction_keep_recent_tokens(Some(100))
        .compaction_reserve_tokens(Some(50))
    })
    .await;

    // Turn 1: establish the secret fact early so it lands in the summarized portion.
    let cap = EventCapture::subscribe(&harness);
    with_timeout(
        SINGLE_TURN,
        harness.prompt("My secret code is 42. Please remember this number."),
    )
    .await
    .expect("prompt failed");
    cap.settle().await;

    // Turns 2–3: filler to build up history that will be compacted.
    for i in 2..=3 {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(
            SINGLE_TURN,
            harness.prompt(format!("Tell me fact #{i} about geography.")),
        )
        .await
        .expect("prompt failed");
        cap.settle().await;
    }

    // Compact the history.
    let _stats = with_timeout(MULTI_TURN, harness.compact())
        .await
        .expect("compaction failed");

    // Ask about the secret fact — it should survive compaction.
    let cap = EventCapture::subscribe(&harness);
    with_timeout(SINGLE_TURN, harness.prompt("What is my secret code?"))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let text = cap.texts();
    assert!(
        text.contains("42"),
        "expected response to contain '42' after compaction, got: {text}"
    );
}

/// The circuit breaker reset and compaction query APIs work without error.
/// A subsequent `compact()` succeeds after using them.
#[tokio::test]
async fn circuit_breaker_api_works() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    // No auto_compact — we want manual compact to have something to compress.
    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        b.model_info(Some(ModelInfo {
            context_window: 500,
            max_tokens: 1024,
        }))
        .compaction_model(
            &entry.model,
            ModelInfo {
                context_window: 200_000,
                max_tokens: 4096,
            },
        )
        .compaction_keep_recent_tokens(Some(100))
        .compaction_reserve_tokens(Some(50))
    })
    .await;

    // 3 turns to build up history.
    for i in 1..=3 {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(
            SINGLE_TURN,
            harness.prompt(format!("Tell me fact #{i} about history.")),
        )
        .await
        .expect("prompt failed");
        cap.settle().await;
    }

    // Exercise the circuit breaker + query APIs.
    harness.reset_circuit_breaker();
    harness.set_compaction_query(Some(
        "User is collecting history facts for a report".to_string(),
    ));

    // Manual compact should succeed after using the APIs.
    let stats = with_timeout(MULTI_TURN, harness.compact())
        .await
        .expect("compaction failed after circuit breaker reset");

    assert!(
        stats.compressed_entries > 0,
        "expected compressed_entries > 0, got {}",
        stats.compressed_entries
    );
}
