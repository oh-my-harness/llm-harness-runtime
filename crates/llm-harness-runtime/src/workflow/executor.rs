//! Non-LLM workflow step execution.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Component, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use futures::future::BoxFuture;
use futures::stream::StreamExt;
use llm_harness_types::{ExecutionEnv, ShellOptions};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, LOCATION};
use reqwest::{Method, StatusCode, Url};
use serde_json::{Map, Value};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::control::cost::CostAggregate;
use crate::workflow::model::{Step, StepRecord, StepResult, StructuredStatus, WorkflowContext};
use anyhow::Result;

pub const JSON_TRANSFORM_EXECUTOR_NAME: &str = "json_transform";
pub const SHELL_EXECUTOR_NAME: &str = "shell";
pub const HTTP_CALL_EXECUTOR_NAME: &str = "http_call";

/// Context passed to a deterministic non-LLM step executor.
pub struct ExecutorCtx<'a> {
    /// Current executor step definition.
    pub current_step: &'a Step,
    /// Previous completed step result, if any.
    pub prev_result: Option<&'a StepResult>,
    /// Completed step history before this executor starts.
    pub step_history: &'a [StepRecord],
    /// Execution environment for executors that need filesystem or shell access.
    pub env: Arc<dyn ExecutionEnv>,
    /// Step-level abort token.
    pub abort: CancellationToken,
    /// 共享可变上下文。executor 可读写 KV，跨 step 传递业务数据。
    pub context: Arc<Mutex<WorkflowContext>>,
}

/// Deterministic executor for a non-LLM workflow step.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ExecutorCancellationPolicy {
    /// Drop the executor future as soon as cancellation or a step timeout wins.
    #[default]
    Interruptible,
    /// Signal cancellation, then await the executor's scoped finalization before
    /// returning the original cancellation or timeout result.
    Finalize,
}

pub trait StepExecutor: Send + Sync {
    /// Validate executor-specific step policy before runtime cache lookup.
    ///
    /// This hook must be deterministic and side-effect free. It exists because
    /// a cache hit skips [`StepExecutor::execute`].
    fn validate_step(&self, _step: &Step) -> Result<()> {
        Ok(())
    }

    /// Declare how runtime cancellation and step timeout interact with this
    /// executor.
    ///
    /// `Finalize` is intended for executors with a short commit/finalization
    /// phase that must not be detached after publication starts. The executor
    /// must still observe `ExecutorCtx::abort` before that phase.
    fn cancellation_policy(&self) -> ExecutorCancellationPolicy {
        ExecutorCancellationPolicy::Interruptible
    }

    fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>>;
}

const DEFAULT_SHELL_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_SHELL_MAX_OUTPUT_BYTES: usize = 64 * 1024;
const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_HTTP_MAX_REQUEST_BYTES: usize = 1024 * 1024;
const DEFAULT_HTTP_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const DEFAULT_HTTP_MAX_REDIRECTS: usize = 5;

/// Built-in deterministic executors shipped with the runtime.
pub fn builtin_executors() -> impl Iterator<Item = (&'static str, Arc<dyn StepExecutor>)> {
    [(
        JSON_TRANSFORM_EXECUTOR_NAME,
        Arc::new(JsonTransformExecutor) as Arc<dyn StepExecutor>,
    )]
    .into_iter()
}

/// JSON transform executor.
///
/// Config schema:
/// - `source`: optional, one of `auto`, `config.input`, `prev_result.structured`,
///   `prev_result.output`. Default is `auto`.
/// - `input`: JSON value used when `source = "config.input"` or by `auto`.
/// - `pointer`: optional RFC 6901 JSON Pointer applied to the selected input.
/// - `fields`: optional object mapping output field names to either a pointer string,
///   `{ "pointer": "...", "default": ... }`, or a literal value.
/// - `include_nulls`: optional bool, defaults to true. When false, null fields are omitted.
#[derive(Debug, Default)]
pub struct JsonTransformExecutor;

impl StepExecutor for JsonTransformExecutor {
    fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
        Box::pin(async move {
            let config = ctx
                .current_step
                .executor_config()
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            let config_obj = config
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("json_transform config must be an object"))?;
            let input = select_input(config_obj, ctx)?;
            let transformed = if let Some(fields) = config_obj.get("fields") {
                build_fields(fields, &input, config_obj)?
            } else if let Some(pointer) = config_obj.get("pointer") {
                let pointer = pointer
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("json_transform.pointer must be a string"))?;
                read_pointer(&input, pointer)?.clone()
            } else {
                input
            };
            let output = serde_json::to_string(&transformed)?;
            Ok(StepResult {
                output,
                structured: Some(transformed),
                structured_status: StructuredStatus::NotRequired,
                tool_calls_count: 0,
                session_id: String::new(),
                cost: CostAggregate::default(),
                started_at: None,
                ended_at: None,
            })
        })
    }
}

/// Policy-gated shell executor.
///
/// This executor is intentionally not registered by default. Callers must
/// explicitly inject it with `WorkflowEngine::with_executor`, usually with a
/// narrow command allowlist and a sandbox-backed `ExecutionEnv`.
///
/// Config schema:
/// - `command`: required, command name (must be in the allowlist).
/// - `args`: optional array of strings.
/// - `env`: optional object mapping env-var names to string values.
/// - `cwd`: optional relative path (no parent traversal).
/// - `timeout_ms`: optional, defaults to the executor's configured timeout.
/// - `output_truncation`: optional, one of `head` (default), `tail`, `head_tail`.
///   Controls which part of stdout/stderr is kept when output exceeds
///   `max_output_bytes`. `tail` preserves error messages and final results;
///   `head_tail` keeps both ends with a `... [truncated N bytes] ...` marker line.
#[derive(Debug, Clone)]
pub struct ShellExecutor {
    allowed_commands: HashSet<String>,
    default_timeout: Duration,
    max_output_bytes: usize,
}

impl ShellExecutor {
    pub fn new(commands: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            allowed_commands: commands.into_iter().map(Into::into).collect(),
            default_timeout: DEFAULT_SHELL_TIMEOUT,
            max_output_bytes: DEFAULT_SHELL_MAX_OUTPUT_BYTES,
        }
    }

    #[must_use]
    pub fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }
}

impl StepExecutor for ShellExecutor {
    fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
        Box::pin(async move {
            let config = ctx
                .current_step
                .executor_config()
                .ok_or_else(|| anyhow::anyhow!("shell config is required"))?;
            let config = config
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("shell config must be an object"))?;
            let command = required_string(config, "command")?;
            validate_shell_command_name(command)?;
            if !self.allowed_commands.contains(command) {
                anyhow::bail!("shell command '{}' is not allowlisted", command);
            }
            let args = parse_string_array(config.get("args"), "args")?;
            for arg in &args {
                validate_shell_arg(arg)?;
            }
            let env_entries = parse_env_entries(config.get("env"))?;
            let cwd = parse_shell_cwd(config.get("cwd"), ctx.env.working_dir())?;
            let truncation_strategy = TruncationStrategy::parse(config.get("output_truncation"))?;
            let timeout = config
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .map(Duration::from_millis)
                .unwrap_or(self.default_timeout);
            let cmdline = build_shell_command(command, &args);
            let env_refs = env_entries
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect();
            let output = ctx
                .env
                .execute_shell(
                    &cmdline,
                    ShellOptions {
                        cwd: cwd.as_deref(),
                        env: env_refs,
                        timeout: Some(timeout),
                        abort: ctx.abort.clone(),
                        on_stdout: None,
                        on_stderr: None,
                    },
                )
                .await?;
            let (stdout, stdout_truncated) =
                truncate_utf8(&output.stdout, self.max_output_bytes, truncation_strategy);
            let (stderr, stderr_truncated) =
                truncate_utf8(&output.stderr, self.max_output_bytes, truncation_strategy);
            let structured = serde_json::json!({
                "command": command,
                "args": args,
                "exit_code": output.exit_code,
                "stdout": stdout,
                "stderr": stderr,
                "stdout_truncated": stdout_truncated,
                "stderr_truncated": stderr_truncated,
            });
            Ok(StepResult {
                output: serde_json::to_string(&structured)?,
                structured: Some(structured),
                structured_status: StructuredStatus::NotRequired,
                tool_calls_count: 0,
                session_id: String::new(),
                cost: CostAggregate::default(),
                started_at: None,
                ended_at: None,
            })
        })
    }
}

