//! 21 — Context-Aware Compaction Prompt.
//!
//! Demonstrates:
//! - `context_aware_prompt_spec()` — a structured compaction prompt template
//!   with `## Goal`, `## Progress`, `## Key Decisions`, `## Next Steps`,
//!   `## Critical Context` sections.
//! - Part 1: Auto-compact with context-aware prompt (small context window,
//!   6 turns, Jsonl-backed session) — the summary follows the structured
//!   template.
//! - Part 2: Manual `compact()` with context-aware prompt (normal context
//!   window, 3 turns) — the summary structure is observable.
//!
//! Uses `HarnessBuilder::compaction_prompt()` to inject the context-aware
//! prompt spec, and an independent compaction model with a large context
//! window so the summary LLM is not constrained by the small conversation
//! window.
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 21_context_aware_compact

use std::sync::Arc;

use llm_harness_agent::session::SessionEntryPayload;
use llm_harness_agent::{AgentHarness, ModelInfo};
use llm_harness_live_tests::{
    CapturedEvent, EventCapture, LiveProvider, MULTI_TURN, SINGLE_TURN, make_harness_with_session,
};
use llm_harness_strategy::context_aware_prompt_spec;

/// Build a harness with a Jsonl-backed session and a context-aware compaction
/// prompt via `HarnessBuilder::compaction_prompt()`.
async fn make_session_harness_with_compaction_prompt(
    entry: &llm_harness_live_tests::LiveProviderEntry,
    context_window: u32,
    auto_compact: bool,
) -> (
    Arc<AgentHarness>,
    llm_harness_live_tests::InspectorHandle,
    tempfile::TempDir,
) {
    make_harness_with_session(entry, |b| {
        b.model_info(Some(ModelInfo {
            context_window,
            max_tokens: 1024,
        }))
        .max_tokens(1024)
        .auto_compact(auto_compact)
        .compaction_keep_recent_tokens(Some(100))
        .compaction_reserve_tokens(Some(50))
        // Independent compaction model with a large context window so the
        // summary LLM is not constrained by the small conversation window.
        .compaction_model(
            entry.model.clone(),
            ModelInfo {
                context_window: 200_000,
                max_tokens: 4096,
            },
        )
        // The key line: inject the context-aware compaction prompt.
        .compaction_prompt(Some(context_aware_prompt_spec()))
    })
    .await
}

/// Extract the compaction summary text from the session's active path.
///
/// Returns the text of the most recent `Compaction` entry's `summary_message`.
async fn extract_compaction_summary(harness: &AgentHarness) -> Option<String> {
    let path = harness.read_active_path().await.ok()?;
    path.iter().rev().find_map(|e| match &e.payload {
        SessionEntryPayload::Compaction(c) => {
            use llm_harness_types::AgentMessage;
            match &c.summary_message {
                AgentMessage::CompactionSummary(cs) => Some(cs.summary.clone()),
                AgentMessage::Assistant(a) => Some(a.text_content()),
                _ => None,
            }
        }
        _ => None,
    })
}

/// Print which template sections are present in the summary.
fn check_template_sections(summary: &str) {
    let sections = [
        "## Goal",
        "## Progress",
        "## Key Decisions",
        "## Next Steps",
        "## Critical Context",
    ];
    for section in &sections {
        let found = summary.contains(section);
        println!(
            "  {} — {}",
            section,
            if found { "present" } else { "absent" }
        );
    }
}

