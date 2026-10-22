//! Real loopback HTTP + the production Adapter, Harness, social tools and Runner.
//! No real credentials or external model calls. BASELINE_OBSERVE=1 records old behavior.
use llm_harness_agent::{AgentHarness, AgentHarnessOptions, Plugin};
use llm_harness_agent_team::{
    AgentId, AgentRunner, SocialContext, SocialToolPlugin, TaskMessageKind,
};
use llm_harness_loop::{LlmClient, RetryConfig};
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

struct RejectReplayHook;
impl ProviderErrorHook for RejectReplayHook {
    fn on_provider_error<'a>(
        &'a self,
        _: ProviderErrorCtx<'a>,
    ) -> futures::future::BoxFuture<'a, ErrorDirective> {
        // An unsafe hook must not bypass the generation replay guard.
        Box::pin(async { ErrorDirective::Retry })
    }
}
struct Effect {
    count: Arc<AtomicUsize>,
    schema: serde_json::Value,
}
impl Tool for Effect {
    fn name(&self) -> &str {
        "effect"
    }
    fn description(&self) -> &str {
        "count a real side effect"
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        &self.schema
    }
    fn execute<'a>(&'a self, _: serde_json::Value, _: &'a ToolContext) -> ToolFuture<'a> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(ToolResult::full(vec![], json!({}), false)) })
    }
}
async fn case(role: &str, fault: Fault, work: bool, success: bool, inner_retry: bool) -> bool {
    let task_id = format!("task-pipe-http-{role}-1");
    let server = server(fault, work, success, role, &task_id).await;
    let refused;
    let url = if matches!(fault, Fault::Refused) {
        refused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", refused.local_addr().unwrap());
        drop(refused);
        url
    } else {
        server.url.clone()
    };
    let provider: Arc<dyn LlmClient> = Arc::new(
        llm_adapter::openai::OpenAIProvider::builder("mock-only")
            .base_url(url)
            .build(),
    );
    let ctx = SocialContext::new(100);
    let member = AgentId::new(role);
    let controller = AgentId::new("pipeline-controller");
    ctx.register_agent(member.clone(), "test");
    ctx.register_agent(controller.clone(), "test controller");
    ctx.ensure_contact(&member, &controller);
    let mut options = AgentHarnessOptions::new("test-model");
    if inner_retry {
        options.retry = Some(RetryConfig::new(2, 0));
        options.stream_options.max_retries = Some(2);
        if !matches!(fault, Fault::RateLimit) {
            options
                .hooks
                .provider_error
                .push(Arc::new(RejectReplayHook));
        }
    }
    if matches!(fault, Fault::Idle) {
        options.stream_options.stream_idle_timeout_ms = Some(30);
    }
    if matches!(fault, Fault::Timeout) {
        options.stream_options.timeout_ms = Some(30);
    }
    let effects = Arc::new(AtomicUsize::new(0));
    options.tools.push(Arc::new(Effect {
        count: effects.clone(),
        schema: json!({"type":"object","properties":{}}),
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
        &controller,
        &member,
        "work once and report",
        0,
        Some(task_id.clone()),
        None,
    )
    .unwrap();
    let runner = AgentRunner::new(member.clone(), harness.clone(), ctx.clone(), None, None).spawn();
    let reply = tokio::time::timeout(Duration::from_millis(4300), async {
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
    .await
    .ok();
    tokio::time::sleep(Duration::from_millis(40)).await;
    let posts = server.posts.load(Ordering::SeqCst);
    let effect = effects.load(Ordering::SeqCst);
    let idle =
        ctx.get_agent_info(&member).unwrap().status == llm_harness_agent_team::AgentStatus::Idle;
    let empty = !ctx.has_messages(&member);
    println!(
        "HTTP_CASE role={role} fault={fault:?} work={work} success={success} inner_retry={inner_retry} POST={posts} tools={effect} idle={idle} empty={empty} terminal={:?} error={:?}",
        reply.as_ref().map(|m| (&m.task_kind, &m.body)),
        harness.state().error_message
    );
    runner.abort();
    let expected = if matches!(fault, Fault::Refused) {
        0
    } else if matches!(fault, Fault::RateLimit) {
        if work {
            2 + if inner_retry { 2 } else { 0 }
        } else {
            3
        }
    } else if matches!(fault, Fault::Http504) && inner_retry {
        // Patched adapter: 504 → ServerError and 408 → Timeout, so the
        // repair-hook branch never fires — the failed generation is never
        // resent. (Pre-adapter-fix baseline: unbounded hook retry reached
        // 2640+ POSTs, then a loop-side cap allowed one resend.)
        1 + usize::from(work)
    } else if matches!(fault, Fault::PartialSse) && inner_retry {
        // A dropped SSE body is retried at request level (2 retries) because
        // no tool from that generation executed. `work` counts the earlier
        // successful tool turn, which is never replayed.
        1 + usize::from(work) + 2
    } else {
        1 + usize::from(work)
    };
    let terminal_ok = reply.as_ref().is_some_and(|m| {
        m.task_id.as_deref() == Some(task_id.as_str())
            && if success {
                if role == "orchestrator" {
                    m.body.starts_with("ACTION: FINISH")
                } else {
                    m.task_kind == Some(TaskMessageKind::Result)
                }
            } else if role == "orchestrator" {
                m.body.starts_with("ACTION: ESCALATE")
            } else {
                m.task_kind == Some(TaskMessageKind::Error)
            }
    });
    posts == expected && effect == usize::from(work) && idle && empty && terminal_ok
}
#[tokio::test]
async fn http_failure_matrix() {
    let mut failed = Vec::new();
    for role in ["coder", "reviewer", "orchestrator"] {
        for fault in [
            Fault::DropPost,
            Fault::PartialSse,
            Fault::Http502,
            Fault::Invalid,
            Fault::Refused,
        ] {
            for work in [false, true] {
                if work && matches!(fault, Fault::Refused) {
                    continue;
                }
                if !case(role, fault, work, false, false).await {
                    failed.push(format!("{role}/{fault:?}/work={work}"));
                }
            }
        }
        assert!(
            case(role, Fault::DropPost, true, true, false).await,
            "legal continuation {role}"
        );
    }
    // Optional inner retry must not bypass the runner's guard.
    for fault in [Fault::DropPost, Fault::PartialSse, Fault::Http502] {
        if !case("coder", fault, false, false, true).await {
            failed.push(format!("inner/{fault:?}"));
        }
    }
    println!("VIOLATIONS={failed:?}");
    if std::env::var_os("BASELINE_OBSERVE").is_none() {
        assert!(failed.is_empty(), "automatic replay: {failed:?}");
    }
}

#[tokio::test]
async fn optional_retry_and_safe_admission_rejection() {
    for role in ["coder", "reviewer", "orchestrator"] {
        assert!(
            case(role, Fault::RateLimit, false, false, false).await,
            "bounded 429 retry {role}"
        );
        assert!(
            case(role, Fault::RateLimit, true, false, false).await,
            "429 must not replay earlier tools {role}"
        );
    }
    for fault in [Fault::Idle, Fault::Timeout, Fault::RedirectRefused] {
        assert!(
            case("coder", fault, false, false, true).await,
            "inner retry bypass {fault:?}"
        );
    }
    assert!(
        case("coder", Fault::RateLimit, true, false, true).await,
        "safe retry of a rejected next model turn"
    );
}

#[tokio::test]
async fn http504_and_http408_never_resend_a_failed_generation() {
    // With the patched adapter (da50b70... on
    // fix/openai-5xx-408-classification): 504 → ServerError and 408 →
    // Timeout. Neither enters the InvalidRequest repair-hook branch, so a
    // mounted ProviderErrorHook can NEVER resend the same failed
    // generation — the automatic repair resend is gone entirely (pre-fix
    // baseline: unbounded hook retry reached 2640+ POSTs; the interim
    // loop-side cap allowed one resend).
    // Without the hook the outcome is unknown → single POST, runner guard
    // consumes. The tool must never be replayed either way.
    assert!(
        case("coder", Fault::Http504, false, false, false).await,
        "504 without hook must not retry"
    );
    assert!(
        case("coder", Fault::Http504, false, false, true).await,
        "504 + hook must be POST=1: the failed generation is never resent"
    );
    assert!(
        case("coder", Fault::Http504, true, false, true).await,
        "504 + hook with tool progress must not replay the tool turn"
    );
    assert!(
        case("coder", Fault::Http408, false, false, true).await,
        "408 + hook must be POST=1: Timeout is not repairable"
    );
}
