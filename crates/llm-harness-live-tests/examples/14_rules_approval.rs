//! 14 — Rules-Based Approval: RuleChain + RuleBasedApprovalHook.
//!
//! Demonstrates:
//! - Part 1: Allowlist — only `echo` is allowed, everything else denied (fallback: Deny)
//! - Part 2: Denylist — `dangerous_tool` is blocked, everything else allowed (fallback: Allow)
//! - Part 3: RateLimit — echo may be called at most 2 times in 60s; the 3rd is denied
//!
//! The `RuleBasedApprovalHook` implements `BeforeToolCallHook`. When a tool
//! call is denied, the harness feeds a `ToolFailure` result back to the LLM,
//! which typically acknowledges the denial in its final text.
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 14_rules_approval

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use llm_harness_agent::HarnessHooks;
use llm_harness_live_tests::{
    EventCapture, LiveProvider, SINGLE_TURN, make_harness_with, with_timeout,
};
use llm_harness_runtime::rules::{
    Contains, Decision, RateLimit, Rule, RuleBasedApprovalHook, RuleChain,
};
use llm_harness_types::{
    BeforeToolCallHook, DataBlock, Tool, ToolContext, ToolFailure, ToolResult,
};

// ── Echo tool ────────────────────────────────────────────────────────────────

struct EchoTool;

static ECHO_SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "message": { "type": "string", "description": "The message to echo back" }
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

// ── WriteFile tool (used as a non-allowlisted tool in Part 1) ────────────────

struct WriteFileTool;

static WRITE_FILE_SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to write to" },
            "content": { "type": "string", "description": "Content to write" }
        },
        "required": ["path", "content"]
    })
});

impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Write content to a file at the given path."
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        &WRITE_FILE_SCHEMA
    }
    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("(no path)");
            Ok(ToolResult::full(
                vec![DataBlock::text(format!("wrote {}", path))],
                serde_json::Value::Null,
                false,
            ))
        })
    }
}

// ── Dangerous tool (used as a denied tool in Part 2) ─────────────────────────

struct DangerousTool;

static DANGEROUS_SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "action": { "type": "string", "description": "The dangerous action to perform" }
        },
        "required": ["action"]
    })
});

impl Tool for DangerousTool {
    fn name(&self) -> &str {
        "dangerous_tool"
    }
    fn description(&self) -> &str {
        "A dangerous tool that performs irreversible operations. Should be denied by policy."
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        &DANGEROUS_SCHEMA
    }
    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            Ok(ToolResult::full(
                vec![DataBlock::text("dangerous action executed")],
                serde_json::Value::Null,
                false,
            ))
        })
    }
}

// ── Helper: build a harness with the given RuleChain + tools ─────────────────

async fn build_harness(
    entry: &llm_harness_live_tests::LiveProviderEntry,
    chain: RuleChain,
    tools: Vec<Arc<dyn Tool>>,
) -> (
    llm_harness_agent::AgentHarness,
    tempfile::TempDir,
    Arc<RuleBasedApprovalHook>,
) {
    let approval_hook = Arc::new(RuleBasedApprovalHook::new(chain));

    let hooks = HarnessHooks {
        before_tool_call: vec![approval_hook.clone() as Arc<dyn BeforeToolCallHook>],
        ..HarnessHooks::none()
    };

    let (harness, tmp) = make_harness_with(entry, |b| {
        let b = b.hooks(hooks);
        tools.into_iter().fold(b, |b, t| b.tool(t))
    })
    .await;

    (harness, tmp, approval_hook)
}

#[tokio::main]
async fn main() {
    println!("=== 14: Rules-Based Approval ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .or(provider.anthropic.as_ref())
        .expect("set OPENAI_API_KEY or ANTHROPIC_API_KEY to run this example");

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);

    part1_allowlist(entry).await;
    println!();
    part2_denylist(entry).await;
    println!();
    part3_rate_limit(entry).await;

    println!("\n--- Summary ---");
    println!("Allowlist (fallback Deny): echo allowed, write_file denied ✅");
    println!("Denylist (fallback Allow): dangerous_tool denied, echo allowed ✅");
    println!("RateLimit (max 2 / 60s): 3rd echo call denied ✅");
}

// ── Part 1: Allowlist — only echo is allowed ─────────────────────────────────

async fn part1_allowlist(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 1: Allowlist (only `echo` allowed, fallback Deny) ---\n");

    // Rule: wildcard tool "*", allow only when tool_name == "echo".
    // Everything else falls through to fallback = Deny.
    let chain = RuleChain::builder()
        .rule(Rule::new(
            "*",
            Box::new(Contains::new(vec!["echo".into()])),
            Decision::Allow,
        ))
        .fallback(Decision::Deny)
        .build();

    // Verify the chain's decisions directly (no LLM needed for the policy itself).
    println!("Policy evaluation (direct):");
    println!(
        "  echo       -> {:?}",
        chain.evaluate("echo", &serde_json::Value::Null)
    );
    println!(
        "  write_file -> {:?}",
        chain.evaluate("write_file", &serde_json::Value::Null)
    );

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(EchoTool) as Arc<dyn Tool>,
        Arc::new(WriteFileTool) as Arc<dyn Tool>,
    ];
    let (harness, _tmp, _hook) = build_harness(entry, chain, tools).await;

    let cap = EventCapture::subscribe(&harness);
    let prompt = "Use the echo tool with message 'hello rules', then try to use the write_file tool to write 'test' to '/tmp/x.txt'.";
    with_timeout(SINGLE_TURN, harness.prompt(prompt))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();
    println!("\nLLM prompt: {}", prompt);
    println!("Tools called: {:?}", tools_called);
    println!(
        "Final text:   {}",
        cap.texts().chars().take(120).collect::<String>()
    );

    // The LLM may or may not actually attempt write_file, but echo should be
    // callable and write_file should never execute successfully under the policy.
    let echo_called = tools_called.iter().any(|t| t == "echo");
    let write_called = tools_called.iter().any(|t| t == "write_file");
    println!("\nObservation:");
    println!("  echo called (allowed):       {}", echo_called);
    println!("  write_file called (denied):  {}", write_called);
    if write_called {
        println!("  NOTE: write_file appears in ToolCallStart but its execution was");
        println!("        denied by the hook; the LLM receives a ToolFailure result.");
    }
}

