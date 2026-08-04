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
├── llm-harness-types/      # core type definitions
├── llm-harness-loop/       # agent loop
├── llm-harness-agent/      # agent harness
├── llm-harness-runtime/    # runtime: workflow engine, executors, event stream, builder
├── llm-harness-runtime-knowledge/ # backend-neutral knowledge and access contracts
├── llm-harness-runtime-knowledge-local/ # root-confined Markdown/Text knowledge source
├── llm-harness-runtime-session-recall/ # opt-in authoritative cross-Session history recall
├── llm-harness-runtime-memory/ # composable memory read/write contracts and service
├── llm-harness-runtime-sandbox-*/  # sandbox backends (bwrap, seatbelt, os)
├── llm-harness-runtime-mcp/        # MCP integration
├── llm-harness-runtime-audit-jsonl/ # audit logging
├── llm-harness-runtime-trace-otel/  # OpenTelemetry tracing
└── session-viewer/         # session viewer (visualize JsonlSessionRepo sessions)
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
