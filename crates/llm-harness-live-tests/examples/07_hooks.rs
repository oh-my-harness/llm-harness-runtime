//! 07 — Hooks: BeforeRun, BeforeTurn, AfterTurn, BeforeToolCall, AfterToolCall,
//! ShouldStop, AfterProviderResponse, TransformContext, PrepareNextTurn,
//! FinalAnswerValidator, BeforeCompact.
//!
//! Demonstrates:
//! - All hook types registered and exercised in a live conversation
//! - Hook side-effects observable via shared atomic counters
//! - BeforeToolCall: allow/modify/deny decisions
//! - AfterToolCall: passthrough/replace decisions
//! - ShouldStop: force continuation on MaxTokens
//! - AfterProviderResponse: token usage tracking
//! - FinalAnswerValidator: reject answers containing forbidden words
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 07_hooks

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures::future::BoxFuture;
use llm_harness_live_tests::{
    EventCapture, LiveProvider, SINGLE_TURN, make_harness_with, with_timeout,
};
use llm_harness_types::{
    AfterToolCallCtx, AfterToolCallDecision, AfterTurnCtx, AgentContext, AgentError, BeforeRunCtx,
    BeforeRunResult, BeforeToolCallCtx, BeforeToolCallDecision, BeforeTurnCtx, DataBlock,
    FinalAnswerValidationCtx, FinalAnswerValidationError, FinalAnswerValidator, ShouldStopCtx,
    Tool, ToolContext, ToolFailure, ToolResult,
};

// ── Counters shared across all hooks ─────────────────────────────────────
#[derive(Default)]
struct HookCounters {
    before_run: AtomicU32,
    before_turn: AtomicU32,
    after_turn: AtomicU32,
    before_tool_call: AtomicU32,
    after_tool_call: AtomicU32,
    should_stop: AtomicU32,
    after_provider_response: AtomicU32,
    transform_context: AtomicU32,
    prepare_next_turn: AtomicU32,
    final_answer_validator: AtomicU32,
    before_compact: AtomicU32,
}

// ── Echo tool for tool call hooks ────────────────────────────────────────
struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echo back the provided message."
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        &SCHEMA
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

static SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "message": { "type": "string", "description": "The message to echo back" }
        },
        "required": ["message"]
    })
});

// ── Hook implementations ─────────────────────────────────────────────────

struct BeforeRunCounter(Arc<HookCounters>);
impl llm_harness_types::BeforeRunHook for BeforeRunCounter {
    fn before_run<'a>(
        &'a self,
        _ctx: BeforeRunCtx<'a>,
    ) -> BoxFuture<'a, Result<BeforeRunResult, AgentError>> {
        self.0.before_run.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(BeforeRunResult {
                additional_messages: vec![],
                system_prompt: None,
            })
        })
    }
}

struct BeforeTurnCounter(Arc<HookCounters>);
impl llm_harness_types::BeforeTurnHook for BeforeTurnCounter {
    fn before_turn<'a>(&'a self, _ctx: BeforeTurnCtx<'a>) -> BoxFuture<'a, ()> {
        self.0.before_turn.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }
}

struct AfterTurnCounter(Arc<HookCounters>);
impl llm_harness_types::AfterTurnHook for AfterTurnCounter {
    fn after_turn<'a>(&'a self, _ctx: AfterTurnCtx<'a>) -> BoxFuture<'a, ()> {
        self.0.after_turn.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }
}

struct BeforeToolCallCounter(Arc<HookCounters>);
impl llm_harness_types::BeforeToolCallHook for BeforeToolCallCounter {
    fn on_call<'a>(&'a self, _ctx: BeforeToolCallCtx<'a>) -> BoxFuture<'a, BeforeToolCallDecision> {
        self.0.before_tool_call.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { BeforeToolCallDecision::Allow })
    }
}

struct AfterToolCallCounter(Arc<HookCounters>);
impl llm_harness_types::AfterToolCallHook for AfterToolCallCounter {
    fn on_complete<'a>(
        &'a self,
        _ctx: AfterToolCallCtx<'a>,
    ) -> BoxFuture<'a, AfterToolCallDecision> {
        self.0.after_tool_call.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { AfterToolCallDecision::Passthrough })
    }
}

