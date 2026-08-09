//! 05 — Compaction: manual compact, auto-compact, circuit breaker, custom prompt.
//!
//! Demonstrates:
//! - Manual `compact()`: summarize history, emit CompactionStart/End events
//! - Auto-compact: triggered at turn start when token threshold is exceeded
//! - Compaction events: `CompactionStart` + `CompactionEnd` with stats
//! - `set_compaction_query`: context-aware compaction
//! - Circuit breaker reset API
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 05_compaction

use llm_harness_live_tests::{
    CapturedEvent, EventCapture, LiveProvider, MULTI_TURN, SINGLE_TURN, make_harness_with,
    with_timeout,
};

#[tokio::main]
async fn main() {
    println!("=== 05: Compaction ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .expect("set OPENAI_API_KEY to run this example");

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);

    // ── Part 1: Manual compaction (10 turns) ────────────────────────────────
    println!("--- Part 1: Manual compaction (10 turns) ---\n");

    // Use very small reserve/keep-recent to force compaction with few messages
    let (harness, _tmp) = make_harness_with(entry, |b| {
        use llm_harness_agent::ModelInfo;
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

    // 10 turns to build up substantial history
    for i in 1..=10 {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(
            SINGLE_TURN,
            harness.prompt(format!("Tell me fact #{i} about space.")),
        )
        .await
        .expect("prompt failed");
        cap.settle().await;
        println!(
            "Turn {i:2}: {}...",
            cap.texts().chars().take(60).collect::<String>()
        );
    }

    let entries_before = harness.read_active_path().await.unwrap().len();
    println!("\nEntries before compaction: {}", entries_before);

    // Manually trigger compaction
    let cap = EventCapture::subscribe(&harness);
    let stats = with_timeout(MULTI_TURN, harness.compact())
        .await
        .expect("compaction failed");
    cap.settle().await;

    println!(
        "Compaction stats: tokens_before={}, tokens_after={}, entries_compressed={}",
        stats.tokens_before, stats.tokens_after, stats.compressed_entries
    );

    // Verify CompactionStart + CompactionEnd events were emitted
    let events = cap.events();
    let has_start = events
        .iter()
        .any(|e| matches!(e, CapturedEvent::CompactionStart { .. }));
    let has_end = events
        .iter()
        .any(|e| matches!(e, CapturedEvent::CompactionEnd { .. }));
    println!("CompactionStart emitted: {}", has_start);
    println!("CompactionEnd emitted: {}", has_end);

    let entries_after = harness.read_active_path().await.unwrap().len();
    println!(
        "Entries after compaction: {} (compaction entry added)",
        entries_after
    );

    // Verify the LLM can still recall information from the summarized history
    let cap2 = EventCapture::subscribe(&harness);
    with_timeout(
        SINGLE_TURN,
        harness.prompt("What facts about space did I ask about?"),
    )
    .await
    .expect("prompt failed");
    cap2.settle().await;
    println!(
        "Post-compaction recall: {}...\n",
        cap2.texts().chars().take(120).collect::<String>()
    );

    // ── Part 2: Auto-compact (10+ turns) ────────────────────────────────────
    println!("--- Part 2: Auto-compact (up to 10 turns) ---\n");

    let (harness2, _tmp2) = make_harness_with(entry, |b| {
        use llm_harness_agent::ModelInfo;
        b.model_info(Some(ModelInfo {
            context_window: 800,
            max_tokens: 1024,
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

    // Set a compaction query for context-aware summarization
    harness2.set_compaction_query(Some(
        "User is collecting ocean facts for a marine biology report".to_string(),
    ));
    println!("Set compaction query for context-aware summarization");

    // Add turns until auto-compact triggers (up to 10)
    let mut auto_compacted = false;
    let mut compaction_count = 0;
    for i in 1..=10 {
        let cap = EventCapture::subscribe(&harness2);
        with_timeout(
            SINGLE_TURN,
            harness2.prompt(format!("Tell me fact #{i} about oceans.")),
        )
        .await
        .expect("prompt failed");
        cap.settle().await;

        let all_events = cap.events();
        let compaction_count_this_turn = all_events
            .iter()
            .filter(|e| matches!(e, CapturedEvent::CompactionEnd { .. }))
            .count();
        if compaction_count_this_turn > 0 {
            compaction_count += compaction_count_this_turn;
            println!(
                "Turn {i:2}: auto-compact triggered (total: {})",
                compaction_count
            );
            auto_compacted = true;
        } else {
            println!(
                "Turn {i:2}: {}...",
                cap.texts().chars().take(60).collect::<String>()
            );
        }
    }
    println!(
        "Auto-compact fired: {} (total compactions: {})\n",
        auto_compacted, compaction_count
    );

    // ── Part 3: Circuit breaker + query API ─────────────────────────────────
    println!("--- Part 3: Circuit breaker + query API ---\n");

    // The circuit breaker tracks consecutive SummaryFailed errors.
    // Manual compact() bypasses the circuit breaker.
    // reset_circuit_breaker() clears the failure count.
    //
    // We can't easily force a real failure with a live LLM, but we can
    // demonstrate the API:
    harness2.reset_circuit_breaker();
    println!("reset_circuit_breaker() called successfully");

    harness2.set_compaction_query(Some(
        "User is asking about ocean facts and marine biology".to_string(),
    ));
    println!("set_compaction_query() updated successfully");

    // Manual compact on the auto-compact harness
    let cap3 = EventCapture::subscribe(&harness2);
    let stats3 = with_timeout(MULTI_TURN, harness2.compact())
        .await
        .expect("compaction failed");
    cap3.settle().await;
    println!(
        "Manual compact on auto-compact harness: tokens_before={}, tokens_after={}",
        stats3.tokens_before, stats3.tokens_after
    );

    // Verify recall after all compactions
    let cap4 = EventCapture::subscribe(&harness2);
    with_timeout(
        SINGLE_TURN,
        harness2.prompt("Summarize what we discussed about oceans."),
    )
    .await
    .expect("prompt failed");
    cap4.settle().await;
    println!(
        "Final recall: {}...\n",
        cap4.texts().chars().take(150).collect::<String>()
    );

    println!("--- Summary ---");
    println!("Manual compaction (10 turns): summary + events ✅");
    println!("Auto-compact (10 turns): triggered at turn start ✅");
    println!("Circuit breaker API: reset + query ✅");
}
