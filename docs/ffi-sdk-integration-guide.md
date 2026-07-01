# FFI SDK Integration Guide

This guide is for customers integrating the binary SDK without receiving the
runtime source code.

## Supported Integration Paths

- Python: use the `llm_harness_sdk` wheel.
- C ABI: include `llm_harness_ffi.h` and link/load the platform native library.

For customer delivery, prefer the Python SDK unless the host application cannot
embed Python.

## Configuration

Use environment variables for provider credentials:

```powershell
$env:OPENAI_API_KEY = "<redacted>"
```

Then pass the environment variable name:

```python
Harness(
    provider="openai",
    model="gpt-4o-mini",
    api_key_env="OPENAI_API_KEY",
)
```

Do not put API keys in source code, command history, logs, or committed config
files. The direct `api_key` field remains for local compatibility only.

OpenAI-compatible gateways can be configured with `provider`,
`base_url`, and `model`:

```python
Harness(
    provider="openai-compatible",
    model="Pro/MiniMaxAI/MiniMax-M2.5",
    api_key_env="LATTICE_API_KEY",
    base_url="https://api.siliconflow.cn/v1",
)
```

If `base_url` ends with `/v1`, the SDK uses `/chat/completions` automatically.

## Minimal Python Usage

```python
from llm_harness_sdk import Harness, native_abi_version

assert native_abi_version() == 2

with Harness(
    provider="openai-compatible",
    model="Pro/MiniMaxAI/MiniMax-M2.5",
    api_key_env="LATTICE_API_KEY",
    base_url="https://api.siliconflow.cn/v1",
) as harness:
    harness.prompt("Reply with one short sentence.")
    response = harness.get_final_response(timeout=120)
    print(response["text"])
```

## Tool Callback Example

```python
from llm_harness_sdk import Harness

with Harness(
    provider="openai-compatible",
    model="Pro/MiniMaxAI/MiniMax-M2.5",
    api_key_env="LATTICE_API_KEY",
    base_url="https://api.siliconflow.cn/v1",
) as harness:

    @harness.tool(
        "echo",
        description="Echo text back to the model.",
        parameters_schema={
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    )
    def echo(args, ctx):
        if ctx.is_cancelled():
            return {"content": [], "details": {"cancelled": True}, "terminate": False}

        ctx.send_update({
            "content": [{"type": "text", "text": "echo started"}],
            "details": {},
            "terminate": False,
        })
        return {
            "content": [{"type": "text", "text": args["text"]}],
            "details": {},
            "terminate": False,
        }

    harness.prompt("Use the echo tool with text hello.")
    print(harness.get_final_response(timeout=120)["text"])
```

Tool callbacks may be normal functions or async functions. Async callbacks run
on a per-`Harness` event loop thread and are stopped when `close()` succeeds.

## Lifecycle Rules

- Create one `Harness` per independent session.
- Only one active prompt is allowed per `Harness`.
- Use `collect_until_settled()` or `get_final_response()` for normal completion.
- Use `abort()` when the application wants to cancel an active prompt.
- Always call `close()` or use `with Harness(...)` so native workers and Python
  callback objects are released.
- `close()` aborts any active prompt and waits for native worker shutdown.

Recommended pattern:

```python
with Harness(**config) as harness:
    harness.prompt("...")
    events = harness.collect_until_settled(timeout=120)
```

## Events

`wait_event()` returns one JSON event or `None` on timeout. The common event
types are:

- `prompt_start`: prompt accepted by the SDK.
- `text_delta`: streamed text chunk.
- `tool_start`: tool execution started.
- `tool_update`: tool sent intermediate output.
- `tool_end`: tool execution completed.
- `message_end`: final assistant message.
- `settled`: prompt reached terminal state.
- `aborted`: prompt was cancelled by close/abort.
- `error`: native or provider error.
- `warning`: non-fatal SDK warning, such as event queue overflow.

Terminal events are `settled`, `aborted`, and `error`. A single prompt emits at
most one terminal event. `collect_until_settled()` keeps its historical name, but
it returns after the requested number of terminal events.

`get_final_response()` aggregates events into:

- `text`
- `message`
- `usage`
- `tool_calls`
- `errors`
- `events`

## Production Config Fields

- `event_queue_capacity`: max pending events. Default: `10000`.
- `event_forwarder_timeout_secs`: max Rust-side wait for terminal events.
  Default: `300`; set `0` to disable.
- `callback_timeout`: Python-side async tool callback timeout. Default: `300`.
- `max_tokens`: provider max output tokens.
- `work_dir`: working directory for runtime filesystem operations.

## Error Handling

Python raises `HarnessError` for native failures. The error object may contain:

- `code`: numeric FFI error code.
- `kind`: stable error category.
- `payload`: full native error JSON.

Common error codes:

- `-1`: invalid argument or invalid JSON.
- `-10`: internal runtime error.
- `-11`: tool callback error.
- `-20`: prompt already running on the same harness.
- `-21`: close or wait timeout.

Error messages redact common `sk-...` and `Bearer ...` secrets, but customer
code should still avoid logging raw provider config.

## C ABI Notes

- Every `char *` returned by the native library must be released with
  `ffi_harness_free_string`.
- Every `ffi_harness_t *` returned by `ffi_harness_new` must be released with
  `ffi_harness_free`.
- For deterministic errors, call `ffi_harness_close` before
  `ffi_harness_free`.
- Tool callbacks must allocate `result_json_out` with
  `ffi_harness_alloc_string`.
- Check `ffi_harness_abi_version()` before using the library.

See `crates/llm-harness-ffi/include/llm_harness_ffi.h` for the full C ABI
contract.
