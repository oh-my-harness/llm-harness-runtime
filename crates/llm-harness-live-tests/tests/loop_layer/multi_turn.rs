//! Tests: multi-turn conversation via prompt().
//!
//! follow_up() + continue_run() is not the correct API for multi-turn
//! conversation — follow_up is designed for injecting messages during a
//! run (after should_stop), not between runs. Use prompt() for multi-turn:
//! it preserves session history and handles queued_next_turn.

use llm_harness_live_tests::{
    EventCapture, LiveProvider, MULTI_TURN, SINGLE_TURN, make_harness, with_timeout,
};

/// Follow-up resumes a settled conversation via prompt().
#[tokio::test]
async fn prompt_continues_after_settled() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let (harness, _inspector, _tmp) = make_harness(entry).await;

    // Turn 1: initial prompt.
    let capture1 = EventCapture::subscribe(&harness);
    with_timeout(SINGLE_TURN, harness.prompt("My favorite color is blue."))
        .await
        .expect("first prompt failed");
    capture1.settle().await;
    assert!(!capture1.texts().is_empty(), "turn 1 should produce text");

    // Turn 2: prompt() resumes the conversation with history preserved.
    let capture2 = EventCapture::subscribe(&harness);
    with_timeout(MULTI_TURN, harness.prompt("What is my favorite color?"))
        .await
        .expect("second prompt failed");
    capture2.settle().await;
    assert!(!capture2.texts().is_empty(), "turn 2 should produce text");
}

/// prompt() preserves session history — the LLM remembers prior turns.
#[tokio::test]
async fn prompt_preserves_session_history() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let (harness, _inspector, _tmp) = make_harness(entry).await;

    // Turn 1: tell the LLM a secret word.
    let capture1 = EventCapture::subscribe(&harness);
    with_timeout(
        SINGLE_TURN,
        harness.prompt("The secret word is 'pineapple'. Remember it."),
    )
    .await
    .expect("first prompt failed");
    capture1.settle().await;
    assert!(!capture1.texts().is_empty());

    // Turn 2: ask about the secret word.
    let capture2 = EventCapture::subscribe(&harness);
    with_timeout(
        MULTI_TURN,
        harness.prompt("What is the secret word I told you?"),
    )
    .await
    .expect("second prompt failed");
    capture2.settle().await;

    let text = capture2.texts().to_lowercase();
    assert!(
        text.contains("pineapple"),
        "LLM should remember the secret word from the previous turn. Got: {}",
        capture2.texts()
    );
}

/// set_system_prompt takes effect on the next prompt().
#[tokio::test]
async fn set_system_prompt_affects_next_turn() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let (harness, _inspector, _tmp) = make_harness(entry).await;

    // Turn 1: normal prompt.
    let capture1 = EventCapture::subscribe(&harness);
    with_timeout(SINGLE_TURN, harness.prompt("Say hello."))
        .await
        .expect("first prompt failed");
    capture1.settle().await;
    assert!(!capture1.texts().is_empty());

    // Change system prompt to pirate mode.
    harness.set_system_prompt(Some(
        "You are a pirate. Always start your response with 'Arrr'.".to_string(),
    ));

    // Turn 2: should speak like a pirate.
    let capture2 = EventCapture::subscribe(&harness);
    with_timeout(MULTI_TURN, harness.prompt("Say hello again."))
        .await
        .expect("second prompt failed");
    capture2.settle().await;

    let text = capture2.texts().to_lowercase();
    assert!(
        text.contains("arrr") || text.contains("arr"),
        "LLM should speak like a pirate after system prompt change. Got: {}",
        capture2.texts()
    );
}
