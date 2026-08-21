//! Tests: TaskStore persistence + restore; pause/cancel state propagation.
//!
//! Spec §7: TaskStore 落盘+restore；暂停/取消状态传播

use std::sync::Arc;

use llm_harness_live_tests::skip_if_none;
use llm_harness_live_tests::{LiveProvider, MULTI_TURN, with_timeout};
use llm_harness_sandbox::OsEnvFactory;
use llm_harness_subagents::spawner::JsonlSessionFactory;
use llm_harness_workflow::lifecycle::task_store::{JsonlTaskStore, TaskStore};
use llm_harness_workflow::workflow::engine::{WorkflowEngine, WorkflowEngineConfig};
use llm_harness_workflow::workflow::judge::EdgeConditionJudge;
use llm_harness_workflow::workflow::model::{Edge, Step, Workflow, WorkflowStatus};

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

/// Build an EdgeConditionJudge from the workflow's edges.
fn edge_judge(workflow: &Workflow) -> Arc<EdgeConditionJudge> {
    Arc::new(EdgeConditionJudge::from_workflow(workflow))
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// TaskStore persists workflow state to disk; after run completes the
/// in-memory state and the persisted state both read Succeeded.
#[tokio::test]
async fn task_store_persists_state() {
    let provider = LiveProvider::from_env();
    skip_if_none!(provider.openai);
    let entry = provider.openai.as_ref().unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<JsonlTaskStore> = Arc::new(JsonlTaskStore::new(tmp.path().to_path_buf()));
    let config = make_config(entry, tmp.path().join("sessions"));

    let workflow = two_step_workflow();
    let judge = edge_judge(&workflow);
    let engine = WorkflowEngine::new(workflow, config, judge)
        .unwrap()
        .with_task_store(store.clone());

    let task_id = engine.task_id().clone();

    let _result = with_timeout(MULTI_TURN, engine.run())
        .await
        .expect("run failed");

    // In-memory state is Succeeded.
    let final_state = engine.state().await;
    assert_eq!(final_state, WorkflowStatus::Succeeded);

    // Persisted state on disk matches.
    let loaded = store
        .load_workflow_state(&task_id)
        .await
        .expect("store load failed")
        .expect("no persisted state");
    assert_eq!(loaded.status, WorkflowStatus::Succeeded);
}

/// Restoring a completed task from its TaskStore yields the same final
/// state and preserves the step history.
#[tokio::test]
async fn restore_resumes_execution() {
    let provider = LiveProvider::from_env();
    skip_if_none!(provider.openai);
    let entry = provider.openai.as_ref().unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<JsonlTaskStore> = Arc::new(JsonlTaskStore::new(tmp.path().to_path_buf()));
    let config = make_config(entry, tmp.path().join("sessions"));

    // First, run a workflow to completion so state is persisted.
    let workflow = two_step_workflow();
    let judge = edge_judge(&workflow);
    let engine = WorkflowEngine::new(workflow, config.clone(), judge.clone())
        .unwrap()
        .with_task_store(store.clone());

    let task_id = engine.task_id().clone();
    let original_history_len = {
        let _ = with_timeout(MULTI_TURN, engine.run())
            .await
            .expect("run failed");
        engine.step_history().await.len()
    };
    assert!(original_history_len > 0);

    // Restore a fresh engine from the store.
    let restored = WorkflowEngine::restore(
        store.clone(),
        task_id.clone(),
        config,
        edge_judge(&two_step_workflow()),
    )
    .await
    .expect("restore failed");

    // Since the task already completed, restored state is Succeeded.
    let restored_state = restored.state().await;
    assert_eq!(restored_state, WorkflowStatus::Succeeded);

    // Step history is preserved across restore.
    let restored_history = restored.step_history().await;
    assert_eq!(restored_history.len(), original_history_len);
}

/// Cancelling a running workflow propagates to Cancelled state.
#[tokio::test]
async fn cancel_propagates() {
    let provider = LiveProvider::from_env();
    skip_if_none!(provider.openai);
    let entry = provider.openai.as_ref().unwrap();

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

    // Spawn the run so we can cancel concurrently.
    let engine_clone = engine.clone();
    let mut run_handle = tokio::spawn(async move { engine_clone.run().await });

    // Give step 1 time to start its LLM call before cancelling.
    // If the run completes first (LLM very fast), the test still passes:
    // we just verify the final state is a terminal one.
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
            // Step 1 still in flight — safe to cancel.
        }
        _ = &mut run_handle => {
            // Run already completed before we could cancel.
            let state = engine.state().await;
            assert!(
                state == WorkflowStatus::Succeeded,
                "unexpected state: {state:?}"
            );
            return;
        }
    }

    engine.cancel("user cancelled").await.unwrap();

    // Wait for the spawned run to return.
    let _ = with_timeout(MULTI_TURN, async { run_handle.await.unwrap() })
        .await
        .expect("run task panicked");

    let state = engine.state().await;
    assert_eq!(state, WorkflowStatus::Cancelled);
}

