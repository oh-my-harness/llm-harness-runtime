//! Focused regression tests for the Orchestrator decision protocol, through
//! the real production-relevant path: `PipelineController::wait_for_task` →
//! `AgentRunner` loop → correlated `send_message` → `DecisionWorkerRun`
//! sealing → controller advance. Exercises real internal scheduling, sealing,
//! and completion behavior (not only `parse_action`).

use std::sync::Arc as StdArc;
use std::sync::Arc;
use std::time::Duration;

use llm_harness_agent_team::{AgentId, AgentRunner, AgentStatus, SocialContext, TaskMessageKind};
use llm_harness_agent_team_studio::pipeline::{
    Action, ORCHESTRATOR_DECISION_TIMEOUT, ORCHESTRATOR_ID, PipelineController, parse_action,
};

const CONTROLLER_ID: &str = "pipeline-controller";
const ORCH_ID: &str = "orchestrator";
type StdLlm = Arc<dyn llm_harness_loop::LlmClient>;

fn action_reply(body: &str, task_id: &str) -> llm_harness_loop::test_utils::MockResponse {
    action_reply_kind(None, body, task_id)
}

fn action_reply_kind(
    kind: Option<TaskMessageKind>,
    body: &str,
    task_id: &str,
) -> llm_harness_loop::test_utils::MockResponse {
    use llm_harness_loop::test_utils::MockResponse;
    let mut args = serde_json::json!({
        "to": CONTROLLER_ID,
        "task_id": task_id,
        "body": body,
    });
    if let Some(kind) = kind {
        args["task_kind"] = serde_json::Value::String(
            match kind {
                TaskMessageKind::Result => "result",
                TaskMessageKind::Blocked => "blocked",
                TaskMessageKind::Error => "error",
                TaskMessageKind::Ack => "ack",
                TaskMessageKind::Progress => "progress",
            }
            .to_owned(),
        );
    }
    MockResponse::tool_use_end_turn("reply", "send_message", &args.to_string())
}

/// One scenario world: a registered orchestrator + controller pair, a mock
/// model replaying `script`, and a spawned real `AgentRunner`.
struct DecisionScene {
    ctx: SocialContext,
    orchestrator: AgentId,
    controller_id: AgentId,
    call_count: StdArc<std::sync::atomic::AtomicUsize>,
    runner: tokio::task::JoinHandle<()>,
}

async fn scene(script: Vec<llm_harness_loop::test_utils::MockResponse>) -> DecisionScene {
    use llm_harness_agent::Plugin;

    let client = Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(script));
    let call_count = client.call_count.clone();
    let ctx = SocialContext::new(10);
    let orchestrator = AgentId::new(ORCH_ID);
    let controller_id = AgentId::new(CONTROLLER_ID);
    ctx.register_agent(orchestrator.clone(), "workflow routing");
    ctx.register_agent(controller_id.clone(), "controller");
    ctx.ensure_contact(&orchestrator, &controller_id);

    let plugin = llm_harness_agent_team::SocialToolPlugin::new(
        orchestrator.clone(),
        ctx.clone(),
        llm_harness_strategy::status_panel::TodoStore::new(),
    );
    let mut options = llm_harness_agent::AgentHarnessOptions::new("test-model");
    plugin.register_tools(&mut options.tools);
    let harness = StdArc::new(
        llm_harness_agent::AgentHarness::new_in_memory(
            client as StdLlm,
            StdArc::new(llm_harness_loop::test_utils::NoOpEnv),
            options,
        )
        .await,
    );
    let runner = AgentRunner::new(orchestrator.clone(), harness, ctx.clone(), None, None).spawn();
    DecisionScene {
        ctx,
        orchestrator,
        controller_id,
        call_count,
        runner,
    }
}

impl DecisionScene {
    /// Send the decision assignment exactly as the controller dispatch does.
    fn dispatch(&self, task_id: &str) {
        self.ctx
            .send_direct_with_task(
                &self.controller_id,
                &self.orchestrator,
                "Objective: decide. Choose exactly ONE next action.",
                10_000,
                Some(task_id.to_owned()),
                None,
            )
            .unwrap();
    }

