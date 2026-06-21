# examples/coding-agent Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a working coding agent (4 tools + one-shot/pipe/REPL + event rendering) on top of the llm-harness framework.

**Architecture:** `CodingAgentPlugin` implements the `Plugin` trait to register 4 tools and inject system prompt via `BeforeRunHook`. `main.rs` uses `HarnessBuilder` to assemble everything, then subscribes to `AgentHarnessEvent` for output rendering.

**Tech Stack:** Rust 2024 edition, tokio, serde, thiserror, rustyline 14, atty 0.2, dirs-next 2

## Global Constraints

- `examples/coding-agent/` is a workspace member; depends on framework crates via `path`
- Tools implement `Tool` trait from `llm-harness-types`; no internal state, all context via `ToolContext`
- System prompt injected via `BeforeRunHook` (spec §3 方案 A)
- CLI manual arg parsing, no clap (spec §5)
- Phase 1 only: 4 tools, no session persistence, no settings, no Phase manager, no audit, no budget
- Rust edition = `2024` (workspace default)
- Commit after every task; `cargo fmt` + `cargo clippy --all-targets` before commit
- **Do NOT modify framework crates** — coding-agent is a consumer. If a framework gap blocks a task, report it and skip that task.

## File Map

```
examples/coding-agent/
├── Cargo.toml              ← Task 1
├── spec.md                 ← existing
├── CLAUDE.md               ← existing
├── plan.md                 ← this file
├── src/
│   ├── lib.rs              ← Task 2
│   ├── prompt.rs           ← Task 3 (system prompt + SystemPromptHook)
│   ├── tools/
│   │   ├── mod.rs          ← Task 4 (module declarations)
│   │   ├── read.rs         ← Task 5
│   │   ├── bash.rs         ← Task 6
│   │   ├── edit.rs         ← Task 7
│   │   └── write.rs        ← Task 8
│   ├── plugin.rs           ← Task 9 (CodingAgentPlugin)
│   ├── output.rs           ← Task 10 (event→terminal renderer)
│   └── main.rs             ← Task 11 (CLI entry point)
└── tests/
    └── integration.rs      ← Task 12 (MockLlmClient integration)
```

## Task Dependency Graph

```
Task 1 (scaffold)
  └→ Task 2 (lib.rs — declares modules)
       └→ Task 3 (prompt.rs — independent)
            └→ Task 4 (tools/mod.rs — module declarations)
                 ├→ Task 5 (ReadTool)
                 ├→ Task 6 (BashTool)
                 ├→ Task 7 (EditTool)
                 └→ Task 8 (WriteTool)
                      └→ Task 9 (plugin.rs — depends on all tools + SystemPromptHook)
                           └→ Task 10 (output.rs — depends on AgentHarness types)
                                └→ Task 11 (main.rs — wires everything)
                                     └→ Task 12 (integration tests)
```

## Key Framework APIs (reference for all tasks)

These are the types and signatures available to the coding-agent. Every task lists its own "Consumes" section with exact signatures.

### Tool trait (`llm-harness-types::Tool`)
```rust
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> &serde_json::Value;
    fn execute<'a>(&'a self, args: serde_json::Value, ctx: &'a ToolContext)
        -> BoxFuture<'a, Result<ToolResult, ToolError>>;
}
```

### ToolContext (`llm-harness-types::ToolContext`)
```rust
pub struct ToolContext {
    pub env: Arc<dyn ExecutionEnv>,  // file system + shell
    pub abort: CancellationToken,    // user abort signal
    pub tool_use_id: String,         // unique tool call ID
    pub turn_index: u32,             // current turn (0-based)
    pub assistant_message: Arc<AssistantMessage>,
    pub update_tx: mpsc::Sender<ToolResult>,
}
```

### ToolResult / ToolError
```rust
pub struct ToolResult {
    pub content: Vec<ContentBlock>,
    pub details: serde_json::Value,
    pub terminate: bool,
}
pub enum ToolError {
    InvalidArguments(String), Aborted, Execution(String), Other(anyhow::Error),
}
```

### ContentBlock
```rust
pub enum ContentBlock {
    Text { text: String },
    Thinking { thinking: String, signature: Option<String> },
    Image { source: ImageSource },
    ToolUse { id: String, name: String, input: serde_json::Value },
}
```

### ExecutionEnv (key methods for Phase 1 tools)
```rust
pub trait ExecutionEnv: Send + Sync {
    fn working_dir(&self) -> &Path;
    fn read_text_file<'a>(&'a self, path: &'a Path, abort: CancellationToken)
        -> BoxFuture<'a, Result<String, EnvError>>;
    fn write_file<'a>(&'a self, path: &'a Path, content: &'a [u8], abort: CancellationToken)
        -> BoxFuture<'a, Result<(), EnvError>>;
    fn create_dir<'a>(&'a self, path: &'a Path, recursive: bool, abort: CancellationToken)
        -> BoxFuture<'a, Result<(), EnvError>>;
    fn execute_shell<'a>(&'a self, cmd: &'a str, opts: ShellOptions<'a>)
        -> BoxFuture<'a, Result<ShellOutput, EnvError>>;
}
// ShellOptions { cwd, env, timeout, abort, on_stdout, on_stderr }
// ShellOutput { stdout: String, stderr: String, exit_code: i32 }
```

### Plugin trait (`llm-harness-agent::Plugin`)
```rust
pub trait Plugin: Send + Sync {
    fn name(&self) -> &str;
    fn register_tools(&self, _tools: &mut Vec<Arc<dyn Tool>>) {}
    fn register_hooks(&self, _hooks: &mut HarnessHooks) {}
    // ... other methods have default no-op implementations
}
```

### HarnessHooks (key field for Phase 1)
```rust
pub struct HarnessHooks {
    pub before_run: Vec<Arc<dyn BeforeRunHook>>,
    // ... 10 other fields
}
impl HarnessHooks { pub fn none() -> Self { /* all vec![] */ } }
```

### BeforeRunHook
```rust
pub struct BeforeRunCtx<'a> {
    pub prompt_text: &'a str,
    pub initial_messages: &'a mut Vec<AgentMessage>,
    pub system_prompt: &'a mut Option<String>,
    pub resources: &'a AgentHarnessResources,
}
pub struct BeforeRunResult {
    pub additional_messages: Vec<AgentMessage>,
    pub system_prompt: Option<String>,
}
pub trait BeforeRunHook: Send + Sync {
    fn before_run<'a>(&'a self, ctx: BeforeRunCtx<'a>)
        -> BoxFuture<'a, Result<BeforeRunResult, AgentError>>;
}
```

