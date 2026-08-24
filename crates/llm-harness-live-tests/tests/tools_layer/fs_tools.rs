//! Tests for FS tools (Spec §7).
//!
//! Verifies bash/read/write/edit/grep/glob tools via real LLM calls.

#![cfg(feature = "live-llm")]

use std::sync::Arc;

use llm_harness_live_tests::{
    EventCapture, LiveProvider, SINGLE_TURN, assert_tool_was_called, make_harness_with_env,
    skip_if_none, with_timeout,
};
use llm_harness_tools::{FileSnapshotStore, FsToolsPlugin};
use parking_lot::RwLock;

// ── Helpers ─────────────────────────────────────────────────────────────

/// Build a harness with FsToolsPlugin installed and a shared snapshot store.
async fn make_fs_harness(
    entry: &llm_harness_live_tests::LiveProviderEntry,
) -> (
    Arc<llm_harness_agent::AgentHarness>,
    Arc<dyn llm_harness_types::ExecutionEnv>,
    llm_harness_live_tests::InspectorHandle,
    tempfile::TempDir,
) {
    let store = Arc::new(RwLock::new(FileSnapshotStore::new()));
    make_harness_with_env(entry, |b| b.install(&FsToolsPlugin::new(Some(store)))).await
}

// ── Tests ───────────────────────────────────────────────────────────────

/// The LLM calls `bash` with `echo hello-from-test` and the tool executes
/// successfully, returning the merged stdout/stderr.
#[tokio::test]
async fn bash_executes_and_returns_output() {
    let provider = LiveProvider::from_env();
    let entry = provider.openai.as_ref().or(provider.anthropic.as_ref());
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let (harness, _env, _inspector, _tmp) = make_fs_harness(entry).await;

    let cap = EventCapture::subscribe(&harness);

    with_timeout(
        SINGLE_TURN,
        harness.prompt(
            r#"Use the bash tool with command "echo hello-from-test" and then tell me the output."#,
        ),
    )
    .await
    .expect("prompt should succeed");

    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "bash");
}

/// The LLM calls `read` on a pre-created file and retrieves its content.
#[tokio::test]
async fn read_returns_file_content() {
    let provider = LiveProvider::from_env();
    let entry = provider.openai.as_ref().or(provider.anthropic.as_ref());
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let (harness, env, _inspector, _tmp) = make_fs_harness(entry).await;

    // Pre-create a file with known content.
    std::fs::write(env.working_dir().join("greeting.txt"), "hello-from-test\n").unwrap();

    let cap = EventCapture::subscribe(&harness);

    with_timeout(
        SINGLE_TURN,
        harness.prompt(
            "Use the read tool to read the file \"greeting.txt\" and then tell me its content.",
        ),
    )
    .await
    .expect("prompt should succeed");

    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "read");
}

/// The LLM calls `write` to create a new file on disk.
#[tokio::test]
async fn write_creates_file() {
    let provider = LiveProvider::from_env();
    let entry = provider.openai.as_ref().or(provider.anthropic.as_ref());
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let (harness, env, _inspector, _tmp) = make_fs_harness(entry).await;

    let cap = EventCapture::subscribe(&harness);

    with_timeout(
        SINGLE_TURN,
        harness.prompt(
            r#"Use the write tool to create a file at path "output.txt" with content "created-by-llm\n"."#,
        ),
    )
    .await
    .expect("prompt should succeed");

    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "write");

    // Verify the file was actually created on disk.
    assert!(
        env.working_dir().join("output.txt").exists(),
        "output.txt should exist after write tool was called"
    );
}

/// The LLM calls `read` to populate the snapshot, then `edit` to swap a line.
#[tokio::test]
async fn edit_modifies_file() {
    let provider = LiveProvider::from_env();
    let entry = provider.openai.as_ref().or(provider.anthropic.as_ref());
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let (harness, env, _inspector, _tmp) = make_fs_harness(entry).await;

    // Pre-create a file with 3 lines.
    std::fs::write(
        env.working_dir().join("editable.txt"),
        "line one\nline two\nline three\n",
    )
    .unwrap();

    let cap = EventCapture::subscribe(&harness);

    with_timeout(
        SINGLE_TURN,
        harness.prompt(
            "First, use the read tool to read the file \"editable.txt\". \
             Then, use the edit tool to swap line 2 (\"line two\") with \"line two modified\". \
             Use a swap operation with start=2, end=2, and lines=[\"line two modified\"].",
        ),
    )
    .await
    .expect("prompt should succeed");

    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "edit");
}

/// The LLM calls `grep` to search for a pattern across pre-created files.
#[tokio::test]
async fn grep_searches_content() {
    let provider = LiveProvider::from_env();
    let entry = provider.openai.as_ref().or(provider.anthropic.as_ref());
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let (harness, env, _inspector, _tmp) = make_fs_harness(entry).await;

    // Pre-create files with known content containing the search target.
    std::fs::write(env.working_dir().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
    std::fs::write(env.working_dir().join("b.txt"), "delta\nbeta\nepsilon\n").unwrap();

    let cap = EventCapture::subscribe(&harness);

    with_timeout(
        SINGLE_TURN,
        harness.prompt(
            "Use the grep tool with pattern \"beta\" and paths [\".\"] to search for the word 'beta' in all files. Then tell me which files contain it.",
        ),
    )
    .await
    .expect("prompt should succeed");

    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "grep");
}

/// The LLM calls `glob` to find files matching a pattern.
#[tokio::test]
async fn glob_matches_files() {
    let provider = LiveProvider::from_env();
    let entry = provider.openai.as_ref().or(provider.anthropic.as_ref());
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let (harness, env, _inspector, _tmp) = make_fs_harness(entry).await;

    // Pre-create files with known names.
    std::fs::write(env.working_dir().join("report_01.txt"), "data\n").unwrap();
    std::fs::write(env.working_dir().join("report_02.txt"), "data\n").unwrap();
    std::fs::write(env.working_dir().join("notes.md"), "data\n").unwrap();

    let cap = EventCapture::subscribe(&harness);

    with_timeout(
        SINGLE_TURN,
        harness.prompt(
            "Use the glob tool with paths [\"report_*.txt\"] to find files matching the pattern 'report_*.txt'. Then tell me what files you found.",
        ),
    )
    .await
    .expect("prompt should succeed");

    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "glob");
}
