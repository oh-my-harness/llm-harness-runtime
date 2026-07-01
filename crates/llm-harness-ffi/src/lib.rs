#![allow(clippy::not_unsafe_ptr_arg_deref)]
// C ABI entrypoints accept raw pointers by design. Each exported function
// validates null pointers, catches panics, and reports failures via error codes
// or FfiError JSON instead of unwinding across the FFI boundary.

use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use llm_harness_agent::{AgentHarness, AgentHarnessEvent};
use llm_harness_loop::{AnthropicProvider, LlmClient, OpenAIProvider};
use llm_harness_runtime::builder::HarnessBuilder;
use llm_harness_runtime_sandbox_os::OsEnv;
use llm_harness_types::{
    AgentEvent, ContentBlock, Tool, ToolContext, ToolError, ToolExecutionMode, ToolResult,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroize;

const FFI_OK: c_int = 0;
const FFI_NO_EVENT: c_int = 1;
const FFI_ERR_INVALID_ARG: c_int = -1;
const FFI_ERR_CALLBACK: c_int = -11;
const FFI_ERR_BUSY: c_int = -20;
const FFI_ERR_TIMEOUT: c_int = -21;
const FFI_ERR_INTERNAL: c_int = -10;
const FFI_ABI_VERSION: u32 = 2;
const DEFAULT_EVENT_QUEUE_CAPACITY: usize = 10_000;
const DEFAULT_EVENT_FORWARDER_TIMEOUT_SECS: u64 = 300;
const DEFAULT_FREE_WAIT_TIMEOUT_MS: u32 = 30_000;

type ToolExecuteFn = unsafe extern "C" fn(
    args_json: *const c_char,
    ctx_handle: *mut ffi_tool_ctx_t,
    result_json_out: *mut *mut c_char,
) -> c_int;

#[derive(Debug, Clone, Serialize)]
struct FfiError {
    code: c_int,
    kind: &'static str,
    message: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct HarnessConfig {
    provider: String,
    model: String,
    api_key: Option<String>,
    api_key_env: Option<String>,
    work_dir: Option<PathBuf>,
    max_tokens: Option<u32>,
    base_url: Option<String>,
    chat_path: Option<String>,
    messages_path: Option<String>,
    event_queue_capacity: Option<usize>,
    event_forwarder_timeout_secs: Option<u64>,
}

#[derive(Clone)]
struct ToolRegistration {
    name: String,
    description: String,
    schema: Value,
    execution_mode: ToolExecutionMode,
    callback: ToolExecuteFn,
}

#[derive(Deserialize)]
struct ToolRegistrationDto {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default = "default_tool_schema")]
    parameters_schema: Value,
    #[serde(default)]
    execution_mode: Option<String>,
}

#[derive(Deserialize)]
#[serde(default)]
struct ToolResultDto {
    content: Vec<ContentBlock>,
    details: Value,
    terminate: bool,
    error: Option<String>,
}

impl Default for ToolResultDto {
    fn default() -> Self {
        Self {
            content: vec![],
            details: Value::Null,
            terminate: false,
            error: None,
        }
    }
}

struct SharedState {
    events: Mutex<VecDeque<String>>,
    event_cv: Condvar,
    lifecycle: Mutex<()>,
    event_queue_capacity: usize,
    dropped_events: AtomicU64,
    pending_message_end: Mutex<Option<Value>>,
    last_error: Mutex<Option<FfiError>>,
    prompt_running: AtomicBool,
    cancelled: AtomicBool,
    terminal_emitted: AtomicBool,
}

impl SharedState {
    fn new(event_queue_capacity: usize) -> Self {
        Self {
            events: Mutex::new(VecDeque::new()),
            event_cv: Condvar::new(),
            lifecycle: Mutex::new(()),
            event_queue_capacity,
            dropped_events: AtomicU64::new(0),
            pending_message_end: Mutex::new(None),
            last_error: Mutex::new(None),
            prompt_running: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            terminal_emitted: AtomicBool::new(false),
        }
    }

    fn emit_raw(&self, event: Value) {
        let mut events = self.events.lock().unwrap();
        self.push_event(&mut events, event);
        self.event_cv.notify_all();
    }

    fn push_event(&self, events: &mut VecDeque<String>, event: Value) {
        let event_text = event.to_string();
        if self.event_queue_capacity == 0 {
            return;
        }
        if events.len() < self.event_queue_capacity {
            events.push_back(event_text);
            return;
        }

        let slots_needed = if self.event_queue_capacity > 1 { 2 } else { 1 };
        let mut dropped = 0_u64;
        while events.len() + slots_needed > self.event_queue_capacity {
            if events.pop_front().is_some() {
                dropped += 1;
            } else {
                break;
            }
        }

        if dropped > 0 && self.event_queue_capacity > 1 {
            let dropped_total = self.dropped_events.fetch_add(dropped, Ordering::SeqCst) + dropped;
            events.push_back(
                json!({
                    "type": "warning",
                    "kind": "event_queue_overflow",
                    "message": "event queue overflow; oldest events dropped",
                    "dropped_events_total": dropped_total,
                })
                .to_string(),
            );
        } else if dropped > 0 {
            self.dropped_events.fetch_add(dropped, Ordering::SeqCst);
        }

        events.push_back(event_text);
    }

    fn queue_message_end(&self, event: Value) {
        *self.pending_message_end.lock().unwrap() = Some(event);
    }

    fn flush_pending_message_end(&self) {
        let pending = self.pending_message_end.lock().unwrap().take();
        if let Some(event) = pending {
            self.emit_raw(event);
        }
    }

    fn emit_boundary(&self, event: Value) {
        self.flush_pending_message_end();
        self.emit_raw(event);
    }

