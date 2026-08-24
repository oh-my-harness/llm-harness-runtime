//! Tests: all 11 hook types registered and exercised via live LLM calls.
//! Each test runs 3 turns so hook counters surface real invocation
//! patterns rather than one-shot artifacts.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures::future::BoxFuture;
use llm_harness_live_tests::{LiveProvider, SINGLE_TURN, make_harness_with, with_timeout};
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

// ── Echo tool for tool-call hook tests ───────────────────────────────────
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

// ── Plugin variants ──────────────────────────────────────────────────────

/// Registers only BeforeRunHook + EchoTool.
struct BeforeRunOnlyPlugin {
    counters: Arc<HookCounters>,
}
impl llm_harness_agent::Plugin for BeforeRunOnlyPlugin {
    fn name(&self) -> &str {
        "before-run-only"
    }
    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        tools.push(Arc::new(EchoTool) as Arc<dyn Tool>);
    }
    fn register_hooks(&self, hooks: &mut llm_harness_agent::HarnessHooks) {
        hooks
            .before_run
            .push(Arc::new(BeforeRunCounter(self.counters.clone())));
    }
}

/// Registers BeforeTurn + AfterTurn.
struct TurnPairPlugin {
    counters: Arc<HookCounters>,
}
impl llm_harness_agent::Plugin for TurnPairPlugin {
    fn name(&self) -> &str {
        "turn-pair"
    }
    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        tools.push(Arc::new(EchoTool) as Arc<dyn Tool>);
    }
    fn register_hooks(&self, hooks: &mut llm_harness_agent::HarnessHooks) {
        hooks
            .before_turn
            .push(Arc::new(BeforeTurnCounter(self.counters.clone())));
        hooks
            .after_turn
            .push(Arc::new(AfterTurnCounter(self.counters.clone())));
    }
}

/// Registers BeforeToolCall + AfterToolCall + EchoTool.
struct ToolCallPairPlugin {
    counters: Arc<HookCounters>,
}
impl llm_harness_agent::Plugin for ToolCallPairPlugin {
    fn name(&self) -> &str {
        "tool-call-pair"
    }
    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        tools.push(Arc::new(EchoTool) as Arc<dyn Tool>);
    }
    fn register_hooks(&self, hooks: &mut llm_harness_agent::HarnessHooks) {
        hooks
            .before_tool_call
            .push(Arc::new(BeforeToolCallCounter(self.counters.clone())));
        hooks
            .after_tool_call
            .push(Arc::new(AfterToolCallCounter(self.counters.clone())));
    }
}

/// Registers only ShouldStopHook.
struct ShouldStopOnlyPlugin {
    counters: Arc<HookCounters>,
}
impl llm_harness_agent::Plugin for ShouldStopOnlyPlugin {
    fn name(&self) -> &str {
        "should-stop-only"
    }
    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        tools.push(Arc::new(EchoTool) as Arc<dyn Tool>);
    }
    fn register_hooks(&self, hooks: &mut llm_harness_agent::HarnessHooks) {
        hooks
            .should_stop
            .push(Arc::new(ShouldStopCounter(self.counters.clone())));
    }
}

/// Registers only AfterProviderResponseHook.
struct AfterProviderResponseOnlyPlugin {
    counters: Arc<HookCounters>,
}
impl llm_harness_agent::Plugin for AfterProviderResponseOnlyPlugin {
    fn name(&self) -> &str {
        "after-provider-response-only"
    }
    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        tools.push(Arc::new(EchoTool) as Arc<dyn Tool>);
    }
    fn register_hooks(&self, hooks: &mut llm_harness_agent::HarnessHooks) {
        hooks
            .after_provider_response
            .push(Arc::new(AfterProviderResponseCounter(
                self.counters.clone(),
            )));
    }
}

/// Registers ALL 11 hook types + EchoTool.
struct AllHooksPlugin {
    counters: Arc<HookCounters>,
}
impl llm_harness_agent::Plugin for AllHooksPlugin {
    fn name(&self) -> &str {
        "all-hooks"
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

// ── Prompts ──────────────────────────────────────────────────────────────

/// Three prompts that invite echo-tool usage.
const ECHO_PROMPTS: &[&str] = &[
    "Use the echo tool to echo 'Hook test 1'.",
    "Use the echo tool to echo 'Hook test 2'.",
    "Use the echo tool to echo 'Hook test 3'.",
];

/// Three plain conversational prompts (no tool needed).
const CHAT_PROMPTS: &[&str] = &[
    "Tell me a one-sentence fact about the ocean.",
    "Name a primary color.",
    "What is 2 plus 2?",
];

// ── Tests ────────────────────────────────────────────────────────────────

/// BeforeRunHook fires exactly once per `prompt()` call.
#[tokio::test]
async fn before_run_fires_once_per_prompt() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let counters = Arc::new(HookCounters::default());
    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        b.install(&BeforeRunOnlyPlugin {
            counters: counters.clone(),
        })
    })
    .await;

    for prompt in ECHO_PROMPTS {
        with_timeout(SINGLE_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt should succeed");
    }

    let before_run = counters.before_run.load(Ordering::SeqCst);
    assert_eq!(
        before_run, 3,
        "before_run must fire once per prompt (expected 3, got {before_run})"
    );
}

