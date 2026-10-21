//! Repro: Orchestrator runs a tool, then the model ends normally WITHOUT a
//! valid ACTION. The guard (missing_after_tools) previously covered only
//! controlled workers, so the Orchestrator path fell into the
//! MissingTerminalReport retry and replayed the already-executed tool.
//! Assertions: exactly 2 POSTs, tool executed exactly once, runtime-authored
//! ACTION: ESCALATE to the controller, agent idle, inbox empty.
use llm_harness_agent::{AgentHarness, AgentHarnessOptions, Plugin};
use llm_harness_agent_team::{
    AgentId, AgentRunner, SocialContext, SocialToolPlugin, TaskMessageKind,
};
use llm_harness_loop::LlmClient;
use llm_harness_strategy::status_panel::TodoStore;
use llm_harness_types::*;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

#[path = "support/http_provider.rs"]
mod http_provider;
use http_provider::{Fault, server};

struct EffectShim {
    count: Arc<AtomicUsize>,
}
impl Tool for EffectShim {
    fn name(&self) -> &str {
        "effect"
    }
    fn description(&self) -> &str {
        "count a real side effect"
    }
    fn parameters_schema(&self) -> &'static serde_json::Value {
        &EFFECT_SCHEMA
    }
    fn execute<'a>(&'a self, _: serde_json::Value, _: &'a ToolContext) -> ToolFuture<'a> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(ToolResult::full(vec![], json!({}), false)) })
    }
}
static EFFECT_SCHEMA: std::sync::LazyLock<serde_json::Value> =
    std::sync::LazyLock::new(|| json!({"type":"object","properties":{}}));

#[tokio::test]
async fn orchestrator_malformed_action_after_tool_is_not_replayed() {
    // Side-effect tool ran, then a correlated but malformed ACTION was
    // delivered and the run ended normally. The malformed delivery is
    // visible progress at the controller; the runner must not requeue the
    // assignment (a replay would run the tool again) and must not author an
    // ESCALATE — the decision timeout owns the terminal.
    orchestrator_or_worker_case("orchestrator", Fault::MalformedAction, "prose first").await;
}

#[tokio::test]
async fn orchestrator_silent_end_after_tool_is_not_replayed() {
    orchestrator_or_worker_case("orchestrator", Fault::NoTerminal, "ACTION: ESCALATE").await;
}

#[tokio::test]
async fn coder_silent_end_after_tool_is_not_replayed() {
    // Worker equivalent: same guard must cover controlled workers (regression
    // check that broadening the guard keeps the coder path identical).
    orchestrator_or_worker_case(
        "coder",
        Fault::NoTerminal,
        "runtime execution failure: automatic replay suppressed",
    )
    .await;
}

async fn orchestrator_or_worker_case(role: &str, fault: Fault, expect_prefix: &str) {
    let task_id = "task-pipe-noaction-1";
    let server = server(fault, true, false, role, task_id).await;
    let provider: Arc<dyn LlmClient> = Arc::new(
        llm_adapter::openai::OpenAIProvider::builder("mock-only")
            .base_url(server.url.clone())
            .build(),
    );
    let ctx = SocialContext::new(100);
    let member = AgentId::new(role);
    let controller = AgentId::new("pipeline-controller");
    ctx.register_agent(member.clone(), "test");
    ctx.register_agent(controller.clone(), "test controller");
    ctx.ensure_contact(&member, &controller);
    let mut options = AgentHarnessOptions::new("test-model");
    let effects = Arc::new(AtomicUsize::new(0));
    options.tools.push(Arc::new(EffectShim {
        count: effects.clone(),
    }));
    SocialToolPlugin::new(member.clone(), ctx.clone(), TodoStore::new())
        .register_tools(&mut options.tools);
    let harness = Arc::new(
        AgentHarness::new_in_memory(
            provider,
            Arc::new(llm_harness_loop::test_utils::NoOpEnv),
            options,
        )
        .await,
    );
    ctx.send_direct_with_task(
        &AgentId::new("pipeline-controller"),
        &member,
        "work once and decide",
        0,
        Some(task_id.to_owned()),
        None,
    )
    .unwrap();
    let runner = AgentRunner::new(member.clone(), harness.clone(), ctx.clone(), None, None).spawn();
    let reply = tokio::time::timeout(Duration::from_millis(6000), async {
        loop {
            if let Some(m) = ctx
                .drain_inbox(&controller)
                .into_iter()
                .find(|m| m.task_kind != Some(TaskMessageKind::Progress))
            {
                break m;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    let posts = server.posts.load(Ordering::SeqCst);
    let effect = effects.load(Ordering::SeqCst);
    let idle =
        ctx.get_agent_info(&member).unwrap().status == llm_harness_agent_team::AgentStatus::Idle;
    let empty = !ctx.has_messages(&member);
    println!(
        "NOACTION posts={posts} tools={effect} idle={idle} empty={empty} terminal={:?} err={:?}",
        reply
            .as_ref()
            .map(|m| (&m.task_kind, m.body.chars().take(90).collect::<String>())),
        harness.state().error_message
    );
    runner.abort();
    // The executed tool must not be replayed; the run must reach a clean
    // idle state without silent loss.
    // - MalformedAction: one generation = effect POST + malformed-delivery
    //   POST + plain-stop POST = 3; the only controller-facing message is the
    //   model's own (non-parsing) delivery. No runner requeue, no ESCALATE —
    //   the decision timeout owns the terminal.
    // - NoTerminal decision run: silent end → immediate runner ESCALATE
    //   (2 POSTs). Worker silent end: one corrective turn (3 POSTs).
    let expected_posts = match (role, fault) {
        ("orchestrator", Fault::MalformedAction) => 3,
        ("orchestrator", _) => 2,
        _ => 3,
    };
    assert_eq!(
        posts, expected_posts,
        "no replay beyond the corrective turn"
    );
    assert_eq!(effect, 1, "executed tool must run exactly once");
    assert!(idle && empty);
    let reply = reply.expect("controller must receive a terminal");
    assert!(
        reply.body.starts_with(expect_prefix),
        "expected terminal prefix {}: {}",
        expect_prefix,
        reply.body
    );
    // The controller must never receive a runner-authored terminal for the
    // malformed-delivery case: the model's own message is the last word and
    // the decision timeout owns the workflow terminal.
    if fault == Fault::MalformedAction {
        assert_eq!(
            reply.task_kind, None,
            "runner must not author a terminal for an attempted decision"
        );
    }
}