    /// Drain-and-wait helper: collect controller-inbox messages until `pick`
    /// returns Some for the Nth message.
    async fn wait_for(
        &self,
        mut pick: impl FnMut(
            Vec<llm_harness_agent_team::InboxMessage>,
        ) -> Option<llm_harness_agent_team::InboxMessage>,
        what: &str,
    ) -> llm_harness_agent_team::InboxMessage {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let messages = self.ctx.drain_inbox(&self.controller_id);
                if let Some(found) = pick(messages) {
                    return found;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    /// Wait through the real runner loop until the orchestrator settles Idle.
    async fn settle_idle(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.ctx.get_agent_info(&self.orchestrator).unwrap().status != AgentStatus::Idle {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("orchestrator must quiesce to Idle");
        tokio::task::yield_now().await;
    }
}

impl Drop for DecisionScene {
    fn drop(&mut self) {
        self.runner.abort();
    }
}

/// Scenario 1: a valid ACTION delivered as a correlated (untyped) decision
/// terminalizes the controller wait, and the seal leaves the Runner Idle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_correlated_action_terminalizes_and_quiesces() {
    let task_id = "task-pipe-smoke-1-decision-1";
    let scene = scene(vec![action_reply(
        "ACTION: DISPATCH coder\nimplement the feature and report evidence",
        task_id,
    )])
    .await;
    scene.dispatch(task_id);

    let decision = scene
        .wait_for(
            |msgs| msgs.into_iter().find(|m| parse_action(&m.body).is_ok()),
            "valid correlated ACTION",
        )
        .await;
    assert_eq!(decision.from.as_str(), ORCH_ID);
    assert_eq!(decision.task_id.as_deref(), Some(task_id));
    assert_eq!(
        decision.body,
        "ACTION: DISPATCH coder\nimplement the feature and report evidence"
    );
    scene.settle_idle().await;
    assert_eq!(
        scene.call_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the accepted decision seals the run: exactly one provider turn"
    );
    assert!(
        scene.ctx.drain_inbox(&scene.controller_id).is_empty(),
        "no duplicate decision may be persisted after the accepted one"
    );
}

/// Scenario 2/3: a typed ACK / PROGRESS followed by a valid ACTION. ACK and
/// PROGRESS reach the controller inbox but must not terminate the decision;
/// the later ACTION does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ack_then_progress_then_action() {
    let task_id = "task-pipe-smoke-2-decision-1";
    let scene = scene(vec![
        action_reply_kind(
            Some(TaskMessageKind::Ack),
            "decision acknowledged; deliberating",
            task_id,
        ),
        action_reply_kind(
            Some(TaskMessageKind::Progress),
            "weighing dispatch vs finish",
            task_id,
        ),
        action_reply("ACTION: FINISH\nfinal user-facing answer", task_id),
    ])
    .await;
    scene.dispatch(task_id);

    let mut ack_seen = false;
    let mut progress_seen = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for msg in scene.ctx.drain_inbox(&scene.controller_id) {
                if parse_action(&msg.body).is_ok() {
                    return msg;
                }
                match msg.task_kind {
                    Some(TaskMessageKind::Ack) => ack_seen = true,
                    Some(TaskMessageKind::Progress) => progress_seen = true,
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("sealed ACTION must reach the controller");

    scene.settle_idle().await;
    assert!(
        ack_seen && progress_seen,
        "typed ACK and PROGRESS must be delivered as ordinary progress"
    );
    assert_eq!(
        scene.call_count.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "ACK/PROGRESS must not seal; only the final ACTION does"
    );
}

/// Scenario 4: a terminal/result message whose body carries malformed ACTION
/// text is not a decision: the runner retries, and only a valid ACTION seals.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_action_in_result_message_is_not_a_decision() {
    let task_id = "task-pipe-smoke-4-decision-1";
    let scene = scene(vec![
        action_reply_kind(
            Some(TaskMessageKind::Result),
            "prose first\nACTION: FINISH\nlate",
            task_id,
        ),
        action_reply("ACTION: FINISH\nwell-formed retry", task_id),
    ])
    .await;
    scene.dispatch(task_id);

    let decision = scene
        .wait_for(
            |msgs| msgs.into_iter().find(|m| parse_action(&m.body).is_ok()),
            "valid ACTION after malformed attempt",
        )
        .await;
    assert_eq!(decision.body, "ACTION: FINISH\nwell-formed retry");
    scene.settle_idle().await;
    assert_eq!(
        scene.call_count.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "malformed terminal reply must be retried, then sealed by the valid ACTION"
    );
}

/// Scenario 5: a plain-text message beginning with ACK but delivered without
/// ACK kind is ordinary progress; the decision stays open for a real ACTION.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ack_prefixed_plain_text_is_not_a_decision() {
    let task_id = "task-pipe-smoke-5-decision-1";
    let scene = scene(vec![
        llm_harness_loop::test_utils::MockResponse::text("ACK — decision in progress..."),
        action_reply("ACTION: ESCALATE\nblocked on credentials", task_id),
    ])
    .await;
    scene.dispatch(task_id);

    let decision = scene
        .wait_for(
            |msgs| msgs.into_iter().find(|m| parse_action(&m.body).is_ok()),
            "valid ACTION after plain-text ACK noise",
        )
        .await;
    assert_eq!(decision.body, "ACTION: ESCALATE\nblocked on credentials");
    scene.settle_idle().await;
    assert_eq!(
        scene.call_count.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "plain-text ACK is not a decision terminal: one retry, then the ACTION"
    );
}

/// Scenario 6: two ACTION messages from one decision task — the runner's
/// decision seal must make the second ACTION impossible at execution level.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_action_never_executes_after_seal() {
    let task_id = "task-pipe-smoke-6-decision-1";
    let scene = scene(vec![
        action_reply("ACTION: DISPATCH coder\nfirst assignment", task_id),
        action_reply("ACTION: FINISH\nunwanted second action", task_id),
    ])
    .await;
    scene.dispatch(task_id);

