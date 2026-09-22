#![cfg(feature = "live-llm")]

//! Smoke tests for the live LLM test harness.
//!
//! Verifies that:
//! - ProviderFactory reads env vars and constructs real providers
//! - make_harness creates a working AgentHarness
//! - A single prompt round-trip completes without error
//! - EventCapture collects text deltas
//! - Assertion helpers work correctly

use llm_harness_live_tests::{
    CapturedEvent, EventCapture, LiveProvider, MULTI_TURN, assert_text_contains,
    assert_tool_was_called, make_harness, make_skill_dir, with_timeout,
};

// ── Unit tests for helpers (no LLM needed) ──────────────────────────────────

#[test]
fn provider_factory_reads_env_or_returns_none() {
    let provider = LiveProvider::from_env();
    let count = provider.all_providers().len();
    assert!(count <= 2, "at most 2 providers");
}

#[test]
fn assert_text_contains_finds_substring_case_insensitive() {
    assert_text_contains("Hello World", "hello");
    assert_text_contains("Hello World", "WORLD");
    assert_text_contains("Hello World", "o W");
}

#[test]
#[should_panic]
fn assert_text_contains_panics_on_missing_substring() {
    assert_text_contains("Hello", "goodbye");
}

#[test]
fn assert_tool_was_called_finds_tool() {
    let events = vec![CapturedEvent::ToolCallStart {
        tool_name: "bash".to_string(),
    }];
    assert_tool_was_called(&events, "bash");
}

#[test]
fn make_skill_dir_creates_skill_md() {
    let tmp = tempfile::tempdir().unwrap();
    let path = make_skill_dir(
        tmp.path(),
        "my-skill",
        "A test skill",
        "# My Skill\nBody text.",
    );
    assert!(path.join("SKILL.md").exists());
}

// ── Live LLM smoke tests ────────────────────────────────────────────────────

#[tokio::test]
async fn openai_provider_smoke() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let (harness, _inspector, _tmp) = make_harness(entry).await;
    let capture = EventCapture::subscribe(&harness);

    with_timeout(MULTI_TURN, harness.prompt("Say hello in one word."))
        .await
        .expect("prompt should succeed");

    capture.settle().await;
    let texts = capture.texts();
    assert!(!texts.is_empty(), "should have captured text deltas");
    println!("OpenAI response: {}", texts);
}

#[tokio::test]
async fn anthropic_provider_smoke() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.anthropic {
        Some(e) => e,
        None => {
            eprintln!("skipping: no anthropic provider");
            return;
        }
    };

    let (harness, _inspector, _tmp) = make_harness(entry).await;
    let capture = EventCapture::subscribe(&harness);

    with_timeout(MULTI_TURN, harness.prompt("Say hello in one word."))
        .await
        .expect("prompt should succeed");

    capture.settle().await;
    let texts = capture.texts();
    assert!(!texts.is_empty(), "should have captured text deltas");
    println!("Anthropic response: {}", texts);
}
