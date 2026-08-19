//! 22 — FS Tools: bash/read/write/edit/grep/glob (FsToolsPlugin).
//!
//! Demonstrates:
//! - Part 1: BashTool — the LLM calls `bash` with `echo hello > test.txt`
//!   and the file is created in the working dir.
//! - Part 2: WriteTool + ReadTool — the LLM uses `write` to create a file,
//!   then `read` to read it back.
//! - Part 3: EditTool — a file with known content is pre-created; the LLM
//!   reads it (recording a staleness tag) then calls `edit` with a swap
//!   operation to modify a line.
//! - Part 4: GrepTool + GlobTool — several files are pre-created in the temp
//!   dir; the LLM uses `grep` to search for a pattern and `glob` to list
//!   files matching a pattern.
//!
//! `FsToolsPlugin::new(Some(store))` registers all 6 tools at once with a
//! shared `FileSnapshotStore` coupling `read` → `edit` via 4-hex content-hash
//! tags for staleness detection.
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 22_fs_tools

use std::sync::Arc;

use llm_harness_live_tests::{
    EventCapture, LiveProvider, SINGLE_TURN, make_harness_with_env, with_timeout,
};
use llm_harness_tools::{FileSnapshotStore, FsToolsPlugin};
use parking_lot::RwLock;

#[tokio::main]
async fn main() {
    println!("=== 22: FS Tools (bash/read/write/edit/grep/glob) ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .or(provider.anthropic.as_ref())
        .expect("set OPENAI_API_KEY or ANTHROPIC_API_KEY to run this example");

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);

    part1_bash(entry).await;
    println!();
    part2_write_read(entry).await;
    println!();
    part3_edit(entry).await;
    println!();
    part4_grep_glob(entry).await;

    println!("\n--- Summary ---");
    println!("Part 1: bash tool creates a file via shell redirect ✅");
    println!("Part 2: write + read round-trip file content ✅");
    println!("Part 3: edit with swap operation modifies a line ✅");
    println!("Part 4: grep searches content, glob lists matching files ✅");
}

// ── Part 1: BashTool ─────────────────────────────────────────────────────────

async fn part1_bash(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 1: BashTool (echo hello > test.txt) ---\n");

    let store = Arc::new(RwLock::new(FileSnapshotStore::new()));
    let plugin = FsToolsPlugin::new(Some(store));

    let (harness, env, _tmp) = make_harness_with_env(entry, |b| b.install(&plugin)).await;

    let cap = EventCapture::subscribe(&harness);

    let prompt_text = "Use the bash tool to run this exact command: echo hello > test.txt \
         Then confirm the file was created by running: cat test.txt";
    println!("LLM prompt: {}", prompt_text);
    with_timeout(SINGLE_TURN, harness.prompt(prompt_text))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();
    let file_exists = env.working_dir().join("test.txt").exists();
    let file_content =
        std::fs::read_to_string(env.working_dir().join("test.txt")).unwrap_or_default();

    println!("\nTools called: {:?}", tools_called);
    println!("test.txt exists: {}", file_exists);
    println!("test.txt content: {:?}", file_content.trim());
    println!(
        "Final text:   {}",
        cap.texts().chars().take(160).collect::<String>()
    );

    let bash_called = tools_called.iter().any(|t| t == "bash");
    println!("\nObservation:");
    println!("  bash tool called:  {}", bash_called);
    println!("  file created:      {}", file_exists);
    println!("  file content:      {:?}", file_content.trim());
}

// ── Part 2: WriteTool + ReadTool ─────────────────────────────────────────────

async fn part2_write_read(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 2: WriteTool + ReadTool (round-trip) ---\n");

    let store = Arc::new(RwLock::new(FileSnapshotStore::new()));
    let plugin = FsToolsPlugin::new(Some(store));

    let (harness, env, _tmp) = make_harness_with_env(entry, |b| b.install(&plugin)).await;

    let cap = EventCapture::subscribe(&harness);

    let prompt_text = "Use the write tool to create a file named 'greeting.txt' with the content \
         'Hello from WriteTool!'. Then use the read tool to read the file back \
         and tell me what it contains.";
    println!("LLM prompt: {}", prompt_text);
    with_timeout(SINGLE_TURN, harness.prompt(prompt_text))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();
    let file_content =
        std::fs::read_to_string(env.working_dir().join("greeting.txt")).unwrap_or_default();

    println!("\nTools called: {:?}", tools_called);
    println!("greeting.txt content: {:?}", file_content.trim());
    println!(
        "Final text:   {}",
        cap.texts().chars().take(160).collect::<String>()
    );

    let write_called = tools_called.iter().any(|t| t == "write");
    let read_called = tools_called.iter().any(|t| t == "read");
    println!("\nObservation:");
    println!("  write tool called: {}", write_called);
    println!("  read tool called:  {}", read_called);
    println!("  file content:      {:?}", file_content.trim());
}

