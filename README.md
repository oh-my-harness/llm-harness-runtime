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

This is a **private** repo — `pip --find-links <github-url>` cannot fetch release assets without authentication. Download wheels with `gh` instead:

```bash
gh release download v0.2.0 -R oh-my-harness/llm-harness-runtime --dir wheels --pattern '*.whl'
pip install llm_harness_py --find-links wheels
```

### Rust crates

Downstream Rust projects consume workspace crates via git rev:

```toml
llm-harness-runtime = { git = "https://github.com/oh-my-harness/llm-harness-runtime", rev = "<commit-sha>" }
```

## Building wheels

Use the build script (requires Rust, maturin, Python 3.12, and `gh` authenticated):

```bash
./scripts/build-wheels.sh 0.2.0            # build macOS wheels + upload to release
./scripts/build-wheels.sh 0.2.0 --build-only  # build only, skip upload
```

macOS wheels (Apple Silicon + Intel) are cross-compiled on an Apple Silicon machine. Linux and Windows wheels must be built on their respective platforms — run `maturin build --release` in `crates/llm-harness-py/` and upload with `gh release upload`.

### Manual build

```bash
cd crates/llm-harness-py
maturin build --release --target aarch64-apple-darwin --interpreter python3.12
maturin develop --release     # install into active venv for local testing
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

Workspace version is in the root `Cargo.toml` (`[workspace.package]`). Tag `v<version>` for releases; run `./scripts/build-wheels.sh <version>` to build and upload wheels.