    fn emit_terminal(&self, event: Value) -> bool {
        if self
            .terminal_emitted
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.emit_boundary(event);
            true
        } else {
            false
        }
    }

    fn set_last_error(&self, err: FfiError) {
        *self.last_error.lock().unwrap() = Some(err.clone());
        self.emit_terminal(json!({ "type": "error", "error": err }));
    }

    fn start_prompt(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
        self.terminal_emitted.store(false, Ordering::SeqCst);
    }

    fn finish_prompt(&self) {
        self.prompt_running.store(false, Ordering::SeqCst);
        self.event_cv.notify_all();
    }
}

#[repr(C)]
pub struct ffi_harness_t {
    shared: Arc<SharedState>,
    runtime: Arc<Runtime>,
    harness: Arc<AgentHarness>,
    event_forwarder_timeout: Option<Duration>,
    tools: Mutex<Vec<ToolRegistration>>,
}

struct ToolCtxInner {
    shared: Arc<SharedState>,
    abort: CancellationToken,
    update_tx: tokio::sync::mpsc::Sender<ToolResult>,
}

#[repr(C)]
pub struct ffi_tool_ctx_t {
    inner: Arc<ToolCtxInner>,
}

fn redact_prefixed_secret(input: &str, prefix: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(index) = rest.find(prefix) {
        output.push_str(&rest[..index]);
        let token_start = index;
        let token = &rest[token_start..];
        let suffix = &token[prefix.len()..];
        let suffix_len = suffix
            .find(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')))
            .unwrap_or(suffix.len());
        let token_len = prefix.len() + suffix_len;
        let matched = &token[..token_len];

        if matched.len() > prefix.len() + 4 {
            output.push_str(prefix);
            output.push_str("***");
        } else {
            output.push_str(matched);
        }
        rest = &token[token_len..];
    }

    output.push_str(rest);
    output
}

fn redact_secrets(message: String) -> String {
    let message = redact_prefixed_secret(&message, "sk-");
    redact_prefixed_secret(&message, "Bearer ")
}

fn ffi_error(code: c_int, kind: &'static str, message: impl Into<String>) -> FfiError {
    FfiError {
        code,
        kind,
        message: redact_secrets(message.into()),
    }
}

fn default_tool_schema() -> Value {
    json!({ "type": "object", "properties": {} })
}

fn write_string_out(out: *mut *mut c_char, text: String) -> c_int {
    if out.is_null() {
        return FFI_ERR_INVALID_ARG;
    }

    match CString::new(text) {
        Ok(s) => {
            unsafe {
                *out = s.into_raw();
            }
            FFI_OK
        }
        Err(_) => FFI_ERR_INTERNAL,
    }
}

fn write_error_out(out: *mut *mut c_char, err: &FfiError) -> c_int {
    if !out.is_null() {
        let text = serde_json::to_string(err).unwrap_or_else(|_| {
            "{\"code\":-10,\"kind\":\"internal\",\"message\":\"failed to serialize error\"}"
                .to_string()
        });
        let _ = write_string_out(out, text);
    }
    err.code
}

unsafe fn cstr_to_string(ptr: *const c_char) -> Result<String, FfiError> {
    if ptr.is_null() {
        return Err(ffi_error(
            FFI_ERR_INVALID_ARG,
            "invalid_arg",
            "null string pointer",
        ));
    }

    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map(|s| s.to_owned())
        .map_err(|_| {
            ffi_error(
                FFI_ERR_INVALID_ARG,
                "invalid_utf8",
                "string argument is not valid utf-8",
            )
        })
}

unsafe fn harness_from_ptr<'a>(h: *mut ffi_harness_t) -> Result<&'a ffi_harness_t, FfiError> {
    unsafe { h.as_ref() }
        .ok_or_else(|| ffi_error(FFI_ERR_INVALID_ARG, "invalid_handle", "null harness handle"))
}

unsafe fn tool_ctx_from_ptr<'a>(ctx: *mut ffi_tool_ctx_t) -> Result<&'a ffi_tool_ctx_t, FfiError> {
    unsafe { ctx.as_ref() }
        .ok_or_else(|| ffi_error(FFI_ERR_INVALID_ARG, "invalid_context", "null tool context"))
}

fn catch_ffi_i32<F>(error_json_out: *mut *mut c_char, f: F) -> c_int
where
    F: FnOnce() -> Result<c_int, FfiError>,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(code)) => code,
        Ok(Err(err)) => write_error_out(error_json_out, &err),
        Err(_) => write_error_out(
            error_json_out,
            &ffi_error(FFI_ERR_INTERNAL, "panic", "panic crossed FFI boundary"),
        ),
    }
}

fn catch_ffi_ptr<F>(error_json_out: *mut *mut c_char, f: F) -> *mut ffi_harness_t
where
    F: FnOnce() -> Result<*mut ffi_harness_t, FfiError>,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(ptr)) => ptr,
        Ok(Err(err)) => {
            let _ = write_error_out(error_json_out, &err);
            ptr::null_mut()
        }
        Err(_) => {
            let _ = write_error_out(
                error_json_out,
                &ffi_error(FFI_ERR_INTERNAL, "panic", "panic crossed FFI boundary"),
            );
            ptr::null_mut()
        }
    }
}

fn build_runtime() -> Result<Arc<Runtime>, FfiError> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .map(Arc::new)
        .map_err(|e| {
            ffi_error(
                FFI_ERR_INTERNAL,
                "runtime",
                format!("failed to build tokio runtime: {e}"),
            )
        })
}

fn event_queue_capacity(config: &HarnessConfig) -> usize {
    config
        .event_queue_capacity
        .unwrap_or(DEFAULT_EVENT_QUEUE_CAPACITY)
}

fn event_forwarder_timeout(config: &HarnessConfig) -> Option<Duration> {
    match config.event_forwarder_timeout_secs {
        Some(0) => None,
        Some(secs) => Some(Duration::from_secs(secs)),
        None => Some(Duration::from_secs(DEFAULT_EVENT_FORWARDER_TIMEOUT_SECS)),
    }
}

