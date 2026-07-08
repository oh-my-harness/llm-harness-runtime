# llm-harness-runtime

Rust implementation of the oh-my-harness LLM agent orchestration runtime: workflow engine, agent harness, tools, event stream, and sandboxing.

> **Private repo.** Source is not distributed. Public consumers receive only compiled artifacts (Python wheels, git rev crates for Rust downstreams).

## Artifacts

### Python SDK (`llm_harness_py`)

PyO3 bindings exposing `WorkflowEngine`, `AgentHarness`, tools, hooks, plugins, and event stream to Python.

Prebuilt wheels (CPython 3.12) are published on the [Releases](../../releases) page for:

| Platform | Wheel tag |
|----------|-----------|
| macOS Apple Silicon | `cp312-cp312-macosx_*_arm64` |
| macOS Intel | `cp312-cp312-macosx_*_x86_64` |
| Linux x64 | `cp312-cp312-manylinux_*_x86_64` |
| Windows x64 | `cp312-cp312-win_amd64` |

Install a released wheel:

```bash
pip install llm_harness_py --find-links https://github.com/oh-my-harness/llm-harness-runtime/releases/latest/download/
```

Or pin a version:

```bash
pip install llm_harness_py==0.2.0 --find-links https://github.com/oh-my-harness/llm-harness-runtime/releases/expanded_assets/v0.2.0
```

### Rust crates

Downstream Rust projects consume workspace crates via git rev:

```toml
llm-harness-runtime = { git = "https://github.com/oh-my-harness/llm-harness-runtime", rev = "<commit-sha>" }
```

## Building from source

Requires Rust (stable, edition 2024) and Python 3.12 with `maturin`:

```bash
cd crates/llm-harness-py
maturin develop --release     # install into active venv
# or
maturin build --release       # produce a wheel in target/wheels/
```

## Layout

```
crates/
├── llm-harness-types/      # core type definitions
├── llm-harness-loop/       # agent loop
├── llm-harness-rules/      # rules engine
├── llm-harness-agent/      # agent harness
├── llm-harness-runtime/    # runtime: workflow engine, executors, event stream
├── llm-harness-runtime-sandbox-*/  # sandbox backends (bwrap, seatbelt, os)
├── llm-harness-runtime-mcp/        # MCP integration
├── llm-harness-runtime-audit-jsonl/ # audit logging
├── llm-harness-runtime-trace-otel/  # OpenTelemetry tracing
└── llm-harness-py/         # PyO3 Python bindings (cdylib)
```

## Versioning

Workspace version is in the root `Cargo.toml` (`[workspace.package]`). Tag `v<version>` to trigger wheel builds and publish a release.