/// BeforeTurn + AfterTurn fire symmetrically across 3 turns.
#[tokio::test]
async fn before_turn_and_after_turn_fire_per_turn() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let counters = Arc::new(HookCounters::default());
    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        b.install(&TurnPairPlugin {
            counters: counters.clone(),
        })
    })
    .await;

    for prompt in ECHO_PROMPTS {
        with_timeout(SINGLE_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt should succeed");
    }

    let before_turn = counters.before_turn.load(Ordering::SeqCst);
    let after_turn = counters.after_turn.load(Ordering::SeqCst);
    assert!(
        before_turn >= 3,
        "before_turn should fire >= 3 times (got {before_turn})"
    );
    assert!(
        after_turn >= 3,
        "after_turn should fire >= 3 times (got {after_turn})"
    );
    assert_eq!(
        before_turn, after_turn,
        "before_turn and after_turn must be symmetric ({before_turn} != {after_turn})"
    );
}

/// BeforeToolCall + AfterToolCall fire symmetrically for each tool invocation.
#[tokio::test]
async fn tool_call_hooks_fire_symmetrically() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let counters = Arc::new(HookCounters::default());
    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        b.install(&ToolCallPairPlugin {
            counters: counters.clone(),
        })
    })
    .await;

    for prompt in ECHO_PROMPTS {
        with_timeout(SINGLE_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt should succeed");
    }

    let before_tool = counters.before_tool_call.load(Ordering::SeqCst);
    let after_tool = counters.after_tool_call.load(Ordering::SeqCst);
    assert!(
        before_tool > 0,
        "before_tool_call should have fired at least once (got {before_tool})"
    );
    assert_eq!(
        before_tool, after_tool,
        "before_tool_call and after_tool_call must be symmetric ({before_tool} != {after_tool})"
    );
}

/// ShouldStopHook fires on the natural stop of each run.
#[tokio::test]
async fn should_stop_fires_on_natural_stop() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let counters = Arc::new(HookCounters::default());
    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        b.install(&ShouldStopOnlyPlugin {
            counters: counters.clone(),
        })
    })
    .await;

    for prompt in CHAT_PROMPTS {
        with_timeout(SINGLE_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt should succeed");
    }
    let should_stop = counters.should_stop.load(Ordering::SeqCst);

    assert!(
        should_stop >= 3,
        "should_stop should fire >= 3 times (one per run; got {should_stop})"
    );
}

/// AfterProviderResponseHook fires once per LLM provider call.
#[tokio::test]
async fn after_provider_response_tracks_llm_calls() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let counters = Arc::new(HookCounters::default());
    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        b.install(&AfterProviderResponseOnlyPlugin {
            counters: counters.clone(),
        })
    })
    .await;

    for prompt in CHAT_PROMPTS {
        with_timeout(SINGLE_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt should succeed");
    }

    let after_prov = counters.after_provider_response.load(Ordering::SeqCst);
    assert!(
        after_prov >= 3,
        "after_provider_response should fire >= 3 times (one per LLM call; got {after_prov})"
    );
}

/// All 11 hook types are registered and exercised across 3 turns.
#[tokio::test]
async fn all_hooks_registered_and_exercised() {
    let provider = LiveProvider::from_env();
    let entry = match &provider.openai {
        Some(e) => e,
        None => {
            eprintln!("skipping: no openai provider");
            return;
        }
    };

    let counters = Arc::new(HookCounters::default());
    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        b.install(&AllHooksPlugin {
            counters: counters.clone(),
        })
    })
    .await;

    for prompt in ECHO_PROMPTS {
        with_timeout(SINGLE_TURN, harness.prompt(*prompt))
            .await
            .expect("prompt should succeed");
    }

    // Every hook except before_compact (which only fires under compaction)
    // must have been invoked at least once across 3 turns.
    let before_run = counters.before_run.load(Ordering::SeqCst);
    let before_turn = counters.before_turn.load(Ordering::SeqCst);
    let after_turn = counters.after_turn.load(Ordering::SeqCst);
    let before_tool = counters.before_tool_call.load(Ordering::SeqCst);
    let after_tool = counters.after_tool_call.load(Ordering::SeqCst);
    let should_stop = counters.should_stop.load(Ordering::SeqCst);
    let after_prov = counters.after_provider_response.load(Ordering::SeqCst);
    let transform = counters.transform_context.load(Ordering::SeqCst);
    let prepare = counters.prepare_next_turn.load(Ordering::SeqCst);
    let validator = counters.final_answer_validator.load(Ordering::SeqCst);
    let before_compact = counters.before_compact.load(Ordering::SeqCst);

    assert!(before_run > 0, "before_run must fire (got {before_run})");
    assert!(before_turn > 0, "before_turn must fire (got {before_turn})");
    assert!(after_turn > 0, "after_turn must fire (got {after_turn})");
    assert!(
        before_tool > 0,
        "before_tool_call must fire with echo tool (got {before_tool})"
    );
    assert!(
        after_tool > 0,
        "after_tool_call must fire with echo tool (got {after_tool})"
    );
    assert!(should_stop > 0, "should_stop must fire (got {should_stop})");
    assert!(
        after_prov > 0,
        "after_provider_response must fire (got {after_prov})"
    );
    assert!(
        transform > 0,
        "transform_context must fire (got {transform})"
    );
    assert!(prepare > 0, "prepare_next_turn must fire (got {prepare})");
    assert!(
        validator > 0,
        "final_answer_validator must fire (got {validator})"
    );
    // before_compact only fires when compaction triggers; with 3 short turns
    // it may legitimately be 0 — just verify the counter is readable.
    let _ = before_compact;

    // Symmetry invariants.
    assert_eq!(
        before_tool, after_tool,
        "tool-call hooks must be symmetric ({before_tool} != {after_tool})"
    );
    assert_eq!(
        before_turn, after_turn,
        "turn hooks must be symmetric ({before_turn} != {after_turn})"
    );
}