/// Policy for the HTTP workflow executor.
///
/// `HttpCallExecutor` is intentionally not registered by default. Business
/// code must provide a narrow policy before workflows can perform network IO.
#[derive(Debug, Clone)]
pub struct HttpCallPolicy {
    pub allowed_hosts: HashSet<String>,
    pub allowed_schemes: HashSet<String>,
    pub allowed_methods: HashSet<Method>,
    pub max_timeout: Duration,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub allow_redirects: bool,
    pub max_redirects: usize,
    pub redact_headers: HashSet<String>,
    pub allow_private_ip_targets: bool,
}

impl HttpCallPolicy {
    pub fn new(hosts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            allowed_hosts: hosts.into_iter().map(normalize_http_host).collect(),
            allowed_schemes: ["https"].into_iter().map(ToString::to_string).collect(),
            allowed_methods: [Method::GET, Method::POST].into_iter().collect(),
            max_timeout: DEFAULT_HTTP_TIMEOUT,
            max_request_bytes: DEFAULT_HTTP_MAX_REQUEST_BYTES,
            max_response_bytes: DEFAULT_HTTP_MAX_RESPONSE_BYTES,
            allow_redirects: false,
            max_redirects: DEFAULT_HTTP_MAX_REDIRECTS,
            redact_headers: default_redacted_headers(),
            allow_private_ip_targets: false,
        }
    }

    #[must_use]
    pub fn with_allowed_schemes(
        mut self,
        schemes: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.allowed_schemes = schemes
            .into_iter()
            .map(|scheme| scheme.into().to_ascii_lowercase())
            .collect();
        self
    }

    #[must_use]
    pub fn with_allowed_methods(mut self, methods: impl IntoIterator<Item = Method>) -> Self {
        self.allowed_methods = methods.into_iter().collect();
        self
    }

    #[must_use]
    pub fn with_max_timeout(mut self, timeout: Duration) -> Self {
        self.max_timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_max_request_bytes(mut self, max_request_bytes: usize) -> Self {
        self.max_request_bytes = max_request_bytes;
        self
    }

    #[must_use]
    pub fn with_max_response_bytes(mut self, max_response_bytes: usize) -> Self {
        self.max_response_bytes = max_response_bytes;
        self
    }

    #[must_use]
    pub fn with_redirects(mut self, allow_redirects: bool, max_redirects: usize) -> Self {
        self.allow_redirects = allow_redirects;
        self.max_redirects = max_redirects;
        self
    }

    #[must_use]
    pub fn with_redact_headers(
        mut self,
        headers: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.redact_headers = headers
            .into_iter()
            .map(|header| header.into().to_ascii_lowercase())
            .collect();
        self
    }

    #[must_use]
    pub fn allow_private_ip_targets(mut self, allow_private_ip_targets: bool) -> Self {
        self.allow_private_ip_targets = allow_private_ip_targets;
        self
    }
}

/// Policy-gated HTTP executor for deterministic workflow steps.
#[derive(Debug, Clone)]
pub struct HttpCallExecutor {
    policy: HttpCallPolicy,
    client: reqwest::Client,
}

impl HttpCallExecutor {
    pub fn new(policy: HttpCallPolicy) -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("HTTP client configuration should be valid");
        Self { policy, client }
    }
}

impl StepExecutor for HttpCallExecutor {
    fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
        Box::pin(async move {
            let config = ctx
                .current_step
                .executor_config()
                .ok_or_else(|| anyhow::anyhow!("http_call config is required"))?;
            let config = config
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("http_call config must be an object"))?;
            let spec = HttpRequestSpec::parse(config, &self.policy)?;
            let started = Instant::now();
            let mut redirects = 0usize;
            let mut current_url = spec.url.clone();

            loop {
                validate_http_url(&current_url, &self.policy)?;
                let mut request = self
                    .client
                    .request(spec.method.clone(), current_url.clone())
                    .headers(spec.headers.clone())
                    .timeout(spec.timeout);
                if let Some(body) = &spec.body {
                    request = request.body(body.clone());
                }

                let response = tokio::select! {
                    biased;
                    _ = ctx.abort.cancelled() => anyhow::bail!("http_call aborted"),
                    response = request.send() => response?,
                };
                let status = response.status();

                if status.is_redirection() {
                    if !self.policy.allow_redirects {
                        anyhow::bail!(
                            "http_call redirect from {} is not allowed",
                            sanitize_url(&current_url)
                        );
                    }
                    redirects += 1;
                    if redirects > self.policy.max_redirects {
                        anyhow::bail!(
                            "http_call exceeded redirect limit {}",
                            self.policy.max_redirects
                        );
                    }
                    let location = response
                        .headers()
                        .get(LOCATION)
                        .ok_or_else(|| anyhow::anyhow!("http_call redirect missing location"))?
                        .to_str()
                        .map_err(|_| anyhow::anyhow!("http_call redirect location is invalid"))?;
                    let next_url = current_url.join(location).map_err(|err| {
                        anyhow::anyhow!("http_call redirect location is invalid: {err}")
                    })?;
                    validate_http_url(&next_url, &self.policy)?;
                    current_url = next_url;
                    continue;
                }

                if !spec.expect_status.accepts(status) {
                    anyhow::bail!(
                        "http_call status {} for {} {} did not match expectation",
                        status.as_u16(),
                        spec.method.as_str(),
                        sanitize_url(&current_url)
                    );
                }

                let response_headers = redact_header_map(response.headers(), &self.policy);
                let content_type = response
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_ascii_lowercase);
                let (body_bytes, body_truncated) =
                    read_limited_response(response, self.policy.max_response_bytes, &ctx.abort)
                        .await?;
                let body = parse_response_body(
                    &body_bytes,
                    body_truncated,
                    spec.response_format,
                    content_type.as_deref(),
                )?;
                let structured = serde_json::json!({
                    "status": status.as_u16(),
                    "url": current_url.as_str(),
                    "method": spec.method.as_str(),
                    "headers": response_headers,
                    "body": body,
                    "body_truncated": body_truncated,
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                });
                return Ok(StepResult {
                    output: serde_json::to_string(&structured)?,
                    structured: Some(structured),
                    structured_status: StructuredStatus::NotRequired,
                    tool_calls_count: 0,
                    session_id: String::new(),
                    cost: CostAggregate::default(),
                    started_at: None,
                    ended_at: None,
                });
            }
        })
    }
}

#[derive(Debug, Clone)]
struct HttpRequestSpec {
    url: Url,
    method: Method,
    headers: HeaderMap,
    body: Option<Vec<u8>>,
    timeout: Duration,
    expect_status: StatusExpectation,
    response_format: ResponseFormat,
}

