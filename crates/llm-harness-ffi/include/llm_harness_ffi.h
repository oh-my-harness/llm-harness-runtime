#ifndef LLM_HARNESS_FFI_H
#define LLM_HARNESS_FFI_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32)
#if defined(LLM_HARNESS_FFI_BUILD)
#define LLM_HARNESS_FFI_API __declspec(dllexport)
#else
#define LLM_HARNESS_FFI_API __declspec(dllimport)
#endif
#else
#define LLM_HARNESS_FFI_API
#endif

#define LLM_HARNESS_FFI_ABI_VERSION 2u

#define LLM_HARNESS_FFI_OK 0
#define LLM_HARNESS_FFI_NO_EVENT 1
#define LLM_HARNESS_FFI_ERR_INVALID_ARG -1
#define LLM_HARNESS_FFI_ERR_INTERNAL -10
#define LLM_HARNESS_FFI_ERR_CALLBACK -11
#define LLM_HARNESS_FFI_ERR_BUSY -20
#define LLM_HARNESS_FFI_ERR_TIMEOUT -21

typedef struct ffi_harness_t ffi_harness_t;
typedef struct ffi_tool_ctx_t ffi_tool_ctx_t;

/*
 * Tool callback invoked by the native harness.
 *
 * args_json is a UTF-8 JSON object owned by the native library and valid only
 * for the duration of the callback. ctx_handle is valid only during the
 * callback. On success, set *result_json_out to a UTF-8 JSON string allocated
 * with ffi_harness_alloc_string and return LLM_HARNESS_FFI_OK.
 *
 * Result JSON shape:
 *   {
 *     "content": [{"type": "text", "text": "..."}],
 *     "details": {},
 *     "terminate": false
 *   }
 *
 * On callback failure, return LLM_HARNESS_FFI_ERR_CALLBACK and optionally set
 * *result_json_out to {"error":"..."} allocated with ffi_harness_alloc_string.
 */
typedef int (*ffi_tool_execute_fn)(
    const char *args_json,
    ffi_tool_ctx_t *ctx_handle,
    char **result_json_out);

/* Return the C ABI version. Clients should require
 * LLM_HARNESS_FFI_ABI_VERSION before calling other functions. */
LLM_HARNESS_FFI_API uint32_t ffi_harness_abi_version(void);

/* Return the native package version string. The returned pointer must be freed
 * with ffi_harness_free_string. Returns NULL only on allocation failure. */
LLM_HARNESS_FFI_API char *ffi_harness_version(void);

/* Return the callback error code for SDK bindings that cannot consume C
 * preprocessor macros directly. Equivalent to
 * LLM_HARNESS_FFI_ERR_CALLBACK. */
LLM_HARNESS_FFI_API int ffi_harness_error_code_callback(void);

/*
 * Create a harness from UTF-8 JSON config.
 *
 * Required config fields:
 *   provider: "openai", "openai-compatible", or "anthropic"
 *   model: provider model name
 *   api_key_env: recommended name of an environment variable containing key
 *
 * Optional fields include api_key, base_url, chat_path, messages_path,
 * work_dir, max_tokens, event_queue_capacity, and
 * event_forwarder_timeout_secs.
 *
 * Prefer api_key_env for customer delivery. Direct api_key is kept for local
 * compatibility only. On failure, returns NULL and writes an allocated error
 * JSON string to error_json_out when error_json_out is not NULL. Free that
 * string with ffi_harness_free_string.
 */
LLM_HARNESS_FFI_API ffi_harness_t *ffi_harness_new(
    const char *config_json,
    char **error_json_out);

/*
 * Abort any active prompt and wait for prompt worker shutdown.
 *
 * timeout_ms == 0 waits indefinitely. Non-zero timeout returns
 * LLM_HARNESS_FFI_ERR_TIMEOUT if the worker does not become idle in time.
 * This function does not free the handle; call ffi_harness_free exactly once
 * after close succeeds, or call ffi_harness_free directly for best-effort
 * cleanup.
 */
LLM_HARNESS_FFI_API int ffi_harness_close(
    ffi_harness_t *h,
    uint32_t timeout_ms,
    char **error_json_out);

/* Free a harness handle exactly once. This function performs best-effort
 * cancel + abort + bounded wait before dropping native state. For deterministic
 * error reporting, call ffi_harness_close before ffi_harness_free. */
LLM_HARNESS_FFI_API void ffi_harness_free(ffi_harness_t *h);

/* Free a string returned by this library. Passing NULL is allowed. */
LLM_HARNESS_FFI_API void ffi_harness_free_string(char *s);

/* Allocate a native-owned UTF-8 string. Intended for tool callbacks returning
 * result_json_out. The native library will take ownership of callback result
 * strings and free them after parsing. */
LLM_HARNESS_FFI_API char *ffi_harness_alloc_string(const char *src);

/* Return the last error JSON observed by a harness, or NULL if no error is
 * recorded. The returned pointer must be freed with ffi_harness_free_string. */
LLM_HARNESS_FFI_API char *ffi_harness_last_error(ffi_harness_t *h);

/*
 * Register a tool callback.
 *
 * tool_json is UTF-8 JSON:
 *   {
 *     "name": "tool_name",
 *     "description": "...",
 *     "parameters_schema": {"type":"object","properties":{}},
 *     "execution_mode": "parallel" | "sequential"
 *   }
 *
 * Keep the callback function valid until the harness is closed/freed. This
 * function may be called before prompts start. Updating tools while a prompt is
 * active is not part of the public contract.
 */
LLM_HARNESS_FFI_API int ffi_harness_register_tool(
    ffi_harness_t *h,
    const char *tool_json,
    ffi_tool_execute_fn callback,
    char **error_json_out);

/*
 * Start a prompt asynchronously. Only one prompt may be active per harness.
 * Returns LLM_HARNESS_FFI_OK after the worker is started. Use
 * ffi_harness_wait_event to consume streaming events until a terminal event
 * such as "settled", "aborted", or "error".
 */
LLM_HARNESS_FFI_API int ffi_harness_prompt(
    ffi_harness_t *h,
    const char *text,
    char **error_json_out);

/* Request cancellation of the active prompt. The handle remains valid. */
LLM_HARNESS_FFI_API void ffi_harness_abort(ffi_harness_t *h);

/*
 * Wait for the next event.
 *
 * timeout_ms == 0 waits indefinitely. If no event is available before a
 * non-zero timeout expires, returns LLM_HARNESS_FFI_NO_EVENT. On success,
 * writes an allocated UTF-8 JSON event string to event_json_out; free it with
 * ffi_harness_free_string.
 */
LLM_HARNESS_FFI_API int ffi_harness_wait_event(
    ffi_harness_t *h,
    uint32_t timeout_ms,
    char **event_json_out,
    char **error_json_out);

/* Return non-zero if the current tool callback context has been cancelled. */
LLM_HARNESS_FFI_API int ffi_tool_is_cancelled(ffi_tool_ctx_t *ctx);

/* Send an intermediate tool result from inside a tool callback. result_json has
 * the same shape as callback result JSON. The function copies/parses the input;
 * the caller keeps ownership of result_json. */
LLM_HARNESS_FFI_API int ffi_tool_send_update(
    ffi_tool_ctx_t *ctx,
    const char *result_json);

#ifdef __cplusplus
}
#endif

#endif
