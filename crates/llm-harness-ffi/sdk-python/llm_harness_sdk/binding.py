import asyncio
import inspect
import json
import platform
import threading
import time
from pathlib import Path

from cffi import FFI

from .errors import HarnessError


EXPECTED_ABI_VERSION = 2
TERMINAL_EVENTS = {"settled", "aborted", "error"}

ffi = FFI()
ffi.cdef(
    """
    typedef struct ffi_harness_t ffi_harness_t;
    typedef struct ffi_tool_ctx_t ffi_tool_ctx_t;

    typedef int (*ffi_tool_execute_fn)(
        const char* args_json,
        ffi_tool_ctx_t* ctx_handle,
        char** result_json_out
    );

    unsigned int ffi_harness_abi_version(void);
    char* ffi_harness_version(void);
    int ffi_harness_error_code_callback(void);

    ffi_harness_t* ffi_harness_new(const char* config_json, char** error_json_out);
    void ffi_harness_free(ffi_harness_t* h);
    void ffi_harness_free_string(char* s);
    char* ffi_harness_alloc_string(const char* src);
    char* ffi_harness_last_error(ffi_harness_t* h);

    int ffi_harness_register_tool(
        ffi_harness_t* h,
        const char* tool_json,
        ffi_tool_execute_fn callback,
        char** error_json_out
    );

    int ffi_harness_prompt(ffi_harness_t* h, const char* text, char** error_json_out);
    void ffi_harness_abort(ffi_harness_t* h);
    int ffi_harness_close(
        ffi_harness_t* h,
        unsigned int timeout_ms,
        char** error_json_out
    );

    int ffi_harness_wait_event(
        ffi_harness_t* h,
        unsigned int timeout_ms,
        char** event_json_out,
        char** error_json_out
    );

    int ffi_tool_is_cancelled(ffi_tool_ctx_t* ctx);
    int ffi_tool_send_update(ffi_tool_ctx_t* ctx, const char* result_json);
    """
)


def _native_library_name():
    system = platform.system()
    if system == "Windows":
        return "llm_harness_ffi.dll"
    if system == "Darwin":
        return "libllm_harness_ffi.dylib"
    return "libllm_harness_ffi.so"


def _load_lib():
    package_dir = Path(__file__).resolve().parent
    packaged = package_dir / "native" / _native_library_name()
    if packaged.exists():
        return ffi.dlopen(str(packaged))

    try:
        repo_root = package_dir.parents[3]
        dev = repo_root / "target" / "release" / _native_library_name()
        if dev.exists():
            return ffi.dlopen(str(dev))
    except IndexError:
        pass

    raise RuntimeError(
        "native library not found; run "
        "build the llm-harness FFI native library and place it in "
        "`llm_harness_sdk/native/` or `target/release/`"
    )


lib = _load_lib()


def _decode_and_free(ptr):
    if ptr == ffi.NULL:
        return None
    try:
        return ffi.string(ptr).decode("utf-8")
    finally:
        lib.ffi_harness_free_string(ptr)


def _raise_from_ptr(ptr):
    raise HarnessError.from_error_json(_decode_and_free(ptr))


def native_abi_version():
    return int(lib.ffi_harness_abi_version())


def native_version():
    return _decode_and_free(lib.ffi_harness_version())


def _validate_native_abi():
    actual = native_abi_version()
    if actual != EXPECTED_ABI_VERSION:
        raise RuntimeError(
            "unsupported llm-harness FFI ABI version "
            f"{actual}; expected {EXPECTED_ABI_VERSION}"
        )


def _check_rc(rc, err_out):
    if rc < 0:
        _raise_from_ptr(err_out[0])


def _timeout_ms(timeout):
    if timeout is None:
        return 0
    if timeout < 0:
        raise ValueError("timeout must be non-negative or None")
    return max(1, int(timeout * 1000))


_validate_native_abi()
CALLBACK_ERROR_CODE = int(lib.ffi_harness_error_code_callback())


