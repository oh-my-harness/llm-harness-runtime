# llm-harness Python SDK

Python wrapper for the production `llm-harness-ffi` native runtime.

Build the native library first:

```powershell
$env:CARGO_NET_GIT_FETCH_WITH_CLI = "true"
cargo build -p llm-harness-ffi --release
```

Build a platform wheel:

```powershell
cd crates\llm-harness-ffi\sdk-python
python -m pip install build
python -m build --wheel --no-isolation
```

Smoke test the installed wheel from the repository root:

```powershell
$wheel = Get-ChildItem crates\llm-harness-ffi\sdk-python\dist\*.whl | Select-Object -First 1
python scripts\smoke_ffi_sdk_wheel.py $wheel.FullName --expected-abi 2
```

The wheel contains:

- `llm_harness_sdk` Python package
- current-platform native library under `llm_harness_sdk/native/`

Minimal usage:

```python
from llm_harness_sdk import Harness, native_abi_version

assert native_abi_version() == 2
with Harness(
    provider="openai",
    model="gpt-4o-mini",
    api_key_env="OPENAI_API_KEY",
) as harness:

    @harness.tool(
        "echo",
        description="Echo text",
        parameters_schema={
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    )
    def echo(args, ctx):
        return {
            "content": [{"type": "text", "text": args["text"]}],
            "details": {},
            "terminate": False,
        }

    harness.prompt("Use the echo tool with text hello")
    response = harness.get_final_response()
    print(response["text"])
```

Use `api_key_env` for customer delivery. Avoid embedding API keys in source.

OpenAI-compatible gateways are configured with `provider="openai-compatible"`
and `base_url`. If `base_url` already ends with `/v1`, for example
`https://api.siliconflow.cn/v1`, the SDK uses `/chat/completions` as the
request path automatically.

`Harness.close(timeout=30.0)` aborts any active prompt and waits for the
native worker to stop before releasing Python callback objects. Use
`collect_until_settled()` first if the business flow needs to finish naturally
instead of being aborted by close.

Useful production config fields:

- `event_queue_capacity`: max pending SDK events before oldest events are dropped and a warning is emitted. Default: `10000`.
- `event_forwarder_timeout_secs`: max time Rust waits for terminal runtime events after a prompt. Default: `300`; set `0` to disable the Rust-side timeout.
- `callback_timeout`: max seconds the Python SDK waits for an async tool callback. Default: `300`. This is a Python-side option and is not sent to native config.

`get_final_response()` waits for `settled` and returns a compact response dict:
`text`, `message`, `usage`, `tool_calls`, `errors`, and raw `events`.
`Harness.aggregate_events(events)` provides the same aggregation for an event
list you collected yourself.

Async tool callbacks run on a per-`Harness` event loop thread and are stopped
when `close()` succeeds. Avoid calling `harness.prompt()` from inside a tool
callback; communicate follow-up work through the event stream or your own
application queue.

Customer-facing documentation:

- `docs/ffi-sdk-integration-guide.md`
