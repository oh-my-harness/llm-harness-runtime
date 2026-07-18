# llm-harness-runtime

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
├── llm-harness-runtime-sandbox-*/  # sandbox backends (bwrap, seatbelt, os)
├── llm-harness-runtime-mcp/        # MCP integration
├── llm-harness-runtime-audit-jsonl/ # audit logging
└── llm-harness-runtime-trace-otel/  # OpenTelemetry tracing
```

## Versioning

Workspace version is in the root `Cargo.toml` (`[workspace.package]`). To release a new version, bump the version in `Cargo.toml`.