class _AsyncCallbackRunner:
    def __init__(self, timeout=300):
        self._timeout = timeout
        self._loop = None
        self._thread = None
        self._lock = threading.Lock()
        self._closed = False

    def _start_locked(self):
        if self._closed:
            raise HarnessError("async callback runner is closed")
        if self._loop is not None:
            return

        loop = asyncio.new_event_loop()

        def run_loop():
            asyncio.set_event_loop(loop)
            try:
                loop.run_forever()
            finally:
                pending = asyncio.all_tasks(loop)
                for task in pending:
                    task.cancel()
                if pending:
                    loop.run_until_complete(
                        asyncio.gather(*pending, return_exceptions=True)
                    )
                loop.run_until_complete(loop.shutdown_asyncgens())
                loop.close()

        thread = threading.Thread(target=run_loop, daemon=True)
        thread.start()
        self._loop = loop
        self._thread = thread

    def run(self, coro):
        with self._lock:
            self._start_locked()
            loop = self._loop

        future = asyncio.run_coroutine_threadsafe(coro, loop)
        return future.result(timeout=self._timeout)

    def close(self, timeout=1.0):
        with self._lock:
            if self._closed:
                return
            self._closed = True
            loop = self._loop
            thread = self._thread
            self._loop = None
            self._thread = None

        if loop is not None and thread is not None:
            loop.call_soon_threadsafe(loop.stop)
            if threading.current_thread() is not thread:
                thread.join(timeout=timeout)

    def is_alive(self):
        thread = self._thread
        return bool(thread and thread.is_alive())


def _aggregate_final_response(events):
    text_parts = []
    final_text = None
    message = None
    usage = None
    errors = []
    progress = []
    tool_calls = []
    tool_calls_by_id = {}

    for event in events:
        event_type = event.get("type")
        if event_type == "text_delta":
            text_parts.append(event.get("text", ""))
        elif event_type == "progress":
            progress.append(event.get("text", ""))
        elif event_type == "final_answer":
            final_text = event.get("text", "")
        elif event_type == "message_end":
            message = event.get("message")
            if isinstance(message, dict):
                usage = message.get("usage")
        elif event_type == "tool_start":
            tool_call = {
                "id": event.get("tool_call_id"),
                "name": event.get("tool_name"),
                "args": event.get("args"),
                "result": None,
            }
            tool_calls.append(tool_call)
            tool_calls_by_id[tool_call["id"]] = tool_call
        elif event_type == "tool_end":
            tool_call_id = event.get("tool_call_id")
            tool_call = tool_calls_by_id.get(tool_call_id)
            if tool_call is None:
                tool_call = {
                    "id": tool_call_id,
                    "name": event.get("tool_name"),
                    "args": None,
                    "result": None,
                }
                tool_calls.append(tool_call)
                tool_calls_by_id[tool_call_id] = tool_call
            tool_call["result"] = event.get("result")
        elif event_type == "error":
            errors.append(event)

    text = final_text if final_text is not None else "".join(text_parts)
    if not text and isinstance(message, dict):
        for block in message.get("content", []):
            if isinstance(block, dict) and block.get("type") == "text":
                text += block.get("text", "")

    return {
        "text": text,
        "message": message,
        "usage": usage,
        "tool_calls": tool_calls,
        "errors": errors,
        "progress": progress,
        "events": events,
    }


class ToolContext:
    def __init__(self, raw):
        self._raw = raw

    def is_cancelled(self):
        return bool(lib.ffi_tool_is_cancelled(self._raw))

    def send_update(self, result):
        payload = json.dumps(result).encode("utf-8")
        rc = lib.ffi_tool_send_update(self._raw, payload)
        if rc < 0:
            raise HarnessError(f"ffi_tool_send_update failed: {rc}")


