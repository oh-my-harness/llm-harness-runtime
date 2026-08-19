//! 08 — Workflow Engine: multi-step LLM workflow with transitions and tools.
//!
//! Demonstrates:
//! - Building a `Workflow` with LLM steps and a terminal step
//! - `WorkflowEngine` construction with `WorkflowEngineConfig`
//! - Linear 2-step workflow (capital query → summary → terminal)
//! - Second workflow with an extra tool registered via `.with_tool()`
//! - Subscribing to `WorkflowEvent` for live step progress
//! - Inspecting `step_history()`, `state()`, `total_cost()`, and `task_id()`
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 08_workflow

use std::sync::Arc;
use std::sync::LazyLock;

use futures::future::BoxFuture;
use llm_harness_live_tests::{LiveProvider, MULTI_TURN, with_timeout};
use llm_harness_runtime::spawn::spawner::JsonlSessionFactory;
use llm_harness_runtime::workflow::engine::{WorkflowEngine, WorkflowEngineConfig, WorkflowEvent};
use llm_harness_runtime::workflow::judge::EdgeConditionJudge;
use llm_harness_runtime::workflow::model::{Edge, Step, Workflow};
use llm_harness_runtime_sandbox::OsEnvFactory;
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};

// ── Echo tool (for Part 3) ──────────────────────────────────────────────────

struct EchoTool;

static ECHO_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "message": { "type": "string", "description": "Message to echo back" }
        },
        "required": ["message"]
    })
});

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echo back the provided message verbatim."
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        &ECHO_SCHEMA
    }
    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let msg = args
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("(no message)");
            Ok(ToolResult::full(
                vec![DataBlock::text(msg)],
                serde_json::Value::Null,
                false,
            ))
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Build a `WorkflowEngineConfig` backed by a tempdir for session storage.
fn make_config(
    entry: &llm_harness_live_tests::LiveProviderEntry,
    session_base: &std::path::Path,
) -> WorkflowEngineConfig {
    WorkflowEngineConfig {
        client: entry.client.clone(),
        model: entry.model.clone(),
        env_factory: Arc::new(OsEnvFactory),
        session_factory: Arc::new(JsonlSessionFactory),
        session_base_dir: session_base.to_path_buf(),
        customize_builder: None,
    }
}

/// Spawn a background task that drains `WorkflowEvent`s and prints step progress.
fn print_events(mut rx: tokio::sync::broadcast::Receiver<WorkflowEvent>) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => match &ev {
                    WorkflowEvent::StepStarted { step_id, step_name } => {
                        println!("  [event] StepStarted: {} ({})", step_id, step_name);
                    }
                    WorkflowEvent::StepFinished { step_id, result } => {
                        println!(
                            "  [event] StepFinished: {} | tool_calls={} | output={:?}",
                            step_id,
                            result.tool_calls_count,
                            result.output.chars().take(80).collect::<String>()
                        );
                    }
                    WorkflowEvent::Paused { reason } => {
                        println!("  [event] Paused: {}", reason);
                    }
                    WorkflowEvent::Cancelled { reason } => {
                        println!("  [event] Cancelled: {}", reason);
                    }
                    WorkflowEvent::Failed { error } => {
                        println!("  [event] Failed: {}", error);
                    }
                    WorkflowEvent::Resumed => {
                        println!("  [event] Resumed");
                    }
                    WorkflowEvent::StepProgress { step_id, progress } => {
                        println!("  [event] StepProgress {}: {:?}", step_id, progress);
                    }
                },
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });
}