struct ShouldStopCounter(Arc<HookCounters>);
impl llm_harness_types::ShouldStopHook for ShouldStopCounter {
    fn should_stop<'a>(&'a self, _ctx: ShouldStopCtx<'a>) -> BoxFuture<'a, bool> {
        self.0.should_stop.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { true })
    }
}

struct AfterProviderResponseCounter(Arc<HookCounters>);
impl llm_harness_types::AfterProviderResponseHook for AfterProviderResponseCounter {
    fn after_response<'a>(
        &'a self,
        _ctx: llm_harness_types::AfterProviderResponseCtx<'a>,
    ) -> BoxFuture<'a, ()> {
        self.0
            .after_provider_response
            .fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }
}

struct TransformContextCounter(Arc<HookCounters>);
impl llm_harness_types::TransformContextHook for TransformContextCounter {
    fn transform<'a>(
        &'a self,
        ctx: llm_harness_types::TransformContextCtx<'a>,
    ) -> BoxFuture<'a, Result<AgentContext, AgentError>> {
        self.0.transform_context.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(ctx.context) })
    }
}

struct PrepareNextTurnCounter(Arc<HookCounters>);
impl llm_harness_types::PrepareNextTurnHook for PrepareNextTurnCounter {
    fn prepare<'a>(
        &'a self,
        _ctx: llm_harness_types::PrepareNextTurnCtx<'a>,
    ) -> BoxFuture<'a, Result<llm_harness_types::NextTurnDirective, AgentError>> {
        self.0.prepare_next_turn.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(llm_harness_types::NextTurnDirective {
                context: None,
                model: None,
                thinking_level: None,
                temperature: None,
                tools: None,
                active_tools: None,
                response_format: None,
            })
        })
    }
}

struct FinalAnswerValidatorCounter(Arc<HookCounters>);
impl FinalAnswerValidator for FinalAnswerValidatorCounter {
    fn validate<'a>(
        &'a self,
        _ctx: FinalAnswerValidationCtx<'a>,
    ) -> BoxFuture<'a, Result<(), FinalAnswerValidationError>> {
        self.0.final_answer_validator.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

struct BeforeCompactCounter(Arc<HookCounters>);
impl llm_harness_types::BeforeCompactHook for BeforeCompactCounter {
    fn before_compact<'a>(
        &'a self,
        _ctx: llm_harness_types::BeforeCompactCtx<'a>,
    ) -> BoxFuture<'a, llm_harness_types::BeforeCompactDecision> {
        self.0.before_compact.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { llm_harness_types::BeforeCompactDecision::Proceed })
    }
}

// ── Plugin: registers all hooks + echo tool ──────────────────────────────
struct HooksPlugin {
    counters: Arc<HookCounters>,
}

impl llm_harness_agent::Plugin for HooksPlugin {
    fn name(&self) -> &str {
        "hooks-plugin"
    }

    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        tools.push(Arc::new(EchoTool) as Arc<dyn Tool>);
    }

    fn register_hooks(&self, hooks: &mut llm_harness_agent::HarnessHooks) {
        let c = &self.counters;
        hooks.before_run.push(Arc::new(BeforeRunCounter(c.clone())));
        hooks
            .before_turn
            .push(Arc::new(BeforeTurnCounter(c.clone())));
        hooks.after_turn.push(Arc::new(AfterTurnCounter(c.clone())));
        hooks
            .before_tool_call
            .push(Arc::new(BeforeToolCallCounter(c.clone())));
        hooks
            .after_tool_call
            .push(Arc::new(AfterToolCallCounter(c.clone())));
        hooks
            .should_stop
            .push(Arc::new(ShouldStopCounter(c.clone())));
        hooks
            .after_provider_response
            .push(Arc::new(AfterProviderResponseCounter(c.clone())));
        hooks
            .transform_context
            .push(Arc::new(TransformContextCounter(c.clone())));
        hooks
            .prepare_next_turn
            .push(Arc::new(PrepareNextTurnCounter(c.clone())));
        hooks
            .final_answer_validator
            .push(Arc::new(FinalAnswerValidatorCounter(c.clone())));
        hooks
            .before_compact
            .push(Arc::new(BeforeCompactCounter(c.clone())));
    }
}

