//! 09 — Workflow Recovery: TaskStore persistence, pause/resume, cancel, and crash recovery.
//!
//! Demonstrates:
//! - Building a WorkflowEngine with a JsonlTaskStore for disk persistence
//! - Running a multi-step linear workflow to completion (state → Succeeded)
//! - Verifying state is persisted to disk (plan.json + workflow.json)
//! - Pause (non-blocking flag) and resume across a tokio task boundary
//! - Cancel a running workflow (state → Cancelled)
//! - Crash recovery via `WorkflowEngine::restore` from a TaskStore
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 09_workflow_recovery

use std::sync::Arc;

use llm_harness_live_tests::{LiveProvider, MULTI_TURN, with_timeout};
use llm_harness_runtime::lifecycle::task::TaskId;
use llm_harness_runtime::lifecycle::task_store::{JsonlTaskStore, TaskStore};
use llm_harness_runtime::spawn::spawner::JsonlSessionFactory;
use llm_harness_runtime::workflow::engine::{WorkflowEngine, WorkflowEngineConfig};
use llm_harness_runtime::workflow::judge::EdgeConditionJudge;
use llm_harness_runtime::workflow::model::{Edge, Step, Workflow, WorkflowStatus};
use llm_harness_sandbox::OsEnvFactory;

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Build a WorkflowEngineConfig backed by the live provider + real OS env.
fn make_config(
    entry: &llm_harness_live_tests::LiveProviderEntry,
    session_base_dir: std::path::PathBuf,
) -> WorkflowEngineConfig {
    WorkflowEngineConfig {
        client: entry.client.clone(),
        model: entry.model.clone(),
        env_factory: Arc::new(OsEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir,
        customize_builder: None,
    }
}

/// A 2-step linear workflow: step1 (LLM) → terminal.
fn two_step_workflow() -> Workflow {
    Workflow {
        entry_step: "step1".into(),
        steps: vec![
            Step::llm(
                "step1",
                "Step 1",
                "Reply with exactly: hello from step one",
                vec![],
            ),
            Step::terminal("done", "Done", 0, Some("workflow complete".into())),
        ],
        edges: vec![Edge {
            from: "step1".into(),
            to: "done".into(),
            condition: None,
        }],
    }
}

/// A 3-step linear workflow: step1 → step2 → terminal.
fn three_step_workflow() -> Workflow {
    Workflow {
        entry_step: "step1".into(),
        steps: vec![
            Step::llm("step1", "Step 1", "Reply with exactly: alpha", vec![]),
            Step::llm("step2", "Step 2", "Reply with exactly: beta", vec![]),
            Step::terminal("done", "Done", 0, Some("workflow complete".into())),
        ],
        edges: vec![
            Edge {
                from: "step1".into(),
                to: "step2".into(),
                condition: None,
            },
            Edge {
                from: "step2".into(),
                to: "done".into(),
                condition: None,
            },
        ],
    }
}

/// Build an EdgeConditionJudge from the workflow's edges.
/// For linear workflows with unconditional edges, this resolves
/// `from → to` automatically and returns `Transition::Abort` when
/// there are no outgoing edges (terminal step).
fn edge_judge(workflow: &Workflow) -> Arc<EdgeConditionJudge> {
    Arc::new(EdgeConditionJudge::from_workflow(workflow))
}

/// Print a labeled section header.
fn header(title: &str) {
    let bar = "=".repeat(60);
    println!("\n{bar}");
    println!("  {title}");
    println!("{bar}\n");
}

// ── Part 1: Build with TaskStore and run to completion ──────────────────────

async fn part1_run_to_completion(
    entry: &llm_harness_live_tests::LiveProviderEntry,
    store_dir: &std::path::Path,
) -> (TaskId, Arc<JsonlTaskStore>) {
    header("Part 1: Build with TaskStore and run to completion");

    let store: Arc<JsonlTaskStore> = Arc::new(JsonlTaskStore::new(store_dir.to_path_buf()));
    let config = make_config(entry, store_dir.join("sessions"));

    let workflow = two_step_workflow();
    let judge = edge_judge(&workflow);
    let engine = WorkflowEngine::new(workflow, config, judge)
        .unwrap()
        .with_task_store(store.clone());

    let task_id = engine.task_id().clone();
    println!("Task ID: {}", task_id.0);
    println!("Initial state: {:?}", engine.state().await);

    let result = with_timeout(MULTI_TURN, engine.run())
        .await
        .expect("run failed");

    let final_state = engine.state().await;
    println!("Final state: {:?}", final_state);
    println!(
        "Result: turns={}, final_message={:?}",
        result.turns, result.final_message
    );

    // Show state is persisted to disk.
    let task_dir = store_dir.join(&task_id.0);
    let plan_exists = task_dir.join("plan.json").exists();
    let state_exists = task_dir.join("workflow.json").exists();
    println!("Persisted files:");
    println!("  plan.json     exists = {plan_exists}");
    println!("  workflow.json exists = {state_exists}");

    // Verify via store API.
    let loaded_state = store.load_workflow_state(&task_id).await.unwrap();
    println!(
        "Loaded state from store: {:?}",
        loaded_state.as_ref().map(|s| s.status.clone())
    );

    let history = engine.step_history().await;
    println!("Step history ({} entries):", history.len());
    for record in &history {
        println!(
            "  - step_id={}, has_result={}",
            record.step_id,
            record.result.is_some()
        );
    }

    (task_id, store)
}

// ── Part 2: Pause and resume ────────────────────────────────────────────────

async fn part2_pause_and_resume(entry: &llm_harness_live_tests::LiveProviderEntry) {
    header("Part 2: Pause and resume");

    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<JsonlTaskStore> = Arc::new(JsonlTaskStore::new(tmp.path().to_path_buf()));
    let config = make_config(entry, tmp.path().join("sessions"));

    let workflow = three_step_workflow();
    let judge = edge_judge(&workflow);
    let engine = Arc::new(
        WorkflowEngine::new(workflow, config, judge)
            .unwrap()
            .with_task_store(store.clone()),
    );

    let task_id = engine.task_id().clone();
    println!("Task ID: {}", task_id.0);

    // Spawn the run in a tokio task.
    let engine_clone = engine.clone();
    let mut run_handle = tokio::spawn(async move { engine_clone.run().await });

    // Give step 1 time to start, then pause.
    // pause() is non-blocking: it sets a flag that run() checks at step
    // boundaries. After step 1 completes and the engine transitions to
    // step 2, it will consume the pause flag and enter Paused.
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
            // Step 1 still in flight — safe to pause.
        }
        _ = &mut run_handle => {
            // Run already completed (LLM was very fast). Skip pause demo.
            println!("Run completed before pause could be applied (LLM very fast).");
            println!("Final state: {:?}", engine.state().await);
            return;
        }
    }

    println!("Calling pause(\"manual pause\")...");
    engine.pause("manual pause").unwrap();

    // Wait for the pause to take effect at the step boundary.
    // The engine checks pause_requested between steps. Poll until
    // Paused or Succeeded (LLM may complete before pause is checked).
    let state = with_timeout(MULTI_TURN, async {
        loop {
            let s = engine.state().await;
            if s == WorkflowStatus::Paused || s == WorkflowStatus::Succeeded {
                return s;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    })
    .await;

    println!("State after pause: {:?}", state);

    if state == WorkflowStatus::Succeeded {
        // LLM completed all steps before pause flag was checked.
        println!("Workflow completed before pause took effect (LLM very fast).");
        let _ = run_handle.await;
        return;
    }

    let current_step = engine.current_step().await;
    println!("Current step: {}", current_step);

    // Resume and re-run.
    println!("Calling resume()...");
    engine.resume().await.unwrap();
    let state_after_resume = engine.state().await;
    println!("State after resume: {:?}", state_after_resume);

    // Drain the old run handle (it returned Paused).
    let _ = run_handle.await;

    // Re-run from the paused step.
    let result = with_timeout(MULTI_TURN, engine.run())
        .await
        .expect("run after resume failed");

    let final_state = engine.state().await;
    println!("Final state: {:?}", final_state);
    println!(
        "Result: turns={}, final_message={:?}",
        result.turns, result.final_message
    );
}

// ── Part 3: Cancel ──────────────────────────────────────────────────────────

async fn part3_cancel(entry: &llm_harness_live_tests::LiveProviderEntry) {
    header("Part 3: Cancel");

    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<JsonlTaskStore> = Arc::new(JsonlTaskStore::new(tmp.path().to_path_buf()));
    let config = make_config(entry, tmp.path().join("sessions"));

    let workflow = two_step_workflow();
    let judge = edge_judge(&workflow);
    let engine = Arc::new(
        WorkflowEngine::new(workflow, config, judge)
            .unwrap()
            .with_task_store(store.clone()),
    );

    let task_id = engine.task_id().clone();
    println!("Task ID: {}", task_id.0);

    // Spawn the run.
    let engine_clone = engine.clone();
    let mut run_handle = tokio::spawn(async move { engine_clone.run().await });

    // Give step 1 time to start its LLM call, then cancel.
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
            // Step 1 still in flight — safe to cancel.
        }
        _ = &mut run_handle => {
            // Run already completed (LLM was very fast). Skip cancel demo.
            println!("Run completed before cancel could be applied (LLM very fast).");
            println!("Final state: {:?}", engine.state().await);
            return;
        }
    }

    println!("Calling cancel(\"user cancelled\")...");
    engine.cancel("user cancelled").await.unwrap();

    // Wait for the spawned run to return.
    let _ = with_timeout(MULTI_TURN, run_handle)
        .await
        .expect("run task panicked");

    let state = engine.state().await;
    println!("State after cancel: {:?}", state);

    let history = engine.step_history().await;
    println!("Step history ({} entries):", history.len());
    for record in &history {
        println!(
            "  - step_id={}, has_result={}, transition={:?}",
            record.step_id,
            record.result.is_some(),
            record.transition
        );
    }
}