### HarnessBuilder
```rust
impl HarnessBuilder {
    pub fn new(model: impl Into<String>) -> Self;
    pub fn install(mut self, plugin: &dyn Plugin) -> Self;
    pub fn provider(mut self, pattern: impl Into<String>, client: Arc<dyn LlmClient>) -> Self;
    pub async fn build(self, env: Arc<dyn ExecutionEnv>) -> Result<AgentHarness, HarnessBuildError>;
}
// LlmClient = llm_adapter::Provider (re-exported by llm-harness-loop)

### AgentHarness (key methods)
```rust
impl AgentHarness {
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<AgentHarnessEvent>>;
    pub async fn prompt(&self, text: impl Into<String>) -> Result<(), HarnessError>;
}
```

### AgentHarnessEvent / AgentEvent
```rust
pub enum AgentHarnessEvent {
    Agent(AgentEvent),                           // wraps low-level events
    PhaseChange { from: HarnessPhase, to: HarnessPhase },
    // ... other variants, but Phase 1 only needs the two above + Settled
    Settled,
    Aborted,
}
pub enum AgentEvent {
    TextDelta { message_id: String, text: String },
    ThinkingDelta { message_id: String, thinking: String, signature: Option<String> },
    // ... other variants
    Error(AgentError),
}
pub enum HarnessPhase { Idle, Turning, Compacting, Branching }
```

### LLM Providers (llm_adapter)
```rust
// Anthropic
AnthropicProvider::builder(api_key).build()  // → impl Provider (aka LlmClient)
// OpenAI-compatible
OpenAIProvider::builder(api_key).base_url(url).chat_path(path).build()  // → impl Provider
```

### Test utilities (behind `test-utils` feature)
```rust
// From llm-harness-loop::test_utils
MockLlmClient::new(responses: Vec<MockResponse>)  // → impl Provider
MockResponse::text("hello")                        // simple text response
MockResponse::tool_use("id", "tool", r#"{"k":"v"}"#)  // tool-use response
NoOpEnv  // → impl ExecutionEnv (all ops return errors)
```

---

### Task 1: Scaffold Cargo.toml + register workspace member

**Files:**
- Create: `examples/coding-agent/Cargo.toml`
- Modify: `Cargo.toml` (root, add workspace member)

**Interfaces:**
- Consumes: nothing
- Produces: `coding-agent` package compilable; deps available for all later tasks

- [ ] **Step 1: Write Cargo.toml**

Create `examples/coding-agent/Cargo.toml`:

```toml
[package]
name = "coding-agent"
version.workspace = true
edition.workspace = true
license.workspace = true

[[bin]]
name = "coding-agent"
path = "src/main.rs"

[dependencies]
llm-harness-agent    = { path = "../../crates/llm-harness-agent" }
llm-harness-runtime  = { path = "../../crates/llm-harness-runtime" }
llm-harness-types    = { path = "../../crates/llm-harness-types" }
llm_adapter          = { workspace = true }

tokio       = { workspace = true, features = ["macros", "rt-multi-thread"] }
serde       = { workspace = true }
serde_json  = { workspace = true }
futures     = { workspace = true }
thiserror   = { workspace = true }
chrono      = { workspace = true }
rustyline   = "14"
atty        = "0.2"
dirs-next   = "2"

[dev-dependencies]
llm-harness-loop = { workspace = true, features = ["test-utils"] }
llm-harness-runtime-sandbox-os = { path = "../../crates/llm-harness-runtime-sandbox-os" }
tokio     = { workspace = true, features = ["macros", "rt-multi-thread", "test-util"] }
tempfile  = "3"
```

- [ ] **Step 2: Add workspace member**

Edit root `Cargo.toml`, add `"examples/coding-agent"` to the `members` list:

```toml
[workspace]
members = [
    # core
    "crates/llm-harness-types",
    "crates/llm-harness-loop",
    "crates/llm-harness-agent",
    # runtime
    "crates/llm-harness-runtime",
    "crates/llm-harness-runtime-sandbox-os",
    "crates/llm-harness-runtime-sandbox-bwrap",
    "crates/llm-harness-runtime-sandbox-seatbelt",
    "crates/llm-harness-runtime-auth",
    "crates/llm-harness-runtime-mcp",
    "crates/llm-harness-runtime-audit-jsonl",
    "crates/llm-harness-runtime-trace-otel",
    # examples
    "examples/coding-agent",
]
```

- [ ] **Step 3: Verify it builds (empty lib.rs + main.rs)**

Create minimal placeholder files:

`examples/coding-agent/src/lib.rs`:
```rust
//! coding-agent: a CLI coding assistant built on llm-harness.
```

`examples/coding-agent/src/main.rs`:
```rust
fn main() {
    println!("Hello from coding-agent");
}
```

Run:
```bash
cargo check -p coding-agent
```
Expected: compiles without errors (warnings about unused imports are fine).

- [ ] **Step 4: Commit**

```bash
git add examples/coding-agent/Cargo.toml examples/coding-agent/src/lib.rs examples/coding-agent/src/main.rs Cargo.toml
git commit -m "feat(coding-agent): scaffold Cargo.toml and workspace membership"
```

---

### Task 2: lib.rs — module declarations

**Files:**
- Modify: `examples/coding-agent/src/lib.rs`

**Interfaces:**
- Consumes: Task 1 (compilable package)
- Produces: `pub mod prompt; pub mod tools; pub mod plugin; pub mod output;` — all downstream tasks import from `coding_agent::`

- [ ] **Step 1: Write lib.rs with all module declarations**

Replace `examples/coding-agent/src/lib.rs` with:

```rust
//! coding-agent: a CLI coding assistant built on the llm-harness framework.
//!
//! Phase 1 provides 4 tools (read, bash, edit, write), one-shot / pipe / REPL
//! modes, and event-based terminal output rendering.

pub mod output;
pub mod plugin;
pub mod prompt;
pub mod tools;
```

- [ ] **Step 2: Verify it still compiles**

```bash
cargo check -p coding-agent
```
Expected: compiles; will warn about unused modules (they don't exist yet), that's fine.

- [ ] **Step 3: Commit**

```bash
git add examples/coding-agent/src/lib.rs
git commit -m "feat(coding-agent): declare module structure in lib.rs"
```

---

### Task 3: prompt.rs — system prompt builder + SystemPromptHook

**Files:**
- Create: `examples/coding-agent/src/prompt.rs`

**Interfaces:**
- Consumes: Task 2 (`coding_agent::prompt` module declared)
- Produces:
  - `pub fn build_system_prompt(tools: &[Arc<dyn Tool>], cwd: &Path, context_files: &[ContextFile]) -> String`
  - `pub struct ContextFile { pub path: String, pub content: String }`
  - `pub fn find_context_files(cwd: &Path) -> Vec<ContextFile>` — walks up from `cwd` to find CLAUDE.md / AGENTS.md
  - `pub struct SystemPromptHook { prompt: String }` — implements `BeforeRunHook`, sets `ctx.system_prompt`

Reference: `/Users/hhl/Documents/projs/oh-my-harness/coding-agent/src/prompt.rs` (simplify — Phase 1 has no skills, no custom_prompt, no append, no extra_guidelines).

- [ ] **Step 1: Write tests first**

Append to `examples/coding-agent/src/prompt.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use llm_harness_types::Tool;
    use futures::future::BoxFuture;

    /// Minimal mock tool for prompt tests.
    struct MockTool { name: &'static str, desc: &'static str }

    impl Tool for MockTool {
        fn name(&self) -> &str { self.name }
        fn description(&self) -> &str { self.desc }
        fn parameters_schema(&self) -> &serde_json::Value {
            static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
            S.get_or_init(|| serde_json::json!({}))
        }
        fn execute<'a>(&'a self, _args: serde_json::Value, _ctx: &'a llm_harness_types::ToolContext)
            -> BoxFuture<'a, Result<llm_harness_types::ToolResult, llm_harness_types::ToolError>>
        { Box::pin(async { Err(llm_harness_types::ToolError::Aborted) }) }
    }

    #[test]
    fn prompt_contains_tool_names_and_descriptions() {
        let read = MockTool { name: "read", desc: "Read a file" };
        let bash = MockTool { name: "bash", desc: "Run a command" };
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(read), Arc::new(bash)];
        let prompt = build_system_prompt(&tools, std::path::Path::new("/home/user/project"), &[]);
        assert!(prompt.contains("read: Read a file"), "got: {prompt}");
        assert!(prompt.contains("bash: Run a command"), "got: {prompt}");
        assert!(prompt.contains("/home/user/project"), "got: {prompt}");
    }

    #[test]
    fn prompt_appends_context_files() {
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let cfs = vec![ContextFile { path: "CLAUDE.md".into(), content: "# Rules\nBe careful.".into() }];
        let prompt = build_system_prompt(&tools, std::path::Path::new("/tmp"), &cfs);
        assert!(prompt.contains("<project_instructions path=\"CLAUDE.md\">"), "got: {prompt}");
        assert!(prompt.contains("Be careful."), "got: {prompt}");
    }

    #[test]
    fn prompt_no_context_files_no_project_context_block() {
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let prompt = build_system_prompt(&tools, std::path::Path::new("/tmp"), &[]);
        assert!(!prompt.contains("<project_instructions"), "got: {prompt}");
        assert!(!prompt.contains("<project_context"), "got: {prompt}");
    }

    #[test]
    fn prompt_includes_date() {
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let prompt = build_system_prompt(&tools, std::path::Path::new("/tmp"), &[]);
        assert!(prompt.contains("Current date:"), "got: {prompt}");
    }

    #[test]
    fn prompt_empty_tools_shows_none() {
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let prompt = build_system_prompt(&tools, std::path::Path::new("/tmp"), &[]);
        assert!(prompt.contains("(none)"), "got: {prompt}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p coding-agent -- prompt
```
Expected: FAIL — `build_system_prompt` not defined.

- [ ] **Step 3: Write implementation**

Write the full `examples/coding-agent/src/prompt.rs`:

```rust
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_agent::HarnessHooks;
use llm_harness_types::{AgentError, AgentMessage, BeforeRunCtx, BeforeRunHook, BeforeRunResult, Tool};
use llm_harness_types::AgentHarnessResources;

/// A pre-loaded project context file (e.g. CLAUDE.md).
#[derive(Debug, Clone)]
pub struct ContextFile {
    /// Relative path shown in the prompt.
    pub path: String,
    /// File contents.
    pub content: String,
}

/// Walk up from `cwd` to find CLAUDE.md and AGENTS.md.
///
/// Stops at the filesystem root. Files are included in the order they
/// are found (closest to `cwd` first).
pub fn find_context_files(cwd: &Path) -> Vec<ContextFile> {
    let mut files = Vec::new();
    for dir in cwd.ancestors() {
        for name in &["CLAUDE.md", "AGENTS.md"] {
            let path = dir.join(name);
            if path.is_file() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    // Show the path relative to cwd, or absolute if above cwd.
                    let display = if let Ok(rel) = path.strip_prefix(cwd) {
                        rel.to_string_lossy().to_string()
                    } else {
                        path.to_string_lossy().to_string()
                    };
                    // Avoid duplicates (stop after first match per filename).
                    if !files.iter().any(|cf: &ContextFile| cf.path == display) {
                        files.push(ContextFile { path: display, content });
                    }
                }
            }
        }
    }
    files
}