fn wait_for_prompt_idle(shared: &SharedState, timeout_ms: u32) -> Result<(), FfiError> {
    if !shared.prompt_running.load(Ordering::SeqCst) {
        return Ok(());
    }

    let mut events = shared.events.lock().unwrap();
    if timeout_ms == 0 {
        while shared.prompt_running.load(Ordering::SeqCst) {
            events = shared.event_cv.wait(events).unwrap();
        }
        return Ok(());
    }

    let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
    while shared.prompt_running.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }

        let remaining = deadline - now;
        let (next_events, wait_result) = shared.event_cv.wait_timeout(events, remaining).unwrap();
        events = next_events;
        if wait_result.timed_out() && shared.prompt_running.load(Ordering::SeqCst) {
            break;
        }
    }

    if shared.prompt_running.load(Ordering::SeqCst) {
        Err(ffi_error(
            FFI_ERR_TIMEOUT,
            "timeout",
            "timed out waiting for active prompt to stop",
        ))
    } else {
        Ok(())
    }
}

fn zeroize_direct_api_key(config: &mut HarnessConfig) {
    if let Some(mut api_key) = config.api_key.take() {
        api_key.zeroize();
    }
}

fn resolve_api_key(config: &mut HarnessConfig) -> Result<String, FfiError> {
    if let Some(mut api_key) = config.api_key.take() {
        if !api_key.trim().is_empty() {
            let resolved = api_key.clone();
            api_key.zeroize();
            return Ok(resolved);
        }
        api_key.zeroize();
    }

    if let Some(env_name) = &config.api_key_env {
        let mut api_key = std::env::var(env_name).map_err(|_| {
            ffi_error(
                FFI_ERR_INVALID_ARG,
                "missing_api_key",
                format!("environment variable {env_name} is not set"),
            )
        })?;
        if api_key.trim().is_empty() {
            api_key.zeroize();
            return Err(ffi_error(
                FFI_ERR_INVALID_ARG,
                "missing_api_key",
                format!("environment variable {env_name} is empty"),
            ));
        }

        let resolved = api_key.clone();
        api_key.zeroize();
        return Ok(resolved);
    }

    Err(ffi_error(
        FFI_ERR_INVALID_ARG,
        "missing_api_key",
        "config must include api_key or api_key_env",
    ))
}

fn openai_chat_path(config: &HarnessConfig) -> Option<String> {
    if let Some(chat_path) = &config.chat_path {
        return Some(chat_path.clone());
    }

    let base_url = config.base_url.as_deref()?.trim_end_matches('/');
    if base_url.ends_with("/v1") {
        Some("/chat/completions".to_string())
    } else {
        None
    }
}

fn provider_from_config(config: &mut HarnessConfig) -> Result<Arc<dyn LlmClient>, FfiError> {
    let provider = config.provider.clone();
    match provider.as_str() {
        "openai" | "openai-compatible" => {
            let api_key = resolve_api_key(config)?;
            let mut builder = OpenAIProvider::builder(api_key);
            if let Some(base_url) = &config.base_url {
                builder = builder.base_url(base_url.clone());
            }
            if let Some(chat_path) = openai_chat_path(config) {
                builder = builder.chat_path(chat_path);
            }
            Ok(Arc::new(builder.build()) as Arc<dyn LlmClient>)
        }
        "anthropic" => {
            let api_key = resolve_api_key(config)?;
            let mut builder = AnthropicProvider::builder(api_key);
            if let Some(base_url) = &config.base_url {
                builder = builder.base_url(base_url.clone());
            }
            if let Some(messages_path) = &config.messages_path {
                builder = builder.messages_path(messages_path.clone());
            }
            Ok(Arc::new(builder.build()) as Arc<dyn LlmClient>)
        }
        other => {
            zeroize_direct_api_key(config);
            Err(ffi_error(
                FFI_ERR_INVALID_ARG,
                "invalid_provider",
                format!("unsupported provider: {other}"),
            ))
        }
    }
}

fn parse_harness_config(config_json: *const c_char) -> Result<HarnessConfig, FfiError> {
    let mut config_text = unsafe { cstr_to_string(config_json)? };
    let mut config: HarnessConfig = match serde_json::from_str(&config_text) {
        Ok(config) => config,
        Err(e) => {
            config_text.zeroize();
            return Err(ffi_error(
                FFI_ERR_INVALID_ARG,
                "invalid_config",
                format!("config_json is not valid JSON: {e}"),
            ));
        }
    };
    config_text.zeroize();

    if config.provider.trim().is_empty() {
        zeroize_direct_api_key(&mut config);
        return Err(ffi_error(
            FFI_ERR_INVALID_ARG,
            "missing_provider",
            "config.provider is required",
        ));
    }
    if config.model.trim().is_empty() {
        zeroize_direct_api_key(&mut config);
        return Err(ffi_error(
            FFI_ERR_INVALID_ARG,
            "missing_model",
            "config.model is required",
        ));
    }
    if config.event_queue_capacity == Some(0) {
        zeroize_direct_api_key(&mut config);
        return Err(ffi_error(
            FFI_ERR_INVALID_ARG,
            "invalid_config",
            "config.event_queue_capacity must be greater than zero",
        ));
    }
    Ok(config)
}

fn parse_tool_registration(
    tool_json: *const c_char,
    callback: Option<ToolExecuteFn>,
) -> Result<ToolRegistration, FfiError> {
    let callback = callback.ok_or_else(|| {
        ffi_error(
            FFI_ERR_INVALID_ARG,
            "invalid_callback",
            "tool callback is null",
        )
    })?;
    let tool_text = unsafe { cstr_to_string(tool_json)? };
    let dto: ToolRegistrationDto = serde_json::from_str(&tool_text).map_err(|e| {
        ffi_error(
            FFI_ERR_INVALID_ARG,
            "invalid_tool",
            format!("tool_json is not valid JSON: {e}"),
        )
    })?;
    if dto.name.trim().is_empty() {
        return Err(ffi_error(
            FFI_ERR_INVALID_ARG,
            "invalid_tool",
            "tool.name is required",
        ));
    }

    let execution_mode = match dto.execution_mode.as_deref() {
        None | Some("parallel") => ToolExecutionMode::Parallel,
        Some("sequential") => ToolExecutionMode::Sequential,
        Some(other) => {
            return Err(ffi_error(
                FFI_ERR_INVALID_ARG,
                "invalid_tool",
                format!("unsupported execution_mode: {other}"),
            ));
        }
    };

    Ok(ToolRegistration {
        name: dto.name,
        description: dto.description,
        schema: dto.parameters_schema,
        execution_mode,
        callback,
    })
}

