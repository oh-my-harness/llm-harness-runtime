# llm-harness-runtime

> **[MISSION.md](MISSION.md)** — 本仓库及 harness 小组的纲领文档。从第一性原理推导 harness 的本质与使命，所有开发/业务决策以此为准。

Rust implementation of the oh-my-harness LLM agent orchestration runtime: workflow engine, agent harness, tools, event stream, and sandboxing.

> **Private repo.** Source is not distributed. Public consumers receive only git rev crates for Rust downstreams.

---

## Python SDK

The Python SDK (`llm_harness_py`) has been migrated to the Senza repo ([github.com/oh-my-harness/Senza](https://github.com/oh-my-harness/Senza)). Refer to the Senza repo for installation, API documentation, and wheel builds.

---

## For Rust developers — consuming workspace crates

Downstream Rust projects consume workspace crates via git rev:

```toml
llm-harness-runtime = { git = "https://github.com/oh-my-harness/llm-harness-runtime", rev = "<commit-sha>" }
```

---

## Layout

```
crates/
├── llm-harness-types/               # core type definitions
├── llm-harness-loop/                # agent loop engine
├── llm-harness-agent/               # agent harness (session, compaction, skills)
├── llm-harness-runtime/             # runtime: builder, workflow, spawn, lifecycle, sandbox
├── llm-harness-strategy/            # strategy layer: safety, loop-safety, status panel,
│                                    #   memory defense, notify, injection filter,
│                                    #   context-aware compaction, event streams
├── llm-harness-runtime-tools/       # built-in tools: bash, read, write, edit, grep, glob
├── llm-harness-runtime-mcp/         # MCP server integration (tools, prompts, resources)
├── llm-harness-runtime-knowledge/   # backend-neutral knowledge access contracts
├── llm-harness-runtime-knowledge-local/ # local Markdown/Text knowledge source (BM25)
├── llm-harness-runtime-memory/      # composable memory read/write + mutation gate
├── llm-harness-runtime-session-recall/ # cross-Session history recall (in-memory + SQLite FTS5)
├── llm-harness-runtime-sandbox-*/   # sandbox backends (bwrap, seatbelt, os)
├── llm-harness-runtime-audit-jsonl/ # audit logging (JSONL + hash chain)
├── llm-harness-runtime-trace-otel/  # OpenTelemetry tracing export
├── llm-harness-live-tests/          # live LLM integration tests (23 examples + 62+ tests)
└── session-viewer/                  # session viewer (visualize JsonlSessionRepo sessions)
```

---

## Features

- **Loop engine** — sync loop with async tools, retry with exponential backoff + jitter,
  streaming, parallel/sequential tool execution, dynamic config (thinking level, temperature)
- **Agent harness** — session persistence (InMemory + Jsonl), branching/fork/navigate,
  compaction with circuit breaker + context-aware prompt, skills, prompt templates,
  model switching, RunExtensions
- **Runtime** — HarnessBuilder assembly, WorkflowEngine (linear/conditional/loop/executor),
  crash recovery + pause/cancel, OsEnvSandbox, sub-agent spawn + MessageBus,
  tracing + audit (hash chain), budget/pricing control, rule-chain approval
- **Strategy** — SafetyDefaultsPlugin (bash blacklist + path traversal),
  InjectionFilterPlugin, StatusPanelPlugin + todo_write, SourceTagPlugin,
  ProjectInstructionPlugin (CLAUDE.md auto-inject), LoopSafetyPlugin (max_turns,
  repetition, failure breaker, death spiral, truncation continuation),
  MemoryDefensePlugin, ToolOutputGuardPlugin, NotifyPlugin,
  context-aware compaction prompt, EventStreams (Timer, Heartbeat,
  ShellMonitor, Webhook, Filter)
- **Infrastructure tools** — BashTool, ReadTool, WriteTool, EditTool, GrepTool, GlobTool
  (FsToolsPlugin), MCP integration, Knowledge (search/read/evidence), Memory
  (write/forget + mutation gate), SessionRecall (history search + projector)
- **Sandboxing** — OsEnv (production), bwrap (Linux), seatbelt (macOS)

---

## Live LLM Integration Tests

The `llm-harness-live-tests` crate contains 23 runnable examples and 62+ tests
covering all 5 architecture layers with real LLM APIs.

```bash
# Run all tests (requires .env with API keys)
source .env
cargo test --features live-llm -p llm-harness-live-tests -- --test-threads=1

# Run a single example
cargo run --features live-llm -p llm-harness-live-tests --example 22_fs_tools
```

---

## Session Viewer

`session-viewer` visualizes agent sessions persisted by `JsonlSessionRepo` (the shared persistence layer used by both Senza and `llm-harness-runtime`). It reads the on-disk format (`meta.json` + `entries.jsonl`) directly — no runtime crate dependency — and renders a self-contained HTML page in the browser.

### Rust (this repo)

```bash
cargo run -p session-viewer --bin session_viewer -- /path/to/sessions [--port PORT]
```

`<dir>` may be a sessions root (containing `<session_id>/` subdirs) or a single session directory.

### Python (Senza)

```bash
python -m senza.viewer /path/to/sessions [--port PORT]
```

The Python viewer delegates JSONL parsing to the Rust `session-viewer` crate via `senza.read_sessions()` — no duplicate parsing logic.

```python
import senza.viewer
senza.viewer.serve("/path/to/sessions")  # blocks, opens browser
```

### Features

- Session tree with branch switching (active branch highlighted)
- Message rendering: user / assistant / tool-result, with collapsible thinking and tool-use blocks
- Token usage stats (input / output / reasoning / cache)
- Config entries: model change, thinking level, active tools, compaction, labels, branch points
- Image content inlined as base64

## Session Recall

Cross-Session History Recall is an optional, fail-closed runtime feature. See
[the Session Recall integration guide](docs/session-recall-integration.md) for
authority wiring, per-Session eligibility, per-run opt-in, and operational
limits. The default index is dependency-free and in-memory; an opt-in
SQLite/FTS5 backend persists the derived projection across restarts.

## Versioning

Workspace version is in the root `Cargo.toml` (`[workspace.package]`). To release a new version, bump the version in `Cargo.toml`.