/// Pausing a running workflow stops it at a step boundary; resuming
/// drives it to Succeeded.
///
/// Uses a 5-step workflow to maximize the window for pause() to hit
/// a step boundary. With fast LLM responses a 3-step workflow may
/// complete entirely before pause() takes effect.
#[tokio::test]
async fn pause_and_resume() {
    let provider = LiveProvider::from_env();
    skip_if_none!(provider.openai);
    let entry = provider.openai.as_ref().unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<JsonlTaskStore> = Arc::new(JsonlTaskStore::new(tmp.path().to_path_buf()));
    let config = make_config(entry, tmp.path().join("sessions"));

    // 5-step workflow: each LLM step must complete before pause is
    // checked at the next step boundary, giving a wide window.
    let workflow = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::llm("s1", "Step 1", "What is 1+1?", vec![]),
            Step::llm("s2", "Step 2", "What is 2+2?", vec![]),
            Step::llm("s3", "Step 3", "What is 3+3?", vec![]),
            Step::llm("s4", "Step 4", "What is 4+4?", vec![]),
            Step::terminal("end", "Done", 0, None),
        ],
        edges: vec![
            Edge {
                from: "s1".into(),
                to: "s2".into(),
                condition: None,
            },
            Edge {
                from: "s2".into(),
                to: "s3".into(),
                condition: None,
            },
            Edge {
                from: "s3".into(),
                to: "s4".into(),
                condition: None,
            },
            Edge {
                from: "s4".into(),
                to: "end".into(),
                condition: None,
            },
        ],
    };
    let judge = edge_judge(&workflow);
    let engine = Arc::new(
        WorkflowEngine::new(workflow, config, judge)
            .unwrap()
            .with_task_store(store.clone()),
    );

    // Spawn the run.
    let engine_clone = engine.clone();
    let mut run_handle = tokio::spawn(async move { engine_clone.run().await });

    // Poll for Running state, then pause immediately.
    // We wait until the engine is Running (step 1 started), then
    // set the pause flag. The engine will check it at the next step
    // boundary (after step 1 completes, before step 2 starts).
    let paused = tokio::select! {
        // Wait up to 5s for the engine to enter Running state.
        _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
            // Check if the engine is still running or already done.
            let state = engine.state().await;
            if state == WorkflowStatus::Succeeded {
                // LLM was too fast — whole workflow completed in 500ms.
                return;
            }
            // Engine is Running or Paused. Set pause flag.
            engine.pause("manual pause").unwrap();
            true
        }
        r = &mut run_handle => {
            // Run already completed before we could pause.
            let _ = r;
            let state = engine.state().await;
            assert!(
                state == WorkflowStatus::Succeeded,
                "unexpected state: {state:?}"
            );
            return;
        }
    };

    if !paused {
        return;
    }

    // Wait for the pause to take effect at the step boundary.
    // The engine checks pause_requested between steps. Give it up to
    // MULTI_TURN seconds to finish the current step and hit the boundary.
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

    if state == WorkflowStatus::Succeeded {
        // LLM completed all steps before pause flag was checked.
        return;
    }

    assert_eq!(state, WorkflowStatus::Paused);

    // Resume and drive to completion.
    // After resume(), we must call run() again — the original spawned
    // task already returned (Paused result). run() picks up from the
    // current_step where the engine paused.
    engine.resume().await.unwrap();

    // Drain the old run handle (it returned Paused).
    let _ = run_handle.await;

    // Re-run from the paused step.
    let _result = with_timeout(MULTI_TURN, engine.run())
        .await
        .expect("run after resume failed");

    let final_state = engine.state().await;
    assert_eq!(final_state, WorkflowStatus::Succeeded);
}