fn parse_tool_result_value(value: Value) -> Result<ToolResult, ToolError> {
    let dto: ToolResultDto = serde_json::from_value(value)
        .map_err(|e| ToolError::InvalidArguments(format!("invalid tool result JSON: {e}")))?;
    if let Some(error) = dto.error {
        return Err(ToolError::Execution(error));
    }
    Ok(ToolResult {
        content: dto.content,
        details: dto.details,
        terminate: dto.terminate,
    })
}

fn tool_result_to_json(result: &ToolResult) -> Value {
    json!({
        "content": result.content,
        "details": result.details,
        "terminate": result.terminate,
    })
}

fn callback_string_from_raw(ptr: *mut c_char) -> Result<String, ToolError> {
    if ptr.is_null() {
        return Err(ToolError::Execution(
            "tool callback returned null result".to_string(),
        ));
    }

    let result_json = unsafe { CString::from_raw(ptr) };
    result_json
        .into_string()
        .map_err(|_| ToolError::Execution("tool callback returned invalid utf-8".to_string()))
}

struct FfiCallbackTool {
    registration: ToolRegistration,
    shared: Arc<SharedState>,
}

impl Tool for FfiCallbackTool {
    fn name(&self) -> &str {
        &self.registration.name
    }

    fn description(&self) -> &str {
        &self.registration.description
    }

    fn parameters_schema(&self) -> &Value {
        &self.registration.schema
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        self.registration.execution_mode
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        let shared = self.shared.clone();
        let registration = self.registration.clone();
        let abort = ctx.abort.clone();
        let update_tx = ctx.update_tx.clone();

        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                invoke_tool_callback(shared, registration, args, abort, update_tx)
            })
            .await
            .map_err(|e| ToolError::Execution(format!("callback worker join failed: {e}")))?
        })
    }
}

fn invoke_tool_callback(
    shared: Arc<SharedState>,
    tool: ToolRegistration,
    args: Value,
    abort: CancellationToken,
    update_tx: tokio::sync::mpsc::Sender<ToolResult>,
) -> Result<ToolResult, ToolError> {
    let ctx = Box::new(ffi_tool_ctx_t {
        inner: Arc::new(ToolCtxInner {
            shared,
            abort,
            update_tx,
        }),
    });
    let ctx_ptr = Box::into_raw(ctx);

    let args = CString::new(args.to_string())
        .map_err(|_| ToolError::Execution("tool args JSON contains NUL".to_string()))?;

    let callback_result = catch_unwind(AssertUnwindSafe(|| {
        let mut result_ptr: *mut c_char = ptr::null_mut();
        let rc = unsafe { (tool.callback)(args.as_ptr(), ctx_ptr, &mut result_ptr) };
        (rc, result_ptr)
    }));

    unsafe {
        drop(Box::from_raw(ctx_ptr));
    }

    match callback_result {
        Ok((FFI_OK, result_ptr)) => {
            let result_json = callback_string_from_raw(result_ptr)?;
            let value: Value = serde_json::from_str(&result_json).map_err(|e| {
                ToolError::InvalidArguments(format!("tool callback returned invalid JSON: {e}"))
            })?;
            parse_tool_result_value(value)
        }
        Ok((rc, result_ptr)) => {
            let payload = if result_ptr.is_null() {
                String::new()
            } else {
                callback_string_from_raw(result_ptr).unwrap_or_default()
            };
            Err(ToolError::Execution(if payload.is_empty() {
                format!("tool callback returned error code {rc}")
            } else {
                format!("tool callback returned error code {rc}: {payload}")
            }))
        }
        Err(_) => Err(ToolError::Execution("tool callback panicked".to_string())),
    }
}

fn make_tool_objects(shared: Arc<SharedState>, tools: &[ToolRegistration]) -> Vec<Arc<dyn Tool>> {
    tools
        .iter()
        .cloned()
        .map(|registration| {
            Arc::new(FfiCallbackTool {
                registration,
                shared: shared.clone(),
            }) as Arc<dyn Tool>
        })
        .collect()
}

fn forward_agent_event(shared: &Arc<SharedState>, event: &AgentEvent) {
    match event {
        AgentEvent::TextDelta { message_id, text } => shared.emit_raw(json!({
            "type": "text_delta",
            "message_id": message_id,
            "text": text,
        })),
        AgentEvent::MessageEnd {
            message_id,
            message,
        } => shared.queue_message_end(json!({
            "type": "message_end",
            "message_id": message_id,
            "message": message,
        })),
        AgentEvent::ToolExecutionUpdate {
            tool_use_id,
            partial,
        } => shared.emit_raw(json!({
            "type": "tool_update",
            "tool_call_id": tool_use_id,
            "partial": tool_result_to_json(partial),
        })),
        AgentEvent::AgentEnd { new_messages } => shared.emit_boundary(json!({
            "type": "agent_end",
            "new_messages": new_messages,
        })),
        AgentEvent::Error(err) => {
            shared.emit_terminal(json!({
                "type": "error",
                "message": err.to_string(),
            }));
        }
        _ => {}
    }
}