/// Build the system prompt string.
///
/// Includes tool list, guidelines, context files (CLAUDE.md/AGENTS.md),
/// current date, and working directory.
pub fn build_system_prompt(
    tools: &[Arc<dyn Tool>],
    cwd: &Path,
    context_files: &[ContextFile],
) -> String {
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let cwd_str = cwd.to_string_lossy().replace('\\', "/");

    let tool_list = if tools.is_empty() {
        "(none)".to_string()
    } else {
        tools
            .iter()
            .map(|t| format!("- {}: {}", t.name(), t.description()))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let has_bash = tools.iter().any(|t| t.name() == "bash");
    let guidelines = if has_bash {
        "- Use bash for file operations like ls, grep, find\n\
         - Be concise in your responses\n\
         - Show file paths clearly when working with files"
    } else {
        "- Be concise in your responses\n\
         - Show file paths clearly when working with files"
    };

    let mut prompt = format!(
        "You are an expert coding assistant. \
         You help users by reading files, executing commands, editing code, and writing new files.\n\n\
         Available tools:\n{tool_list}\n\n\
         Guidelines:\n{guidelines}"
    );

    if !context_files.is_empty() {
        prompt.push_str("\n\n<project_context>\n\n");
        prompt.push_str("Project-specific instructions and guidelines:\n\n");
        for cf in context_files {
            prompt.push_str(&format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                cf.path, cf.content
            ));
        }
        prompt.push_str("</project_context>");
    }

    prompt.push_str(&format!("\nCurrent date: {date}"));
    prompt.push_str(&format!("\nCurrent working directory: {cwd_str}"));

    prompt
}

// ── SystemPromptHook ─────────────────────────────────────────────────────────

/// A `BeforeRunHook` that injects the system prompt into every run.
///
/// This is "方案 A" from the design spec: since `HarnessBuilder` does not yet
/// have a `.system_prompt()` method, the plugin registers this hook to set
/// the system prompt before each `prompt()` call.
pub struct SystemPromptHook {
    prompt: String,
}

impl SystemPromptHook {
    pub fn new(prompt: String) -> Self {
        Self { prompt }
    }
}