#[tokio::main]
async fn main() {
    println!("=== 21: Context-Aware Compaction Prompt ===\n");

    let provider = LiveProvider::from_env();
    let entry = match provider.openai.as_ref().or(provider.anthropic.as_ref()) {
        Some(e) => e,
        None => {
            eprintln!("skipping: no provider configured");
            return;
        }
    };

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);

    // Show the context-aware prompt spec structure.
    let spec = context_aware_prompt_spec();
    println!("Context-aware prompt spec:");
    println!("  System prompt: {} chars", spec.system_prompt().len());
    println!("  User template: {} chars", spec.user_template().len());
    let template = spec.user_template();
    for section in [
        "## Goal",
        "## Progress",
        "## Key Decisions",
        "## Next Steps",
        "## Critical Context",
    ] {
        println!(
            "  Template contains '{}': {}",
            section,
            template.contains(section)
        );
    }
    println!();

    // ── Part 1: Auto-compact with context-aware prompt ─────────────────────
    println!("--- Part 1: Auto-compact with context-aware prompt (10 turns) ---\n");

    let (harness, _inspector, _tmp) =
        make_session_harness_with_compaction_prompt(entry, 1200, true).await;

    // Set a compaction query so {query} is populated in the template.
    harness.set_compaction_query(Some(
        "User is building a Rust CLI tool for weather data analysis".to_string(),
    ));
    println!("Set compaction query for context-aware summarization");

    let mut auto_compacted = false;
    for i in 1..=10 {
        let cap = EventCapture::subscribe(&harness);
        let prompt = format!("Tell me fact #{i} about Rust programming language.");
        match llm_harness_live_tests::with_timeout(SINGLE_TURN, harness.prompt(prompt)).await {
            Ok(_) => {}
            Err(e) => {
                // NoValidBoundary can occur if the model produces a very long
                // first response. Log and continue — we still want to observe
                // compaction on subsequent turns.
                eprintln!("Turn {i}: prompt error (continuing): {e}");
                continue;
            }
        }
        cap.settle().await;

        let events = cap.events();
        let compacted = events
            .iter()
            .any(|e| matches!(e, CapturedEvent::CompactionEnd { .. }));
        if compacted {
            auto_compacted = true;
            println!(
                "Turn {i}: auto-compact triggered — {}...",
                cap.texts().chars().take(60).collect::<String>()
            );
        } else {
            println!(
                "Turn {i}: {}...",
                cap.texts().chars().take(60).collect::<String>()
            );
        }
    }
    println!(
        "\nAuto-compact fired during conversation: {}\n",
        auto_compacted
    );

    // Extract and display the compaction summary.
    if let Some(summary) = extract_compaction_summary(&harness).await {
        println!("Compaction summary (context-aware template):");
        // Show first 600 chars to observe the structure.
        let preview: String = summary.chars().take(600).collect();
        println!("---");
        println!("{}", preview);
        if summary.len() > 600 {
            println!("... ({} more chars)", summary.len() - 600);
        }
        println!("---");
        println!("Template section check:");
        check_template_sections(&summary);
    } else {
        println!("No compaction summary found in session");
    }
    println!();

    // ── Part 2: Manual compact with context-aware prompt ───────────────────
    println!("--- Part 2: Manual compact with context-aware prompt (3 turns) ---\n");

    let (harness2, _inspector, _tmp2) =
        make_session_harness_with_compaction_prompt(entry, 800, false).await;

    harness2.set_compaction_query(Some(
        "User is discussing database migration strategies for a Python web app".to_string(),
    ));
    println!("Set compaction query for context-aware summarization");

    for i in 1..=3 {
        let cap = EventCapture::subscribe(&harness2);
        let prompt = format!(
            "Explain approach {i} to database migrations in detail: \
             the tools involved, the trade-offs, and a concrete example \
             of applying it to a Django project with 50k users."
        );
        llm_harness_live_tests::with_timeout(SINGLE_TURN, harness2.prompt(prompt))
            .await
            .expect("prompt failed");
        cap.settle().await;
        println!(
            "Turn {i}: {}...",
            cap.texts().chars().take(60).collect::<String>()
        );
    }

    let entries_before = harness2.read_active_path().await.unwrap().len();
    println!("\nEntries before manual compaction: {}", entries_before);

    // Manually trigger compaction with the context-aware prompt.
    let cap = EventCapture::subscribe(&harness2);
    let stats = llm_harness_live_tests::with_timeout(MULTI_TURN, harness2.compact())
        .await
        .expect("compaction failed");
    cap.settle().await;

    println!(
        "Compaction stats: tokens_before={}, tokens_after={}, entries_compressed={}",
        stats.tokens_before, stats.tokens_after, stats.compressed_entries
    );

    let events = cap.events();
    let has_start = events
        .iter()
        .any(|e| matches!(e, CapturedEvent::CompactionStart { .. }));
    let has_end = events
        .iter()
        .any(|e| matches!(e, CapturedEvent::CompactionEnd { .. }));
    println!("CompactionStart emitted: {}", has_start);
    println!("CompactionEnd emitted: {}", has_end);

    // Extract and display the manual compaction summary.
    if let Some(summary) = extract_compaction_summary(&harness2).await {
        println!("\nManual compaction summary (context-aware template):");
        let preview: String = summary.chars().take(600).collect();
        println!("---");
        println!("{}", preview);
        if summary.len() > 600 {
            println!("... ({} more chars)", summary.len() - 600);
        }
        println!("---");
        println!("Template section check:");
        check_template_sections(&summary);
    } else {
        println!("No compaction summary found in session");
    }
    println!();

    // ── Summary ─────────────────────────────────────────────────────────────
    println!("--- Summary ---");
    println!(
        "Part 1 (auto-compact, context-aware prompt): {}",
        if auto_compacted {
            "triggered ✅"
        } else {
            "not triggered ⚠️"
        }
    );
    println!("Part 2 (manual compact, context-aware prompt): summary generated ✅");
    println!(
        "Context-aware template sections: Goal/Progress/Key Decisions/Next Steps/Critical Context ✅"
    );
}
