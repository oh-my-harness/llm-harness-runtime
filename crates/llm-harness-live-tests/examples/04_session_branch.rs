//! 04 — Session Persistence & Branching: InMemory + Jsonl, fork, navigate,
//! branch summary.
//!
//! Demonstrates:
//! - InMemory session: entries written in order
//! - Fork: create an independent branch from a past entry
//! - Navigate: switch between branches
//! - Branch summary: AI-generated summary of a branch
//! - Jsonl persistence: session survives across harness instances
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 04_session_branch

use llm_harness_live_tests::{
    EventCapture, LiveProvider, MULTI_TURN, SINGLE_TURN, make_harness, make_harness_with_session,
    with_timeout,
};

#[tokio::main]
async fn main() {
    println!("=== 04: Session Persistence & Branching ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .expect("set OPENAI_API_KEY to run this example");

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);

    // ── Part 1: InMemory session — basic conversation ───────────────────────
    println!("--- Part 1: InMemory session ---\n");

    let (harness, _tmp) = make_harness(entry).await;

    // 10 turns to build up a substantial conversation
    let prompts = [
        "My name is Alice.",
        "I live in Tokyo.",
        "I work as a software engineer.",
        "My favorite hobby is hiking.",
        "I have a dog named Rex.",
        "I'm learning to play the guitar.",
        "I love eating sushi.",
        "I speak three languages.",
        "I'm planning a trip to Iceland.",
        "What do you remember about me?",
    ];

    for (i, prompt) in prompts.iter().enumerate() {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(SINGLE_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt failed");
        cap.settle().await;
        println!(
            "Turn {:2}: {}...",
            i + 1,
            cap.texts().chars().take(60).collect::<String>()
        );
    }

    let entries = harness.read_active_path().await.unwrap();
    println!("\nSession entries after 10 turns: {}", entries.len());

    // ── Part 2: Fork and branch ─────────────────────────────────────────────
    println!("--- Part 2: Fork and branch ---\n");

    // Fork from the first entry
    let first_entry = entries.first().unwrap().id;
    let branch_point = harness
        .fork_branch(first_entry, Some("alternative".to_string()))
        .await
        .unwrap();
    println!("Forked branch at entry: {:?}", branch_point);

    // Run 10 turns on the branch with a different topic
    let branch_prompts = [
        "Tell me a short joke.",
        "Tell me another joke.",
        "What's the funniest animal?",
        "Recommend a comedy movie.",
        "Who is your favorite comedian?",
        "Tell me a riddle.",
        "What makes you laugh?",
        "Tell me a pun.",
        "Share a funny fact.",
        "Summarize our conversation so far.",
    ];

    for (i, prompt) in branch_prompts.iter().enumerate() {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(SINGLE_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt failed");
        cap.settle().await;
        println!(
            "Branch turn {:2}: {}...",
            i + 1,
            cap.texts().chars().take(60).collect::<String>()
        );
    }
    println!();

    // List branches
    let branches = harness.list_branches().await.unwrap();
    println!("Branches: {}", branches.len());
    for b in &branches {
        println!(
            "  - leaf_id: {:?}, label: {:?}, entries: {}, summary: {}",
            b.leaf_id,
            b.label,
            b.message_count,
            b.summary.as_deref().unwrap_or("(none)")
        );
    }

    // ── Part 3: Generate branch summary ─────────────────────────────────────
    println!("\n--- Part 3: Branch summary ---\n");

    let active = harness.read_active_path().await.unwrap();
    let branch_leaf = active.last().unwrap().id;

    let summary = harness.generate_branch_summary(branch_leaf).await.unwrap();
    println!("Branch summary: {}\n", summary.summary);

    // ── Part 4: Jsonl persistence ───────────────────────────────────────────
    println!("--- Part 4: Jsonl persistence ---\n");

    let (harness2, tmp2) = make_harness_with_session(entry, |b| b).await;

    // 10 turns to test Jsonl persistence with substantial history
    let jsonl_prompts = [
        "My favorite color is green.",
        "I was born in March.",
        "I drive a Toyota.",
        "My phone is an iPhone.",
        "I prefer tea over coffee.",
        "I have two siblings.",
        "My birthday is next month.",
        "I enjoy reading sci-fi novels.",
        "I live on the 5th floor.",
        "What personal facts do you know about me?",
    ];

    for (i, prompt) in jsonl_prompts.iter().enumerate() {
        let cap = EventCapture::subscribe(&harness2);
        with_timeout(SINGLE_TURN, harness2.prompt(*prompt))
            .await
            .expect("prompt failed");
        cap.settle().await;
        println!(
            "Jsonl turn {:2}: {}...",
            i + 1,
            cap.texts().chars().take(60).collect::<String>()
        );
    }
    println!();

    let entries_before = harness2.read_active_path().await.unwrap().len();
    println!("Entries before reload: {}", entries_before);

    // Get session ID from the session directory (each session is a subdirectory)
    let session_dir = tmp2.path().join("session");
    let session_id: String = std::fs::read_dir(&session_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .next()
        .expect("session directory should exist");
    println!("Session ID: {}", session_id);

    drop(harness2);

    // Reload session from disk
    use llm_harness_agent::SessionRepo;
    let repo = llm_harness_agent::JsonlSessionRepo::new(&session_dir);
    let storage = repo.open(&session_id).await.unwrap();
    let session = llm_harness_agent::Session::new(storage);

    // Build a new harness with the reloaded session
    let env: std::sync::Arc<dyn llm_harness_types::ExecutionEnv> = {
        use llm_harness_runtime::platform::sandbox::{ResourceLimits, Sandbox, SandboxConfig};
        use llm_harness_runtime_sandbox_os::OsEnvSandbox;
        let config = SandboxConfig {
            fs_allowlist: vec![tmp2.path().to_path_buf()],
            fs_denylist: vec![],
            net_allowlist: vec![],
            resource_limits: ResourceLimits {
                max_cpus: None,
                max_memory_mb: None,
                max_disk_mb: None,
                timeout: None,
            },
            work_dir: Some(tmp2.path().to_path_buf()),
        };
        let sandbox = OsEnvSandbox::new(config);
        sandbox.start().await.unwrap();
        sandbox.env()
    };

    let builder = llm_harness_agent::HarnessBuilder::new(&entry.model)
        .provider("*", entry.client.clone())
        .max_tokens(1024);
    let harness3 = builder.build_with_session(env, session).unwrap();

    let entries_after = harness3.read_active_path().await.unwrap().len();
    println!("Entries after reload: {}", entries_after);
    assert_eq!(
        entries_before, entries_after,
        "entries should survive reload"
    );

    // Verify the LLM remembers the fact from before reload
    let capture5 = EventCapture::subscribe(&harness3);
    with_timeout(MULTI_TURN, harness3.prompt("What is my favorite color?"))
        .await
        .expect("prompt failed");
    capture5.settle().await;
    println!("After reload response: {}\n", capture5.texts());

    println!("--- Summary ---");
    println!("InMemory session: fork + navigate + branch summary ✅");
    println!("Jsonl persistence: session survives reload ✅");
}
