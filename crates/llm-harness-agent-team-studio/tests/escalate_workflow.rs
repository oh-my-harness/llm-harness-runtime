//! Focused regressions for the controlled coding-team workflow.
//!
//! Both tests drive the REAL controller → runner → send_message stack:
//! mock LLM clients reply through the production `send_message` tool, the
//! real `AgentRunner` loop delivers them, and the real
//! `PipelineController::run` / `wait_for_task` / decision handler reacts.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use llm_harness_agent_team::AgentId;
use llm_harness_agent_team_studio::issue::IssueStatus;
use llm_harness_agent_team_studio::pipeline::{
    PipelineController, PipelinePhase, TerminalAttribution, TerminalUserOutcome,
};
use llm_harness_agent_team_studio::team_manager::TeamManager;
use llm_harness_agent_team_studio::team_spec::{MemberSpec, ModelRef, TeamSpec};
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
use llm_harness_loop::{
    ChatRequest, ChatResponse, LlmClient, LlmError, ProviderCapabilities, RequestContent,
    StreamHandle,
};

/// Serializes the process-wide provider factory, mirroring integration.rs.
static FACTORY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Hard test-side bound for one scripted workflow; mirrors integration.rs.
const STAGE_BOUND: Duration = Duration::from_secs(8);

static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

fn unique_dir(prefix: &str) -> std::path::PathBuf {
    let n = DIR_SEQ.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{n}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn make_repo() -> std::path::PathBuf {
    let dir = unique_dir("esc-e2e-repo");
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    dir
}

fn settings() -> llm_harness_agent_team_studio::settings::Settings {
    llm_harness_agent_team_studio::settings::Settings {
        models: llm_harness_agent_team_studio::roles::ModelTier {
            strong: "m".into(),
            main: "m".into(),
            cheap: "m".into(),
        },
        scout_interval_secs: 0,
        base_url: String::new(),
    }
}

/// Four-member decision team, mirroring the integration-suite pattern.
fn decision_spec(id: &str, repo: &std::path::Path) -> TeamSpec {
    TeamSpec {
        id: id.into(),
        template: None,
        template_baseline: None,
        name: id.into(),
        repo_path: Some(repo.to_string_lossy().to_string()),
        sandbox_mode: None,
        members: ["orchestrator", "coder", "reviewer-robustness", "judge"]
            .iter()
            .map(|m| MemberSpec {
                id: (*m).into(),
                persona: format!("{m} persona"),
                role_label: m.to_string(),
                model: ModelRef::Explicit((*m).into()),
                toolkits: vec![],
                behaviors: vec![],
                provider_override: None,
                persona_file: None,
            })
            .collect(),
    }
}

fn install_providers(providers: Vec<(&str, Arc<dyn LlmClient>)>) {
    let providers: Vec<(String, Arc<dyn LlmClient>)> = providers
        .into_iter()
        .map(|(id, client)| (id.to_owned(), client))
        .collect();
    llm_harness_agent_team_studio::env_cfg::install_provider_factory(Arc::new(move || {
        providers.clone()
    }));
}

fn flatten_request(req: &ChatRequest) -> String {
    req.messages()
        .iter()
        .filter_map(|m| match m {
            llm_harness_loop::LlmMessage::User(c) => Some(
                c.iter()
                    .filter_map(|p| match p {
                        RequestContent::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract the controlled task id a dispatch body demanded.
fn task_id_from_request(req: &ChatRequest) -> String {
    flatten_request(req)
        .rsplit_once("task_id=\"")
        .expect("controlled dispatch carries task_id=\"...\"")
        .1
        .split('"')
        .next()
        .unwrap()
        .to_string()
}

/// Ordered record of pipeline lifecycle events from the controller sink.
#[derive(Default)]
struct StageLedger {
    events: Mutex<Vec<(String, serde_json::Value)>>,
}

/// Model-isolated scripted mock: replies through the production
/// `send_message` tool with the task id the dispatch carried. Bodies are
/// consumed in order, one per provider call.
struct EchoClient {
    bodies: Mutex<VecDeque<String>>,
    replies: AtomicUsize,
    call_count: AtomicUsize,
}

impl EchoClient {
    fn new(bodies: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            bodies: Mutex::new(bodies.iter().map(|s| s.to_string()).collect()),
            replies: AtomicUsize::new(0),
            call_count: AtomicUsize::new(0),
        })
    }
}

impl LlmClient for EchoClient {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::new(false, false, false)
    }

    fn chat<'a, 'b, 'f>(
        &'a self,
        req: &'b ChatRequest,
    ) -> futures::future::BoxFuture<'f, Result<ChatResponse, LlmError>>
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        Box::pin(async move { self.chat_stream(req).await?.collect().await })
    }

    fn chat_stream<'a, 'b, 'f>(
        &'a self,
        req: &'b ChatRequest,
    ) -> futures::future::BoxFuture<'f, Result<StreamHandle, LlmError>>
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        Box::pin(async move {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            // Decision sealing makes the correlated send_message the run's
            // final tool action, so one scripted body per provider call.
            // Exhausted scripts fall back to an idle ACK-shaped result so
            // unrelated members (judge) never panic the runner.
            let body = self
                .bodies
                .lock()
                .pop_front()
                .unwrap_or_else(|| "(idle: no scripted work)".into());
            let task = if flatten_request(req).contains("task_id=\"") {
                task_id_from_request(req)
            } else {
                String::new()
            };
            let task_kind = if body.starts_with("ACTION:") {
                None
            } else {
                Some("result")
            };
            self.replies.fetch_add(1, Ordering::SeqCst);
            let mut args = serde_json::json!({
                "to": "pipeline-controller",
                "task_id": task,
                "body": body,
            });
            if let Some(kind) = task_kind {
                args["task_kind"] = serde_json::Value::String(kind.into());
            }
            let response = MockResponse::tool_use_end_turn(
                "reply",
                "send_message",
                &args.to_string(),
            );
            MockLlmClient::with_single(response).chat_stream(req).await
        })
    }
}