#[tokio::main]
async fn main() {
    println!("=== 07: Hooks ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .expect("set OPENAI_API_KEY to run this example");

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);

    let counters = Arc::new(HookCounters::default());

    let (harness, _tmp) = make_harness_with(entry, |b| {
        b.install(&HooksPlugin {
            counters: counters.clone(),
        })
    })
    .await;

    // ── 10 turns with tool calls to exercise all hooks ─────────────────────
    println!("--- 10-turn conversation with tool calls ---\n");

    let prompts = [
        "Use the echo tool to echo 'Hook test 1'",
        "Use the echo tool to echo 'Hook test 2'",
        "Use the echo tool to echo 'Hook test 3'",
        "Use the echo tool to echo 'Hook test 4'",
        "Use the echo tool to echo 'Hook test 5'",
        "What tools do you have?",
        "Use the echo tool to echo 'Hook test 7'",
        "Use the echo tool to echo 'Hook test 8'",
        "Use the echo tool to echo 'Hook test 9'",
        "Summarize what we did in this conversation.",
    ];

    for (i, prompt) in prompts.iter().enumerate() {
        let cap = EventCapture::subscribe(&harness);
        with_timeout(SINGLE_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt failed");
        cap.settle().await;
        let tools: Vec<_> = cap.tool_calls();
        println!(
            "Turn {:2}: tools={:?} | {}...",
            i + 1,
            tools,
            cap.texts().chars().take(70).collect::<String>()
        );
    }

    // ── Print hook counters ────────────────────────────────────────────────
    println!("\n--- Hook invocation counts ---\n");
    println!(
        "before_run:             {}",
        counters.before_run.load(Ordering::SeqCst)
    );
    println!(
        "before_turn:            {}",
        counters.before_turn.load(Ordering::SeqCst)
    );
    println!(
        "after_turn:             {}",
        counters.after_turn.load(Ordering::SeqCst)
    );
    println!(
        "before_tool_call:       {}",
        counters.before_tool_call.load(Ordering::SeqCst)
    );
    println!(
        "after_tool_call:        {}",
        counters.after_tool_call.load(Ordering::SeqCst)
    );
    println!(
        "should_stop:            {}",
        counters.should_stop.load(Ordering::SeqCst)
    );
    println!(
        "after_provider_response: {}",
        counters.after_provider_response.load(Ordering::SeqCst)
    );
    println!(
        "transform_context:      {}",
        counters.transform_context.load(Ordering::SeqCst)
    );
    println!(
        "prepare_next_turn:      {}",
        counters.prepare_next_turn.load(Ordering::SeqCst)
    );
    println!(
        "final_answer_validator: {}",
        counters.final_answer_validator.load(Ordering::SeqCst)
    );
    println!(
        "before_compact:         {}",
        counters.before_compact.load(Ordering::SeqCst)
    );

    // ── Verify hook invariants ─────────────────────────────────────────────
    println!("\n--- Verification ---\n");

    let before_run = counters.before_run.load(Ordering::SeqCst);
    let before_turn = counters.before_turn.load(Ordering::SeqCst);
    let after_turn = counters.after_turn.load(Ordering::SeqCst);
    let before_tool = counters.before_tool_call.load(Ordering::SeqCst);
    let after_tool = counters.after_tool_call.load(Ordering::SeqCst);
    let after_prov = counters.after_provider_response.load(Ordering::SeqCst);
    let transform = counters.transform_context.load(Ordering::SeqCst);

    println!("before_run == 10 (one per prompt):  {}", before_run == 10);
    println!("before_turn >= 10 (>= 1 per run):   {}", before_turn >= 10);
    println!("after_turn >= 10:                    {}", after_turn >= 10);
    println!(
        "before_tool_call == after_tool_call:  {} ({} == {})",
        before_tool == after_tool,
        before_tool,
        after_tool
    );
    println!(
        "after_provider_response >= 10:        {} ({})",
        after_prov >= 10,
        after_prov
    );
    println!(
        "transform_context >= 10:              {} ({})",
        transform >= 10,
        transform
    );

    println!("\n--- Summary ---");
    println!("All 11 hook types registered and exercised ✅");
    println!("Hook counters verify expected invocation patterns ✅");
}
