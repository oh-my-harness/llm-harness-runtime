//! 17 — Source Instruction: SourceTagPlugin + ProjectInstructionPlugin.
//!
//! Demonstrates:
//! - Part 1: SourceTagPlugin — wraps tool results in `<external_content source="...">` XML tags
//! - Part 2: ProjectInstructionPlugin — injects CLAUDE.md content into the system prompt
//! - Part 3: Combined — both plugins active in the same harness
//!
//! ## SourceTagPlugin
//!
//! An `AfterToolCallHook` that wraps text blocks of tool results in
//! `<external_content source="label">...</external_content>` tags. This lets
//! downstream prompt-injection defenses identify content originating from
//! external tools (files, web pages, etc.) versus trusted system/user text.
//!
//! ## ProjectInstructionPlugin
//!
//! A `BeforeRunHook` that scans for `CLAUDE.md` / `AGENTS.md` / `.cursorrules`
//! / `SOUL.md` in `env.working_dir()` and parent directories, then appends the
//! merged content (XML-wrapped) to the system prompt before each LLM call.
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 17_source_instruction

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_agent::AgentHarness;
use llm_harness_agent::HarnessBuilder;
use llm_harness_live_tests::{EventCapture, LiveProvider, SINGLE_TURN, with_timeout};
use llm_harness_platform::sandbox::{ResourceLimits, Sandbox, SandboxConfig};
use llm_harness_sandbox::OsEnvSandbox;
use llm_harness_strategy::{ProjectInstructionPlugin, SourceTagEntry, SourceTagPlugin};
use llm_harness_types::{DataBlock, ExecutionEnv, Tool, ToolContext, ToolFailure, ToolResult};

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

/// Register EchoTool as both a tool and a plugin (so `install` works).
impl llm_harness_agent::Plugin for EchoTool {
    fn name(&self) -> &str {
        "echo-plugin"
    }
    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        tools.push(Arc::new(EchoTool) as Arc<dyn Tool>);
    }
}

// ── CLAUDE.md content for Part 2 and Part 3 ─────────────────────────────────

const CLAUDE_MD_CONTENT: &str = "\
# Project Instructions

- Always respond in a friendly tone.
- When you use the echo tool, mention that the content came from an external source.
- End every response with the phrase: 'Project instructions applied.'
";

// ── Helper: create sandbox + env (mirrors make_harness_with_env internals) ───
//
// ProjectInstructionPlugin needs `Arc<dyn ExecutionEnv>` at construction time,
// but `make_harness_with_env` creates env internally — the customize closure
// can't capture it. So we build the sandbox manually, get env, then construct
// the plugin with env in hand.