class Harness:
    def __init__(self, **config):
        self._callbacks = []
        self._handle = ffi.NULL
        self._async_runner = _AsyncCallbackRunner(
            timeout=config.pop("callback_timeout", 300)
        )
        err = ffi.new("char**")
        payload = json.dumps(config).encode("utf-8")
        self._handle = lib.ffi_harness_new(payload, err)
        if self._handle == ffi.NULL:
            self._async_runner.close()
            _raise_from_ptr(err[0])

    def _require_open(self):
        if getattr(self, "_handle", ffi.NULL) == ffi.NULL:
            raise HarnessError("harness is closed")
        return self._handle

    def _close(self, timeout=30.0, raise_errors=True):
        if getattr(self, "_handle", ffi.NULL) != ffi.NULL:
            err = ffi.new("char**")
            rc = lib.ffi_harness_close(self._handle, _timeout_ms(timeout), err)
            if rc < 0:
                if raise_errors:
                    _raise_from_ptr(err[0])
                _decode_and_free(err[0])
                return False

            lib.ffi_harness_free(self._handle)
            self._handle = ffi.NULL
            self._callbacks.clear()
            self._async_runner.close()
        return True

    def close(self, timeout=30.0):
        self._close(timeout=timeout, raise_errors=True)

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        self.close()

    def __del__(self):
        try:
            self._close(timeout=1.0, raise_errors=False)
        except Exception:
            pass

    def tool(
        self,
        name,
        *,
        description="",
        parameters_schema=None,
        execution_mode="parallel",
    ):
        def decorator(fn):
            handle = self._require_open()
            callback = self._build_callback(fn)
            tool_config = {
                "name": name,
                "description": description,
                "parameters_schema": parameters_schema
                or {"type": "object", "properties": {}},
                "execution_mode": execution_mode,
            }
            err = ffi.new("char**")
            rc = lib.ffi_harness_register_tool(
                handle,
                json.dumps(tool_config).encode("utf-8"),
                callback,
                err,
            )
            _check_rc(rc, err)
            self._callbacks.append(callback)
            return fn

        return decorator

    def _build_callback(self, fn):
        @ffi.callback("int(const char*, ffi_tool_ctx_t*, char**)")
        def callback(args_json, ctx_handle, result_json_out):
            try:
                args = json.loads(ffi.string(args_json).decode("utf-8"))
                ctx = ToolContext(ctx_handle)
                result = fn(args, ctx) if _accepts_ctx(fn) else fn(args)

                if inspect.iscoroutine(result):
                    result = self._async_runner.run(result)

                payload = json.dumps(result).encode("utf-8")
                result_json_out[0] = lib.ffi_harness_alloc_string(payload)
                if result_json_out[0] == ffi.NULL:
                    return CALLBACK_ERROR_CODE
                return 0
            except Exception as exc:
                payload = json.dumps({"error": str(exc)}).encode("utf-8")
                result_json_out[0] = lib.ffi_harness_alloc_string(payload)
                return CALLBACK_ERROR_CODE

        return callback

    def prompt(self, text):
        handle = self._require_open()
        err = ffi.new("char**")
        rc = lib.ffi_harness_prompt(handle, text.encode("utf-8"), err)
        _check_rc(rc, err)

    def abort(self):
        lib.ffi_harness_abort(self._require_open())

    def wait_event(self, timeout=1.0):
        handle = self._require_open()
        event = ffi.new("char**")
        err = ffi.new("char**")
        rc = lib.ffi_harness_wait_event(
            handle, int(timeout * 1000), event, err
        )
        if rc == 1:
            return None
        _check_rc(rc, err)
        text = _decode_and_free(event[0])
        return json.loads(text)

    def collect_until_settled(self, expected=1, timeout=300.0):
        events = []
        terminal = 0
        deadline = time.monotonic() + timeout
        while terminal < expected:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"timed out waiting for {expected} terminal events")
            event = self.wait_event(timeout=min(remaining, 0.5))
            if event is None:
                continue
            events.append(event)
            if event.get("type") in TERMINAL_EVENTS:
                terminal += 1
        return events

    @staticmethod
    def aggregate_events(events):
        return _aggregate_final_response(events)

    def get_final_response(self, expected=1, timeout=300.0):
        return self.aggregate_events(
            self.collect_until_settled(expected=expected, timeout=timeout)
        )


def _accepts_ctx(fn):
    try:
        return len(inspect.signature(fn).parameters) >= 2
    except (TypeError, ValueError):
        return False


# ── WorkflowEngine FFI wrapper ─────────────────────────────────────────────

ffi.cdef(
    """
    typedef struct ffi_workflow_t ffi_workflow_t;

    typedef int (*ffi_workflow_executor_fn)(
        const char* step_json,
        const char* context_json,
        const char* prev_result_json,
        char** result_json_out,
        char** error_json_out
    );

    typedef int (*ffi_workflow_judge_fn)(
        const char* step_id,
        const char* step_result_json,
        const char* context_json,
        char** transition_json_out,
        char** error_json_out
    );

    ffi_workflow_t* ffi_workflow_new(
        const char* workflow_json,
        const char* config_json,
        ffi_workflow_executor_fn executor_callback,
        ffi_workflow_judge_fn judge_callback,
        char** error_json_out
    );

    ffi_workflow_t* ffi_workflow_restore(
        const char* task_id,
        const char* config_json,
        ffi_workflow_judge_fn judge_callback,
        char** error_json_out
    );

    int ffi_workflow_register_executor(
        ffi_workflow_t* wf,
        const char* executor_name,
        ffi_workflow_executor_fn executor_callback,
        char** error_json_out
    );

    int ffi_workflow_run(
        ffi_workflow_t* wf,
        char** result_json_out,
        char** error_json_out
    );

    int ffi_workflow_state(
        ffi_workflow_t* wf,
        char** state_json_out,
        char** error_json_out
    );

    int ffi_workflow_get_var(
        ffi_workflow_t* wf,
        const char* key,
        char** value_json_out,
        char** error_json_out
    );

    void ffi_workflow_free(ffi_workflow_t* wf);
    """
)