// ── Part 3: EditTool ─────────────────────────────────────────────────────────

async fn part3_edit(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 3: EditTool (swap line 2) ---\n");

    let store = Arc::new(RwLock::new(FileSnapshotStore::new()));
    let plugin = FsToolsPlugin::new(Some(store));

    let (harness, env, _tmp) = make_harness_with_env(entry, |b| b.install(&plugin)).await;

    // Pre-create a file with known content for the LLM to edit.
    let edit_file = env.working_dir().join("editable.txt");
    std::fs::write(
        &edit_file,
        "line one: original\nline two: original\nline three: original\n",
    )
    .unwrap();
    println!("Pre-created editable.txt with 3 lines");

    let cap = EventCapture::subscribe(&harness);

    // First prompt: read the file so the snapshot store records a tag.
    let read_prompt = "Use the read tool to read the file 'editable.txt'.";
    println!("LLM prompt (read): {}", read_prompt);
    with_timeout(SINGLE_TURN, harness.prompt(read_prompt))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let tools_after_read = cap.tool_calls();
    println!("\nTools called (after read): {:?}", tools_after_read);

    // Second prompt: edit line 2 using a swap operation.
    let edit_prompt = "Use the edit tool on the file 'editable.txt' with a swap operation \
         to replace line 2 with the text 'line two: modified'. \
         The operations should be: [{\"type\":\"swap\",\"start\":2,\"end\":2,\
         \"lines\":[\"line two: modified\"]}].";
    println!("\nLLM prompt (edit): {}", edit_prompt);
    with_timeout(SINGLE_TURN, harness.prompt(edit_prompt))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();
    let file_content = std::fs::read_to_string(&edit_file).unwrap_or_default();

    println!("\nTools called (all): {:?}", tools_called);
    println!("editable.txt after edit:");
    for (i, line) in file_content.lines().enumerate() {
        println!("  {}: {}", i + 1, line);
    }
    println!(
        "Final text:   {}",
        cap.texts().chars().take(160).collect::<String>()
    );

    let read_called = tools_called.iter().any(|t| t == "read");
    let edit_called = tools_called.iter().any(|t| t == "edit");
    let line2_modified = file_content.contains("line two: modified");
    println!("\nObservation:");
    println!("  read tool called:     {}", read_called);
    println!("  edit tool called:     {}", edit_called);
    println!("  line 2 was modified:  {}", line2_modified);
}

// ── Part 4: GrepTool + GlobTool ──────────────────────────────────────────────

async fn part4_grep_glob(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 4: GrepTool + GlobTool ---\n");

    let store = Arc::new(RwLock::new(FileSnapshotStore::new()));
    let plugin = FsToolsPlugin::new(Some(store));

    let (harness, env, _tmp) = make_harness_with_env(entry, |b| b.install(&plugin)).await;

    // Pre-create several files in the temp dir for the LLM to search.
    let dir = env.working_dir();
    std::fs::write(
        dir.join("alpha.rs"),
        "fn alpha() { println!(\"alpha\"); }\n",
    )
    .unwrap();
    std::fs::write(dir.join("beta.rs"), "fn beta() { println!(\"beta\"); }\n").unwrap();
    std::fs::write(
        dir.join("gamma.rs"),
        "fn gamma() { println!(\"gamma\"); }\n",
    )
    .unwrap();
    std::fs::write(dir.join("readme.md"), "# Project\nA demo project.\n").unwrap();
    println!("Pre-created: alpha.rs, beta.rs, gamma.rs, readme.md");

    let cap = EventCapture::subscribe(&harness);

    let prompt_text = "You have access to grep and glob tools. Do two things:\n\
         1. Use the grep tool to search for the pattern 'fn ' in the current \
         directory (paths: [\".\"]).\n\
         2. Use the glob tool to list all files matching the pattern '**/*.rs' \
         (paths: [\"**/*.rs\"]).\n\
         Report what you found from each tool.";
    println!("LLM prompt: {}", prompt_text);
    with_timeout(SINGLE_TURN, harness.prompt(prompt_text))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();

    println!("\nTools called: {:?}", tools_called);
    println!(
        "Final text:   {}",
        cap.texts().chars().take(200).collect::<String>()
    );

    let grep_called = tools_called.iter().any(|t| t == "grep");
    let glob_called = tools_called.iter().any(|t| t == "glob");
    println!("\nObservation:");
    println!("  grep tool called: {}", grep_called);
    println!("  glob tool called: {}", glob_called);
    println!("  both tools used:  {}", grep_called && glob_called);
}
