//! Tests: session persistence & branching.

use llm_harness_agent::SessionRepo;
use llm_harness_live_tests::{
    EventCapture, LiveProvider, SINGLE_TURN, make_harness, make_harness_with_session, with_timeout,
};

/// InMemory session grows by 2 entries per turn (user + assistant).
#[tokio::test]
async fn inmemory_session_grows_with_turns() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let (harness, _tmp) = make_harness(entry).await;

    for i in 1..=3 {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(
            SINGLE_TURN,
            harness.prompt(format!("Tell me fact #{i} about space.")),
        )
        .await
        .expect("prompt failed");
        cap.settle().await;

        let entries = harness.read_active_path().await.unwrap();
        assert_eq!(
            entries.len(),
            i * 2,
            "after turn {i}, expected {} entries, got {}",
            i * 2,
            entries.len()
        );
    }
}

/// Fork creates an independent branch with its own conversation.
#[tokio::test]
async fn fork_creates_independent_branch() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let (harness, _tmp) = make_harness(entry).await;

    // 3 turns on main branch
    for i in 1..=3 {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(
            SINGLE_TURN,
            harness.prompt(format!("Tell me fact #{i} about oceans.")),
        )
        .await
        .expect("prompt failed");
        cap.settle().await;
    }

    let entries = harness.read_active_path().await.unwrap();
    let fork_point = entries.first().unwrap().id;
    let original_leaf = entries.last().unwrap().id;

    // Fork from the first entry
    harness
        .fork_branch(fork_point, Some("alternative".to_string()))
        .await
        .unwrap();

    // 3 turns on the forked branch — different topic
    for i in 1..=3 {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(SINGLE_TURN, harness.prompt(format!("Tell me joke #{i}.")))
            .await
            .expect("prompt failed");
        cap.settle().await;
    }

    // List branches — should have 2
    let branches = harness.list_branches().await.unwrap();
    assert_eq!(
        branches.len(),
        2,
        "expected 2 branches, got {}",
        branches.len()
    );

    // The current active path should be the forked branch, not the original
    let active = harness.read_active_path().await.unwrap();
    let active_leaf = active.last().unwrap().id;
    assert_ne!(
        active_leaf, original_leaf,
        "active branch should differ from original"
    );
}

/// Navigate switches the active cursor back to a different branch.
#[tokio::test]
async fn navigate_switches_active_branch() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let (harness, _tmp) = make_harness(entry).await;

    // 2 turns on main branch
    for i in 1..=2 {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(
            SINGLE_TURN,
            harness.prompt(format!("Tell me fact #{i} about oceans.")),
        )
        .await
        .expect("prompt failed");
        cap.settle().await;
    }

    let entries = harness.read_active_path().await.unwrap();
    let original_leaf = entries.last().unwrap().id;
    let fork_point = entries.first().unwrap().id;

    // Fork
    let new_leaf = harness
        .fork_branch(fork_point, Some("alt".to_string()))
        .await
        .unwrap();

    // 1 turn on branch
    let cap = EventCapture::subscribe(&harness);
    with_timeout(SINGLE_TURN, harness.prompt("Tell me a joke."))
        .await
        .expect("prompt failed");
    cap.settle().await;

    // Navigate back to original leaf
    harness.navigate_tree(original_leaf).await.unwrap();
    let active = harness.read_active_path().await.unwrap();
    assert_eq!(
        active.last().unwrap().id,
        original_leaf,
        "navigate should switch active cursor back to original leaf"
    );

    // Navigate to forked branch
    harness.navigate_tree(new_leaf).await.unwrap();
    let active2 = harness.read_active_path().await.unwrap();
    assert_eq!(
        active2.last().unwrap().id,
        new_leaf,
        "navigate should switch to forked branch leaf"
    );
}

/// Branch summary is generated and non-empty.
#[tokio::test]
async fn branch_summary_generated() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let (harness, _tmp) = make_harness(entry).await;

    // 3 turns
    for i in 1..=3 {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(
            SINGLE_TURN,
            harness.prompt(format!("Tell me fact #{i} about mountains.")),
        )
        .await
        .expect("prompt failed");
        cap.settle().await;
    }

    // Fork
    let entries = harness.read_active_path().await.unwrap();
    let fork_point = entries.first().unwrap().id;
    harness
        .fork_branch(fork_point, Some("summary-test".to_string()))
        .await
        .unwrap();

    // 3 more turns on branch
    for i in 1..=3 {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(
            SINGLE_TURN,
            harness.prompt(format!("Tell me fact #{i} about rivers.")),
        )
        .await
        .expect("prompt failed");
        cap.settle().await;
    }

    let active = harness.read_active_path().await.unwrap();
    let branch_leaf = active.last().unwrap().id;

    let summary = harness.generate_branch_summary(branch_leaf).await.unwrap();
    assert!(
        !summary.summary.is_empty(),
        "branch summary should be non-empty"
    );
    println!("Branch summary: {}", summary.summary);
}

/// Jsonl session survives harness reload.
#[tokio::test]
async fn jsonl_session_survives_reload() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let (harness, tmp) = make_harness_with_session(entry, |b| b).await;

    // 3 turns — tell the LLM a memorable fact
    let prompts = [
        "My secret code is 42.",
        "I live in Paris.",
        "What is my secret code?",
    ];

    for (i, prompt) in prompts.iter().enumerate() {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(SINGLE_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt failed");
        cap.settle().await;
        println!(
            "Turn {}: {}",
            i + 1,
            cap.texts().chars().take(60).collect::<String>()
        );
    }

    let entries_before = harness.read_active_path().await.unwrap().len();

    // Get session ID from directory listing
    let session_dir = tmp.path().join("session");
    let session_id: String = std::fs::read_dir(&session_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .next()
        .expect("session directory should exist");

    drop(harness);

    // Reload session from disk
    let repo = llm_harness_agent::JsonlSessionRepo::new(&session_dir);
    let storage = repo.open(&session_id).await.unwrap();
    let session = llm_harness_agent::Session::new(storage);

    // Build a new harness with the reloaded session
    let env: std::sync::Arc<dyn llm_harness_types::ExecutionEnv> = {
        use llm_harness_runtime::platform::sandbox::{ResourceLimits, Sandbox, SandboxConfig};
        use llm_harness_runtime_sandbox_os::OsEnvSandbox;
        let config = SandboxConfig {
            fs_allowlist: vec![tmp.path().to_path_buf()],
            fs_denylist: vec![],
            net_allowlist: vec![],
            resource_limits: ResourceLimits {
                max_cpus: None,
                max_memory_mb: None,
                max_disk_mb: None,
                timeout: None,
            },
            work_dir: Some(tmp.path().to_path_buf()),
        };
        let sandbox = OsEnvSandbox::new(config);
        sandbox.start().await.unwrap();
        sandbox.env()
    };

    let builder = llm_harness_agent::HarnessBuilder::new(&entry.model)
        .provider("*", entry.client.clone())
        .max_tokens(1024);
    let harness2 = builder.build_with_session(env, session).unwrap();

    let entries_after = harness2.read_active_path().await.unwrap().len();
    assert_eq!(
        entries_before, entries_after,
        "entries should survive reload"
    );

    // Verify the LLM remembers the secret code
    let cap = EventCapture::subscribe(&harness2);
    with_timeout(SINGLE_TURN, harness2.prompt("What is my secret code?"))
        .await
        .expect("prompt failed");
    cap.settle().await;
    let response = cap.texts();
    assert!(
        response.contains("42"),
        "LLM should remember secret code 42 after reload, got: {}",
        response
    );
}