// ── Test 1: ESCALATE through the real decision handler ─────────────────────

async fn run_escalation_case(escalate_body: &str, expected: TerminalUserOutcome) {
    let _guard = FACTORY_LOCK.lock().await;
    let repo = make_repo();

    let orchestrator = EchoClient::new(&[escalate_body]);
    let coder = EchoClient::new(&[]);
    let reviewer = EchoClient::new(&[]);
    let judge = EchoClient::new(&[]);
    install_providers(vec![
        ("orchestrator", orchestrator.clone()),
        ("coder", coder.clone()),
        ("reviewer-robustness", reviewer.clone()),
        ("judge", judge.clone()),
    ]);

    let data_root = unique_dir("ct-escalate");
    let mut pm = TeamManager::new(&data_root);
    pm.create(
        decision_spec("escalate", &repo),
        Some(repo.clone()),
        &settings(),
        "",
    )
    .await
    .unwrap();
    let (ctx, members, issues_root) = {
        let entry = pm.get("escalate").unwrap();
        let members: Vec<String> = entry.spec.members.iter().map(|m| m.id.clone()).collect();
        (
            entry.team.social_context().unwrap().clone(),
            members,
            entry.issues.root().to_path_buf(),
        )
    };
    let ledger = Arc::new(StageLedger::default());
    let controller = Arc::new(
        PipelineController::new("escalate", ctx.clone(), repo.clone(), &members, 3).with_sink({
            let ledger = Arc::clone(&ledger);
            Arc::new(move |event: &str, payload: &serde_json::Value| {
                ledger.events.lock().push((event.to_string(), payload.clone()));
            })
        }),
    );

        let store = llm_harness_agent_team_studio::issue::IssueStore::new(&issues_root);
        let issue = store
            .create("operator", "escalate me", "the orchestrator will refuse")
            .unwrap();
        store.set_status(&issue.id, IssueStatus::Confirmed).unwrap();
        let confirmed = store.load(&issue.id).unwrap().0;

        let run_handle = tokio::spawn({
            let controller = Arc::clone(&controller);
            let issue = confirmed.clone();
            async move { (*controller).run(&issue).await }
        });
        let outcome = tokio::time::timeout(STAGE_BOUND, run_handle)
            .await
            .expect("workflow did not reach a terminal state in time")
            .expect("workflow task panicked");
        // Escalation is not an Err: the controller returns Ok(None).
        assert_eq!(outcome, Ok(None), "escalation must return Ok(None)");

        let state = controller.state(&confirmed.id).unwrap();
        assert_eq!(state.phase, PipelinePhase::Escalated.as_str(), "{state:?}");
        assert_eq!(
            state.terminal_reason.as_deref(),
            Some("orchestrator escalated"),
            "terminal reason must record the escalation"
        );
        let terminal = state.terminal_user_outcome.expect("terminal package");
        assert_eq!(terminal.attribution, expected.attribution);
        assert!(terminal.body.contains("**Outcome:** ESCALATE"));
        assert!(terminal.body.contains(&expected.body));
        assert!(terminal.body.contains("Verification commands and results: unavailable"));
        assert_eq!(
            ledger.events.lock().last().map(|(e, _)| e.clone()),
            Some("pipeline_escalated".to_string()),
            "terminal escalation event must be emitted exactly at the end"
        );

        // No worker dispatch: coder, reviewer, and judge never ran.
        assert_eq!(coder.call_count.load(Ordering::SeqCst), 0);
        assert_eq!(reviewer.call_count.load(Ordering::SeqCst), 0);
        assert_eq!(judge.call_count.load(Ordering::SeqCst), 0);

        // ESCALATE is a distinct terminal event, never "pipeline_completed".
        let names: Vec<String> = ledger
            .events
            .lock()
            .iter()
            .map(|(e, _)| e.clone())
            .collect();
        assert_eq!(names, vec!["pipeline_started", "pipeline_escalated"]);
        assert!(!names.contains(&"pipeline_completed".to_string()));

        pm.shutdown("escalate").await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn orchestrator_escalate_preserves_explanation_and_never_dispatches_workers() {
    run_escalation_case(
        "ACTION: ESCALATE\nBlocked: missing credentials for the target repo.",
        TerminalUserOutcome {
            attribution: TerminalAttribution::Orchestrator,
            body: "Blocked: missing credentials for the target repo.".into(),
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn orchestrator_escalate_empty_body_falls_back_to_workflow_status() {
    run_escalation_case(
        "ACTION: ESCALATE",
        TerminalUserOutcome {
            attribution: TerminalAttribution::WorkflowStatus,
            body:
                "Workflow stopped: the Orchestrator escalated without a user-facing explanation."
                    .into(),
        },
    )
    .await;
}

// ── Test 2: worker execution timeout actually cancels execution ────────────
//
// The production worker deadline (`WORKER_EXECUTION_TIMEOUT`, 1800 s) is
// production-owned and MUST NOT change; `execution_timeout_for` is private
// with no test seam, so the 1800 s deadline branch cannot fire inside a
// bounded test. What the deadline branch *does* is exactly:
//
//   ctx.cancel_controlled_task(task_id); ctx.retire_controlled_task(task_id);
//
// (pipeline.rs:634-641, wait_for_task timeout path), and the runner
// observes that cancellation via `token.cancelled()` in its
// `tokio::select!`, calls `harness.abort()`, consumes the assignment, and
// never retries it (agent_runner.rs:984-997, 1148-1157).
//
// This regression proves that end-to-end path through the REAL runner,
// harness, gate, and send_message stack: cancel the controller-owned
// controlled task while a coder is mid-mutation, then verify the running
// worker stops, no later mutating tool call or retry executes, and no
// active controlled execution remains.

/// A coder mock whose first turn performs a REAL workspace write (through
/// the production write tool) and whose second turn requests another write.
/// Both writes are observable via the filesystem.
#[derive(Default)]
struct MutatingWorkerClient {
    calls: AtomicUsize,
}

impl LlmClient for MutatingWorkerClient {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::new(false, false, false)
    }

    fn chat<'a, 'b, 'f>(
        &'a self,
        req: &'b ChatRequest,
    ) -> futures::future::BoxFuture<'f, Result<ChatResponse, LlmError>>
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        Box::pin(async move { self.chat_stream(req).await?.collect().await })
    }

    fn chat_stream<'a, 'b, 'f>(
        &'a self,
        req: &'b ChatRequest,
    ) -> futures::future::BoxFuture<'f, Result<StreamHandle, LlmError>>
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        Box::pin(async move {
            use llm_harness_loop::test_utils::MockResponse;
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let _task = task_id_from_request(req);
            let response = match call {
                0 => MockResponse::tool_use(
                    "mutate",
                    "write",
                    r#"{"path":"mutation.txt","content":"worker wrote while active"}"#,
                ),
                // Turn ≥1: a stream that stalls after ContentStart. The
                // harness blocks mid-provider-call, so any later scripted
                // call means a NEW run (retry) executed for the cancelled
                // assignment — which must never happen.
                _ => MockResponse::stall_after(1),
            };
            MockLlmClient::with_single(response).chat_stream(req).await
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn controlled_timeout_cancellation_path_stops_active_worker_and_blocks_later_work() {
    let _guard = FACTORY_LOCK.lock().await;
    let repo = make_repo();
    std::fs::write(repo.join("seed.txt"), "seed").unwrap();

    let orchestrator = EchoClient::new(&[
        "ACTION: DISPATCH coder\nImplement the change and report evidence.",
    ]);
    let coder = Arc::new(MutatingWorkerClient::default());
    let reviewer = EchoClient::new(&[]);
    let judge = EchoClient::new(&[]);
    install_providers(vec![
        ("timeout-orchestrator", orchestrator.clone()),
        ("timeout-coder", coder.clone()),
        ("timeout-reviewer", reviewer.clone()),
        ("judge", judge.clone()),
    ]);

    // Real coding-team template: coder carries the fs toolkit (write), the
    // controller's WorkflowToolGuard stays installed on every tool call.
    let store = llm_harness_agent_team_studio::template_store::TemplateStore::new(None);
    let mut spec = store.load("coding-team").unwrap().spec;
    spec.id = "timeout".into();
    spec.repo_path = Some(repo.to_string_lossy().to_string());
    spec.template = Some(llm_harness_agent_team_studio::team_spec::TemplateRef {
        template_id: "coding-team".into(),
        version: "6".into(),
    });
    for member in &mut spec.members {
        member.model = ModelRef::Explicit(
            match member.id.as_str() {
                "orchestrator" => "timeout-orchestrator",
                "coder" => "timeout-coder",
                "reviewer" => "timeout-reviewer",
                other => panic!("unexpected coding-team member {other}"),
            }
            .into(),
        );
    }

    let data_root = unique_dir("ct-timeout-cancel");
    let mut pm = TeamManager::new(&data_root);
    pm.create(spec, Some(repo.clone()), &settings(), "")
        .await
        .unwrap();
    let (ctx, members, workflow_gate, confirmed) = {
        let entry = pm.get("timeout").unwrap();
        let issue = entry
            .issues
            .create("operator", "work that will not finish", "coder never reports")
            .unwrap();
        entry
            .issues
            .set_status(&issue.id, IssueStatus::Confirmed)
            .unwrap();
        let confirmed = entry.issues.load(&issue.id).unwrap().0;
        let members = entry
            .spec
            .members
            .iter()
            .map(|m| m.id.clone())
            .collect::<Vec<_>>();
        let workflow_gate = entry.workflow.as_ref().map(|w| w.gate()).expect("controlled");
        (
            entry.team.social_context().unwrap().clone(),
            members,
            workflow_gate,
            confirmed,
        )
    };

    let ledger = Arc::new(StageLedger::default());
    let controller = Arc::new(
        PipelineController::new("timeout", ctx.clone(), repo.clone(), &members, 2)
            .with_gate(workflow_gate)
            .with_sink({
            let ledger = Arc::clone(&ledger);
            Arc::new(move |event: &str, payload: &serde_json::Value| {
                ledger.events.lock().push((event.to_string(), payload.clone()));
            })
        }),
    );

    let _run_handle = tokio::spawn({
        let controller = Arc::clone(&controller);
        let issue = confirmed.clone();
        async move { (*controller).run(&issue).await }
    });

    // Wait until the worker is ACTIVE mid-execution: its first real write
    // has landed on disk, and its second provider call is stalled mid-stream.
    let marker = repo.join("mutation.txt");
    tokio::time::timeout(STAGE_BOUND, async {
        loop {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("coder never completed its first mutating write");
    assert!(
        ctx.get_agent_info(&AgentId::new("coder"))
            .is_some_and(|info| info.status == llm_harness_agent_team::AgentStatus::Busy),
        "the worker must be active (mid-execution) at this point"
    );

    // Discover the worker task id from the ledger's `task_delegated` event
    // (same payload the controller emits on dispatch) and grab the coder's
    // real harness through the team's public accessor.
    let (worker_task_id, coder_harness) = {
        let entry = pm.get("timeout").unwrap();
        let delegated = ledger
            .events
            .lock()
            .iter()
            .find(|(event, payload)| event == "task_delegated" && payload["to"] == "coder")
            .map(|(event, payload)| (event.clone(), payload.clone()))
            .expect("coder dispatch event must exist while the worker runs");
        let task_id = delegated.1["task_id"].as_str().unwrap().to_string();
        let harness = entry
            .team
            .harness(&AgentId::new("coder"))
            .expect("coder harness must be built");
        (task_id, harness)
    };

    assert!(
        ctx.cancel_controlled_task(&worker_task_id),
        "controller must hold a live cancellation token for the active task"
    );

    // Bounded wait for the runner to observe cancellation and quiesce.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if coder_harness.state().phase == llm_harness_types::HarnessPhase::Idle {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("runner never observed the cancellation");

    tokio::time::sleep(Duration::from_millis(150)).await;
    // The coder blocked mid-provider-call (stalled stream). The token
    // cancel must abort that run; the runner consumes the assignment and
    // never schedules attempt 2. Exactly two provider calls total.
    assert_eq!(
        coder.calls.load(Ordering::SeqCst),
        2,
        "no retry or continuation may re-run the coder for the cancelled task"
    );
    assert!(
        ctx.controlled_task_token(&worker_task_id).is_none(),
        "cleanup must not leave an active controlled execution"
    );

    pm.shutdown("timeout").await.unwrap();
}
