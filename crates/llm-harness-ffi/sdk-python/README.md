# llm-harness Python SDK

Python bindings for the llm-harness native FFI runtime.

## Layout

- `llm_harness_sdk/`: source package.
- `llm_harness_sdk/native/`: optional packaging location for the compiled native
  library.
- `tests/`: source-level tests for the Python wrapper.

The package looks for the native library in this order:

1. `llm_harness_sdk/native/` inside the installed package.
2. `target/release/` in a development checkout.

Native library names:

- Windows: `llm_harness_ffi.dll`
- macOS: `libllm_harness_ffi.dylib`
- Linux: `libllm_harness_ffi.so`

## Build

```powershell
python -m pip install build
python -m build
```

When building a customer wheel, copy the compiled native library into
`llm_harness_sdk/native/` before running `python -m build`.