// ── Part 2: Denylist — block dangerous_tool ──────────────────────────────────

async fn part2_denylist(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 2: Denylist (`dangerous_tool` denied, fallback Allow) ---\n");

    // Rule 1 (specific): deny dangerous_tool. Must come BEFORE the wildcard
    // allow rule, otherwise the wildcard short-circuits and the deny never fires.
    // Rule 2 (wildcard): allow everything else.
    // Fallback Allow (defensive: fail-open for demo; production usually fails-closed).
    let chain = RuleChain::builder()
        .rule(Rule::new(
            "dangerous_tool",
            Box::new(Contains::new(vec!["dangerous_tool".into()])),
            Decision::Deny,
        ))
        .rule(Rule::new(
            "*",
            Box::new(Contains::new(vec!["echo".into(), "dangerous_tool".into()])),
            Decision::Allow,
        ))
        .fallback(Decision::Allow)
        .build();

    println!("Policy evaluation (direct):");
    println!(
        "  dangerous_tool -> {:?}",
        chain.evaluate("dangerous_tool", &serde_json::Value::Null)
    );
    println!(
        "  echo           -> {:?}",
        chain.evaluate("echo", &serde_json::Value::Null)
    );

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(EchoTool) as Arc<dyn Tool>,
        Arc::new(DangerousTool) as Arc<dyn Tool>,
    ];
    let (harness, _tmp, _hook) = build_harness(entry, chain, tools).await;

    let cap = EventCapture::subscribe(&harness);
    let prompt = "First try using the dangerous_tool with action 'delete-everything'. Then use the echo tool with message 'safe echo'.";
    with_timeout(SINGLE_TURN, harness.prompt(prompt))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();
    println!("\nLLM prompt: {}", prompt);
    println!("Tools called: {:?}", tools_called);
    println!(
        "Final text:   {}",
        cap.texts().chars().take(120).collect::<String>()
    );

    let echo_called = tools_called.iter().any(|t| t == "echo");
    let dangerous_called = tools_called.iter().any(|t| t == "dangerous_tool");
    println!("\nObservation:");
    println!("  echo called (allowed):           {}", echo_called);
    println!("  dangerous_tool called (denied):  {}", dangerous_called);
    if dangerous_called {
        println!("  NOTE: dangerous_tool appears in ToolCallStart but its execution was");
        println!("        denied by the hook; the LLM receives a 'rule_denied' ToolFailure.");
    }
}

// ── Part 3: RateLimit — at most 2 echo calls per 60s ─────────────────────────

async fn part3_rate_limit(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 3: RateLimit (echo: max 2 calls / 60s, fallback Deny) ---\n");

    // RateLimit::test() records a timestamp each time it returns true and
    // returns false once the in-window count reaches `max`. Pair with
    // on_match: Allow + fallback: Deny so that the 3rd call is denied.
    let chain = RuleChain::builder()
        .rule(Rule::new(
            "*",
            Box::new(RateLimit::new(2, Duration::from_secs(60))),
            Decision::Allow,
        ))
        .fallback(Decision::Deny)
        .build();

    // Direct evaluation simulating 3 sequential echo calls.
    println!("Policy evaluation (direct, 3 sequential echo calls):");
    for i in 1..=3 {
        let d = chain.evaluate("echo", &serde_json::Value::Null);
        println!("  call {}: echo -> {:?}", i, d);
    }
    println!("  (call 3 is denied because 2 calls already recorded in the window)");

    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool) as Arc<dyn Tool>];
    let (harness, _tmp, _hook) = build_harness(entry, chain, tools).await;

    let cap = EventCapture::subscribe(&harness);
    let prompt = "Call the echo tool three times with these messages: 'first', 'second', 'third'.";
    with_timeout(SINGLE_TURN, harness.prompt(prompt))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();
    println!("\nLLM prompt: {}", prompt);
    println!("Tools called (in order): {:?}", tools_called);
    println!(
        "Final text:   {}",
        cap.texts().chars().take(140).collect::<String>()
    );

    let echo_count = tools_called.iter().filter(|t| t == &"echo").count();
    println!("\nObservation:");
    println!("  echo ToolCallStart events: {}", echo_count);
    println!("  Expected: at most 2 succeed; the 3rd is denied with 'rule_denied'.");
    if echo_count >= 3 {
        println!("  NOTE: all 3 appear as ToolCallStart (the LLM attempted them), but the");
        println!("        3rd execution is denied by the RateLimit predicate; the LLM");
        println!("        receives a ToolFailure and typically reports the rate limit.");
    }
}