class WorkflowEngine:
    """Rust runtime WorkflowEngine via FFI.

    Wraps the native WorkflowEngine state machine. Executor and judge
    are Python callables bridged to Rust via C callbacks.

    Usage:
        engine = WorkflowEngine(workflow, config, executor_fn, judge_fn)
        result = engine.run()
    """

    def __init__(
        self,
        workflow: dict,
        config: dict,
        executor_fn,
        judge_fn,
    ):
        self._executor_callback = self._build_executor_callback(executor_fn)
        self._judge_callback = self._build_judge_callback(judge_fn)
        self._handle = ffi.NULL

        workflow_json = json.dumps(workflow).encode("utf-8")
        config_json = json.dumps(config).encode("utf-8")
        err = ffi.new("char**")
        self._handle = lib.ffi_workflow_new(
            workflow_json,
            config_json,
            self._executor_callback,
            self._judge_callback,
            err,
        )
        if self._handle == ffi.NULL:
            _raise_from_ptr(err[0])

    def _build_executor_callback(self, fn):
        @ffi.callback(
            "int(const char*, const char*, const char*, char**, char**)"
        )
        def callback(step_json, context_json, prev_result_json, result_out, error_out):
            try:
                step = json.loads(ffi.string(step_json).decode("utf-8"))
                context = json.loads(ffi.string(context_json).decode("utf-8"))
                prev = None
                if prev_result_json != ffi.NULL:
                    prev = json.loads(ffi.string(prev_result_json).decode("utf-8"))

                result = fn(step, context, prev)

                payload = json.dumps(result).encode("utf-8")
                result_out[0] = lib.ffi_harness_alloc_string(payload)
                if result_out[0] == ffi.NULL:
                    return CALLBACK_ERROR_CODE
                return 0
            except Exception as exc:
                payload = json.dumps({"error": str(exc)}).encode("utf-8")
                error_out[0] = lib.ffi_harness_alloc_string(payload)
                return CALLBACK_ERROR_CODE

        return callback

    def _build_judge_callback(self, fn):
        @ffi.callback(
            "int(const char*, const char*, const char*, char**, char**)"
        )
        def callback(step_id, step_result_json, context_json, transition_out, error_out):
            try:
                sid = ffi.string(step_id).decode("utf-8")
                result = json.loads(ffi.string(step_result_json).decode("utf-8"))
                context = json.loads(ffi.string(context_json).decode("utf-8"))

                transition = fn(sid, result, context)

                payload = json.dumps(transition).encode("utf-8")
                transition_out[0] = lib.ffi_harness_alloc_string(payload)
                if transition_out[0] == ffi.NULL:
                    return CALLBACK_ERROR_CODE
                return 0
            except Exception as exc:
                payload = json.dumps({"error": str(exc)}).encode("utf-8")
                error_out[0] = lib.ffi_harness_alloc_string(payload)
                return CALLBACK_ERROR_CODE

        return callback

    def register_executor(self, name, executor_fn):
        handle = self._require_open()
        callback = self._build_executor_callback(executor_fn)
        err = ffi.new("char**")
        rc = lib.ffi_workflow_register_executor(
            handle,
            name.encode("utf-8"),
            callback,
            err,
        )
        _check_rc(rc, err)
        self._executor_callbacks.append(callback)

    def run(self):
        handle = self._require_open()
        result = ffi.new("char**")
        err = ffi.new("char**")
        rc = lib.ffi_workflow_run(handle, result, err)
        _check_rc(rc, err)
        text = _decode_and_free(result[0])
        return json.loads(text) if text else None

    def state(self):
        handle = self._require_open()
        state = ffi.new("char**")
        err = ffi.new("char**")
        rc = lib.ffi_workflow_state(handle, state, err)
        _check_rc(rc, err)
        text = _decode_and_free(state[0])
        return json.loads(text) if text else None

    def get_var(self, key):
        handle = self._require_open()
        value = ffi.new("char**")
        err = ffi.new("char**")
        rc = lib.ffi_workflow_get_var(handle, key.encode("utf-8"), value, err)
        _check_rc(rc, err)
        text = _decode_and_free(value[0])
        return json.loads(text) if text else None

    def _require_open(self):
        if getattr(self, "_handle", ffi.NULL) == ffi.NULL:
            raise HarnessError("workflow engine is closed")
        return self._handle

    def close(self):
        if getattr(self, "_handle", ffi.NULL) != ffi.NULL:
            lib.ffi_workflow_free(self._handle)
            self._handle = ffi.NULL
            self._executor_callback = None
            self._judge_callback = None

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        self.close()

    def __del__(self):
        try:
            self.close()
        except Exception:
            pass


# Storage for executor callbacks to prevent GC
WorkflowEngine._executor_callbacks = []
