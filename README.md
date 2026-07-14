# llm-harness-runtime

Rust implementation of the oh-my-harness LLM agent orchestration runtime: workflow engine, agent harness, tools, event stream, and sandboxing.

> **Private repo.** Source is not distributed. Public consumers receive only compiled artifacts (Python wheels, git rev crates for Rust downstreams).

---

## For Python developers — building agents with `llm_harness_py`

The runtime exposes a PyO3 module `llm_harness_py` with everything you need to build LLM agents: harness, tools, hooks, plugins, providers, and a workflow engine.

### Install

```bash
python3 -m venv .venv && source .venv/bin/activate
pip install llm_harness_py \
  --find-links https://github.com/oh-my-harness/llm-harness-py-wheels/releases/expanded_assets/v0.2.0
```

> Wheels are hosted in the public [llm-harness-py-wheels](https://github.com/oh-my-harness/llm-harness-py-wheels) repo (compiled binaries only, no source code). CPython 3.9+.

### Quickstart: a single-turn agent with a custom tool

```python
import llm_harness_py as lh

# 1. Create an LLM provider (OpenAI-compatible)
provider = lh.create_openai_provider(
    api_key="sk-...",
    base_url="https://api.openai.com",  # or any OpenAI-compatible endpoint
)

# 2. Define a tool (JSON schema + Python callback)
weather_tool = lh.create_tool(
    name="get_weather",
    description="Get the current weather for a city.",
    parameters_schema='{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}',
    callback=lambda args: f"Weather in {args['city']}: sunny, 22°C",
)

# 3. Build the harness
harness = (
    lh.HarnessBuilder("gpt-4o")
        .system_prompt("You are a helpful assistant. Use tools when appropriate.")
        .provider("*", provider)
        .tool(weather_tool)
        .build()
)

# 4. Prompt and stream events
harness.prompt("What's the weather in Tokyo?")
for event in harness.events():
    print(event)
```

### Key API surface

| Concept | Factory / Class | Purpose |
|---------|----------------|---------|
| LLM provider | `lh.create_openai_provider(api_key, base_url=...)` | OpenAI-compatible HTTP client |
| LLM provider | `lh.create_anthropic_provider(api_key, base_url=...)` | Anthropic-compatible HTTP client |
| Tool | `lh.create_tool(name, description, parameters_schema, callback)` | Custom function the LLM can call (sync or `async def`) |
| Agent harness | `lh.HarnessBuilder(model).provider(...).tool(...).build()` | Fluent builder for a single-agent loop |
| Run | `harness.prompt(text)` / `harness.events()` | Send a prompt, stream events |
| Hook | `lh.create_before_run_hook(callback)`, `create_should_stop_hook(callback)`, ... | Intercept the agent loop at various stages |
| Plugin | `lh.create_plugin(name, tools=[...], hooks=[...])` | Bundle tools + hooks into a reusable unit |
| Event channel | `lh.create_event_channel(task_id)` | For human-in-the-loop / external event injection |
| Workflow engine | `lh.WorkflowEngine(workflow, provider, model, judge)` | Multi-step graph orchestration |
| Judge | `lh.create_judge(callback)` | Conditional routing between workflow steps |
| Executor | `lh.create_executor(callback)` | Non-LLM step in a workflow (e.g. render, wait) |

### Real-world example

See [blender-scene-generator](https://github.com/oh-my-harness/blender-scene-generator) — a full app built on this SDK: 4-step workflow (planner → builder → reviewer → renderer), custom Blender tools, human-in-the-loop review via event channels, and live WebSocket event streaming.

### Local development (against runtime source)

If you need to modify the Rust runtime itself:

```bash
cd crates/llm-harness-py
python3 -m venv .venv && source .venv/bin/activate
pip install maturin
maturin develop --release    # builds and installs into the active venv
```

Changes to Rust source take effect after re-running `maturin develop --release`.

---

## For Rust developers — consuming workspace crates

Downstream Rust projects consume workspace crates via git rev:

```toml
llm-harness-runtime = { git = "https://github.com/oh-my-harness/llm-harness-runtime", rev = "<commit-sha>" }
```

---

## Building wheels (maintainers)

Use the build scripts (requires Rust, maturin, Python 3.12, and `gh` authenticated with write access to the public wheels repo):

```bash
./scripts/build-wheels.sh 0.2.0               # build macOS wheels + upload to wheels repo
./scripts/build-wheels.sh 0.2.0 --build-only  # build only, skip upload
powershell -ExecutionPolicy Bypass -File ./scripts/build-windows-wheel.ps1 -Version 0.2.0
```

Wheels are uploaded to the public [llm-harness-py-wheels](https://github.com/oh-my-harness/llm-harness-py-wheels) repo's GitHub Release.

### Platform support

| Platform | Wheel tag | Status |
|----------|-----------|--------|
| macOS Apple Silicon | `cp39-abi3-macosx_11_0_arm64` | ✅ |
| macOS Intel | `cp39-abi3-macosx_10_12_x86_64` | ✅ |
| Linux x64 | `cp39-abi3-manylinux_2_17_x86_64` | ✅ |
| Windows x64 | `cp39-abi3-win_amd64` | ✅ |

Wheels are built with the abi3 (stable ABI) feature — one wheel covers CPython 3.9–3.13+. macOS wheels are cross-compiled on an Apple Silicon machine. Linux x64 wheels are built via Docker manylinux container. Windows x64 wheels are built natively on Windows with `scripts/build-windows-wheel.ps1`.

### Manual build

```bash
cd crates/llm-harness-py
maturin build --release --target aarch64-apple-darwin --interpreter python3
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
├── llm-harness-runtime-trace-otel/  # OpenTelemetry tracing
└── llm-harness-py/         # PyO3 Python bindings (cdylib)
```

## Versioning

Workspace version is in the root `Cargo.toml` (`[workspace.package]`). To release a new version: bump the version, run `./scripts/build-wheels.sh <version>` on macOS/Linux for macOS + Linux wheels, then run `scripts/build-windows-wheel.ps1 -Version <version>` on Windows for the Windows wheel.