fn forward_harness_event(shared: &Arc<SharedState>, event: &AgentHarnessEvent) {
    match event {
        AgentHarnessEvent::Agent(agent_event) => forward_agent_event(shared, agent_event),
        AgentHarnessEvent::ToolCallStart {
            tool_use_id,
            tool_name,
            args,
        } => shared.emit_boundary(json!({
            "type": "tool_start",
            "tool_call_id": tool_use_id,
            "tool_name": tool_name,
            "args": args,
        })),
        AgentHarnessEvent::ToolCallEnd {
            tool_use_id,
            tool_name,
            result,
        } => shared.emit_boundary(json!({
            "type": "tool_end",
            "tool_call_id": tool_use_id,
            "tool_name": tool_name,
            "result": {
                "content": result.content,
                "details": result.details,
                "is_error": result.is_error,
            },
        })),
        AgentHarnessEvent::SavePoint { entries_flushed } => shared.emit_boundary(json!({
            "type": "save_point",
            "entries_flushed": entries_flushed,
        })),
        AgentHarnessEvent::Settled => {
            shared.emit_terminal(json!({ "type": "settled" }));
        }
        AgentHarnessEvent::Aborted => {
            shared.emit_terminal(json!({ "type": "aborted" }));
        }
        AgentHarnessEvent::PhaseChange { from, to } => shared.emit_boundary(json!({
            "type": "phase_change",
            "from": format!("{from:?}"),
            "to": format!("{to:?}"),
        })),
        _ => {}
    }
}

fn terminal_event_from_harness(event: &AgentHarnessEvent) -> Option<Value> {
    match event {
        AgentHarnessEvent::Settled => Some(json!({ "type": "settled" })),
        AgentHarnessEvent::Aborted => Some(json!({ "type": "aborted" })),
        AgentHarnessEvent::Agent(AgentEvent::Error(err)) => Some(json!({
            "type": "error",
            "message": err.to_string(),
        })),
        _ => None,
    }
}

