//! Tests: provider connection, streaming, error propagation.

use llm_harness_live_tests::{
    CapturedEvent, EventCapture, LiveProvider, LiveProviderEntry, MULTI_TURN, make_harness,
    make_harness_with, with_timeout,
};
use llm_harness_loop::OpenAIProvider;

/// OpenAI provider connects and returns non-empty text.
#[tokio::test]
async fn openai_provider_connects_and_returns_text() {
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

    with_timeout(MULTI_TURN, harness.prompt("Say hello."))
        .await
        .expect("prompt should succeed");

    capture.settle().await;
    let text = capture.texts();
    assert!(!text.is_empty(), "should have received text from LLM");
}

/// Anthropic provider connects and returns non-empty text.
#[tokio::test]
async fn anthropic_provider_connects_and_returns_text() {
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

    with_timeout(MULTI_TURN, harness.prompt("Say hello."))
        .await
        .expect("prompt should succeed");

    capture.settle().await;
    let text = capture.texts();
    assert!(!text.is_empty(), "should have received text from LLM");
}

/// Streaming produces a sequence of TextDelta events ending with Settled.
#[tokio::test]
async fn streaming_produces_text_deltas_then_settled() {
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

    with_timeout(MULTI_TURN, harness.prompt("Count from 1 to 5."))
        .await
        .expect("prompt should succeed");

    capture.settle().await;
    let events = capture.events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, CapturedEvent::TextDelta(_))),
        "should have text delta events"
    );
    assert!(
        events.iter().any(|e| matches!(e, CapturedEvent::Settled)),
        "should have Settled event"
    );
}

/// Invalid API key surfaces as error, not panic.
///
/// Only runs against the official OpenAI API (no `OPENAI_API_BASE` set),
/// because custom endpoints may not validate API keys.
#[tokio::test]
async fn invalid_api_key_surfaces_as_error() {
    // Skip when using a custom endpoint — it may accept any key.
    if !std::env::var("OPENAI_API_BASE")
        .unwrap_or_default()
        .is_empty()
    {
        eprintln!("skipping: custom endpoint may not validate API keys");
        return;
    }

    let bad_client = OpenAIProvider::builder("sk-invalid-key").build();
    let entry = LiveProviderEntry {
        name: "openai".to_string(),
        model: "gpt-4o".to_string(),
        client: std::sync::Arc::new(bad_client),
    };

    let result = with_timeout(MULTI_TURN, async {
        let (harness, _inspector, _tmp) = make_harness_with(&entry, |b| b).await;
        harness.prompt("Hello").await
    })
    .await;

    assert!(
        result.is_err(),
        "invalid API key should produce an error, not succeed"
    );
}