// ── Part 4: Crash recovery via restore ──────────────────────────────────────

async fn part4_crash_recovery(
    entry: &llm_harness_live_tests::LiveProviderEntry,
    store_dir: &std::path::Path,
    task_id: &TaskId,
    store: Arc<JsonlTaskStore>,
) {
    header("Part 4: Crash recovery via restore");

    println!("Restoring task: {}", task_id.0);

    let config = make_config(entry, store_dir.join("sessions"));

    // Restore the engine from the task store using the task_id from Part 1.
    // NoopJudge is pub(crate), so we use EdgeConditionJudge which handles
    // unconditional edges identically.
    let restored = WorkflowEngine::restore(
        store.clone(),
        task_id.clone(),
        config,
        edge_judge(&two_step_workflow()),
    )
    .await
    .expect("restore failed");

    let state = restored.state().await;
    println!("Restored state: {:?}", state);
    println!("(Expected: Succeeded, since Part 1 completed)");

    let history = restored.step_history().await;
    println!("Step history preserved ({} entries):", history.len());
    for record in &history {
        println!(
            "  - step_id={}, has_result={}",
            record.step_id,
            record.result.is_some()
        );
    }

    // Verify the workflow definition was also persisted and restored.
    let loaded_workflow = store.load_workflow(task_id).await.unwrap();
    println!(
        "Restored workflow: entry_step={}, steps={}",
        loaded_workflow.as_ref().unwrap().entry_step,
        loaded_workflow.as_ref().unwrap().steps.len()
    );
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    println!("=== 09: Workflow Recovery ===\n");

    let provider = LiveProvider::from_env();
    let entry = match provider.openai.as_ref().or(provider.anthropic.as_ref()) {
        Some(e) => e,
        None => {
            eprintln!("skipping: no provider configured (set OPENAI_API_KEY or ANTHROPIC_API_KEY)");
            return;
        }
    };
    println!("Provider: {} | Model: {}", entry.name, entry.model);

    // Part 1 uses a tempdir that must survive through Part 4 (restore).
    let persistent_tmp = tempfile::tempdir().unwrap();
    let store_dir = persistent_tmp.path().to_path_buf();

    let (task_id, store) = part1_run_to_completion(entry, &store_dir).await;
    part2_pause_and_resume(entry).await;
    part3_cancel(entry).await;
    part4_crash_recovery(entry, &store_dir, &task_id, store).await;

    println!("\n=== All parts complete ===");
}