async fn make_sandbox_and_env() -> (tempfile::TempDir, Arc<dyn ExecutionEnv>) {
    let tmp = tempfile::tempdir().unwrap();
    let config = SandboxConfig {
        fs_allowlist: vec![tmp.path().to_path_buf()],
        fs_denylist: vec![],
        net_allowlist: vec![],
        resource_limits: ResourceLimits {
            max_cpus: None,
            max_processes: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: Some(tmp.path().to_path_buf()),
        trust_profile: Default::default(),
        host_binds: Vec::new(),
    };
    let sandbox = OsEnvSandbox::new(config);
    sandbox.start().await.unwrap();
    let env: Arc<dyn ExecutionEnv> = sandbox.env();
    (tmp, env)
}

/// Build a harness with a customization callback that has access to `env`.
async fn build_with_env(
    entry: &llm_harness_live_tests::LiveProviderEntry,
    customize: impl FnOnce(HarnessBuilder, &Arc<dyn ExecutionEnv>) -> HarnessBuilder,
) -> (Arc<AgentHarness>, Arc<dyn ExecutionEnv>, tempfile::TempDir) {
    let (tmp, env) = make_sandbox_and_env().await;
    let builder = HarnessBuilder::new(&entry.model)
        .provider("*", entry.client.clone())
        .max_tokens(1024);
    let builder = customize(builder, &env);
    let harness = Arc::new(builder.build(env.clone()).await.unwrap());
    (harness, env, tmp)
}

#[tokio::main]
async fn main() {
    println!("=== 17: Source Instruction (SourceTag + ProjectInstruction) ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .or(provider.anthropic.as_ref())
        .expect("set OPENAI_API_KEY or ANTHROPIC_API_KEY to run this example");

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);

    part1_source_tag(entry).await;
    println!();
    part2_project_instruction(entry).await;
    println!();
    part3_combined(entry).await;

    println!("\n--- Summary ---");
    println!("SourceTagPlugin: echo results wrapped in <external_content source=\"demo\">");
    println!("ProjectInstructionPlugin: CLAUDE.md injected into system prompt");
    println!("Combined: both protections active simultaneously");
}

// ── Part 1: SourceTagPlugin — tool results wrapped in XML tags ───────────────

async fn part1_source_tag(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 1: SourceTagPlugin (echo -> <external_content source=\"demo\">) ---\n");

    // SourceTagPlugin wraps text blocks of the echo tool's result in
    // <external_content source="demo">...</external_content> tags.
    // The wrapping happens in AfterToolCallHook, so the LLM sees the tagged
    // result in its context and can report on it.
    let source_tag = SourceTagPlugin::new(vec![SourceTagEntry {
        tool: "echo".into(),
        label: "demo".into(),
    }]);

    let (harness, _env, _tmp) =
        build_with_env(entry, |b, _| b.install(&EchoTool).install(&source_tag)).await;

    let cap = EventCapture::subscribe(&harness);

    let prompt = "Use the echo tool with message 'hello from external source'. \
                  Then tell me: did the tool result appear inside any XML tags? \
                  If so, quote the exact tag name and source attribute you see.";
    println!("Prompt: {}\n", prompt);
    with_timeout(SINGLE_TURN, harness.prompt(prompt))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();
    println!("Tools called: {:?}", tools_called);
    println!("\n--- LLM Response ---");
    println!("{}", cap.texts());

    println!("\nObservation:");
    println!("  The echo tool result was wrapped in <external_content source=\"demo\">");
    println!("  by the SourceTagPlugin's AfterToolCallHook before being fed back to");
    println!("  the LLM. The LLM should be able to see and report the XML tags.");
}

// ── Part 2: ProjectInstructionPlugin — CLAUDE.md injection ───────────────────

async fn part2_project_instruction(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 2: ProjectInstructionPlugin (CLAUDE.md -> system prompt) ---\n");

    // Build the harness with ProjectInstructionPlugin installed.
    // The plugin needs env at construction time; build_with_env passes it
    // to the customize closure.
    let (harness, env, _tmp) = build_with_env(entry, |b, env| {
        b.install(&ProjectInstructionPlugin::new(env.clone()))
    })
    .await;

    // Write CLAUDE.md to the sandbox working_dir.
    // The ProjectInstructionHook runs in before_run (BeforeRunHook), so it
    // scans for CLAUDE.md on each prompt() call — writing it after build but
    // before prompt() is sufficient.
    let claude_md_path = env.working_dir().join("CLAUDE.md");
    std::fs::write(&claude_md_path, CLAUDE_MD_CONTENT).unwrap();
    println!("Wrote CLAUDE.md to: {}", claude_md_path.display());
    println!("CLAUDE.md content:\n{}\n", CLAUDE_MD_CONTENT);

    let cap = EventCapture::subscribe(&harness);

    // The CLAUDE.md instructions say to end with "Project instructions applied."
    // and to respond in a friendly tone. If injection works, the LLM follows
    // these instructions.
    let prompt = "Say hello and tell me what instructions you received.";
    println!("Prompt: {}\n", prompt);
    with_timeout(SINGLE_TURN, harness.prompt(prompt))
        .await
        .expect("prompt failed");
    cap.settle().await;

    println!("\n--- LLM Response ---\n{}", cap.texts());

    let text = cap.texts();
    let follows_instructions = text.to_lowercase().contains("project instructions applied");

    println!("\nObservation:");
    println!("  ProjectInstructionPlugin scanned working_dir for CLAUDE.md and");
    println!("  injected its content into the system prompt via BeforeRunHook.");
    println!("  The LLM should follow the injected instructions (friendly tone,");
    println!("  ending with 'Project instructions applied.').");
    println!(
        "  Response follows CLAUDE.md instructions: {}",
        follows_instructions
    );
}

// ── Part 3: Combined — both plugins active ───────────────────────────────────

async fn part3_combined(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 3: Combined (SourceTag + ProjectInstruction) ---\n");

    let source_tag = SourceTagPlugin::new(vec![SourceTagEntry {
        tool: "echo".into(),
        label: "demo".into(),
    }]);

    // Build harness with both plugins. env is available in the closure for
    // ProjectInstructionPlugin construction.
    let (harness, env, _tmp) = build_with_env(entry, |b, env| {
        b.install(&EchoTool)
            .install(&source_tag)
            .install(&ProjectInstructionPlugin::new(env.clone()))
    })
    .await;

    // Write CLAUDE.md to the sandbox working_dir.
    let claude_md_path = env.working_dir().join("CLAUDE.md");
    std::fs::write(&claude_md_path, CLAUDE_MD_CONTENT).unwrap();
    println!("Wrote CLAUDE.md to: {}", claude_md_path.display());

    let cap = EventCapture::subscribe(&harness);

    // This prompt tests both:
    // 1. SourceTag: echo result should be wrapped in <external_content source="demo">
    // 2. ProjectInstruction: response should end with "Project instructions applied."
    let prompt = "Use the echo tool with message 'combined test'. \
                  Then tell me what XML tags you see around the echo result, \
                  and confirm you received any project instructions.";
    println!("Prompt: {}\n", prompt);
    with_timeout(SINGLE_TURN, harness.prompt(prompt))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();
    println!("Tools called: {:?}", tools_called);
    println!("\n--- LLM Response ---\n{}", cap.texts());

    let text = cap.texts();
    let has_instruction_marker = text.to_lowercase().contains("project instructions applied");

    println!("\nObservation:");
    println!("  Both plugins are active simultaneously:");
    println!("    - SourceTagPlugin wrapped echo result in <external_content source=\"demo\">");
    println!("    - ProjectInstructionPlugin injected CLAUDE.md into the system prompt");
    println!(
        "  Response follows CLAUDE.md instructions: {}",
        has_instruction_marker
    );
}