    let decision = scene
        .wait_for(
            |msgs| msgs.into_iter().find(|m| parse_action(&m.body).is_ok()),
            "first decision",
        )
        .await;
    assert_eq!(decision.body, "ACTION: DISPATCH coder\nfirst assignment");

    scene.settle_idle().await;
    assert_eq!(
        scene.call_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the sealed decision run must never start the second provider turn"
    );
    assert!(
        scene.ctx.drain_inbox(&scene.controller_id).is_empty(),
        "the second ACTION must never be persisted"
    );
}

/// Scenario 7: timeout without a valid ACTION. The controller-side
/// deadline (180s decision timeout) is virtualized with `start_paused`; the
/// orchestrator never supplies a valid ACTION, so the decision must
/// terminalize as a visible workflow failure with WorkflowStatus attribution.
#[tokio::test(start_paused = true)]
async fn timeout_without_valid_action_terminalizes_visibly() {
    let ctx = SocialContext::new(64);
    let orchestrator = AgentId::new(ORCHESTRATOR_ID);
    let controller_id = AgentId::new(CONTROLLER_ID);
    ctx.register_agent(orchestrator.clone(), "workflow routing");
    ctx.register_agent(controller_id.clone(), "controller");
    let workspace = tempfile::tempdir().unwrap();
    let controller = StdArc::new(PipelineController::new(
        "decision-timeout-e2e",
        ctx.clone(),
        workspace.path().to_path_buf(),
        &[ORCHESTRATOR_ID.to_string()],
        1,
    ));
    let issue = llm_harness_agent_team_studio::issue::Issue {
        id: "decision-timeout-e2e".into(),
        status: llm_harness_agent_team_studio::issue::IssueStatus::Confirmed,
        source: "test".into(),
        created: "test".into(),
        title: "wait for a decision".into(),
        body: "the provider never responds with a valid ACTION".into(),
    };
    let run = {
        let controller = controller.clone();
        let issue = issue.clone();
        tokio::spawn(async move { controller.run(&issue).await })
    };
    tokio::task::yield_now().await;
    tokio::time::advance(ORCHESTRATOR_DECISION_TIMEOUT).await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    let error = run.await.unwrap().unwrap_err();
    assert!(error.contains("timed out"), "unexpected error: {error}");
    let state = controller.state(&issue.id).unwrap();
    assert_eq!(state.phase, "escalated");
    let terminal = state.terminal_user_outcome.unwrap();
    assert_eq!(
        terminal.attribution,
        llm_harness_agent_team_studio::pipeline::TerminalAttribution::WorkflowStatus
    );
    assert!(
        terminal
            .body
            .contains("Workflow stopped: Orchestrator decision timed out.")
    );
}

/// An accepted decision must never leave the Runner Busy: after the FINISH
/// ACTION seals and the controller accepts it, the orchestrator must settle
/// Idle through the real runner loop, with no further provider turns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_decision_does_not_leave_the_runner_busy() {
    let task_id = "task-pipe-smoke-busy-decision-1";
    let scene = scene(vec![
        action_reply("ACTION: FINISH\nfinal answer", task_id),
        // A follow-up turn would only exist if the seal failed.
        action_reply("ACTION: ESCALATE\nmust never run", task_id),
    ])
    .await;
    scene.dispatch(task_id);

    let decision = scene
        .wait_for(
            |msgs| msgs.into_iter().find(|m| parse_action(&m.body).is_ok()),
            "accepted FINISH decision",
        )
        .await;
    assert_eq!(decision.body, "ACTION: FINISH\nfinal answer");
    scene.settle_idle().await;

    assert_eq!(
        scene.call_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "accepted decision must seal the run: no continuation provider turn"
    );
    assert_eq!(
        scene
            .ctx
            .get_agent_info(&scene.orchestrator)
            .unwrap()
            .status,
        AgentStatus::Idle,
        "an accepted decision must not leave the Runner indefinitely Busy"
    );
}

/// Scenario 7 path through `parse_action`: a PROGRESS body that happens to
/// embed an ACTION on a later line is still progress; only the first line is
/// executable.
#[test]
fn action_must_be_the_first_line() {
    assert!(parse_action("ACK — decision in progress...\nACTION: FINISH\nx").is_err());
    assert!(parse_action("PROGRESS: working\nACTION: FINISH\nx").is_err());
    assert!(parse_action("ACTION: DISPATCH coder\nwork").is_ok());
}

/// Silence dead-code lint while keeping helpers local to this file.
#[allow(dead_code)]
fn _unused(_action: &Action) {}