impl HttpRequestSpec {
    fn parse(config: &Map<String, Value>, policy: &HttpCallPolicy) -> Result<Self> {
        let mut url = parse_http_url(required_http_string(config, "url")?, policy)?;
        append_http_query(&mut url, config.get("query"))?;
        let method = parse_http_method(config.get("method"), policy)?;
        let mut headers = parse_http_headers(config.get("headers"))?;
        let body = parse_http_body(config, &method, &mut headers, policy)?;
        let timeout = parse_http_timeout(config.get("timeout_ms"), policy.max_timeout)?;
        let expect_status = StatusExpectation::parse(config.get("expect_status"))?;
        let response_format = ResponseFormat::parse(config.get("response_format"))?;
        Ok(Self {
            url,
            method,
            headers,
            body,
            timeout,
            expect_status,
            response_format,
        })
    }
}

#[derive(Debug, Clone)]
enum StatusExpectation {
    Range(u16, u16),
    Codes(HashSet<u16>),
}

impl StatusExpectation {
    fn parse(value: Option<&Value>) -> Result<Self> {
        let Some(value) = value else {
            return Ok(Self::Range(200, 299));
        };
        if let Some(values) = value.as_array() {
            if values.is_empty() {
                anyhow::bail!("http_call expect_status array must not be empty");
            }
            return Ok(Self::Codes(
                values
                    .iter()
                    .map(|value| parse_status_code(value, "http_call expect_status[]"))
                    .collect::<Result<_>>()?,
            ));
        }
        let range = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("http_call expect_status must be an array or object"))?;
        let min = range
            .get("min")
            .ok_or_else(|| anyhow::anyhow!("http_call expect_status.min is required"))
            .and_then(|value| parse_status_code(value, "http_call expect_status.min"))?;
        let max = range
            .get("max")
            .ok_or_else(|| anyhow::anyhow!("http_call expect_status.max is required"))
            .and_then(|value| parse_status_code(value, "http_call expect_status.max"))?;
        if min > max {
            anyhow::bail!("http_call expect_status.min must be <= max");
        }
        Ok(Self::Range(min, max))
    }

    fn accepts(&self, status: StatusCode) -> bool {
        let code = status.as_u16();
        match self {
            Self::Range(min, max) => (*min..=*max).contains(&code),
            Self::Codes(codes) => codes.contains(&code),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ResponseFormat {
    Auto,
    Json,
    Text,
    BytesBase64,
}

impl ResponseFormat {
    fn parse(value: Option<&Value>) -> Result<Self> {
        let Some(value) = value else {
            return Ok(Self::Auto);
        };
        match value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("http_call response_format must be a string"))?
        {
            "auto" => Ok(Self::Auto),
            "json" => Ok(Self::Json),
            "text" => Ok(Self::Text),
            "bytes_base64" => Ok(Self::BytesBase64),
            other => anyhow::bail!("http_call response_format '{}' is not supported", other),
        }
    }
}

fn default_redacted_headers() -> HashSet<String> {
    ["authorization", "cookie", "set-cookie", "x-api-key"]
        .into_iter()
        .map(ToString::to_string)
        .collect()
}

fn normalize_http_host(host: impl Into<String>) -> String {
    host.into().trim_end_matches('.').to_ascii_lowercase()
}

fn required_http_string<'a>(config: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    config
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("http_call config '{}' must be a non-empty string", key))
}

fn parse_http_url(value: &str, policy: &HttpCallPolicy) -> Result<Url> {
    let url =
        Url::parse(value).map_err(|err| anyhow::anyhow!("http_call url is invalid: {err}"))?;
    validate_http_url(&url, policy)?;
    Ok(url)
}

fn validate_http_url(url: &Url, policy: &HttpCallPolicy) -> Result<()> {
    let scheme = url.scheme().to_ascii_lowercase();
    if !policy.allowed_schemes.contains(&scheme) {
        anyhow::bail!("http_call scheme '{}' is not allowlisted", url.scheme());
    }
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("http_call url credentials are not allowed");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("http_call url must include a host"))?;
    let normalized_host = normalize_http_host(host);
    if !policy.allowed_hosts.contains(&normalized_host) {
        anyhow::bail!("http_call host '{}' is not allowlisted", normalized_host);
    }
    if !policy.allow_private_ip_targets
        && normalized_host
            .parse::<IpAddr>()
            .is_ok_and(is_forbidden_ip_target)
    {
        anyhow::bail!(
            "http_call private or local IP target '{}' is not allowed",
            normalized_host
        );
    }
    Ok(())
}

fn is_forbidden_ip_target(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_forbidden_ipv4_target(ip),
        IpAddr::V6(ip) => is_forbidden_ipv6_target(ip),
    }
}

fn is_forbidden_ipv4_target(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 0
        || octets[0] == 10
        || octets[0] == 127
        || (octets[0] == 169 && octets[1] == 254)
        || (octets[0] == 172 && (16..=31).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 168)
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
        || octets[0] >= 224
        || ip == Ipv4Addr::new(255, 255, 255, 255)
}

fn is_forbidden_ipv6_target(ip: Ipv6Addr) -> bool {
    if let Some(ipv4) = ip.to_ipv4_mapped() {
        return is_forbidden_ipv4_target(ipv4);
    }
    let segments = ip.segments();
    ip == Ipv6Addr::UNSPECIFIED
        || ip == Ipv6Addr::LOCALHOST
        || (segments[0] & 0xff00) == 0xff00
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
}

fn append_http_query(url: &mut Url, value: Option<&Value>) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let query = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("http_call query must be an object"))?;
    let mut pairs = url.query_pairs_mut();
    for (key, value) in query {
        let value = match value {
            Value::String(value) => value.clone(),
            Value::Bool(value) => value.to_string(),
            Value::Number(value) => value.to_string(),
            _ => anyhow::bail!(
                "http_call query value for '{}' must be a string, number, or bool",
                key
            ),
        };
        pairs.append_pair(key, &value);
    }
    Ok(())
}

fn parse_http_method(value: Option<&Value>, policy: &HttpCallPolicy) -> Result<Method> {
    let method = value
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow::anyhow!("http_call method must be a non-empty string"))
        })
        .transpose()?
        .unwrap_or("GET");
    let method = method.to_ascii_uppercase();
    let method = Method::from_bytes(method.as_bytes())
        .map_err(|err| anyhow::anyhow!("http_call method is invalid: {err}"))?;
    if !policy.allowed_methods.contains(&method) {
        anyhow::bail!("http_call method '{}' is not allowlisted", method.as_str());
    }
    Ok(method)
}

fn parse_http_headers(value: Option<&Value>) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    let Some(value) = value else {
        return Ok(headers);
    };
    let entries = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("http_call headers must be an object"))?;
    for (key, value) in entries {
        if is_hop_by_hop_header(key) {
            anyhow::bail!("http_call header '{}' is not allowed", key);
        }
        let value = value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("http_call header '{}' must be a string", key))?;
        let name = HeaderName::from_bytes(key.as_bytes())
            .map_err(|err| anyhow::anyhow!("http_call header name '{}' is invalid: {err}", key))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| anyhow::anyhow!("http_call header value for '{}' is invalid", key))?;
        headers.insert(name, value);
    }
    Ok(headers)
}

fn is_hop_by_hop_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "transfer-encoding"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "upgrade"
    )
}