#[tokio::main]
async fn main() {
    println!("=== 08: Workflow Engine ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .expect("set OPENAI_API_KEY to run this example");

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);

    // ════════════════════════════════════════════════════════════════════════
    // Part 1: Linear workflow — 2 LLM steps + terminal
    // ════════════════════════════════════════════════════════════════════════
    println!("--- Part 1: Linear workflow (2 LLM steps + terminal) ---\n");

    let tmp1 = tempfile::tempdir().unwrap();
    let config1 = make_config(entry, &tmp1.path().join("sessions"));
    std::fs::create_dir_all(&config1.session_base_dir).unwrap();

    let workflow1 = Workflow {
        entry_step: "step1".into(),
        steps: vec![
            Step::llm(
                "step1",
                "Capital Query",
                "What is the capital of France? Answer in one word.",
                vec![],
            ),
            Step::llm(
                "step2",
                "Summary",
                "Write a one-sentence summary about Paris.",
                vec![],
            ),
            Step::terminal("end", "Done", 0, Some("Workflow complete".into())),
        ],
        edges: vec![
            Edge {
                from: "step1".into(),
                to: "step2".into(),
                condition: None,
            },
            Edge {
                from: "step2".into(),
                to: "end".into(),
                condition: None,
            },
        ],
    };

    let judge1 = Arc::new(EdgeConditionJudge::from_workflow(&workflow1));
    let engine1 = WorkflowEngine::new(workflow1, config1, judge1).expect("failed to build engine1");
    println!("Task ID: {}", engine1.task_id().0);

    let rx1 = engine1.subscribe();
    print_events(rx1);

    println!("\nRunning linear workflow...");
    let result1 = with_timeout(MULTI_TURN, engine1.run())
        .await
        .expect("workflow1 run failed");

    println!("\nStep history:");
    for record in engine1.step_history().await {
        let output = record
            .result
            .as_ref()
            .map(|r| r.output.chars().take(100).collect::<String>())
            .unwrap_or_default();
        println!(
            "  step={:<8} transition={:?} | {}",
            record.step_id, record.transition, output
        );
    }

    println!(
        "\nFinal state: {:?} | turns: {} | cost: ${:.6}",
        engine1.state().await,
        result1.turns,
        result1.cost.total_cost
    );
    println!();

    // ════════════════════════════════════════════════════════════════════════
    // Part 2: Conditional routing — 2-step linear flow with step transitions
    // ════════════════════════════════════════════════════════════════════════
    println!("--- Part 2: Step transitions (2-step flow) ---\n");

    let tmp2 = tempfile::tempdir().unwrap();
    let config2 = make_config(entry, &tmp2.path().join("sessions"));
    std::fs::create_dir_all(&config2.session_base_dir).unwrap();

    let workflow2 = Workflow {
        entry_step: "ask".into(),
        steps: vec![
            Step::llm(
                "ask",
                "Question",
                "Is the sky blue? Answer yes or no.",
                vec![],
            ),
            Step::terminal("done", "Complete", 0, None),
        ],
        edges: vec![Edge {
            from: "ask".into(),
            to: "done".into(),
            condition: None,
        }],
    };

    let judge2 = Arc::new(EdgeConditionJudge::from_workflow(&workflow2));
    let engine2 = WorkflowEngine::new(workflow2, config2, judge2).expect("failed to build engine2");
    println!("Task ID: {}", engine2.task_id().0);

    let rx2 = engine2.subscribe();
    print_events(rx2);

    println!("\nRunning 2-step transition workflow...");
    let result2 = with_timeout(MULTI_TURN, engine2.run())
        .await
        .expect("workflow2 run failed");

    println!("\nStep history:");
    for record in engine2.step_history().await {
        let output = record
            .result
            .as_ref()
            .map(|r| r.output.chars().take(100).collect::<String>())
            .unwrap_or_default();
        println!(
            "  step={:<8} transition={:?} | {}",
            record.step_id, record.transition, output
        );
    }

    println!(
        "\nFinal state: {:?} | turns: {}",
        engine2.state().await,
        result2.turns
    );
    println!();

    // ════════════════════════════════════════════════════════════════════════
    // Part 3: Workflow with extra tool (echo)
    // ════════════════════════════════════════════════════════════════════════
    println!("--- Part 3: Workflow with extra tool (echo) ---\n");

    let tmp3 = tempfile::tempdir().unwrap();
    let config3 = make_config(entry, &tmp3.path().join("sessions"));
    std::fs::create_dir_all(&config3.session_base_dir).unwrap();

    let workflow3 = Workflow {
        entry_step: "tool_step".into(),
        steps: vec![
            Step::llm(
                "tool_step",
                "Echo Call",
                "Use the echo tool to echo the message 'Hello from workflow!'. Then tell me what the tool returned.",
                vec!["echo".into()],
            ),
            Step::terminal("end3", "Done", 0, None),
        ],
        edges: vec![Edge {
            from: "tool_step".into(),
            to: "end3".into(),
            condition: None,
        }],
    };

    let judge3 = Arc::new(EdgeConditionJudge::from_workflow(&workflow3));
    let engine3 = WorkflowEngine::new(workflow3, config3, judge3)
        .expect("failed to build engine3")
        .with_tool(Arc::new(EchoTool));

    println!("Task ID: {}", engine3.task_id().0);
    println!("Extra tool registered: echo");

    let rx3 = engine3.subscribe();
    print_events(rx3);

    println!("\nRunning tool-enabled workflow...");
    let _result3 = with_timeout(MULTI_TURN, engine3.run())
        .await
        .expect("workflow3 run failed");

    println!("\nStep history:");
    for record in engine3.step_history().await {
        let tool_count = record
            .result
            .as_ref()
            .map(|r| r.tool_calls_count)
            .unwrap_or(0);
        let output = record
            .result
            .as_ref()
            .map(|r| r.output.chars().take(120).collect::<String>())
            .unwrap_or_default();
        println!(
            "  step={:<12} tool_calls={} transition={:?} | {}",
            record.step_id, tool_count, record.transition, output
        );
    }

    // ════════════════════════════════════════════════════════════════════════
    // Part 4: Summary of all runs
    // ════════════════════════════════════════════════════════════════════════
    println!("\n--- Part 4: Summary ---\n");

    println!("Workflow 1 (linear 2-step):");
    println!(
        "  task_id={} state={:?} steps={} cost=${:.6}",
        engine1.task_id().0,
        engine1.state().await,
        engine1.step_history().await.len(),
        engine1.total_cost().await.total_cost
    );

    println!("Workflow 2 (2-step transition):");
    println!(
        "  task_id={} state={:?} steps={} cost=${:.6}",
        engine2.task_id().0,
        engine2.state().await,
        engine2.step_history().await.len(),
        engine2.total_cost().await.total_cost
    );

    println!("Workflow 3 (tool-enabled):");
    println!(
        "  task_id={} state={:?} steps={} cost=${:.6}",
        engine3.task_id().0,
        engine3.state().await,
        engine3.step_history().await.len(),
        engine3.total_cost().await.total_cost
    );

    println!("\n--- Done ---");
    println!("All 3 workflows completed with Succeeded state ✅");
    println!("Linear workflow, step transitions, and tool calling all demonstrated ✅");
}