fn run_prompt(
    shared: Arc<SharedState>,
    runtime: Arc<Runtime>,
    harness: Arc<AgentHarness>,
    prompt: String,
    forwarder_timeout: Option<Duration>,
) {
    let result = catch_unwind(AssertUnwindSafe({
        let shared = shared.clone();
        move || -> Result<(), FfiError> {
            runtime.block_on(async move {
                shared.emit_raw(json!({ "type": "prompt_start", "prompt": prompt }));
                let mut rx = harness.subscribe();
                let forward_shared = shared.clone();
                let forwarder = tokio::spawn(async move {
                    while let Ok(event) = rx.recv().await {
                        if let Some(terminal_event) = terminal_event_from_harness(event.as_ref()) {
                            return Ok(terminal_event);
                        }
                        forward_harness_event(&forward_shared, event.as_ref());
                    }

                    Err(ffi_error(
                        FFI_ERR_INTERNAL,
                        "event_forwarder",
                        "event stream closed before terminal event",
                    ))
                });

                if let Err(err) = harness.prompt(prompt).await {
                    forwarder.abort();
                    return Err(ffi_error(
                        FFI_ERR_INTERNAL,
                        "harness",
                        format!("AgentHarness prompt failed: {err}"),
                    ));
                }

                let terminal_event = if let Some(timeout) = forwarder_timeout {
                    match tokio::time::timeout(timeout, forwarder).await {
                        Ok(Ok(Ok(event))) => Ok(event),
                        Ok(Ok(Err(err))) => Err(err),
                        Ok(Err(err)) => Err(ffi_error(
                            FFI_ERR_INTERNAL,
                            "event_forwarder",
                            format!("event forwarder failed: {err}"),
                        )),
                        Err(_) => Err(ffi_error(
                            FFI_ERR_TIMEOUT,
                            "event_forwarder_timeout",
                            format!(
                                "timed out waiting for settled event after {} seconds",
                                timeout.as_secs()
                            ),
                        )),
                    }
                } else {
                    match forwarder.await {
                        Ok(Ok(event)) => Ok(event),
                        Ok(Err(err)) => Err(err),
                        Err(err) => Err(ffi_error(
                            FFI_ERR_INTERNAL,
                            "event_forwarder",
                            format!("event forwarder failed: {err}"),
                        )),
                    }
                }?;

                shared.finish_prompt();
                shared.emit_terminal(terminal_event);
                Ok(())
            })
        }
    }));

    match result {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            shared.finish_prompt();
            shared.set_last_error(err);
        }
        Err(_) => {
            shared.finish_prompt();
            shared.set_last_error(ffi_error(
                FFI_ERR_INTERNAL,
                "panic",
                "prompt worker panicked",
            ));
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn ffi_harness_abi_version() -> u32 {
    FFI_ABI_VERSION
}

#[unsafe(no_mangle)]
pub extern "C" fn ffi_harness_version() -> *mut c_char {
    let result = catch_unwind(AssertUnwindSafe(|| CString::new(env!("CARGO_PKG_VERSION"))));
    match result {
        Ok(Ok(version)) => version.into_raw(),
        _ => ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn ffi_harness_error_code_callback() -> c_int {
    FFI_ERR_CALLBACK
}

#[unsafe(no_mangle)]
pub extern "C" fn ffi_harness_new(
    config_json: *const c_char,
    error_json_out: *mut *mut c_char,
) -> *mut ffi_harness_t {
    catch_ffi_ptr(error_json_out, || {
        let mut config = parse_harness_config(config_json)?;
        let provider = provider_from_config(&mut config)?;
        let runtime = build_runtime()?;
        let work_dir = config
            .work_dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let env = Arc::new(OsEnv::new(work_dir));

        let mut builder =
            HarnessBuilder::new(config.model.clone()).provider(config.model.clone(), provider);
        if let Some(max_tokens) = config.max_tokens {
            builder = builder.max_tokens(max_tokens);
        }

        let harness = runtime.block_on(builder.build(env)).map_err(|e| {
            ffi_error(
                FFI_ERR_INTERNAL,
                "build_harness",
                format!("failed to build harness: {e}"),
            )
        })?;

        Ok(Box::into_raw(Box::new(ffi_harness_t {
            shared: Arc::new(SharedState::new(event_queue_capacity(&config))),
            runtime,
            harness: Arc::new(harness),
            event_forwarder_timeout: event_forwarder_timeout(&config),
            tools: Mutex::new(vec![]),
        })))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn ffi_harness_free(h: *mut ffi_harness_t) {
    if !h.is_null() {
        let harness = unsafe { Box::from_raw(h) };
        harness.shared.cancelled.store(true, Ordering::SeqCst);
        harness.harness.abort();
        let _ = wait_for_prompt_idle(&harness.shared, DEFAULT_FREE_WAIT_TIMEOUT_MS);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn ffi_harness_free_string(s: *mut c_char) {
    if !s.is_null() {
        unsafe {
            drop(CString::from_raw(s));
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn ffi_harness_alloc_string(src: *const c_char) -> *mut c_char {
    let result = catch_unwind(AssertUnwindSafe(|| unsafe { cstr_to_string(src) }));
    match result {
        Ok(Ok(s)) => CString::new(s)
            .map(|s| s.into_raw())
            .unwrap_or(ptr::null_mut()),
        _ => ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn ffi_harness_last_error(h: *mut ffi_harness_t) -> *mut c_char {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let harness = unsafe { harness_from_ptr(h)? };
        let last = harness.shared.last_error.lock().unwrap().clone();
        match last {
            Some(err) => Ok::<String, FfiError>(serde_json::to_string(&err).unwrap_or_default()),
            None => Ok::<String, FfiError>(String::new()),
        }
    }));

    match result {
        Ok(Ok(text)) if !text.is_empty() => CString::new(text)
            .map(|s| s.into_raw())
            .unwrap_or(ptr::null_mut()),
        _ => ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn ffi_harness_register_tool(
    h: *mut ffi_harness_t,
    tool_json: *const c_char,
    callback: Option<ToolExecuteFn>,
    error_json_out: *mut *mut c_char,
) -> c_int {
    catch_ffi_i32(error_json_out, || {
        let harness = unsafe { harness_from_ptr(h)? };
        let registration = parse_tool_registration(tool_json, callback)?;

        let tool_objects = {
            let mut tools = harness.tools.lock().unwrap();
            if let Some(existing) = tools.iter_mut().find(|tool| tool.name == registration.name) {
                *existing = registration;
            } else {
                tools.push(registration);
            }
            make_tool_objects(harness.shared.clone(), &tools)
        };

        harness
            .runtime
            .block_on(harness.harness.set_tools(tool_objects))
            .map_err(|e| {
                ffi_error(
                    FFI_ERR_INTERNAL,
                    "register_tool",
                    format!("failed to register tool: {e}"),
                )
            })?;
        Ok(FFI_OK)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn ffi_harness_prompt(
    h: *mut ffi_harness_t,
    text: *const c_char,
    error_json_out: *mut *mut c_char,
) -> c_int {
    catch_ffi_i32(error_json_out, || {
        let harness = unsafe { harness_from_ptr(h)? };
        let prompt = unsafe { cstr_to_string(text)? };
        {
            let _lifecycle = harness.shared.lifecycle.lock().unwrap();
            harness
                .shared
                .prompt_running
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .map_err(|_| {
                    ffi_error(
                        FFI_ERR_BUSY,
                        "busy",
                        "a prompt is already running for this harness",
                    )
                })?;
            harness.shared.start_prompt();
        }

        let shared = harness.shared.clone();
        let runtime = harness.runtime.clone();
        let agent_harness = harness.harness.clone();
        let forwarder_timeout = harness.event_forwarder_timeout;
        std::thread::spawn(move || {
            run_prompt(shared, runtime, agent_harness, prompt, forwarder_timeout)
        });
        Ok(FFI_OK)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn ffi_harness_abort(h: *mut ffi_harness_t) {
    let result = catch_unwind(AssertUnwindSafe(|| unsafe { harness_from_ptr(h) }));
    if let Ok(Ok(harness)) = result {
        {
            let _lifecycle = harness.shared.lifecycle.lock().unwrap();
            harness.shared.cancelled.store(true, Ordering::SeqCst);
        }
        harness.harness.abort();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn ffi_harness_close(
    h: *mut ffi_harness_t,
    timeout_ms: u32,
    error_json_out: *mut *mut c_char,
) -> c_int {
    catch_ffi_i32(error_json_out, || {
        let harness = unsafe { harness_from_ptr(h)? };
        {
            let _lifecycle = harness.shared.lifecycle.lock().unwrap();
            harness.shared.cancelled.store(true, Ordering::SeqCst);
        }
        harness.harness.abort();

        wait_for_prompt_idle(&harness.shared, timeout_ms).inspect_err(|err| {
            harness.shared.set_last_error(err.clone());
        })?;
        Ok(FFI_OK)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn ffi_harness_wait_event(
    h: *mut ffi_harness_t,
    timeout_ms: u32,
    event_json_out: *mut *mut c_char,
    error_json_out: *mut *mut c_char,
) -> c_int {
    catch_ffi_i32(error_json_out, || {
        let harness = unsafe { harness_from_ptr(h)? };
        let mut events = harness.shared.events.lock().unwrap();

        if timeout_ms == 0 {
            while events.is_empty() {
                events = harness.shared.event_cv.wait(events).unwrap();
            }
        } else if events.is_empty() {
            let timeout = Duration::from_millis(timeout_ms as u64);
            let (guard, wait_result) = harness
                .shared
                .event_cv
                .wait_timeout(events, timeout)
                .unwrap();
            events = guard;
            if wait_result.timed_out() && events.is_empty() {
                return Ok(FFI_NO_EVENT);
            }
        }

        match events.pop_front() {
            Some(event) => {
                let rc = write_string_out(event_json_out, event);
                if rc == FFI_OK {
                    Ok(FFI_OK)
                } else {
                    Err(ffi_error(
                        FFI_ERR_INTERNAL,
                        "internal",
                        "failed to write event",
                    ))
                }
            }
            None => Ok(FFI_NO_EVENT),
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn ffi_tool_is_cancelled(ctx: *mut ffi_tool_ctx_t) -> c_int {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let ctx = unsafe { tool_ctx_from_ptr(ctx)? };
        Ok::<bool, FfiError>(
            ctx.inner.shared.cancelled.load(Ordering::SeqCst) || ctx.inner.abort.is_cancelled(),
        )
    }));
    match result {
        Ok(Ok(true)) => 1,
        Ok(Ok(false)) => 0,
        _ => 1,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn ffi_tool_send_update(
    ctx: *mut ffi_tool_ctx_t,
    result_json: *const c_char,
) -> c_int {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let ctx = unsafe { tool_ctx_from_ptr(ctx)? };
        let result_json = unsafe { cstr_to_string(result_json)? };
        let parsed = serde_json::from_str::<Value>(&result_json)
            .unwrap_or_else(|_| json!({ "raw": result_json }));
        let partial = parse_tool_result_value(parsed).map_err(|e| {
            ffi_error(
                FFI_ERR_CALLBACK,
                "callback",
                format!("invalid tool update result: {e}"),
            )
        })?;
        ctx.inner.update_tx.try_send(partial).map_err(|e| {
            ffi_error(
                FFI_ERR_CALLBACK,
                "callback",
                format!("failed to send tool update: {e}"),
            )
        })?;
        Ok::<c_int, FfiError>(FFI_OK)
    }));

    match result {
        Ok(Ok(code)) => code,
        Ok(Err(err)) => err.code,
        Err(_) => FFI_ERR_INTERNAL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn dummy_tool(
        _args_json: *const c_char,
        _ctx_handle: *mut ffi_tool_ctx_t,
        result_json_out: *mut *mut c_char,
    ) -> c_int {
        let result = CString::new(
            r#"{"content":[{"type":"text","text":"ok"}],"details":{},"terminate":false}"#,
        )
        .unwrap();
        unsafe {
            *result_json_out = result.into_raw();
        }
        FFI_OK
    }

    fn cstring(text: &str) -> CString {
        CString::new(text).unwrap()
    }

    #[test]
    fn openai_chat_path_uses_chat_completions_when_base_url_includes_v1() {
        let config = HarnessConfig {
            base_url: Some("https://api.siliconflow.cn/v1/".into()),
            ..HarnessConfig::default()
        };

        assert_eq!(
            openai_chat_path(&config),
            Some("/chat/completions".to_string())
        );
    }

    #[test]
    fn explicit_openai_chat_path_wins_over_v1_base_url_default() {
        let config = HarnessConfig {
            base_url: Some("https://api.example.com/v1".into()),
            chat_path: Some("/custom/chat".into()),
            ..HarnessConfig::default()
        };

        assert_eq!(openai_chat_path(&config), Some("/custom/chat".to_string()));
    }

    #[test]
    fn event_forwarder_timeout_defaults_and_allows_disabling() {
        assert_eq!(
            event_forwarder_timeout(&HarnessConfig::default()),
            Some(Duration::from_secs(DEFAULT_EVENT_FORWARDER_TIMEOUT_SECS))
        );

        let config = HarnessConfig {
            event_forwarder_timeout_secs: Some(0),
            ..HarnessConfig::default()
        };
        assert_eq!(event_forwarder_timeout(&config), None);
    }

    #[test]
    fn resolve_api_key_takes_direct_key_from_config() {
        let mut config = HarnessConfig {
            api_key: Some("sk-testsecret1234567890".to_string()),
            ..HarnessConfig::default()
        };

        let resolved = resolve_api_key(&mut config).unwrap();

        assert_eq!(resolved, "sk-testsecret1234567890");
        assert!(config.api_key.is_none());
    }

    #[test]
    fn wait_for_prompt_idle_returns_after_finish_prompt() {
        let shared = Arc::new(SharedState::new(DEFAULT_EVENT_QUEUE_CAPACITY));
        shared.prompt_running.store(true, Ordering::SeqCst);

        let worker_shared = shared.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            worker_shared.finish_prompt();
        });

        assert!(wait_for_prompt_idle(&shared, 1000).is_ok());
        worker.join().unwrap();
    }

    #[test]
    fn terminal_event_is_emitted_once() {
        let shared = SharedState::new(DEFAULT_EVENT_QUEUE_CAPACITY);

        assert!(shared.emit_terminal(json!({ "type": "aborted" })));
        assert!(!shared.emit_terminal(json!({ "type": "settled" })));

        let events: Vec<Value> = shared
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|event| serde_json::from_str(event).unwrap())
            .collect();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "aborted");
    }

    #[test]
    fn set_last_error_is_terminal_and_does_not_emit_settled_afterward() {
        let shared = SharedState::new(DEFAULT_EVENT_QUEUE_CAPACITY);
        shared.set_last_error(ffi_error(FFI_ERR_INTERNAL, "test", "boom"));

        assert!(!shared.emit_terminal(json!({ "type": "settled" })));

        let events: Vec<Value> = shared
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|event| serde_json::from_str(event).unwrap())
            .collect();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "error");
    }

    #[test]
    fn event_queue_overflow_drops_oldest_and_emits_warning() {
        let shared = SharedState::new(2);
        shared.emit_raw(json!({ "type": "test", "seq": 1 }));
        shared.emit_raw(json!({ "type": "test", "seq": 2 }));
        shared.emit_raw(json!({ "type": "test", "seq": 3 }));

        let events: Vec<Value> = shared
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|event| serde_json::from_str(event).unwrap())
            .collect();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "warning");
        assert_eq!(events[0]["kind"], "event_queue_overflow");
        assert_eq!(events[0]["dropped_events_total"], 2);
        assert_eq!(events[1]["seq"], 3);
    }

    #[test]
    fn ffi_error_redacts_common_secret_prefixes() {
        let err = ffi_error(
            FFI_ERR_INTERNAL,
            "test",
            "failed with sk-abcdefghijklmnopqrstuvwxyz and Bearer abcdefghijklmnop",
        );

        assert!(!err.message.contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(!err.message.contains("abcdefghijklmnop"));
        assert!(err.message.contains("sk-***"));
        assert!(err.message.contains("Bearer ***"));
    }

    #[test]
    fn pending_message_end_is_coalesced_until_boundary_event() {
        let shared = SharedState::new(DEFAULT_EVENT_QUEUE_CAPACITY);
        shared.queue_message_end(json!({ "type": "message_end", "seq": 1 }));
        shared.queue_message_end(json!({ "type": "message_end", "seq": 2 }));
        shared.emit_boundary(json!({ "type": "settled" }));

        let events: Vec<Value> = shared
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|event| serde_json::from_str(event).unwrap())
            .collect();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "message_end");
        assert_eq!(events[0]["seq"], 2);
        assert_eq!(events[1]["type"], "settled");
    }

    #[test]
    fn exposes_abi_version_and_package_version() {
        assert_eq!(ffi_harness_abi_version(), FFI_ABI_VERSION);
        assert_eq!(ffi_harness_error_code_callback(), FFI_ERR_CALLBACK);

        let version = ffi_harness_version();
        assert!(!version.is_null());
        let version_text = unsafe { CStr::from_ptr(version) }
            .to_str()
            .unwrap()
            .to_string();
        ffi_harness_free_string(version);

        assert_eq!(version_text, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn new_rejects_missing_provider() {
        let config = cstring(r#"{"model":"gpt-4o-mini","api_key":"test"}"#);
        let mut err: *mut c_char = ptr::null_mut();
        let handle = ffi_harness_new(config.as_ptr(), &mut err);
        assert!(handle.is_null());
        assert!(!err.is_null());
        ffi_harness_free_string(err);
    }

    #[test]
    fn creates_openai_harness_and_registers_tool_without_network_call() {
        let config = cstring(
            r#"{"provider":"openai","model":"gpt-4o-mini","api_key":"test","max_tokens":1024}"#,
        );
        let mut err: *mut c_char = ptr::null_mut();
        let handle = ffi_harness_new(config.as_ptr(), &mut err);
        assert!(!handle.is_null());

        let tool = cstring(
            r#"{"name":"echo","description":"Echo test","parameters_schema":{"type":"object","properties":{"text":{"type":"string"}}}}"#,
        );
        let rc = ffi_harness_register_tool(handle, tool.as_ptr(), Some(dummy_tool), &mut err);
        assert_eq!(rc, FFI_OK);

        let rc = ffi_harness_close(handle, 100, &mut err);
        assert_eq!(rc, FFI_OK);

        ffi_harness_free(handle);
    }

    #[test]
    fn abort_then_close_idle_harness_without_network_call() {
        let config = cstring(
            r#"{"provider":"openai","model":"gpt-4o-mini","api_key":"test","max_tokens":1024}"#,
        );
        let mut err: *mut c_char = ptr::null_mut();
        let handle = ffi_harness_new(config.as_ptr(), &mut err);
        assert!(!handle.is_null());

        ffi_harness_abort(handle);
        let rc = ffi_harness_close(handle, 100, &mut err);
        assert_eq!(rc, FFI_OK);

        ffi_harness_free(handle);
    }

    #[test]
    fn free_without_explicit_close_aborts_idle_harness() {
        let config = cstring(
            r#"{"provider":"openai","model":"gpt-4o-mini","api_key":"test","max_tokens":1024}"#,
        );
        let mut err: *mut c_char = ptr::null_mut();
        let handle = ffi_harness_new(config.as_ptr(), &mut err);
        assert!(!handle.is_null());

        ffi_harness_free(handle);
    }

    #[test]
    fn busy_prompt_does_not_clear_cancelled_state() {
        let config = cstring(
            r#"{"provider":"openai","model":"gpt-4o-mini","api_key":"test","max_tokens":1024}"#,
        );
        let mut err: *mut c_char = ptr::null_mut();
        let handle = ffi_harness_new(config.as_ptr(), &mut err);
        assert!(!handle.is_null());

        let harness = unsafe { &*handle };
        harness.shared.cancelled.store(true, Ordering::SeqCst);
        harness.shared.prompt_running.store(true, Ordering::SeqCst);

        let prompt = cstring("busy");
        let rc = ffi_harness_prompt(handle, prompt.as_ptr(), &mut err);
        assert_eq!(rc, FFI_ERR_BUSY);
        assert!(harness.shared.cancelled.load(Ordering::SeqCst));

        ffi_harness_free_string(err);
        harness.shared.prompt_running.store(false, Ordering::SeqCst);
        ffi_harness_free(handle);
    }

    #[test]
    fn high_frequency_tool_updates_are_forwarded_to_channel() {
        let shared = Arc::new(SharedState::new(DEFAULT_EVENT_QUEUE_CAPACITY));
        let abort = CancellationToken::new();
        let (update_tx, mut update_rx) = tokio::sync::mpsc::channel(128);
        let ctx = Box::new(ffi_tool_ctx_t {
            inner: Arc::new(ToolCtxInner {
                shared,
                abort,
                update_tx,
            }),
        });
        let ctx_ptr = Box::into_raw(ctx);

        for i in 0..64 {
            let update = cstring(&format!(
                r#"{{"content":[{{"type":"text","text":"update {i}"}}],"details":{{"i":{i}}},"terminate":false}}"#
            ));
            let rc = ffi_tool_send_update(ctx_ptr, update.as_ptr());
            assert_eq!(rc, FFI_OK);
        }

        let mut count = 0;
        while update_rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, 64);

        unsafe {
            drop(Box::from_raw(ctx_ptr));
        }
    }

    #[test]
    fn close_rejects_null_handle() {
        let mut err: *mut c_char = ptr::null_mut();
        let rc = ffi_harness_close(ptr::null_mut(), 100, &mut err);
        assert_eq!(rc, FFI_ERR_INVALID_ARG);
        assert!(!err.is_null());
        ffi_harness_free_string(err);
    }
}