impl BeforeRunHook for SystemPromptHook {
    fn before_run<'a>(
        &'a self,
        mut ctx: BeforeRunCtx<'a>,
    ) -> BoxFuture<'a, Result<BeforeRunResult, AgentError>> {
        let prompt = self.prompt.clone();
        Box::pin(async move {
            // Set the system prompt on the mutable reference.
            *ctx.system_prompt = Some(prompt);
            Ok(BeforeRunResult {
                additional_messages: vec![],
                system_prompt: None, // we already modified ctx.system_prompt directly
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::Tool;
    use futures::future::BoxFuture;

    /// Minimal mock tool for prompt tests.
    struct MockTool { name: &'static str, desc: &'static str }

    impl Tool for MockTool {
        fn name(&self) -> &str { self.name }
        fn description(&self) -> &str { self.desc }
        fn parameters_schema(&self) -> &serde_json::Value {
            static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
            S.get_or_init(|| serde_json::json!({}))
        }
        fn execute<'a>(&'a self, _args: serde_json::Value, _ctx: &'a llm_harness_types::ToolContext)
            -> BoxFuture<'a, Result<llm_harness_types::ToolResult, llm_harness_types::ToolError>>
        { Box::pin(async { Err(llm_harness_types::ToolError::Aborted) }) }
    }

    #[test]
    fn prompt_contains_tool_names_and_descriptions() {
        let read = MockTool { name: "read", desc: "Read a file" };
        let bash = MockTool { name: "bash", desc: "Run a command" };
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(read), Arc::new(bash)];
        let prompt = build_system_prompt(&tools, std::path::Path::new("/home/user/project"), &[]);
        assert!(prompt.contains("read: Read a file"), "got: {prompt}");
        assert!(prompt.contains("bash: Run a command"), "got: {prompt}");
        assert!(prompt.contains("/home/user/project"), "got: {prompt}");
    }

    #[test]
    fn prompt_appends_context_files() {
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let cfs = vec![ContextFile { path: "CLAUDE.md".into(), content: "# Rules\nBe careful.".into() }];
        let prompt = build_system_prompt(&tools, std::path::Path::new("/tmp"), &cfs);
        assert!(prompt.contains("<project_instructions path=\"CLAUDE.md\">"), "got: {prompt}");
        assert!(prompt.contains("Be careful."), "got: {prompt}");
    }

    #[test]
    fn prompt_no_context_files_no_project_context_block() {
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let prompt = build_system_prompt(&tools, std::path::Path::new("/tmp"), &[]);
        assert!(!prompt.contains("<project_instructions"), "got: {prompt}");
        assert!(!prompt.contains("<project_context"), "got: {prompt}");
    }

    #[test]
    fn prompt_includes_date() {
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let prompt = build_system_prompt(&tools, std::path::Path::new("/tmp"), &[]);
        assert!(prompt.contains("Current date:"), "got: {prompt}");
    }

    #[test]
    fn prompt_empty_tools_shows_none() {
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let prompt = build_system_prompt(&tools, std::path::Path::new("/tmp"), &[]);
        assert!(prompt.contains("(none)"), "got: {prompt}");
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p coding-agent -- prompt
```
Expected: PASS (7 tests).

- [ ] **Step 5: Run clippy**

```bash
cargo clippy -p coding-agent --all-targets
```
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add examples/coding-agent/src/prompt.rs
git commit -m "feat(coding-agent): add system prompt builder and SystemPromptHook"
```

---

### Task 4: tools/mod.rs — module declarations + tool constants

**Files:**
- Create: `examples/coding-agent/src/tools/mod.rs`

**Interfaces:**
- Consumes: Task 2 (`coding_agent::tools` module declared)
- Produces:
  - `pub mod read; pub mod bash; pub mod edit; pub mod write;`
  - `pub const PHASE_1_TOOLS: &[&str] = &["read", "bash", "edit", "write"];`

- [ ] **Step 1: Write tests first**

Create `examples/coding-agent/src/tools/mod.rs`:

```rust
pub mod bash;
pub mod edit;
pub mod read;
pub mod write;

/// Phase 1 tool names in declaration order.
pub const PHASE_1_TOOLS: &[&str] = &["read", "bash", "edit", "write"];

#[cfg(test)]
mod tests {
    #[test]
    fn phase_1_has_four_tools() {
        assert_eq!(super::PHASE_1_TOOLS.len(), 4);
    }

    #[test]
    fn phase_1_tools_are_unique() {
        let mut sorted: Vec<&str> = super::PHASE_1_TOOLS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), super::PHASE_1_TOOLS.len());
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test -p coding-agent -- tools::tests
```
Expected: PASS (2 tests).

- [ ] **Step 3: Verify clippy + commit**

```bash
cargo clippy -p coding-agent --all-targets
git add examples/coding-agent/src/tools/mod.rs
git commit -m "feat(coding-agent): add tools module with Phase 1 constants"
```

---

### Task 5: ReadTool

**Files:**
- Create: `examples/coding-agent/src/tools/read.rs`

**Interfaces:**
- Consumes: Task 4 (`tools::read` module declared); `Tool`, `ToolContext`, `ToolResult`, `ToolError`, `ContentBlock`, `ExecutionEnv` from `llm-harness-types`
- Produces: `pub struct ReadTool` implementing `Tool`

Reference: `/Users/hhl/Documents/projs/oh-my-harness/coding-agent/src/tools/read.rs` (adopt as-is with minor adjustments).

- [ ] **Step 1: Write the full file with tests**

Create `examples/coding-agent/src/tools/read.rs`:

```rust
use futures::future::BoxFuture;
use llm_harness_types::*;
use serde_json::Value;

/// Maximum lines returned by a single Read call.
pub const MAX_LINES: usize = 2000;
/// Maximum bytes returned by a single Read call.
pub const MAX_BYTES: usize = 100 * 1024;

/// Read file contents with optional line-range selection.
pub struct ReadTool {
    schema: Value,
}

impl ReadTool {
    pub fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to read (relative or absolute)"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "1-indexed line number to start reading from"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to read"
                    }
                },
                "required": ["file_path"]
            }),
        }
    }
}

impl Default for ReadTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Output is truncated to 2000 lines or 100 KB. \
         Use offset/limit for large files."
    }

    fn parameters_schema(&self) -> &Value {
        &self.schema
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move { read_file(args, ctx).await })
    }
}

async fn read_file(args: Value, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
    let path_str = args["file_path"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArguments("file_path required".into()))?;

    let offset = args["offset"].as_u64().map(|v| v as usize);
    let limit = args["limit"].as_u64().map(|v| v as usize);

    let abs_path = resolve_path(path_str, ctx.env.working_dir());

    let content = ctx
        .env
        .read_text_file(&abs_path, ctx.abort.clone())
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?;

    let text = format_text_output(&content, offset, limit);
    Ok(ToolResult {
        content: vec![ContentBlock::Text { text }],
        details: Value::Null,
        terminate: false,
    })
}

fn resolve_path(path: &str, cwd: &std::path::Path) -> std::path::PathBuf {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// Slice lines by offset/limit and apply MAX_LINES / MAX_BYTES truncation.
/// Returns formatted text with a continuation notice when truncated.
pub fn format_text_output(content: &str, offset: Option<usize>, limit: Option<usize>) -> String {
    let all_lines: Vec<&str> = content.split('\n').collect();
    let total_lines = all_lines.len();

    // Apply 1-indexed offset.
    let start = offset.map(|o| o.saturating_sub(1)).unwrap_or(0);
    if start >= total_lines {
        return format!(
            "[Offset {} is beyond end of file ({} lines total)]",
            offset.unwrap_or(1),
            total_lines
        );
    }

    // Apply limit.
    let end = limit
        .map(|l| (start + l).min(total_lines))
        .unwrap_or(total_lines);
    let selected = &all_lines[start..end];

    // Apply MAX_LINES truncation.
    let (lines, truncated_by_lines) = if selected.len() > MAX_LINES {
        (&selected[..MAX_LINES], true)
    } else {
        (selected, false)
    };

    // Apply MAX_BYTES truncation.
    let mut byte_count = 0;
    let mut byte_limit_at = None;
    for (i, line) in lines.iter().enumerate() {
        byte_count += line.len() + 1; // +1 for newline
        if byte_count > MAX_BYTES {
            byte_limit_at = Some(i);
            break;
        }
    }

    let (final_lines, truncated) = if let Some(n) = byte_limit_at {
        (&lines[..n], true)
    } else {
        (lines, truncated_by_lines)
    };

    let output_end = start + final_lines.len(); // 0-indexed exclusive
    let mut text = final_lines.join("\n");

    if truncated {
        let next_offset = output_end + 1; // 1-indexed
        let remaining = total_lines - output_end;
        text.push_str(&format!(
            "\n\n[{} more lines. Use offset={} to continue.]",
            remaining, next_offset
        ));
    } else if end < total_lines {
        // User limit stopped early but file has more.
        let next_offset = end + 1;
        let remaining = total_lines - end;
        text.push_str(&format!(
            "\n\n[{} more lines. Use offset={} to continue.]",
            remaining, next_offset
        ));
    }

    text
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_loop::test_utils::NoOpEnv;
    use std::io::Write as IoWrite;
    use std::sync::Arc;
    use tempfile::NamedTempFile;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn make_ctx(env: Arc<dyn ExecutionEnv>) -> (ToolContext, mpsc::Receiver<ToolResult>) {
        let (tx, rx) = mpsc::channel(4);
        let ctx = ToolContext {
            env,
            abort: CancellationToken::new(),
            tool_use_id: "test-id".into(),
            turn_index: 0,
            assistant_message: Arc::new(AssistantMessage {
                content: vec![],
                stop_reason: None,
                timestamp: chrono::Utc::now(),
                provider: None,
                api: None,
                model: None,
                usage: None,
                error_message: None,
            }),
            update_tx: tx,
        };
        (ctx, rx)
    }

    #[test]
    fn format_returns_full_content_for_small_file() {
        let content = "line1\nline2\nline3";
        let out = format_text_output(content, None, None);
        assert_eq!(out, "line1\nline2\nline3");
    }

    #[test]
    fn format_applies_offset_1indexed() {
        let content = "a\nb\nc\nd";
        let out = format_text_output(content, Some(2), None);
        assert!(out.starts_with("b\nc\nd"), "got: {:?}", out);
    }

    #[test]
    fn format_applies_limit() {
        let content = "a\nb\nc\nd\ne";
        let out = format_text_output(content, None, Some(2));
        assert!(out.starts_with("a\nb"), "got: {:?}", out);
        assert!(out.contains("more lines"), "should have continuation notice");
    }

    #[test]
    fn format_offset_beyond_eof_returns_error_message() {
        let content = "a\nb";
        let out = format_text_output(content, Some(99), None);
        assert!(out.contains("beyond end of file"), "got: {:?}", out);
    }

    #[test]
    fn format_truncates_at_max_lines() {
        let content = (0..MAX_LINES + 10)
            .map(|i| format!("line{}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let out = format_text_output(&content, None, None);
        assert!(out.contains("more lines"), "should be truncated");
        let line_count = out.lines().count();
        assert!(line_count <= MAX_LINES + 3, "too many lines: {}", line_count);
    }

    #[tokio::test]
    async fn execute_reads_real_file() {
        use llm_harness_runtime_sandbox_os::OsEnv;

        let mut tmp = NamedTempFile::new().unwrap();
        writeln!(tmp, "hello from file").unwrap();
        let path = tmp.path().to_path_buf();

        let env = Arc::new(OsEnv::new(path.parent().unwrap().to_path_buf()));
        let (ctx, _rx) = make_ctx(env);

        let tool = ReadTool::new();
        let args = serde_json::json!({ "file_path": path.to_str().unwrap() });
        let result = tool.execute(args, &ctx).await.unwrap();

        let text = match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("hello from file"), "got: {:?}", text);
    }

    #[tokio::test]
    async fn execute_returns_error_for_missing_file() {
        use llm_harness_runtime_sandbox_os::OsEnv;

        let env = Arc::new(OsEnv::new(std::path::PathBuf::from("/tmp")));
        let (ctx, _rx) = make_ctx(env);

        let tool = ReadTool::new();
        let args = serde_json::json!({ "file_path": "/tmp/nonexistent_file_xyz_12345.txt" });
        let result = tool.execute(args, &ctx).await;
        assert!(result.is_err(), "expected error for missing file");
    }

    #[tokio::test]
    async fn execute_with_noop_env_returns_error() {
        let env = Arc::new(NoOpEnv);
        let (ctx, _rx) = make_ctx(env);

        let tool = ReadTool::new();
        let args = serde_json::json!({ "file_path": "/any/path" });
        let result = tool.execute(args, &ctx).await;
        assert!(result.is_err(), "NoOpEnv should fail");
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test -p coding-agent -- tools::read
```
Expected: 8 tests PASS.

- [ ] **Step 3: Format, clippy, commit**

```bash
cargo fmt
cargo clippy -p coding-agent --all-targets
git add examples/coding-agent/src/tools/read.rs
git commit -m "feat(coding-agent): add ReadTool with line-range and truncation"
```

---

### Task 6: BashTool

**Files:**
- Create: `examples/coding-agent/src/tools/bash.rs`

**Interfaces:**
- Consumes: Task 4; `Tool`, `ToolContext`, `ShellOptions`, `ShellOutput`, `EnvError` from `llm-harness-types`
- Produces: `pub struct BashTool` implementing `Tool`

Reference: `/Users/hhl/Documents/projs/oh-my-harness/coding-agent/src/tools/bash.rs` (adopt as-is).

- [ ] **Step 1: Write the full file with tests**

Create `examples/coding-agent/src/tools/bash.rs`:

```rust
use futures::future::BoxFuture;
use llm_harness_types::*;
use serde_json::Value;

/// Maximum combined stdout+stderr bytes returned by a single Bash call.
pub const MAX_OUTPUT_BYTES: usize = 100 * 1024;

/// Execute a shell command and return its combined stdout + stderr.
pub struct BashTool {
    schema: Value,
}

impl BashTool {
    pub fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Bash command to execute"
                    },
                    "timeout": {
                        "type": "number",
                        "description": "Timeout in seconds (optional)"
                    }
                },
                "required": ["command"]
            }),
        }
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command and return combined stdout and stderr. \
         Output is truncated to 100 KB."
    }

    fn parameters_schema(&self) -> &Value {
        &self.schema
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move { run_bash(args, ctx).await })
    }
}

async fn run_bash(args: Value, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
    let command = args["command"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArguments("command required".into()))?;

    let timeout_secs = args["timeout"].as_f64();

    let opts = ShellOptions {
        cwd: None,
        env: vec![],
        timeout: timeout_secs.map(std::time::Duration::from_secs_f64),
        abort: ctx.abort.clone(),
        on_stdout: None,
        on_stderr: None,
    };

    let output = ctx.env.execute_shell(command, opts).await;
    // Treat ShellFailed (non-zero exit) as a valid output — bash tools always
    // return output, even when commands fail.
    let output = match output {
        Ok(o) => o,
        Err(EnvError::ShellFailed { exit_code, stderr }) => ShellOutput {
            stdout: String::new(),
            stderr,
            exit_code,
        },
        Err(e) => return Err(ToolError::Execution(e.to_string())),
    };

    let text = format_output(&output);
    Ok(ToolResult {
        content: vec![ContentBlock::Text { text }],
        details: serde_json::json!({ "exit_code": output.exit_code }),
        terminate: false,
    })
}

/// Combine stdout + stderr, truncate to MAX_OUTPUT_BYTES, annotate exit code.
pub fn format_output(output: &ShellOutput) -> String {
    let combined = format!("{}{}", output.stdout, output.stderr);
    let truncated = if combined.len() > MAX_OUTPUT_BYTES {
        let cut = &combined[..MAX_OUTPUT_BYTES];
        format!(
            "{}\n[Output truncated: {} bytes omitted]",
            cut,
            combined.len() - MAX_OUTPUT_BYTES
        )
    } else {
        combined
    };

    if output.exit_code != 0 {
        format!("{}\n[Exit code: {}]", truncated, output.exit_code)
    } else {
        truncated
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_runtime_sandbox_os::OsEnv;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn make_ctx(env: Arc<dyn ExecutionEnv>) -> ToolContext {
        let (tx, _rx) = mpsc::channel(4);
        ToolContext {
            env,
            abort: CancellationToken::new(),
            tool_use_id: "test-id".into(),
            turn_index: 0,
            assistant_message: Arc::new(AssistantMessage {
                content: vec![],
                stop_reason: None,
                timestamp: chrono::Utc::now(),
                provider: None,
                api: None,
                model: None,
                usage: None,
                error_message: None,
            }),
            update_tx: tx,
        }
    }

    #[test]
    fn format_output_success() {
        let out = ShellOutput {
            stdout: "hello\n".into(),
            stderr: String::new(),
            exit_code: 0,
        };
        let text = format_output(&out);
        assert_eq!(text, "hello\n");
    }

    #[test]
    fn format_output_nonzero_exit_appends_code() {
        let out = ShellOutput {
            stdout: "oops\n".into(),
            stderr: String::new(),
            exit_code: 1,
        };
        let text = format_output(&out);
        assert!(text.contains("[Exit code: 1]"), "got: {:?}", text);
    }

    #[test]
    fn format_output_truncates_large_output() {
        let big = "x".repeat(MAX_OUTPUT_BYTES + 100);
        let out = ShellOutput {
            stdout: big,
            stderr: String::new(),
            exit_code: 0,
        };
        let text = format_output(&out);
        assert!(text.contains("truncated"), "got: {:?}", &text[..100]);
        assert!(text.len() <= MAX_OUTPUT_BYTES + 200);
    }

    #[test]
    fn format_output_combines_stdout_stderr() {
        let out = ShellOutput {
            stdout: "out".into(),
            stderr: "err".into(),
            exit_code: 0,
        };
        let text = format_output(&out);
        assert!(text.contains("out") && text.contains("err"), "got: {:?}", text);
    }

    #[tokio::test]
    async fn execute_echo_command() {
        let env = Arc::new(OsEnv::new(std::path::PathBuf::from("/tmp")));
        let ctx = make_ctx(env);

        let tool = BashTool::new();
        let args = serde_json::json!({ "command": "echo hello_world" });
        let result = tool.execute(args, &ctx).await.unwrap();

        let text = match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("hello_world"), "got: {:?}", text);
    }

    #[tokio::test]
    async fn execute_nonzero_exit_returns_ok_with_exit_code() {
        let env = Arc::new(OsEnv::new(std::path::PathBuf::from("/tmp")));
        let ctx = make_ctx(env);

        let tool = BashTool::new();
        let args = serde_json::json!({ "command": "exit 42" });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("42"), "should report exit code 42; got: {:?}", text);
    }

    #[tokio::test]
    async fn execute_captures_stderr() {
        let env = Arc::new(OsEnv::new(std::path::PathBuf::from("/tmp")));
        let ctx = make_ctx(env);

        let tool = BashTool::new();
        let args = serde_json::json!({ "command": "echo err_output >&2" });
        let result = tool.execute(args, &ctx).await.unwrap();
        let text = match &result.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("err_output"), "got: {:?}", text);
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test -p coding-agent -- tools::bash
```
Expected: 7 tests PASS.

- [ ] **Step 3: Format, clippy, commit**

```bash
cargo fmt
cargo clippy -p coding-agent --all-targets
git add examples/coding-agent/src/tools/bash.rs
git commit -m "feat(coding-agent): add BashTool with output truncation"
```

---

### Task 7: EditTool

**Files:**
- Create: `examples/coding-agent/src/tools/edit.rs`

**Interfaces:**
- Consumes: Task 4; `Tool`, `ToolContext`, `ToolResult`, `ToolError`, `ContentBlock`, `ExecutionEnv` from `llm-harness-types`
- Produces: `pub struct EditTool` implementing `Tool`

Reference: `/Users/hhl/Documents/projs/oh-my-harness/coding-agent/src/tools/edit.rs` (adopt as-is).

- [ ] **Step 1: Write the full file with tests**

Create `examples/coding-agent/src/tools/edit.rs`:

```rust
use futures::future::BoxFuture;
use llm_harness_types::*;
use serde_json::Value;

/// Replace an exact string in a file. The old_string must appear exactly once.
pub struct EditTool {
    schema: Value,
}

impl EditTool {
    pub fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to edit (relative or absolute)"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "Exact text to replace. Must appear exactly once in the file."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "Replacement text."
                    }
                },
                "required": ["file_path", "old_string", "new_string"]
            }),
        }
    }
}

impl Default for EditTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Perform an exact string replacement in a file. \
         old_string must appear exactly once in the file."
    }

    fn parameters_schema(&self) -> &Value {
        &self.schema
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move { edit_file(args, ctx).await })
    }
}

async fn edit_file(args: Value, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
    let path_str = args["file_path"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArguments("file_path required".into()))?;
    let old_string = args["old_string"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArguments("old_string required".into()))?;
    let new_string = args["new_string"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArguments("new_string required".into()))?;

    let abs_path = resolve_path(path_str, ctx.env.working_dir());

    let content = ctx
        .env
        .read_text_file(&abs_path, ctx.abort.clone())
        .await
        .map_err(|e| ToolError::Execution(format!("Could not read {}: {}", path_str, e)))?;

    let count = content.matches(old_string).count();
    match count {
        0 => {
            return Err(ToolError::Execution(format!(
                "old_string not found in {}",
                path_str
            )));
        }
        1 => {}
        n => {
            return Err(ToolError::Execution(format!(
                "old_string appears {} times in {}; must appear exactly once",
                n, path_str
            )));
        }
    }

    let new_content = content.replacen(old_string, new_string, 1);

    ctx.env
        .write_file(&abs_path, new_content.as_bytes(), ctx.abort.clone())
        .await
        .map_err(|e| ToolError::Execution(format!("Could not write {}: {}", path_str, e)))?;

    Ok(ToolResult {
        content: vec![ContentBlock::Text {
            text: format!("Successfully edited {}", path_str),
        }],
        details: Value::Null,
        terminate: false,
    })
}

fn resolve_path(path: &str, cwd: &std::path::Path) -> std::path::PathBuf {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_runtime_sandbox_os::OsEnv;
    use std::io::Write as IoWrite;
    use std::sync::Arc;
    use tempfile::NamedTempFile;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn make_ctx(env: Arc<dyn ExecutionEnv>) -> ToolContext {
        let (tx, _rx) = mpsc::channel(4);
        ToolContext {
            env,
            abort: CancellationToken::new(),
            tool_use_id: "test-id".into(),
            turn_index: 0,
            assistant_message: Arc::new(AssistantMessage {
                content: vec![],
                stop_reason: None,
                timestamp: chrono::Utc::now(),
                provider: None,
                api: None,
                model: None,
                usage: None,
                error_message: None,
            }),
            update_tx: tx,
        }
    }

    #[tokio::test]
    async fn edit_replaces_unique_string() {
        let mut tmp = NamedTempFile::new().unwrap();
        write!(tmp, "hello world").unwrap();
        let path = tmp.path().to_path_buf();

        let env = Arc::new(OsEnv::new(path.parent().unwrap().to_path_buf()));
        let ctx = make_ctx(env);

        let tool = EditTool::new();
        let args = serde_json::json!({
            "file_path": path.to_str().unwrap(),
            "old_string": "world",
            "new_string": "Rust"
        });
        tool.execute(args, &ctx).await.unwrap();

        let updated = std::fs::read_to_string(&path).unwrap();
        assert_eq!(updated, "hello Rust");
    }

    #[tokio::test]
    async fn edit_fails_when_string_not_found() {
        let mut tmp = NamedTempFile::new().unwrap();
        write!(tmp, "hello world").unwrap();
        let path = tmp.path().to_path_buf();

        let env = Arc::new(OsEnv::new(path.parent().unwrap().to_path_buf()));
        let ctx = make_ctx(env);

        let tool = EditTool::new();
        let args = serde_json::json!({
            "file_path": path.to_str().unwrap(),
            "old_string": "nonexistent",
            "new_string": "x"
        });
        let err = tool.execute(args, &ctx).await.unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {:?}", err);
    }

    #[tokio::test]
    async fn edit_fails_when_string_not_unique() {
        let mut tmp = NamedTempFile::new().unwrap();
        write!(tmp, "abc abc abc").unwrap();
        let path = tmp.path().to_path_buf();

        let env = Arc::new(OsEnv::new(path.parent().unwrap().to_path_buf()));
        let ctx = make_ctx(env);

        let tool = EditTool::new();
        let args = serde_json::json!({
            "file_path": path.to_str().unwrap(),
            "old_string": "abc",
            "new_string": "xyz"
        });
        let err = tool.execute(args, &ctx).await.unwrap_err();
        assert!(err.to_string().contains("3 times"), "got: {:?}", err);
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test -p coding-agent -- tools::edit
```
Expected: 3 tests PASS.

- [ ] **Step 3: Format, clippy, commit**

```bash
cargo fmt
cargo clippy -p coding-agent --all-targets
git add examples/coding-agent/src/tools/edit.rs
git commit -m "feat(coding-agent): add EditTool with uniqueness validation"
```

---

### Task 8: WriteTool

**Files:**
- Create: `examples/coding-agent/src/tools/write.rs`

**Interfaces:**
- Consumes: Task 4; `Tool`, `ToolContext`, `ToolResult`, `ToolError`, `ContentBlock`, `ExecutionEnv` from `llm-harness-types`
- Produces: `pub struct WriteTool` implementing `Tool`

Reference: `/Users/hhl/Documents/projs/oh-my-harness/coding-agent/src/tools/write.rs` (adopt as-is).

- [ ] **Step 1: Write the full file with tests**

Create `examples/coding-agent/src/tools/write.rs`:

```rust
use futures::future::BoxFuture;
use llm_harness_types::*;
use serde_json::Value;

/// Write (create or overwrite) a file with given content.
pub struct WriteTool {
    schema: Value,
}

impl WriteTool {
    pub fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to write (relative or absolute)"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file"
                    }
                },
                "required": ["file_path", "content"]
            }),
        }
    }
}

impl Default for WriteTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file, creating it or overwriting it if it exists. \
         Parent directories are created automatically."
    }

    fn parameters_schema(&self) -> &Value {
        &self.schema
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move { write_file(args, ctx).await })
    }
}

async fn write_file(args: Value, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
    let path_str = args["file_path"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArguments("file_path required".into()))?;
    let content = args["content"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArguments("content required".into()))?;

    let abs_path = resolve_path(path_str, ctx.env.working_dir());

    // Create parent directories if needed.
    if let Some(parent) = abs_path.parent() {
        ctx.env
            .create_dir(parent, true, ctx.abort.clone())
            .await
            .map_err(|e| ToolError::Execution(format!("Could not create directories: {}", e)))?;
    }

    ctx.env
        .write_file(&abs_path, content.as_bytes(), ctx.abort.clone())
        .await
        .map_err(|e| ToolError::Execution(format!("Could not write {}: {}", path_str, e)))?;

    Ok(ToolResult {
        content: vec![ContentBlock::Text {
            text: format!("Successfully wrote {} bytes to {}", content.len(), path_str),
        }],
        details: Value::Null,
        terminate: false,
    })
}

fn resolve_path(path: &str, cwd: &std::path::Path) -> std::path::PathBuf {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_runtime_sandbox_os::OsEnv;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn make_ctx(env: Arc<dyn ExecutionEnv>) -> ToolContext {
        let (tx, _rx) = mpsc::channel(4);
        ToolContext {
            env,
            abort: CancellationToken::new(),
            tool_use_id: "test-id".into(),
            turn_index: 0,
            assistant_message: Arc::new(AssistantMessage {
                content: vec![],
                stop_reason: None,
                timestamp: chrono::Utc::now(),
                provider: None,
                api: None,
                model: None,
                usage: None,
                error_message: None,
            }),
            update_tx: tx,
        }
    }

    #[tokio::test]
    async fn write_creates_new_file() {
        let dir = TempDir::new().unwrap();
        let env = Arc::new(OsEnv::new(dir.path().to_path_buf()));
        let ctx = make_ctx(env);

        let target = dir.path().join("new_file.txt");
        let tool = WriteTool::new();
        let args = serde_json::json!({
            "file_path": target.to_str().unwrap(),
            "content": "hello from write"
        });
        tool.execute(args, &ctx).await.unwrap();

        let text = std::fs::read_to_string(&target).unwrap();
        assert_eq!(text, "hello from write");
    }

    #[tokio::test]
    async fn write_overwrites_existing_file() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("existing.txt");
        std::fs::write(&target, "old content").unwrap();

        let env = Arc::new(OsEnv::new(dir.path().to_path_buf()));
        let ctx = make_ctx(env);

        let tool = WriteTool::new();
        let args = serde_json::json!({
            "file_path": target.to_str().unwrap(),
            "content": "new content"
        });
        tool.execute(args, &ctx).await.unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new content");
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("a/b/c/file.txt");

        let env = Arc::new(OsEnv::new(dir.path().to_path_buf()));
        let ctx = make_ctx(env);

        let tool = WriteTool::new();
        let args = serde_json::json!({
            "file_path": target.to_str().unwrap(),
            "content": "nested"
        });
        tool.execute(args, &ctx).await.unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "nested");
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test -p coding-agent -- tools::write
```
Expected: 3 tests PASS.

- [ ] **Step 3: Format, clippy, commit**

```bash
cargo fmt
cargo clippy -p coding-agent --all-targets
git add examples/coding-agent/src/tools/write.rs
git commit -m "feat(coding-agent): add WriteTool with parent directory creation"
```

---

### Task 9: plugin.rs — CodingAgentPlugin

**Files:**
- Create: `examples/coding-agent/src/plugin.rs`

**Interfaces:**
- Consumes: Tasks 3 (SystemPromptHook, build_system_prompt, find_context_files), 4-8 (all 4 tools), and Task 2 (plugin module declared)
- Produces: `pub struct CodingAgentPlugin` implementing `Plugin` — register 4 tools + SystemPromptHook

- [ ] **Step 1: Write the full file with tests**

Create `examples/coding-agent/src/plugin.rs`:

```rust
use std::path::{Path, PathBuf};
use std::sync::Arc;

use llm_harness_agent::{HarnessHooks, Plugin};
use llm_harness_types::Tool;

use crate::prompt::{self, SystemPromptHook};
use crate::tools::{bash, edit, read, write};

/// The coding-agent plugin: registers 4 tools and injects the system prompt.
///
/// This is the central integration point between the coding-agent application
/// and the llm-harness framework. It implements the `Plugin` trait so it can
/// be installed via `HarnessBuilder::install()`.
pub struct CodingAgentPlugin {
    system_prompt: String,
}

impl CodingAgentPlugin {
    /// Create a new plugin from the given working directory.
    ///
    /// Walks up from `cwd` to find CLAUDE.md and AGENTS.md context files,
    /// then builds the system prompt with the Phase 1 tool list.
    pub fn new(cwd: &Path) -> Self {
        let context_files = prompt::find_context_files(cwd);
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(read::ReadTool::new()),
            Arc::new(bash::BashTool::new()),
            Arc::new(edit::EditTool::new()),
            Arc::new(write::WriteTool::new()),
        ];
        let system_prompt = prompt::build_system_prompt(&tools, cwd, &context_files);
        Self { system_prompt }
    }

    /// Return the pre-built system prompt string.
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }
}

impl Plugin for CodingAgentPlugin {
    fn name(&self) -> &str {
        "coding-agent"
    }

    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        tools.push(Arc::new(read::ReadTool::new()));
        tools.push(Arc::new(bash::BashTool::new()));
        tools.push(Arc::new(edit::EditTool::new()));
        tools.push(Arc::new(write::WriteTool::new()));
    }

    fn register_hooks(&self, hooks: &mut HarnessHooks) {
        // 方案 A: inject system prompt via BeforeRunHook (since HarnessBuilder
        // does not yet have a .system_prompt() method).
        hooks
            .before_run
            .push(Arc::new(SystemPromptHook::new(self.system_prompt.clone())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_name_is_coding_agent() {
        let plugin = CodingAgentPlugin::new(Path::new("."));
        assert_eq!(plugin.name(), "coding-agent");
    }

    #[test]
    fn plugin_registers_four_tools() {
        let plugin = CodingAgentPlugin::new(Path::new("."));
        let mut tools: Vec<Arc<dyn Tool>> = vec![];
        plugin.register_tools(&mut tools);
        assert_eq!(tools.len(), 4);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"edit"));
        assert!(names.contains(&"write"));
    }

    #[test]
    fn plugin_registers_before_run_hook() {
        let plugin = CodingAgentPlugin::new(Path::new("."));
        let mut hooks = HarnessHooks::none();
        plugin.register_hooks(&mut hooks);
        assert_eq!(hooks.before_run.len(), 1, "should register one BeforeRunHook");
    }

    #[test]
    fn system_prompt_is_not_empty() {
        let plugin = CodingAgentPlugin::new(Path::new("."));
        let prompt = plugin.system_prompt();
        assert!(!prompt.is_empty());
        assert!(prompt.contains("coding assistant"));
        assert!(prompt.contains("read"));
        assert!(prompt.contains("bash"));
        assert!(prompt.contains("edit"));
        assert!(prompt.contains("write"));
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test -p coding-agent -- plugin
```
Expected: 4 tests PASS.

- [ ] **Step 3: Format, clippy, commit**

```bash
cargo fmt
cargo clippy -p coding-agent --all-targets
git add examples/coding-agent/src/plugin.rs
git commit -m "feat(coding-agent): add CodingAgentPlugin with BeforeRunHook system prompt injection"
```

---

### Task 10: output.rs — event→terminal renderer

**Files:**
- Create: `examples/coding-agent/src/output.rs`

**Interfaces:**
- Consumes: Task 2 (output module declared); `AgentHarnessEvent`, `AgentEvent`, `HarnessPhase` from `llm-harness-agent` + `llm-harness-types`
- Produces:
  - `pub async fn render_one_shot(mut rx: broadcast::Receiver<Arc<AgentHarnessEvent>>)` — streams TextDelta to stdout, prints newline on Idle, errors to stderr
  - Module-level documentation explaining the event loop pattern

- [ ] **Step 1: Write output.rs**

Create `examples/coding-agent/src/output.rs`:

```rust
//! Event-to-terminal rendering for coding-agent.
//!
//! Subscribes to `AgentHarnessEvent` broadcast and renders:
//! - `TextDelta` → stdout (one-shot/pipe mode, real-time streaming)
//! - `Error` → stderr
//! - `PhaseChange { to: Idle }` → break (run complete)
//!
//! This is intentionally minimal in Phase 1; Phase 2+ can add
//! ToolCallStart/ToolCallEnd formatted output, thinking toggles, etc.

use std::io::Write;

use llm_harness_agent::AgentHarnessEvent;
use llm_harness_types::{AgentEvent, HarnessPhase};
use tokio::sync::broadcast;

/// Stream events from a one-shot run to stdout/stderr.
///
/// Spawn this as a tokio task BEFORE calling `harness.prompt()`.
/// It processes events concurrently with the agent loop and
/// returns when the harness reaches `Idle`.
///
/// # Example
///
/// ```ignore
/// let mut events = harness.subscribe();
/// let printer = tokio::spawn(render_one_shot(events));
/// harness.prompt("hello").await?;
/// printer.await.unwrap();
/// println!(); // trailing newline
/// ```
pub async fn render_one_shot(mut rx: broadcast::Receiver<Arc<AgentHarnessEvent>>) {
    loop {
        match rx.recv().await {
            Ok(ev) => match ev.as_ref() {
                AgentHarnessEvent::Agent(AgentEvent::TextDelta { text, .. }) => {
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                }
                AgentHarnessEvent::Agent(AgentEvent::Error(e)) => {
                    eprintln!("\nAgent error: {e}");
                }
                AgentHarnessEvent::PhaseChange {
                    to: HarnessPhase::Idle,
                    ..
                } => break,
                _ => {}
            },
            Err(broadcast::error::RecvError::Closed) => break,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the module compiles and the function signature is callable.
    /// (Actual event-loop testing is done in integration tests with MockLlmClient.)
    #[test]
    fn render_one_shot_is_async_fn() {
        // This is a compile-time test: just confirming the module is well-formed.
        // We can't easily unit-test broadcast receivers without a real harness.
    }
}
```

- [ ] **Step 2: Build check + clippy**

```bash
cargo check -p coding-agent
cargo clippy -p coding-agent --all-targets
```
Expected: compiles cleanly, no warnings.

- [ ] **Step 3: Commit**

```bash
git add examples/coding-agent/src/output.rs
git commit -m "feat(coding-agent): add event-to-terminal renderer"
```

---

### Task 11: main.rs — CLI entry point

**Files:**
- Modify: `examples/coding-agent/src/main.rs` (replace placeholder)

**Interfaces:**
- Consumes: Tasks 2-10 (all modules); `HarnessBuilder`, `AgentHarness`, `AgentHarnessEvent`, Anthropic/OpenAI providers from framework; `CodingAgentPlugin` from `plugin.rs`; `render_one_shot` from `output.rs`
- Produces: working CLI binary with 3 modes:
  1. `coding-agent -p "prompt"` — one-shot, Anthropic (default) or OpenAI-compatible
  2. `echo "prompt" | coding-agent` — pipe
  3. `coding-agent -i` — interactive REPL

Reference: `/Users/hhl/Documents/projs/oh-my-harness/coding-agent/src/bin/coding-agent.rs` (simplify — no sessions, no settings, no budget, no audit, no approval).

- [ ] **Step 1: Write main.rs**

Replace `examples/coding-agent/src/main.rs` with:

```rust
//! CLI for the coding agent (Phase 1).
//!
//! Usage:
//!   coding-agent -p "your prompt"            # one-shot
//!   coding-agent "prompt"                     # positional arg (one-shot)
//!   echo "prompt" | coding-agent              # pipe
//!   coding-agent --interactive                # REPL
//!   coding-agent -i                           # REPL (short)
//!
//! Environment:
//!   ANTHROPIC_API_KEY          Required for Anthropic provider
//!   OPENAI_API_KEY             API key for OpenAI-compatible provider
//!   OPENAI_API_BASE            Base URL (e.g. https://api.siliconflow.cn/v1)
//!   OPENAI_MODEL               Model name for OpenAI-compatible provider
//!   CODING_AGENT_MODEL         Override default model (else claude-sonnet-4-6)

use std::sync::Arc;

use llm_adapter::anthropic::AnthropicProvider;
use llm_adapter::openai::OpenAIProvider;
use llm_adapter::provider::Provider;
use llm_harness_agent::AgentHarnessEvent;
use llm_harness_loop::LlmClient;
use llm_harness_runtime::HarnessBuilder;
use llm_harness_runtime_sandbox_os::OsEnv;
use llm_harness_types::AgentEvent;

use coding_agent::output;
use coding_agent::plugin::CodingAgentPlugin;

const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

#[tokio::main]
async fn main() {
    let code = run().await;
    std::process::exit(code);
}

async fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    // ── --help / -h ─────────────────────────────────────────────────────────────

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return 0;
    }

    // ── Detect mode: interactive vs one-shot ────────────────────────────────────

    let interactive = args.iter().any(|a| a == "--interactive" || a == "-i")
        || (atty::is(atty::Stream::Stdin) && resolve_prompt_opt(&args).is_none());

    let prompt_opt = if interactive {
        None
    } else {
        match resolve_prompt(&args) {
            Ok(p) => Some(p),
            Err(msg) => {
                eprintln!("Error: {msg}");
                return 1;
            }
        }
    };

    // ── Resolve provider ────────────────────────────────────────────────────────

    let openai_key = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    let openai_base = std::env::var("OPENAI_API_BASE")
        .ok()
        .filter(|s| !s.is_empty());

    let (client, default_model): (Arc<dyn LlmClient>, String) =
        if let (Some(key), Some(base)) = (openai_key, openai_base) {
            let base_trimmed = base.trim_end_matches('/').to_string();
            let (base_url, chat_path) = if base_trimmed.ends_with("/v1") {
                (base_trimmed, "/chat/completions".to_string())
            } else {
                (base_trimmed, "/v1/chat/completions".to_string())
            };
            let provider = OpenAIProvider::builder(key)
                .base_url(base_url)
                .chat_path(chat_path)
                .build();
            let model = std::env::var("OPENAI_MODEL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "gpt-4o".to_string());
            (Arc::new(provider), model)
        } else {
            let api_key = match std::env::var("ANTHROPIC_API_KEY") {
                Ok(k) if !k.is_empty() => k,
                _ => {
                    eprintln!("Error: set ANTHROPIC_API_KEY or OPENAI_API_KEY + OPENAI_API_BASE");
                    return 1;
                }
            };
            (
                Arc::new(AnthropicProvider::builder(api_key).build()),
                DEFAULT_MODEL.to_string(),
            )
        };

    let model = std::env::var("CODING_AGENT_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or(default_model);

    // ── Build harness ───────────────────────────────────────────────────────────

    let plugin = CodingAgentPlugin::new(&cwd);
    let env = Arc::new(OsEnv::new(cwd));

    let harness = match HarnessBuilder::new(&model)
        .install(&plugin)
        .provider(&model, client)
        .build(env)
        .await
    {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Error building agent: {e}");
            return 1;
        }
    };

    // ── Run ─────────────────────────────────────────────────────────────────────

    if interactive {
        run_repl(&harness).await
    } else {
        let prompt = prompt_opt.unwrap();
        run_one_shot(&harness, &prompt).await
    }
}

// ── One-shot mode ──────────────────────────────────────────────────────────────

async fn run_one_shot(
    harness: &llm_harness_agent::AgentHarness,
    prompt: &str,
) -> i32 {
    let events = harness.subscribe();
    let printer = tokio::spawn(output::render_one_shot(events));

    if let Err(e) = harness.prompt(prompt).await {
        eprintln!("\nError: {e}");
    }

    let _ = printer.await;
    println!();
    0
}

// ── Interactive REPL ───────────────────────────────────────────────────────────

async fn run_repl(harness: &llm_harness_agent::AgentHarness) -> i32 {
    use rustyline::DefaultEditor;
    use rustyline::error::ReadlineError;

    let history_path = dirs_next::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("coding-agent")
        .join("history");

    let mut editor = match DefaultEditor::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Failed to initialize line editor: {e}");
            return 1;
        }
    };

    if history_path.exists() {
        let _ = editor.load_history(&history_path);
    }

    loop {
        let line = match editor.readline("> ") {
            Ok(l) => l,
            Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => break,
            Err(e) => {
                eprintln!("Error reading input: {e}");
                let _ = save_history(&mut editor, &history_path);
                return 1;
            }
        };

        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line == "exit" || line == "quit" {
            break;
        }
        let _ = editor.add_history_entry(&line);

        if run_one_shot(harness, &line).await != 0 {
            let _ = save_history(&mut editor, &history_path);
            return 1;
        }
    }

    let _ = save_history(&mut editor, &history_path);
    0
}

fn save_history(
    editor: &mut rustyline::DefaultEditor,
    path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    editor.save_history(path)?;
    Ok(())
}

// ── Help ───────────────────────────────────────────────────────────────────────

fn print_help() {
    println!(
        "\
Usage: coding-agent [OPTIONS] [PROMPT]

One-shot mode (default):
  coding-agent -p \"your prompt\"
  coding-agent \"your prompt\"
  echo \"prompt\" | coding-agent

Interactive REPL:
  coding-agent --interactive
  coding-agent -i

Options:
  -p, --print <PROMPT>       Prompt text (one-shot)
  -i, --interactive          Start interactive REPL
  -h, --help                 Show this help

Environment:
  ANTHROPIC_API_KEY          Required API key (Anthropic)
  OPENAI_API_KEY             API key for OpenAI-compatible provider
  OPENAI_API_BASE            Base URL for OpenAI-compatible provider
  OPENAI_MODEL               Model name for OpenAI-compatible provider
  CODING_AGENT_MODEL         Override default model"
    );
}

// ── Arg parsing ────────────────────────────────────────────────────────────────

fn resolve_prompt(args: &[String]) -> Result<String, String> {
    resolve_prompt_opt(args).ok_or_else(|| {
        "No prompt provided. Use -p \"prompt\", pass text as argument, or pipe via stdin.".into()
    })
}

fn resolve_prompt_opt(args: &[String]) -> Option<String> {
    // -p "prompt" or --print "prompt"
    for i in 0..args.len() {
        if (args[i] == "-p" || args[i] == "--print") && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
    }

    // Positional arg: first non-flag, non-flag-value argument.
    let mut skip_next = false;
    for arg in args.iter().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg.starts_with('-') {
            // Flags that take a value: skip their value.
            if arg == "-p" || arg == "--print" {
                skip_next = true;
            }
            continue;
        }
        return Some(arg.clone());
    }

    // Stdin (non-TTY only)
    if !atty::is(atty::Stream::Stdin) {
        let mut buf = String::new();
        if std::io::stdin().read_to_string(&mut buf).is_ok() {
            let trimmed = buf.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }

    None
}
```

- [ ] **Step 2: Build check**

```bash
cargo check -p coding-agent
```
Expected: compiles (may have unused import warnings in test builds — clippy will catch those).

- [ ] **Step 3: Format + clippy**

```bash
cargo fmt
cargo clippy -p coding-agent --all-targets
```
Expected: no warnings. Fix any that appear.

- [ ] **Step 4: Dry-run smoke test (no API key)**

```bash
cargo run -p coding-agent -- --help
```
Expected: prints usage text, exits 0.

```bash
echo "hello" | cargo run -p coding-agent
```
Expected: "Error: set ANTHROPIC_API_KEY or OPENAI_API_KEY + OPENAI_API_BASE", exits 1.

- [ ] **Step 5: Commit**

```bash
git add examples/coding-agent/src/main.rs
git commit -m "feat(coding-agent): add CLI with one-shot, pipe, and REPL modes"
```

---

### Task 12: integration tests

**Files:**
- Create: `examples/coding-agent/tests/integration.rs`

**Interfaces:**
- Consumes: Tasks 2-11 (all modules + CLI); `MockLlmClient`, `MockResponse`, `NoOpEnv` from `llm-harness-loop::test_utils`
- Produces: integration tests verifying:
  1. Agent calls a tool (read) and gets result
  2. TextDelta streaming output works
  3. EditTool fails when old_string is not unique
  4. main.rs errors gracefully when no API key

Reference: `/Users/hhl/Documents/projs/oh-my-harness/coding-agent/tests/integration.rs` (simplified for Phase 1 — no audit, no budget, no phase manager).

- [ ] **Step 1: Write integration tests**

Create `examples/coding-agent/tests/integration.rs`:

```rust
//! Integration tests for coding-agent using MockLlmClient.
//!
//! These tests verify the full harness → plugin → tool pipeline without
//! requiring real API keys or external services.

use std::sync::Arc;

use llm_harness_agent::{AgentHarness, AgentHarnessOptions, HarnessHooks};
use llm_harness_loop::LlmClient;
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse, NoOpEnv};
use llm_harness_types::{
    AgentEvent, AgentMessage, ContentBlock, HarnessPhase, Tool, ToolError, ToolResult,
};

use coding_agent::plugin::CodingAgentPlugin;

/// Build a minimal AgentHarness with MockLlmClient + NoOpEnv + CodingAgentPlugin.
async fn make_test_harness(responses: Vec<MockResponse>) -> AgentHarness {
    let client = Arc::new(MockLlmClient::new(responses)) as Arc<dyn LlmClient>;
    let env: Arc<dyn llm_harness_types::ExecutionEnv> = Arc::new(NoOpEnv);
    let plugin = CodingAgentPlugin::new(std::path::Path::new("."));
    let mut hooks = HarnessHooks::none();
    plugin.register_hooks(&mut hooks);

    let mut tools: Vec<Arc<dyn Tool>> = vec![];
    plugin.register_tools(&mut tools);

    let mut opts = AgentHarnessOptions::new("mock-model");
    opts.tools = tools;
    opts.hooks = hooks;

    AgentHarness::new_in_memory(client, env, opts).await
}

/// Helper: collect all AgentEvent variants from a completed harness run.
async fn collect_events(harness: &AgentHarness) -> Vec<AgentHarnessEvent> {
    use llm_harness_agent::AgentHarnessEvent;
    let mut rx = harness.subscribe();
    let mut events = Vec::new();
    loop {
        match rx.recv().await {
            Ok(ev) => {
                let is_settled = matches!(ev.as_ref(), AgentHarnessEvent::Settled);
                events.push(ev);
                if is_settled {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    events
}

// ── Tests ──────────────────────────────────────────────────────────────────────

/// Agent calls a tool (echo via bash), tool result flows back to LLM, final text emitted.
#[tokio::test]
async fn tool_call_flow_completes() {
    // Mock: first response calls "bash", second response is plain text.
    let responses = vec![
        MockResponse::tool_use("call-1", "bash", r#"{"command":"echo hello"}"#),
        MockResponse::text("Command executed successfully."),
    ];

    let harness = make_test_harness(responses).await;
    harness
        .prompt("run echo hello")
        .await
        .expect("prompt should succeed");

    let events = collect_events(&harness).await;

    // Should have at least PhaseChange to Idle and Settled.
    let has_idle = events.iter().any(|e| {
        matches!(
            e,
            AgentHarnessEvent::PhaseChange {
                to: HarnessPhase::Idle,
                ..
            }
        )
    });
    assert!(has_idle, "should transition to Idle");
}

/// TextDelta events stream correctly for a simple text response.
#[tokio::test]
async fn text_delta_streamed_correctly() {
    let responses = vec![MockResponse::text("Hello from mock!")];

    let harness = make_test_harness(responses).await;
    harness
        .prompt("say hello")
        .await
        .expect("prompt should succeed");

    let events = collect_events(&harness).await;
    let text_deltas: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentHarnessEvent::Agent(AgentEvent::TextDelta { text, .. }) => Some(text.clone()),
            _ => None,
        })
        .collect();

    let full: String = text_deltas.concat();
    assert_eq!(full, "Hello from mock!");
}

/// EditTool must return an error when old_string appears multiple times.
/// This verifies that tool errors propagate correctly.
#[tokio::test]
async fn edit_tool_rejects_non_unique_old_string() {
    use llm_harness_runtime_sandbox_os::OsEnv;
    use llm_harness_types::{ToolContext, AssistantMessage};
    use coding_agent::tools::edit::EditTool;
    use tokio_util::sync::CancellationToken;

    let dir = tempfile::TempDir::new().unwrap();
    let file_path = dir.path().join("test.txt");
    std::fs::write(&file_path, "abc abc abc").unwrap();

    let env = Arc::new(OsEnv::new(dir.path().to_path_buf()));
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let ctx = ToolContext {
        env,
        abort: CancellationToken::new(),
        tool_use_id: "test-id".into(),
        turn_index: 0,
        assistant_message: Arc::new(AssistantMessage {
            content: vec![],
            stop_reason: None,
            timestamp: chrono::Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        }),
        update_tx: tx,
    };

    let tool = EditTool::new();
    let args = serde_json::json!({
        "file_path": file_path.to_str().unwrap(),
        "old_string": "abc",
        "new_string": "xyz"
    });
    let result = tool.execute(args, &ctx).await;
    assert!(result.is_err(), "should reject non-unique old_string");
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("3 times"), "got: {err_msg}");
}

/// Plugin hooks register correctly and HarnessBuilder works end-to-end.
#[tokio::test]
async fn harness_builder_with_plugin_works() {
    use llm_harness_runtime::HarnessBuilder;

    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("done")]));
    let env = Arc::new(NoOpEnv);
    let plugin = CodingAgentPlugin::new(std::path::Path::new("."));

    let harness = HarnessBuilder::new("mock-model")
        .install(&plugin)
        .provider("mock-model", client)
        .build(env)
        .await
        .expect("build should succeed");

    harness.prompt("test").await.expect("prompt should succeed");
}
```

- [ ] **Step 2: Run integration tests**

```bash
cargo test -p coding-agent --test integration
```
Expected: 5 tests PASS.

- [ ] **Step 3: Run all tests together**

```bash
cargo test -p coding-agent
```
Expected: all tests PASS (~30+ tests across all modules).

- [ ] **Step 4: Format + clippy final check**

```bash
cargo fmt
cargo clippy -p coding-agent --all-targets
```
Expected: no warnings. Fix any before committing.

- [ ] **Step 5: Commit**

```bash
git add examples/coding-agent/tests/integration.rs
git commit -m "test(coding-agent): add integration tests with MockLlmClient"
```

---

## Final Verification

After all 12 tasks are complete, run the full suite:

```bash
cargo test -p coding-agent
cargo clippy -p coding-agent --all-targets
cargo fmt --check
```

Expected: all tests pass, no clippy warnings, no format diffs.

Then verify the binary works (with a real API key):
```bash
ANTHROPIC_API_KEY=sk-ant-... cargo run -p coding-agent -- -p "say hello"
```
Expected: streams "Hello! ..." to stdout.