fn parse_http_body(
    config: &Map<String, Value>,
    method: &Method,
    headers: &mut HeaderMap,
    policy: &HttpCallPolicy,
) -> Result<Option<Vec<u8>>> {
    let body_json = config.get("body_json");
    let body_text = config.get("body_text");
    if body_json.is_some() && body_text.is_some() {
        anyhow::bail!("http_call body_json and body_text are mutually exclusive");
    }
    let body = if let Some(body_json) = body_json {
        if !headers.contains_key(CONTENT_TYPE) {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        Some(serde_json::to_vec(body_json)?)
    } else if let Some(body_text) = body_text {
        Some(
            body_text
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("http_call body_text must be a string"))?
                .as_bytes()
                .to_vec(),
        )
    } else {
        None
    };
    if body.is_some() && (*method == Method::GET || *method == Method::HEAD) {
        anyhow::bail!("http_call GET and HEAD requests must not include a body");
    }
    if let Some(body) = &body
        && body.len() > policy.max_request_bytes
    {
        anyhow::bail!(
            "http_call request body exceeds {} bytes",
            policy.max_request_bytes
        );
    }
    Ok(body)
}

fn parse_http_timeout(value: Option<&Value>, max_timeout: Duration) -> Result<Duration> {
    let timeout = value
        .map(|value| {
            value
                .as_u64()
                .map(Duration::from_millis)
                .ok_or_else(|| anyhow::anyhow!("http_call timeout_ms must be an unsigned integer"))
        })
        .transpose()?
        .unwrap_or(max_timeout);
    if timeout > max_timeout {
        anyhow::bail!("http_call timeout_ms exceeds policy maximum");
    }
    if timeout.is_zero() {
        anyhow::bail!("http_call timeout_ms must be greater than zero");
    }
    Ok(timeout)
}

fn parse_status_code(value: &Value, name: &str) -> Result<u16> {
    let code = value
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| anyhow::anyhow!("{name} must be a valid HTTP status code"))?;
    StatusCode::from_u16(code)
        .map_err(|_| anyhow::anyhow!("{name} must be a valid HTTP status code"))?;
    Ok(code)
}

fn sanitize_url(url: &Url) -> String {
    let host = url.host_str().unwrap_or("<missing-host>");
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let port = url
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    format!("{}://{}{}{}", url.scheme(), host, port, url.path())
}

fn redact_header_map(headers: &HeaderMap, policy: &HttpCallPolicy) -> Value {
    let mut redacted = Map::new();
    for (name, value) in headers {
        let key = name.as_str().to_ascii_lowercase();
        let value = if policy.redact_headers.contains(&key) {
            Value::String("[REDACTED]".to_string())
        } else {
            Value::String(
                value
                    .to_str()
                    .map(ToString::to_string)
                    .unwrap_or_else(|_| "<non-utf8>".to_string()),
            )
        };
        match redacted.get_mut(&key) {
            Some(Value::Array(values)) => values.push(value),
            Some(existing) => {
                let first = existing.take();
                *existing = Value::Array(vec![first, value]);
            }
            None => {
                redacted.insert(key, value);
            }
        }
    }
    Value::Object(redacted)
}

async fn read_limited_response(
    response: reqwest::Response,
    max_bytes: usize,
    abort: &CancellationToken,
) -> Result<(Vec<u8>, bool)> {
    let mut body = Vec::new();
    let mut body_truncated = false;
    let mut stream = response.bytes_stream();
    loop {
        let next = tokio::select! {
            biased;
            _ = abort.cancelled() => anyhow::bail!("http_call aborted"),
            next = stream.next() => next,
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk?;
        if body.len() + chunk.len() > max_bytes {
            let remaining = max_bytes.saturating_sub(body.len());
            body.extend_from_slice(&chunk[..remaining]);
            body_truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, body_truncated))
}

fn parse_response_body(
    body: &[u8],
    body_truncated: bool,
    response_format: ResponseFormat,
    content_type: Option<&str>,
) -> Result<Value> {
    match response_format {
        ResponseFormat::Json => {
            if body_truncated {
                anyhow::bail!("http_call JSON response was truncated");
            }
            serde_json::from_slice(body)
                .map_err(|err| anyhow::anyhow!("http_call response body is not valid JSON: {err}"))
        }
        ResponseFormat::Text => Ok(Value::String(String::from_utf8_lossy(body).into_owned())),
        ResponseFormat::BytesBase64 => Ok(Value::String(BASE64_STANDARD.encode(body))),
        ResponseFormat::Auto => {
            if !body_truncated
                && content_type.is_some_and(|content_type| {
                    content_type.contains("application/json") || content_type.contains("+json")
                })
            {
                serde_json::from_slice(body).map_err(|err| {
                    anyhow::anyhow!("http_call response body is not valid JSON: {err}")
                })
            } else {
                Ok(Value::String(String::from_utf8_lossy(body).into_owned()))
            }
        }
    }
}

fn required_string<'a>(config: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    config
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("shell config '{}' must be a non-empty string", key))
}

fn parse_string_array(value: Option<&Value>, key: &str) -> Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(vec![]);
    };
    let values = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("shell config '{}' must be an array", key))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(ToString::to_string)
                .ok_or_else(|| anyhow::anyhow!("shell config '{}[]' must be a string", key))
        })
        .collect()
}

fn parse_env_entries(value: Option<&Value>) -> Result<Vec<(String, String)>> {
    let Some(value) = value else {
        return Ok(vec![]);
    };
    let entries = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("shell config 'env' must be an object"))?;
    entries
        .iter()
        .map(|(key, value)| {
            if !is_safe_env_key(key) {
                anyhow::bail!("shell env key '{}' is not allowed", key);
            }
            let value = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("shell env value for '{}' must be a string", key))?;
            validate_shell_arg(value)?;
            Ok((key.clone(), value.to_string()))
        })
        .collect()
}

fn parse_shell_cwd(value: Option<&Value>, base: &std::path::Path) -> Result<Option<PathBuf>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let cwd = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("shell config 'cwd' must be a string"))?;
    if cwd.is_empty() {
        return Ok(None);
    }
    let relative = PathBuf::from(cwd);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        anyhow::bail!("shell cwd must be a relative path without parent traversal");
    }
    Ok(Some(base.join(relative)))
}

fn validate_shell_command_name(command: &str) -> Result<()> {
    if command.chars().any(|ch| {
        ch.is_whitespace()
            || matches!(
                ch,
                '\0' | '"' | '\'' | '`' | '$' | '&' | '|' | ';' | '<' | '>' | '\r' | '\n'
            )
    }) {
        anyhow::bail!("shell command contains unsafe characters");
    }
    Ok(())
}

fn validate_shell_arg(arg: &str) -> Result<()> {
    if arg.chars().any(|ch| matches!(ch, '\0' | '\r' | '\n')) {
        anyhow::bail!("shell argument contains unsafe control characters");
    }
    Ok(())
}

fn is_safe_env_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        && key
            .chars()
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
}

fn build_shell_command(command: &str, args: &[String]) -> String {
    std::iter::once(command.to_string())
        .chain(args.iter().map(|arg| quote_shell_arg(arg)))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(windows)]
fn quote_shell_arg(arg: &str) -> String {
    let escaped = arg.replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(not(windows))]
fn quote_shell_arg(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Shell output truncation strategy when content exceeds `max_output_bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TruncationStrategy {
    /// Keep the first `max_bytes` (default, backward-compatible).
    #[default]
    Head,
    /// Keep the last `max_bytes` — useful for error messages and final results.
    Tail,
    /// Keep the first `max_bytes / 2` and the last `max_bytes / 2`, joining
    /// them with a marker line. Preserves both the start and the end of
    /// long outputs.
    HeadTail,
}

impl TruncationStrategy {
    fn parse(value: Option<&Value>) -> Result<Self> {
        let Some(value) = value else {
            return Ok(Self::default());
        };
        match value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("shell config 'output_truncation' must be a string"))?
        {
            "head" => Ok(Self::Head),
            "tail" => Ok(Self::Tail),
            "head_tail" => Ok(Self::HeadTail),
            other => anyhow::bail!(
                "shell config 'output_truncation' '{}' is not supported",
                other
            ),
        }
    }
}

fn format_truncation_marker(omitted_bytes: usize) -> String {
    format!("\n... [truncated {omitted_bytes} bytes] ...\n")
}

fn truncate_utf8(value: &str, max_bytes: usize, strategy: TruncationStrategy) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_string(), false);
    }
    match strategy {
        TruncationStrategy::Head => {
            let end = value.floor_char_boundary(max_bytes);
            (value[..end].to_string(), true)
        }
        TruncationStrategy::Tail => {
            let start = value.ceil_char_boundary(value.len() - max_bytes);
            (value[start..].to_string(), true)
        }
        TruncationStrategy::HeadTail => {
            // Reserve marker budget using the maximum possible omitted-byte
            // count (value.len()), so the final marker — which reports the
            // actual omitted bytes — can never grow a digit and overflow.
            let max_marker = format_truncation_marker(value.len());
            if max_marker.len() >= max_bytes {
                let end = value.floor_char_boundary(max_bytes);
                return (value[..end].to_string(), true);
            }
            let budget = max_bytes - max_marker.len();
            let half = budget / 2;
            let head_end = value.floor_char_boundary(half);
            // Tail gets the remaining budget; floor on head_end may have
            // rounded down, so give the slack to the tail.
            let tail_budget = budget - head_end;
            let tail_start = value
                .ceil_char_boundary(value.len().saturating_sub(tail_budget))
                .max(head_end);
            let actual_omitted = tail_start - head_end;
            let marker = format_truncation_marker(actual_omitted);
            let mut result =
                String::with_capacity(head_end + marker.len() + (value.len() - tail_start));
            result.push_str(&value[..head_end]);
            result.push_str(&marker);
            result.push_str(&value[tail_start..]);
            // Invariant: result.len() <= head_end + max_marker.len() + tail_budget
            //          = budget + max_marker.len() = max_bytes.
            (result, true)
        }
    }
}

fn select_input(config: &Map<String, Value>, ctx: &ExecutorCtx<'_>) -> Result<Value> {
    let source = config
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("auto");
    match source {
        "auto" => {
            if let Some(input) = config.get("input") {
                Ok(input.clone())
            } else if let Some(prev) = ctx.prev_result {
                Ok(prev
                    .structured
                    .clone()
                    .unwrap_or_else(|| Value::String(prev.output.clone())))
            } else {
                anyhow::bail!("json_transform requires config.input or a previous result")
            }
        }
        "config.input" => config
            .get("input")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("json_transform source config.input requires input")),
        "prev_result.structured" => ctx
            .prev_result
            .and_then(|prev| prev.structured.clone())
            .ok_or_else(|| anyhow::anyhow!("json_transform requires prev_result.structured")),
        "prev_result.output" => ctx
            .prev_result
            .map(|prev| Value::String(prev.output.clone()))
            .ok_or_else(|| anyhow::anyhow!("json_transform requires prev_result.output")),
        other => anyhow::bail!("json_transform source '{}' is not supported", other),
    }
}

fn build_fields(fields: &Value, input: &Value, config: &Map<String, Value>) -> Result<Value> {
    let fields = fields
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("json_transform.fields must be an object"))?;
    let include_nulls = config
        .get("include_nulls")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let mut output = Map::new();
    for (name, spec) in fields {
        let value = resolve_field_spec(spec, input)?;
        if include_nulls || !value.is_null() {
            output.insert(name.clone(), value);
        }
    }
    Ok(Value::Object(output))
}

fn resolve_field_spec(spec: &Value, input: &Value) -> Result<Value> {
    if let Some(pointer) = spec.as_str() {
        return Ok(read_pointer(input, pointer)?.clone());
    }
    if let Some(spec_obj) = spec.as_object() {
        if let Some(pointer) = spec_obj.get("pointer") {
            let pointer = pointer
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("json_transform field pointer must be a string"))?;
            return match read_pointer(input, pointer) {
                Ok(value) => Ok(value.clone()),
                Err(err) => {
                    if let Some(default) = spec_obj.get("default") {
                        Ok(default.clone())
                    } else {
                        Err(err)
                    }
                }
            };
        }
        if let Some(value) = spec_obj.get("value") {
            return Ok(value.clone());
        }
    }
    Ok(spec.clone())
}

fn read_pointer<'a>(input: &'a Value, pointer: &str) -> Result<&'a Value> {
    input
        .pointer(pointer)
        .ok_or_else(|| anyhow::anyhow!("json_transform pointer '{}' not found", pointer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::{EnvError, FileInfo, ShellOutput};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    type EnvResult<T> = std::result::Result<T, EnvError>;

    fn executor_step(config: Value) -> Step {
        Step::executor(
            "transform",
            "Transform",
            JSON_TRANSFORM_EXECUTOR_NAME,
            Some(config),
        )
    }

    fn http_step(config: Value) -> Step {
        Step::executor("http", "HTTP", HTTP_CALL_EXECUTOR_NAME, Some(config))
    }
    fn prev_result(structured: Option<Value>, output: impl Into<String>) -> StepResult {
        StepResult {
            output: output.into(),
            structured,
            structured_status: StructuredStatus::NotRequired,
            tool_calls_count: 0,
            session_id: "prev".into(),
            cost: CostAggregate::default(),
            started_at: None,
            ended_at: None,
        }
    }

    fn noop_env() -> Arc<dyn ExecutionEnv> {
        Arc::new(llm_harness_loop::test_utils::NoOpEnv)
    }

    fn ctx_for<'a>(step: &'a Step, prev_result: Option<&'a StepResult>) -> ExecutorCtx<'a> {
        ExecutorCtx {
            current_step: step,
            prev_result,
            step_history: &[],
            env: noop_env(),
            abort: CancellationToken::new(),
            context: Arc::new(tokio::sync::Mutex::new(WorkflowContext::new())),
        }
    }

    struct TestHttpResponse {
        status: u16,
        reason: &'static str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        delay: Option<Duration>,
    }

    impl TestHttpResponse {
        fn new(status: u16, headers: Vec<(&str, &str)>, body: impl Into<Vec<u8>>) -> Self {
            Self {
                status,
                reason: match status {
                    200 => "OK",
                    302 => "Found",
                    404 => "Not Found",
                    _ => "OK",
                },
                headers: headers
                    .into_iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect(),
                body: body.into(),
                delay: None,
            }
        }

        fn into_bytes(self) -> Vec<u8> {
            let mut response = format!(
                "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                self.status,
                self.reason,
                self.body.len()
            );
            for (key, value) in self.headers {
                response.push_str(&format!("{key}: {value}\r\n"));
            }
            response.push_str("\r\n");
            let mut bytes = response.into_bytes();
            bytes.extend_from_slice(&self.body);
            bytes
        }
    }

    async fn spawn_test_http_server(
        handler: impl Fn(String) -> TestHttpResponse + Send + Sync + 'static,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handler = Arc::new(handler);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 4096];
                    let mut read = 0usize;
                    loop {
                        if read == buffer.len() {
                            buffer.resize(buffer.len() * 2, 0);
                        }
                        match socket.read(&mut buffer[read..]).await {
                            Ok(0) => return,
                            Ok(size) => {
                                read += size;
                                if buffer[..read]
                                    .windows(4)
                                    .any(|window| window == b"\r\n\r\n")
                                {
                                    break;
                                }
                            }
                            Err(_) => return,
                        }
                    }
                    let request = String::from_utf8_lossy(&buffer[..read]);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    let response = handler(path);
                    if let Some(delay) = response.delay {
                        tokio::time::sleep(delay).await;
                    }
                    let _ = socket.write_all(&response.into_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn local_http_executor(max_response_bytes: usize) -> HttpCallExecutor {
        HttpCallExecutor::new(
            HttpCallPolicy::new(["127.0.0.1"])
                .with_allowed_schemes(["http"])
                .with_max_response_bytes(max_response_bytes)
                .allow_private_ip_targets(true),
        )
    }

    #[tokio::test]
    async fn json_transform_selects_pointer_from_config_input() {
        let step = executor_step(serde_json::json!({
            "source": "config.input",
            "input": {"device": {"voltage": 3.3}},
            "pointer": "/device/voltage"
        }));

        let ctx = ctx_for(&step, None);

        let result = JsonTransformExecutor.execute(&ctx).await.unwrap();

        assert_eq!(result.structured, Some(serde_json::json!(3.3)));
        assert_eq!(result.output, "3.3");
    }

    #[tokio::test]
    async fn json_transform_builds_fields_from_previous_structured_result() {
        let prev = prev_result(
            Some(serde_json::json!({
                "user": {"id": 7, "name": "Ada"},
                "status": "ok"
            })),
            "ignored",
        );
        let step = executor_step(serde_json::json!({
            "fields": {
                "id": "/user/id",
                "name": {"pointer": "/user/name"},
                "missing": {"pointer": "/not/found", "default": "fallback"},
                "constant": {"value": true}
            }
        }));
        let ctx = ctx_for(&step, Some(&prev));

        let result = JsonTransformExecutor.execute(&ctx).await.unwrap();

        assert_eq!(
            result.structured,
            Some(serde_json::json!({
                "id": 7,
                "name": "Ada",
                "missing": "fallback",
                "constant": true
            }))
        );
    }

    #[tokio::test]
    async fn json_transform_rejects_missing_pointer_without_default() {
        let step = executor_step(serde_json::json!({
            "source": "config.input",
            "input": {"value": 1},
            "fields": {"bad": "/missing"}
        }));
        let ctx = ctx_for(&step, None);

        let err = JsonTransformExecutor.execute(&ctx).await.unwrap_err();

        assert!(err.to_string().contains("pointer '/missing' not found"));
    }

    struct FakeEnv {
        last_cmd: Mutex<Option<String>>,
        last_timeout: Mutex<Option<Duration>>,
        last_env: Mutex<Vec<(String, String)>>,
        stdout: Mutex<Option<String>>,
    }

    impl Default for FakeEnv {
        fn default() -> Self {
            Self {
                last_cmd: Mutex::new(None),
                last_timeout: Mutex::new(None),
                last_env: Mutex::new(vec![]),
                stdout: Mutex::new(None),
            }
        }
    }

    impl FakeEnv {
        fn with_stdout(stdout: impl Into<String>) -> Self {
            let env = Self::default();
            *env.stdout.lock().unwrap() = Some(stdout.into());
            env
        }
    }

    impl ExecutionEnv for FakeEnv {
        fn working_dir(&self) -> &Path {
            Path::new("/workspace")
        }
        fn read_text_file<'a>(
            &'a self,
            path: &'a Path,
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<String>> {
            let path = path.to_path_buf();
            Box::pin(async move { Err(EnvError::NotFound(path)) })
        }

        fn read_text_lines<'a>(
            &'a self,
            path: &'a Path,
            _: Option<usize>,
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<Vec<String>>> {
            let path = path.to_path_buf();
            Box::pin(async move { Err(EnvError::NotFound(path)) })
        }

        fn read_binary_file<'a>(
            &'a self,
            path: &'a Path,
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<Vec<u8>>> {
            let path = path.to_path_buf();
            Box::pin(async move { Err(EnvError::NotFound(path)) })
        }

        fn write_file<'a>(
            &'a self,
            path: &'a Path,
            _: &'a [u8],
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<()>> {
            let path = path.to_path_buf();
            Box::pin(async move { Err(EnvError::PermissionDenied(path)) })
        }

        fn append_file<'a>(
            &'a self,
            path: &'a Path,
            _: &'a [u8],
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<()>> {
            let path = path.to_path_buf();
            Box::pin(async move { Err(EnvError::PermissionDenied(path)) })
        }

        fn file_info<'a>(
            &'a self,
            path: &'a Path,
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<FileInfo>> {
            let path = path.to_path_buf();
            Box::pin(async move { Err(EnvError::NotFound(path)) })
        }

        fn list_dir<'a>(
            &'a self,
            path: &'a Path,
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<Vec<FileInfo>>> {
            let path = path.to_path_buf();
            Box::pin(async move { Err(EnvError::NotFound(path)) })
        }

        fn exists<'a>(
            &'a self,
            _: &'a Path,
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<bool>> {
            Box::pin(async { Ok(false) })
        }

        fn create_dir<'a>(
            &'a self,
            path: &'a Path,
            _: bool,
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<()>> {
            let path = path.to_path_buf();
            Box::pin(async move { Err(EnvError::PermissionDenied(path)) })
        }

        fn remove<'a>(
            &'a self,
            path: &'a Path,
            _: bool,
            _: bool,
            _: CancellationToken,
        ) -> BoxFuture<'a, EnvResult<()>> {
            let path = path.to_path_buf();
            Box::pin(async move { Err(EnvError::PermissionDenied(path)) })
        }

        fn create_temp_dir<'a>(&'a self, _: &'a str) -> BoxFuture<'a, EnvResult<PathBuf>> {
            Box::pin(async { Err(EnvError::Other("not implemented".into())) })
        }

        fn execute_shell<'a>(
            &'a self,
            cmd: &'a str,
            opts: ShellOptions<'a>,
        ) -> BoxFuture<'a, EnvResult<ShellOutput>> {
            *self.last_cmd.lock().unwrap() = Some(cmd.to_string());
            *self.last_timeout.lock().unwrap() = opts.timeout;
            *self.last_env.lock().unwrap() = opts
                .env
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect();
            Box::pin(async {
                let stdout = self
                    .stdout
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| "ok\n".to_string());
                Ok(ShellOutput {
                    stdout,
                    stderr: String::new(),
                    exit_code: 0,
                })
            })
        }

        fn cleanup<'a>(&'a self) -> BoxFuture<'a, EnvResult<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn shell_executor_rejects_command_not_allowlisted() {
        let step = Step::executor(
            "shell",
            "Shell",
            SHELL_EXECUTOR_NAME,
            Some(serde_json::json!({"command": "rm", "args": ["-rf", "target"]})),
        );
        let ctx = ctx_for(&step, None);
        let executor = ShellExecutor::new(["echo"]);

        let err = executor.execute(&ctx).await.unwrap_err();

        assert!(err.to_string().contains("not allowlisted"));
    }

    #[tokio::test]
    async fn shell_executor_runs_allowed_command_through_env() {
        let step = Step::executor(
            "shell",
            "Shell",
            SHELL_EXECUTOR_NAME,
            Some(serde_json::json!({
                "command": "python",
                "args": ["-c", "print('ok')"],
                "env": {"RUN_MODE": "test"},
                "timeout_ms": 250
            })),
        );
        let env = Arc::new(FakeEnv::default());
        let ctx = ExecutorCtx {
            current_step: &step,
            prev_result: None,
            step_history: &[],
            env: env.clone(),
            abort: CancellationToken::new(),
            context: Arc::new(tokio::sync::Mutex::new(WorkflowContext::new())),
        };
        let executor = ShellExecutor::new(["python"]);

        let result = executor.execute(&ctx).await.unwrap();

        assert_eq!(result.structured.as_ref().unwrap()["stdout"], "ok\n");
        assert_eq!(
            *env.last_timeout.lock().unwrap(),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            *env.last_env.lock().unwrap(),
            vec![("RUN_MODE".into(), "test".into())]
        );
        assert!(
            env.last_cmd
                .lock()
                .unwrap()
                .as_deref()
                .unwrap_or("")
                .contains("python")
        );
    }

    #[tokio::test]
    async fn shell_executor_rejects_parent_traversal_cwd() {
        let step = Step::executor(
            "shell",
            "Shell",
            SHELL_EXECUTOR_NAME,
            Some(serde_json::json!({
                "command": "echo",
                "args": ["ok"],
                "cwd": "../outside"
            })),
        );
        let ctx = ctx_for(&step, None);
        let executor = ShellExecutor::new(["echo"]);

        let err = executor.execute(&ctx).await.unwrap_err();

        assert!(err.to_string().contains("parent traversal"));
    }

    #[tokio::test]
    async fn shell_executor_tail_truncation_keeps_end() {
        let long_output = "HEAD1234567890TAIL";
        let step = Step::executor(
            "shell",
            "Shell",
            SHELL_EXECUTOR_NAME,
            Some(serde_json::json!({
                "command": "echo",
                "args": ["x"],
                "output_truncation": "tail"
            })),
        );
        let env = Arc::new(FakeEnv::with_stdout(long_output));
        let ctx = ExecutorCtx {
            current_step: &step,
            prev_result: None,
            step_history: &[],
            env: env.clone(),
            abort: CancellationToken::new(),
            context: Arc::new(tokio::sync::Mutex::new(WorkflowContext::new())),
        };
        let executor = ShellExecutor::new(["echo"]).with_max_output_bytes(8);

        let result = executor.execute(&ctx).await.unwrap();
        let structured = result.structured.unwrap();

        assert_eq!(structured["stdout"], "7890TAIL");
        assert_eq!(structured["stdout_truncated"], true);
    }

    #[tokio::test]
    async fn shell_executor_head_tail_truncation_keeps_both_ends() {
        let long_output = "AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHHIIIIJJJJKKKKLLLL"; // 48 bytes
        let step = Step::executor(
            "shell",
            "Shell",
            SHELL_EXECUTOR_NAME,
            Some(serde_json::json!({
                "command": "echo",
                "args": ["x"],
                "output_truncation": "head_tail"
            })),
        );
        let env = Arc::new(FakeEnv::with_stdout(long_output));
        let ctx = ExecutorCtx {
            current_step: &step,
            prev_result: None,
            step_history: &[],
            env: env.clone(),
            abort: CancellationToken::new(),
            context: Arc::new(tokio::sync::Mutex::new(WorkflowContext::new())),
        };
        let executor = ShellExecutor::new(["echo"]).with_max_output_bytes(40);

        let result = executor.execute(&ctx).await.unwrap();
        let structured = result.structured.unwrap();
        let stdout = structured["stdout"].as_str().unwrap();

        assert!(stdout.starts_with("AAAA"));
        assert!(stdout.ends_with("LLLL"));
        assert!(
            stdout.contains("[truncated "),
            "expected byte-count marker in: {stdout}"
        );
        assert!(
            stdout.len() <= 40,
            "head_tail output must respect max_output_bytes, got {} bytes: {stdout}",
            stdout.len()
        );
        assert_eq!(structured["stdout_truncated"], true);
    }

    #[tokio::test]
    async fn shell_executor_truncation_respects_multibyte_utf8() {
        // Each '中' is 3 bytes in UTF-8; 10 chars = 30 bytes. With max_bytes
        // = 8 and the head strategy, the cut point lands inside a multi-byte
        // sequence and must snap back to a char boundary.
        let long_output = "中中中中中中中中中中";
        let step = Step::executor(
            "shell",
            "Shell",
            SHELL_EXECUTOR_NAME,
            Some(serde_json::json!({
                "command": "echo",
                "args": ["x"],
                "output_truncation": "head"
            })),
        );
        let env = Arc::new(FakeEnv::with_stdout(long_output));
        let ctx = ExecutorCtx {
            current_step: &step,
            prev_result: None,
            step_history: &[],
            env: env.clone(),
            abort: CancellationToken::new(),
            context: Arc::new(tokio::sync::Mutex::new(WorkflowContext::new())),
        };
        let executor = ShellExecutor::new(["echo"]).with_max_output_bytes(8);

        let result = executor.execute(&ctx).await.unwrap();
        let structured = result.structured.unwrap();
        let stdout = structured["stdout"].as_str().unwrap();

        // 8 bytes / 3 bytes-per-char → floor to 6 bytes (2 chars). No partial
        // code units, no replacement chars, within budget.
        assert_eq!(stdout, "中中");
        assert!(stdout.len() <= 8, "got {} bytes", stdout.len());
        assert_eq!(structured["stdout_truncated"], true);
    }

    #[tokio::test]
    async fn shell_executor_tail_truncation_respects_multibyte_utf8() {
        // 10× '中' = 30 bytes. Tail with max_bytes=8 must keep the last 2
        // complete chars (6 bytes), snapping forward to a char boundary.
        let long_output = "中中中中中中中中中中";
        let step = Step::executor(
            "shell",
            "Shell",
            SHELL_EXECUTOR_NAME,
            Some(serde_json::json!({
                "command": "echo",
                "args": ["x"],
                "output_truncation": "tail"
            })),
        );
        let env = Arc::new(FakeEnv::with_stdout(long_output));
        let ctx = ExecutorCtx {
            current_step: &step,
            prev_result: None,
            step_history: &[],
            env: env.clone(),
            abort: CancellationToken::new(),
            context: Arc::new(tokio::sync::Mutex::new(WorkflowContext::new())),
        };
        let executor = ShellExecutor::new(["echo"]).with_max_output_bytes(8);

        let result = executor.execute(&ctx).await.unwrap();
        let structured = result.structured.unwrap();
        let stdout = structured["stdout"].as_str().unwrap();

        assert_eq!(stdout, "中中");
        assert!(stdout.len() <= 8, "got {} bytes", stdout.len());
        assert_eq!(structured["stdout_truncated"], true);
    }

    #[tokio::test]
    async fn shell_executor_head_tail_truncation_respects_multibyte_utf8() {
        // 100× '中' = 300 bytes. head_tail with a budget large enough for the
        // marker must keep both ends on char boundaries and stay within budget.
        let long_output = "中".repeat(100); // 300 bytes
        let step = Step::executor(
            "shell",
            "Shell",
            SHELL_EXECUTOR_NAME,
            Some(serde_json::json!({
                "command": "echo",
                "args": ["x"],
                "output_truncation": "head_tail"
            })),
        );
        let env = Arc::new(FakeEnv::with_stdout(long_output));
        let ctx = ExecutorCtx {
            current_step: &step,
            prev_result: None,
            step_history: &[],
            env: env.clone(),
            abort: CancellationToken::new(),
            context: Arc::new(tokio::sync::Mutex::new(WorkflowContext::new())),
        };
        let executor = ShellExecutor::new(["echo"]).with_max_output_bytes(60);

        let result = executor.execute(&ctx).await.unwrap();
        let structured = result.structured.unwrap();
        let stdout = structured["stdout"].as_str().unwrap();

        assert!(
            stdout.len() <= 60,
            "head_tail must respect max_output_bytes, got {} bytes: {stdout}",
            stdout.len()
        );
        assert!(
            stdout.contains("[truncated "),
            "expected marker in: {stdout}"
        );
        // String is still valid UTF-8 (no replacement char) — `as_str` above
        // would have panicked otherwise. Verify it starts and ends with '中'.
        assert!(stdout.starts_with('中'));
        assert!(stdout.ends_with('中'));
        assert_eq!(structured["stdout_truncated"], true);
    }

    #[tokio::test]
    async fn shell_executor_default_truncation_is_head() {
        let long_output = "AAAABBBBCCCCDDDD";
        let step = Step::executor(
            "shell",
            "Shell",
            SHELL_EXECUTOR_NAME,
            Some(serde_json::json!({
                "command": "echo",
                "args": ["x"]
            })),
        );
        let env = Arc::new(FakeEnv::with_stdout(long_output));
        let ctx = ExecutorCtx {
            current_step: &step,
            prev_result: None,
            step_history: &[],
            env: env.clone(),
            abort: CancellationToken::new(),
            context: Arc::new(tokio::sync::Mutex::new(WorkflowContext::new())),
        };
        let executor = ShellExecutor::new(["echo"]).with_max_output_bytes(8);

        let result = executor.execute(&ctx).await.unwrap();
        let structured = result.structured.unwrap();

        assert_eq!(structured["stdout"], "AAAABBBB");
        assert_eq!(structured["stdout_truncated"], true);
    }

    #[tokio::test]
    async fn shell_executor_rejects_invalid_truncation_strategy() {
        let step = Step::executor(
            "shell",
            "Shell",
            SHELL_EXECUTOR_NAME,
            Some(serde_json::json!({
                "command": "echo",
                "args": ["x"],
                "output_truncation": "middle"
            })),
        );
        let ctx = ctx_for(&step, None);
        let executor = ShellExecutor::new(["echo"]);

        let err = executor.execute(&ctx).await.unwrap_err();

        assert!(err.to_string().contains("not supported"));
    }

    #[tokio::test]
    async fn http_executor_rejects_host_not_allowlisted() {
        let step = http_step(serde_json::json!({
            "url": "https://forbidden.example/v1/items"
        }));
        let ctx = ctx_for(&step, None);
        let executor = HttpCallExecutor::new(HttpCallPolicy::new(["api.example.com"]));

        let err = executor.execute(&ctx).await.unwrap_err();

        assert!(err.to_string().contains("host 'forbidden.example'"));
    }

    #[tokio::test]
    async fn http_executor_rejects_http_by_default() {
        let step = http_step(serde_json::json!({
            "url": "http://api.example.com/v1/items"
        }));
        let ctx = ctx_for(&step, None);
        let executor = HttpCallExecutor::new(HttpCallPolicy::new(["api.example.com"]));

        let err = executor.execute(&ctx).await.unwrap_err();

        assert!(err.to_string().contains("scheme 'http'"));
    }

    #[tokio::test]
    async fn http_executor_rejects_private_ip_target_by_default() {
        let step = http_step(serde_json::json!({
            "url": "http://127.0.0.1/v1/items"
        }));
        let ctx = ctx_for(&step, None);
        let executor = HttpCallExecutor::new(
            HttpCallPolicy::new(["127.0.0.1"]).with_allowed_schemes(["http"]),
        );

        let err = executor.execute(&ctx).await.unwrap_err();

        assert!(err.to_string().contains("private or local IP target"));
    }

    #[tokio::test]
    async fn http_executor_rejects_body_conflict() {
        let step = http_step(serde_json::json!({
            "url": "https://api.example.com/v1/items",
            "method": "POST",
            "body_json": {"ok": true},
            "body_text": "conflict"
        }));
        let ctx = ctx_for(&step, None);
        let executor = HttpCallExecutor::new(HttpCallPolicy::new(["api.example.com"]));

        let err = executor.execute(&ctx).await.unwrap_err();

        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[tokio::test]
    async fn http_executor_rejects_hop_by_hop_header() {
        let step = http_step(serde_json::json!({
            "url": "https://api.example.com/v1/items",
            "headers": {"Connection": "close"}
        }));
        let ctx = ctx_for(&step, None);
        let executor = HttpCallExecutor::new(HttpCallPolicy::new(["api.example.com"]));

        let err = executor.execute(&ctx).await.unwrap_err();

        assert!(
            err.to_string()
                .contains("header 'Connection' is not allowed")
        );
    }

    #[tokio::test]
    async fn http_executor_redacts_sensitive_response_headers() {
        let base_url = spawn_test_http_server(|_| {
            TestHttpResponse::new(
                200,
                vec![
                    ("Content-Type", "application/json"),
                    ("Set-Cookie", "session=secret"),
                    ("X-Api-Key", "secret"),
                    ("X-Trace", "trace-1"),
                ],
                br#"{"ok":true}"#.to_vec(),
            )
        })
        .await;
        let step = http_step(serde_json::json!({
            "url": format!("{base_url}/ok"),
            "response_format": "json"
        }));
        let ctx = ctx_for(&step, None);
        let executor = local_http_executor(1024);

        let result = executor.execute(&ctx).await.unwrap();
        let structured = result.structured.unwrap();

        assert_eq!(structured["body"], serde_json::json!({"ok": true}));
        assert_eq!(structured["headers"]["set-cookie"], "[REDACTED]");
        assert_eq!(structured["headers"]["x-api-key"], "[REDACTED]");
        assert_eq!(structured["headers"]["x-trace"], "trace-1");
    }

    #[tokio::test]
    async fn http_executor_truncates_large_response_body() {
        let base_url = spawn_test_http_server(|_| {
            TestHttpResponse::new(200, vec![("Content-Type", "text/plain")], "abcdef")
        })
        .await;
        let step = http_step(serde_json::json!({
            "url": format!("{base_url}/large"),
            "response_format": "text"
        }));
        let ctx = ctx_for(&step, None);
        let executor = local_http_executor(3);

        let result = executor.execute(&ctx).await.unwrap();
        let structured = result.structured.unwrap();

        assert_eq!(structured["body"], "abc");
        assert_eq!(structured["body_truncated"], true);
    }

    #[tokio::test]
    async fn http_executor_rejects_redirect_to_disallowed_host() {
        let base_url = spawn_test_http_server(|_| {
            TestHttpResponse::new(
                302,
                vec![("Location", "http://example.com/not-allowed")],
                "",
            )
        })
        .await;
        let step = http_step(serde_json::json!({
            "url": format!("{base_url}/redirect")
        }));
        let ctx = ctx_for(&step, None);
        let executor = HttpCallExecutor::new(
            HttpCallPolicy::new(["127.0.0.1"])
                .with_allowed_schemes(["http"])
                .with_redirects(true, 3)
                .allow_private_ip_targets(true),
        );

        let err = executor.execute(&ctx).await.unwrap_err();

        assert!(err.to_string().contains("host 'example.com'"));
    }

    #[tokio::test]
    async fn http_executor_rejects_unexpected_status() {
        let base_url = spawn_test_http_server(|_| {
            TestHttpResponse::new(404, vec![("Content-Type", "text/plain")], "missing")
        })
        .await;
        let step = http_step(serde_json::json!({
            "url": format!("{base_url}/missing")
        }));
        let ctx = ctx_for(&step, None);
        let executor = local_http_executor(1024);

        let err = executor.execute(&ctx).await.unwrap_err();

        assert!(err.to_string().contains("status 404"));
    }

    #[tokio::test]
    async fn http_executor_accepts_configured_status() {
        let base_url = spawn_test_http_server(|_| {
            TestHttpResponse::new(404, vec![("Content-Type", "text/plain")], "missing")
        })
        .await;
        let step = http_step(serde_json::json!({
            "url": format!("{base_url}/missing"),
            "expect_status": [404],
            "response_format": "text"
        }));
        let ctx = ctx_for(&step, None);
        let executor = local_http_executor(1024);

        let result = executor.execute(&ctx).await.unwrap();
        let structured = result.structured.unwrap();

        assert_eq!(structured["status"], 404);
        assert_eq!(structured["body"], "missing");
    }
}
