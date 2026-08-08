//! LLM-visible communication tools for main↔sub agent messaging.
//!
//! Seven tools covering the full communication surface:
//! - [`SpawnAgentTool`]: main spawns a sub-agent (async, fire-and-forget).
//! - [`MessageSubagentTool`]: main sends a message to a sub-agent.
//! - [`AwaitSubagentReplyTool`]: main blocks waiting for a sub-agent message/completion.
//! - [`QuerySubagentTool`]: main queries sub-agent status.
//! - [`AbortSubagentTool`]: main aborts a running sub-agent.
//! - [`MessageMainTool`]: sub sends a message to main.
//! - [`AwaitMainMessageTool`]: sub blocks waiting for a main message.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::future::BoxFuture;
use llm_harness_types::{
    ABORT_SUBAGENT_TOOL_NAME, AWAIT_MAIN_MESSAGE_TOOL_NAME, AWAIT_SUBAGENT_REPLY_TOOL_NAME,
    DataBlock, MESSAGE_MAIN_TOOL_NAME, MESSAGE_SUBAGENT_TOOL_NAME, QUERY_SUBAGENT_TOOL_NAME,
    SPAWN_AGENT_TOOL_NAME, Tool, ToolContext, ToolError, ToolExecutionMode, ToolFailure, ToolLevel,
    ToolResult,
};

use crate::spawn::message_bus::{
    AgentStatus, MAIN_AGENT_ID, MessageBus, SendReceipt, SubAgentEvent,
};
use crate::spawn::spawn_types::{
    ContextBlock, ContextMode, ContextSource, ProviderSelection, SpawnOptions,
};
use crate::spawn::spawner::HarnessSubAgentSpawner;

// ── default timeout ───────────────────────────────────────────────────────

const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 120;

fn parse_timeout(args: &serde_json::Value) -> Duration {
    args.get("timeout")
        .and_then(|v| v.as_f64())
        .filter(|&v| v.is_finite() && v > 0.0)
        .map(Duration::from_secs_f64)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_WAIT_TIMEOUT_SECS))
}

/// Sanitize LLM-provided metadata (role/description): truncate to max_len and
/// strip newlines/carriage returns/control chars to prevent output injection.
fn sanitize_metadata(s: &str, max_len: usize) -> String {
    s.chars()
        .filter(|c| !c.is_control() || matches!(c, '\t'))
        .take(max_len)
        .collect()
}

/// Format a [`SubAgentEvent`] as LLM-visible text.
///
/// - `Message`: `[from:{agent_id}] {body}`
/// - `Done` Ok:  `[agent '{id}' completed] {result}`
/// - `Done` Err: `[agent '{id}' failed] {error}`
fn format_event(event: &SubAgentEvent) -> String {
    match event {
        SubAgentEvent::Message { agent_id, body } => {
            format!("[from:{agent_id}] {body}")
        }
        SubAgentEvent::Done { agent_id, result } => match result {
            Ok(r) => format!("[agent '{agent_id}' completed] {}", r.final_answer),
            Err(e) => format!("[agent '{agent_id}' failed] {e}"),
        },
    }
}

// ── Tool 1: SpawnAgentTool ────────────────────────────────────────────────

/// LLM tool: spawn a sub-agent (async, fire-and-forget).
///
/// Always calls `spawner.spawn_async(opts)` and returns the `agent_id`
/// immediately; results arrive later via the event channel.
pub struct SpawnAgentTool {
    spawner: Arc<HarnessSubAgentSpawner>,
    /// 实例级 schema，包含运行时注册的 provider 别名。
    schema: serde_json::Value,
}

impl SpawnAgentTool {
    pub fn new(spawner: Arc<HarnessSubAgentSpawner>) -> Self {
        let aliases = spawner.alias_names();
        let provider_desc = if aliases.is_empty() {
            "Provider alias for the sub-agent's LLM.".to_string()
        } else {
            format!(
                "Provider alias, options: {}. e.g. \"{}\"",
                aliases.join(", "),
                aliases[0]
            )
        };
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "Task instruction for the sub-agent. e.g. \"Read all files in src/ and summarize the architecture\"" },
                "context": { "type": "string", "description": "Additional context text for the sub-agent (e.g. parent conversation summary). e.g. \"The project uses PostgreSQL with Diesel ORM\"" },
                "provider": { "type": "string", "description": provider_desc },
                "role": { "type": "string", "description": "Role label for the sub-agent. e.g. \"code-reviewer\"" },
                "description": { "type": "string", "description": "Responsibility description for the sub-agent. e.g. \"Reviews code for security and performance\"" }
            },
            "required": ["prompt"]
        });
        Self { spawner, schema }
    }
}

impl Tool for SpawnAgentTool {
    fn name(&self) -> &str {
        SPAWN_AGENT_TOOL_NAME
    }

    fn level(&self) -> ToolLevel {
        ToolLevel::BuiltIn
    }

    fn description(&self) -> &str {
        "Dispatch a sub-agent to handle a sub-task. Returns agent_id immediately; sub-agent results appear in subsequent turns. Multiple sub-agents can run in parallel.\n\n\
         Use when: delegating independent sub-tasks (e.g. research, file analysis, test execution) to parallel workers; isolating work that needs its own context window.\n\
         Don't use for: simple sequential operations that don't need isolation; tasks requiring tight back-and-forth (use message_subagent instead after spawning).\n\
         Returns: The spawned agent's ID (e.g. \"agent-3\").\n\n\
         Examples:\n\
         {\"prompt\": \"Read all files in src/ and summarize the architecture\"}\n\
         {\"prompt\": \"Run cargo test and report any failures\", \"provider\": \"claude-3-sonnet\"}"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &self.schema
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let prompt = args
                .get("prompt")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolFailure::invalid_arguments("missing prompt"))?
                .to_string();

            let context = args
                .get("context")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| {
                    ContextMode::Inject(vec![ContextBlock {
                        source: ContextSource::ParentConversation,
                        content: s.to_string(),
                    }])
                })
                .unwrap_or(ContextMode::None);

            let provider = args
                .get("provider")
                .and_then(|v| v.as_str())
                .map(|s| ProviderSelection::Alias(s.to_string()))
                .unwrap_or(ProviderSelection::Inherit);

            let role = args
                .get("role")
                .and_then(|v| v.as_str())
                .map(|s| sanitize_metadata(s, 64))
                .filter(|s| !s.is_empty());
            let description = args
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| sanitize_metadata(s, 256))
                .filter(|s| !s.is_empty());
            let opts = SpawnOptions {
                prompt,
                provider,
                cwd: None,
                context,
                system_prompt: None, // 不暴露给 LLM，由 builder_customize 控制
                thinking_level: None,
                role,
                description,
            };

            let agent_id = self
                .spawner
                .spawn_async(opts)
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;

            Ok(ToolResult::full(
                vec![DataBlock::text(format!(
                    "Spawned agent '{agent_id}'. Results will arrive when done."
                ))],
                serde_json::json!({"agent_id": agent_id}),
                false,
            ))
        })
    }
}
llm_harness_types::assert_builtin_registered!(SpawnAgentTool => SPAWN_AGENT_TOOL_NAME);

// ── Tool 2: MessageSubagentTool (main→sub) ────────────────────────────────

/// LLM tool: main agent sends a fire-and-forget message to a sub-agent.
///
/// `bus.send(main_id, to, message)` delivers to a matching waiter (via
/// oneshot) or steers into the sub-agent's session. Returns an error when
/// the recipient is unknown or already terminated (`SendReceipt::Failed`).
pub struct MessageSubagentTool {
    bus: Arc<MessageBus>,
    main_id: String,
}

impl MessageSubagentTool {
    /// `main_id` is the main agent's identifier (typically `MAIN_AGENT_ID`).
    pub fn new(bus: Arc<MessageBus>, main_id: impl Into<String>) -> Self {
        Self {
            bus,
            main_id: main_id.into(),
        }
    }
}

fn message_subagent_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "to": { "type": "string", "description": "ID of the target sub-agent. e.g. \"agent-3\"" },
            "message": { "type": "string", "description": "Text message to send. e.g. \"Please focus on the database layer first\"" }
        },
        "required": ["to", "message"]
    })
}

static MESSAGE_SUBAGENT_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(message_subagent_schema);

impl Tool for MessageSubagentTool {
    fn name(&self) -> &str {
        MESSAGE_SUBAGENT_TOOL_NAME
    }

    fn level(&self) -> ToolLevel {
        ToolLevel::BuiltIn
    }

    fn description(&self) -> &str {
        "Send a text message to a sub-agent (fire-and-forget, does not wait for a reply).\n\n\
         Use when: providing additional instructions or context to a running sub-agent; redirecting a sub-agent's focus mid-task.\n\
         Don't use for: waiting for a reply (use await_subagent_reply); querying status (use query_subagent); cancelling a sub-agent (use abort_subagent).\n\
         Returns: Confirmation that the message was delivered.\n\n\
         Examples:\n\
         {\"to\": \"agent-3\", \"message\": \"Please focus on the database layer first\"}"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &MESSAGE_SUBAGENT_SCHEMA
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let to = args
                .get("to")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolFailure::invalid_arguments("missing to"))?
                .to_string();
            let message = args
                .get("message")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolFailure::invalid_arguments("missing message"))?
                .to_string();

            match self.bus.send(&self.main_id, &to, &message).await {
                SendReceipt::Injected => Ok(ToolResult::full(
                    vec![DataBlock::text(format!("Message sent to agent '{to}'"))],
                    serde_json::json!({"to": to}),
                    false,
                )),
                SendReceipt::Failed(_) => Err(ToolFailure::new(
                    "message_delivery_failed",
                    format!("Unable to deliver message to agent '{to}'."),
                )),
            }
        })
    }
}
llm_harness_types::assert_builtin_registered!(MessageSubagentTool => MESSAGE_SUBAGENT_TOOL_NAME);

// ── Tool 3: AwaitSubagentReplyTool (main waits for sub) ───────────────────

/// LLM tool: main agent blocks waiting for a sub-agent message or completion.
///
/// Calls `bus.wait(main_id, from, timeout, abort)`. Returns the formatted
/// [`SubAgentEvent`], or a timeout notice when no message arrives in time.
pub struct AwaitSubagentReplyTool {
    bus: Arc<MessageBus>,
    main_id: String,
}

impl AwaitSubagentReplyTool {
    /// `main_id` is the main agent's identifier (typically `MAIN_AGENT_ID`).
    pub fn new(bus: Arc<MessageBus>, main_id: impl Into<String>) -> Self {
        Self {
            bus,
            main_id: main_id.into(),
        }
    }
}

fn await_subagent_reply_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "from": {
                "type": "string",
                "description": "Only wait for messages from this agent_id. Omit to wait for any sub-agent. e.g. \"agent-3\""
            },
            "timeout": {
                "type": "number",
                "description": "Timeout in seconds (default 120). e.g. 60",
                "minimum": 0.001
            }
        }
    })
}

static AWAIT_SUBAGENT_REPLY_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(await_subagent_reply_schema);

impl Tool for AwaitSubagentReplyTool {
    fn name(&self) -> &str {
        AWAIT_SUBAGENT_REPLY_TOOL_NAME
    }

    fn level(&self) -> ToolLevel {
        ToolLevel::BuiltIn
    }

    fn description(&self) -> &str {
        "Block until a message or completion event arrives from a sub-agent.\n\n\
         Use when: waiting for a sub-agent's response before proceeding; collecting results from a dispatched sub-agent.\n\
         Don't use for: sending a message (use message_subagent); non-blocking status checks (use query_subagent).\n\
         Returns: The sub-agent's message text, or a completion event with the final result. Returns a hint text on timeout.\n\n\
         Examples:\n\
         {\"from\": \"agent-3\", \"timeout\": 120}\n\
         {\"from\": \"agent-3\"}"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &AWAIT_SUBAGENT_REPLY_SCHEMA
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let from = args
                .get("from")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let timeout = parse_timeout(&args);

            let event = self
                .bus
                .wait(
                    &self.main_id,
                    from.as_deref(),
                    Some(timeout),
                    ctx.abort.clone(),
                )
                .await;

            let received = event.is_some();
            let text = match event {
                Some(ev) => format_event(&ev),
                None => format!("No message within {}s.", timeout.as_secs()),
            };

            Ok(ToolResult::full(
                vec![DataBlock::text(text)],
                serde_json::json!({
                    "status": if received { "received" } else { "timeout" }
                }),
                false,
            ))
        })
    }
}
llm_harness_types::assert_builtin_registered!(AwaitSubagentReplyTool => AWAIT_SUBAGENT_REPLY_TOOL_NAME);

// ── Tool 4: QuerySubagentTool ─────────────────────────────────────────────

/// LLM tool: query sub-agent status.
///
/// With `agent_id`: returns that agent's status (empty list if unknown).
/// Without: returns all registered agents.
pub struct QuerySubagentTool {
    bus: Arc<MessageBus>,
}

impl QuerySubagentTool {
    pub fn new(bus: Arc<MessageBus>) -> Self {
        Self { bus }
    }
}

fn query_subagent_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "agent_id": {
                "type": "string",
                "description": "Query a specific agent's status. Omit to list all agents. e.g. \"agent-3\""
            }
        }
    })
}

static QUERY_SUBAGENT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(query_subagent_schema);

/// Format a single [`AgentStatus`] as a short label.
fn status_label(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Running => "running",
        AgentStatus::Done(_) => "done",
        AgentStatus::Aborted => "aborted",
    }
}

impl Tool for QuerySubagentTool {
    fn name(&self) -> &str {
        QUERY_SUBAGENT_TOOL_NAME
    }

    fn level(&self) -> ToolLevel {
        ToolLevel::BuiltIn
    }

    fn description(&self) -> &str {
        "Query the status of sub-agents (running, done, or aborted). Pass an agent_id for a single agent; omit to list all.\n\n\
         Use when: checking if a sub-agent has finished; listing all active sub-agents before deciding to wait or proceed.\n\
         Don't use for: getting a sub-agent's result (use await_subagent_reply); cancelling (use abort_subagent).\n\
         Returns: Status string for a single agent, or a list of all agents with their statuses and role descriptions.\n\n\
         Examples:\n\
         {\"agent_id\": \"agent-3\"}\n\
         {}"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &QUERY_SUBAGENT_SCHEMA
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let agent_id = args.get("agent_id").and_then(|v| v.as_str());
            let snapshots = self.bus.query_status(agent_id);

            let text = if snapshots.is_empty() {
                "No agents found.".to_string()
            } else {
                snapshots
                    .iter()
                    .map(|s| {
                        let base = format!("{}: {}", s.agent_id, status_label(&s.status));
                        match (&s.role, &s.description) {
                            (Some(role), Some(desc)) => format!("{base} [{role}] — {desc}"),
                            (Some(role), None) => format!("{base} [{role}]"),
                            _ => base,
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };

            let details = serde_json::json!({
                "agents": snapshots.iter().map(|s| {
                    let mut entry = serde_json::json!({
                        "agent_id": s.agent_id,
                        "status": status_label(&s.status),
                    });
                    if let Some(role) = &s.role {
                        entry["role"] = serde_json::Value::String(role.clone());
                    }
                    if let Some(desc) = &s.description {
                        entry["description"] = serde_json::Value::String(desc.clone());
                    }
                    entry
                }).collect::<Vec<_>>()
            });

            Ok(ToolResult::full(
                vec![DataBlock::text(text)],
                details,
                false,
            ))
        })
    }
}
llm_harness_types::assert_builtin_registered!(QuerySubagentTool => QUERY_SUBAGENT_TOOL_NAME);

// ── Tool 5: AbortSubagentTool ────────────────────────────────────────────

/// LLM tool: abort a running sub-agent.
///
/// Calls `bus.abort_agent(agent_id)` to cancel the sub-agent's abort token.
/// The sub-agent's track task will deliver a Done(Aborted) event.
pub struct AbortSubagentTool {
    bus: Arc<MessageBus>,
}

impl AbortSubagentTool {
    pub fn new(bus: Arc<MessageBus>) -> Self {
        Self { bus }
    }
}

static ABORT_SUBAGENT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "agent_id": {
                "type": "string",
                "description": "ID of the sub-agent to cancel. e.g. \"agent-3\""
            }
        },
        "required": ["agent_id"]
    })
});

impl Tool for AbortSubagentTool {
    fn name(&self) -> &str {
        ABORT_SUBAGENT_TOOL_NAME
    }

    fn level(&self) -> ToolLevel {
        ToolLevel::BuiltIn
    }

    fn description(&self) -> &str {
        "Cancel a running sub-agent. The sub-agent terminates and produces an aborted event.\n\n\
         Use when: a sub-agent is stuck or no longer needed; cleaning up before ending a run.\n\
         Don't use for: gracefully requesting a sub-agent to stop (use message_subagent instead).\n\
         Returns: Confirmation of cancellation.\n\n\
         Examples:\n\
         {\"agent_id\": \"agent-3\"}"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &ABORT_SUBAGENT_SCHEMA
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let agent_id = args
                .get("agent_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolFailure::invalid_arguments("missing agent_id"))?;

            let snapshots = self.bus.query_status(Some(agent_id));
            let exists = snapshots
                .iter()
                .any(|s| matches!(s.status, AgentStatus::Running));

            if !exists {
                return Ok(ToolResult::full(
                    vec![DataBlock::text(format!(
                        "Agent '{agent_id}' not found or not running."
                    ))],
                    serde_json::json!({"agent_id": agent_id, "aborted": false}),
                    false,
                ));
            }

            self.bus.abort_agent(agent_id);

            Ok(ToolResult::full(
                vec![DataBlock::text(format!(
                    "Agent '{agent_id}' abort signal sent."
                ))],
                serde_json::json!({"agent_id": agent_id, "aborted": true}),
                false,
            ))
        })
    }
}
llm_harness_types::assert_builtin_registered!(AbortSubagentTool => ABORT_SUBAGENT_TOOL_NAME);

// ── Tool 6: MessageMainTool (sub→main) ────────────────────────────────────
/// LLM tool: sub-agent sends a fire-and-forget message to the main agent.
///
/// `bus.send(sub_id, MAIN_AGENT_ID, message)` delivers to a matching main-side
/// waiter (via oneshot) or routes to the event channel for the main loop's
/// `should_stop` / `IdleWatcher` to pick up.
pub struct MessageMainTool {
    bus: Arc<MessageBus>,
    sub_id: String,
}

impl MessageMainTool {
    /// `sub_id` is the sub-agent's identifier (its session_id).
    pub fn new(bus: Arc<MessageBus>, sub_id: impl Into<String>) -> Self {
        Self {
            bus,
            sub_id: sub_id.into(),
        }
    }
}

fn message_main_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "message": { "type": "string", "description": "Text message to report to the parent agent. e.g. \"Database migration complete, proceeding to API layer\"" }
        },
        "required": ["message"]
    })
}

static MESSAGE_MAIN_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(message_main_schema);

impl Tool for MessageMainTool {
    fn name(&self) -> &str {
        MESSAGE_MAIN_TOOL_NAME
    }

    fn level(&self) -> ToolLevel {
        ToolLevel::BuiltIn
    }

    fn description(&self) -> &str {
        "Send a text message to the parent agent (fire-and-forget). Used by sub-agents to report intermediate results or request guidance.\n\n\
         Use when: a sub-agent needs to inform the parent of progress or ask a clarifying question without ending its own run.\n\
         Don't use for: delivering the final result (the sub-agent's run completion does that automatically); waiting for a reply (use await_main_message).\n\
         Returns: Confirmation that the message was delivered.\n\n\
         Examples:\n\
         {\"message\": \"Database migration complete, proceeding to API layer\"}"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &MESSAGE_MAIN_SCHEMA
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let message = args
                .get("message")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolFailure::invalid_arguments("missing message"))?
                .to_string();

            // select! on ctx.abort so the sub-agent is not stuck forever if
            // the event channel is full and the main agent is not consuming.
            let receipt = tokio::select! {
                r = self.bus.send(&self.sub_id, MAIN_AGENT_ID, &message) => r,
                _ = ctx.abort.cancelled() => {
                    return Err(ToolFailure::aborted());
                }
            };

            match receipt {
                SendReceipt::Injected => Ok(ToolResult::full(
                    vec![DataBlock::text("Message reported to main agent")],
                    serde_json::json!({}),
                    false,
                )),
                SendReceipt::Failed(_) => Err(ToolFailure::new(
                    "message_delivery_failed",
                    "Unable to deliver message to the main agent.",
                )),
            }
        })
    }
}
llm_harness_types::assert_builtin_registered!(MessageMainTool => MESSAGE_MAIN_TOOL_NAME);

// ── Tool 7: AwaitMainMessageTool (sub waits for main) ─────────────────────

/// LLM tool: sub-agent blocks waiting for a message from the main agent.
///
/// Calls `bus.wait(sub_id, None, timeout, abort)` — no `from` filter because
/// the sub-agent only ever talks to main. Returns the formatted event, or a
/// timeout notice.
pub struct AwaitMainMessageTool {
    bus: Arc<MessageBus>,
    sub_id: String,
}

impl AwaitMainMessageTool {
    /// `sub_id` is the sub-agent's identifier (its session_id).
    pub fn new(bus: Arc<MessageBus>, sub_id: impl Into<String>) -> Self {
        Self {
            bus,
            sub_id: sub_id.into(),
        }
    }
}

fn await_main_message_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "timeout": {
                "type": "number",
                "description": "Timeout in seconds (default 120). e.g. 300",
                "minimum": 0.001
            }
        }
    })
}

static AWAIT_MAIN_MESSAGE_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(await_main_message_schema);

impl Tool for AwaitMainMessageTool {
    fn name(&self) -> &str {
        AWAIT_MAIN_MESSAGE_TOOL_NAME
    }

    fn level(&self) -> ToolLevel {
        ToolLevel::BuiltIn
    }

    fn description(&self) -> &str {
        "Block until a message arrives from the parent agent.\n\n\
         Use when: a sub-agent has asked a question and is waiting for the parent's response; pausing for further instructions.\n\
         Don't use for: sending a message (use message_main); checking if the parent is still active.\n\
         Returns: The parent agent's message text. Returns a hint text on timeout.\n\n\
         Examples:\n\
         {\"timeout\": 300}"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &AWAIT_MAIN_MESSAGE_SCHEMA
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let timeout = parse_timeout(&args);

            let event = self
                .bus
                .wait(&self.sub_id, None, Some(timeout), ctx.abort.clone())
                .await;

            let received = event.is_some();
            let text = match event {
                Some(ev) => format_event(&ev),
                None => format!("No message within {}s.", timeout.as_secs()),
            };

            Ok(ToolResult::full(
                vec![DataBlock::text(text)],
                serde_json::json!({
                    "status": if received { "received" } else { "timeout" }
                }),
                false,
            ))
        })
    }
}
llm_harness_types::assert_builtin_registered!(AwaitMainMessageTool => AWAIT_MAIN_MESSAGE_TOOL_NAME);

// ── format_event helper is also used by tests ─────────────────────────────
// (format_event and parse_timeout are defined above as free functions)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn::message_bus::{AgentEntry, AgentStatus, message_bus_pair};
    use crate::spawn::spawn_types::SubAgentResult;
    use crate::spawn::spawner::{EnvFactory, JsonlSessionFactory};
    use llm_harness_loop::test_utils::{MockLlmClient, MockResponse, NoOpEnv};
    use llm_harness_types::{AgentError, CostAggregate, ExecutionEnv};
    use tokio_util::sync::CancellationToken;

    // ── sanitize_metadata unit tests ─────────────────────────────────────

    #[test]
    fn sanitize_metadata_strips_control_chars() {
        let input = "hello\nworld\r\tprefix\ttab";
        let result = sanitize_metadata(input, 100);
        // \n and \r removed, \t preserved
        assert_eq!(result, "helloworld\tprefix\ttab");
    }

    #[test]
    fn sanitize_metadata_truncates_to_max_len() {
        let input = "abcdefghij";
        let result = sanitize_metadata(input, 5);
        assert_eq!(result, "abcde");
    }

    #[test]
    fn sanitize_metadata_all_control_becomes_empty() {
        let input = "\n\r\u{0001}\u{0002}";
        let result = sanitize_metadata(input, 64);
        assert!(result.is_empty());
    }

    #[test]
    fn sanitize_metadata_preserves_normal_text() {
        let input = "data-analyst";
        let result = sanitize_metadata(input, 64);
        assert_eq!(result, "data-analyst");
    }
    // ── shared test fixtures ──────────────────────────────────────────────

    struct NoopPlugin;
    impl llm_harness_agent::Plugin for NoopPlugin {
        fn name(&self) -> &str {
            "noop"
        }
    }

    struct NoopEnvFactory;
    impl EnvFactory for NoopEnvFactory {
        fn create(
            &self,
            _cwd: &std::path::Path,
        ) -> Result<Arc<dyn ExecutionEnv>, llm_harness_types::AgentError> {
            Ok(Arc::new(NoOpEnv))
        }
    }

    /// Build a SpawnAgentTool backed by a real (lightweight) spawner.
    fn make_spawn_tool() -> (SpawnAgentTool, Arc<MessageBus>) {
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
        let bus = message_bus_pair();
        let spawner = HarnessSubAgentSpawner::new(
            "test-model",
            client,
            std::env::temp_dir().join("tools-test"),
            bus.clone(),
            |_, _, _| Box::new(NoopPlugin),
        )
        .session_factory(Arc::new(JsonlSessionFactory))
        .env_factory(Arc::new(NoopEnvFactory));
        (SpawnAgentTool::new(Arc::new(spawner)), bus)
    }

    fn make_tool_context() -> ToolContext {
        let env: Arc<dyn ExecutionEnv> = Arc::new(NoOpEnv);
        let (update_tx, _update_rx) = tokio::sync::mpsc::channel(8);
        ToolContext {
            run: Arc::new(llm_harness_types::RunContext::new(
                llm_harness_types::RunRequest::default(),
            )),
            env,
            abort: CancellationToken::new(),
            tool_use_id: "test".into(),
            turn_index: 0,
            assistant_message: Arc::new(llm_harness_loop::test_utils::test_assistant_message(
                vec![],
            )),
            update_tx,
        }
    }

    fn make_ctx_with_abort(abort: CancellationToken) -> ToolContext {
        let env: Arc<dyn ExecutionEnv> = Arc::new(NoOpEnv);
        let (update_tx, _update_rx) = tokio::sync::mpsc::channel(8);
        ToolContext {
            run: Arc::new(llm_harness_types::RunContext::new(
                llm_harness_types::RunRequest::default(),
            )),
            env,
            abort,
            tool_use_id: "test".into(),
            turn_index: 0,
            assistant_message: Arc::new(llm_harness_loop::test_utils::test_assistant_message(
                vec![],
            )),
            update_tx,
        }
    }

    /// Register a Running sub-agent entry on the bus.
    fn register_sub(bus: &Arc<MessageBus>, id: &str) {
        bus.register(
            id,
            AgentEntry {
                status: AgentStatus::Running,
                harness: None,
                is_main: false,
                role: None,
                description: None,
            },
        );
    }

    /// Register the Main agent entry on the bus.
    fn register_main(bus: &Arc<MessageBus>) {
        bus.register(
            MAIN_AGENT_ID,
            AgentEntry {
                status: AgentStatus::Running,
                harness: None,
                is_main: true,
                role: None,
                description: None,
            },
        );
    }

    fn done_result() -> SubAgentResult {
        SubAgentResult {
            final_answer: "all done".into(),
            tool_calls_count: 1,
            session_id: "s1".into(),
            cost: CostAggregate::default(),
        }
    }

    // ── Tool 1: SpawnAgentTool ────────────────────────────────────────────

    #[test]
    fn spawn_agent_tool_name_and_schema() {
        let (tool, _) = make_spawn_tool();
        assert_eq!(tool.name(), SPAWN_AGENT_TOOL_NAME);
        assert_eq!(tool.level(), ToolLevel::BuiltIn);
        assert_eq!(tool.execution_mode(), ToolExecutionMode::Parallel);
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
        // `wait` field must be gone.
        assert!(
            schema.get("properties").unwrap().get("wait").is_none(),
            "wait field should be removed from schema"
        );
        assert_eq!(
            schema.get("required").unwrap().as_array().unwrap(),
            &["prompt".to_string()]
        );
    }

    #[tokio::test]
    async fn spawn_agent_returns_agent_id() {
        let (tool, bus) = make_spawn_tool();
        let ctx = make_tool_context();
        let args = serde_json::json!({"prompt": "say ok"});
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(!result.terminate);
        let agent_id = result.details.get("agent_id").unwrap().as_str().unwrap();
        assert!(!agent_id.is_empty());
        match &result.model_content[0] {
            DataBlock::Text { text, .. } => assert!(text.contains("Spawned agent")),
            _ => panic!("expected text"),
        }
        // Let the track task finish.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(bus.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn spawn_agent_missing_prompt_returns_error() {
        let (tool, _) = make_spawn_tool();
        let ctx = make_tool_context();
        let args = serde_json::json!({});
        let err = tool.execute(args, &ctx).await.unwrap_err();
        assert_eq!(err.code, "invalid_arguments");
    }

    #[tokio::test]
    async fn spawn_agent_ignores_legacy_wait_param() {
        // A caller still passing `wait` must not break — the field is ignored.
        let (tool, _) = make_spawn_tool();
        let ctx = make_tool_context();
        let args = serde_json::json!({"prompt": "say ok", "wait": true});
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(!result.terminate);
    }

    // ── Tool 2: MessageSubagentTool ───────────────────────────────────────

    #[test]
    fn message_subagent_schema_fields() {
        let bus = message_bus_pair();
        let tool = MessageSubagentTool::new(bus, MAIN_AGENT_ID);
        assert_eq!(tool.name(), MESSAGE_SUBAGENT_TOOL_NAME);
        assert_eq!(tool.execution_mode(), ToolExecutionMode::Parallel);
        let schema = tool.parameters_schema();
        let props = schema.get("properties").unwrap();
        assert!(props.get("to").is_some());
        assert!(props.get("message").is_some());
    }

    #[tokio::test]
    async fn message_subagent_sends_to_registered_agent() {
        // Sub registered with harness=None (detached). send() correctly
        // returns Failed because the steer channel is unavailable.
        let bus = message_bus_pair();
        register_sub(&bus, "sub-1");
        let tool = MessageSubagentTool::new(bus.clone(), MAIN_AGENT_ID);
        let ctx = make_tool_context();
        let args = serde_json::json!({"to": "sub-1", "message": "hello"});
        let err = tool.execute(args, &ctx).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("sub-1"),
            "error should mention agent id, got: {msg}"
        );
    }

    #[tokio::test]
    async fn message_subagent_unknown_agent_returns_error() {
        let bus = message_bus_pair();
        let tool = MessageSubagentTool::new(bus, MAIN_AGENT_ID);
        let ctx = make_tool_context();
        let args = serde_json::json!({"to": "ghost", "message": "hello"});
        let err = tool.execute(args, &ctx).await.unwrap_err();
        assert_eq!(err.code, "message_delivery_failed");
    }

    #[tokio::test]
    async fn message_subagent_missing_fields_returns_error() {
        let bus = message_bus_pair();
        register_sub(&bus, "sub-1");
        let tool = MessageSubagentTool::new(bus, MAIN_AGENT_ID);
        let ctx = make_tool_context();
        // missing `to`
        let err = tool
            .execute(serde_json::json!({"message": "hi"}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code, "invalid_arguments");
    }

    // ── Tool 3: AwaitSubagentReplyTool ────────────────────────────────────

    #[test]
    fn await_subagent_reply_schema_and_mode() {
        let bus = message_bus_pair();
        let tool = AwaitSubagentReplyTool::new(bus, MAIN_AGENT_ID);
        assert_eq!(tool.name(), AWAIT_SUBAGENT_REPLY_TOOL_NAME);
        assert_eq!(tool.execution_mode(), ToolExecutionMode::Sequential);
        let schema = tool.parameters_schema();
        let props = schema.get("properties").unwrap();
        assert!(props.get("from").is_some());
        assert!(props.get("timeout").is_some());
    }

    #[tokio::test]
    async fn await_subagent_reply_receives_message() {
        let bus = message_bus_pair();
        register_main(&bus);
        register_sub(&bus, "sub-1");

        let tool = AwaitSubagentReplyTool::new(bus.clone(), MAIN_AGENT_ID);
        let ctx = make_tool_context();

        // Spawn the waiter, then send a message from sub-1.
        let bus_clone = bus.clone();
        let wait_handle =
            tokio::spawn(async move { tool.execute(serde_json::json!({}), &ctx).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = bus_clone.send("sub-1", MAIN_AGENT_ID, "hi main").await;

        let result = wait_handle.await.unwrap().unwrap();
        match &result.model_content[0] {
            DataBlock::Text { text, .. } => {
                assert_eq!(text, "[from:sub-1] hi main");
            }
            _ => panic!("expected text"),
        }
        assert_eq!(
            result.details.get("status").unwrap().as_str().unwrap(),
            "received"
        );
    }

    #[tokio::test]
    async fn await_subagent_reply_times_out() {
        let bus = message_bus_pair();
        register_main(&bus);

        let tool = AwaitSubagentReplyTool::new(bus, MAIN_AGENT_ID);
        let ctx = make_tool_context();
        let args = serde_json::json!({"timeout": 0.05});
        let result = tool.execute(args, &ctx).await.unwrap();
        match &result.model_content[0] {
            DataBlock::Text { text, .. } => {
                assert!(text.starts_with("No message within"));
            }
            _ => panic!("expected text"),
        }
        assert_eq!(
            result.details.get("status").unwrap().as_str().unwrap(),
            "timeout"
        );
    }

    #[tokio::test]
    async fn await_subagent_reply_formats_done_event() {
        let bus = message_bus_pair();
        register_main(&bus);
        register_sub(&bus, "sub-1");

        let tool = AwaitSubagentReplyTool::new(bus.clone(), MAIN_AGENT_ID);
        let ctx = make_tool_context();

        // A Done event is delivered via the waiter when the track task fires.
        // We simulate it by spawning a waiter, then sending a Done event
        // directly through the bus's waiter mechanism via `send` (which only
        // produces Message events). Instead, register a Done status and use
        // `wait` — but wait() only returns events sent to waiters, not status
        // changes. So we verify format_event directly for the Done case.
        let done_ev = SubAgentEvent::Done {
            agent_id: "sub-1".into(),
            result: Ok(done_result()),
        };
        assert_eq!(format_event(&done_ev), "[agent 'sub-1' completed] all done");

        let fail_ev = SubAgentEvent::Done {
            agent_id: "sub-1".into(),
            result: Err(AgentError::Aborted),
        };
        assert_eq!(format_event(&fail_ev), "[agent 'sub-1' failed] aborted");

        // Sanity: the wait tool still times out (no event sent).
        let args = serde_json::json!({"timeout": 0.05});
        let _ = tool.execute(args, &ctx).await.unwrap();
    }

    #[tokio::test]
    async fn await_subagent_reply_aborts_on_cancel() {
        let bus = message_bus_pair();
        register_main(&bus);

        let abort = CancellationToken::new();
        let tool = AwaitSubagentReplyTool::new(bus, MAIN_AGENT_ID);
        let ctx = make_ctx_with_abort(abort.clone());

        let abort_clone = abort.clone();
        let wait_handle =
            tokio::spawn(
                async move { tool.execute(serde_json::json!({"timeout": 60}), &ctx).await },
            );

        tokio::time::sleep(Duration::from_millis(50)).await;
        abort_clone.cancel();

        let result = wait_handle.await.unwrap().unwrap();
        // On abort, bus.wait returns None → timeout-formatted text.
        match &result.model_content[0] {
            DataBlock::Text { text, .. } => {
                assert!(text.starts_with("No message within"));
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn await_subagent_reply_filters_by_from() {
        let bus = message_bus_pair();
        register_main(&bus);
        register_sub(&bus, "sub-1");
        register_sub(&bus, "sub-2");

        let tool = AwaitSubagentReplyTool::new(bus.clone(), MAIN_AGENT_ID);
        let ctx = make_tool_context();

        let wait_handle = tokio::spawn(async move {
            tool.execute(serde_json::json!({"from": "sub-2", "timeout": 5}), &ctx)
                .await
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        // sub-1 sends — should NOT match the filtered waiter.
        let _ = bus.send("sub-1", MAIN_AGENT_ID, "from sub-1").await;
        // sub-2 sends — should match.
        let _ = bus.send("sub-2", MAIN_AGENT_ID, "from sub-2").await;

        let result = wait_handle.await.unwrap().unwrap();
        match &result.model_content[0] {
            DataBlock::Text { text, .. } => {
                assert_eq!(text, "[from:sub-2] from sub-2");
            }
            _ => panic!("expected text"),
        }
    }

    // ── Tool 4: QuerySubagentTool ─────────────────────────────────────────

    #[test]
    fn query_subagent_schema_fields() {
        let bus = message_bus_pair();
        let tool = QuerySubagentTool::new(bus);
        assert_eq!(tool.name(), QUERY_SUBAGENT_TOOL_NAME);
        assert_eq!(tool.execution_mode(), ToolExecutionMode::Parallel);
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").unwrap().get("agent_id").is_some());
    }

    #[tokio::test]
    async fn query_subagent_lists_all() {
        let bus = message_bus_pair();
        register_main(&bus);
        register_sub(&bus, "sub-1");
        register_sub(&bus, "sub-2");

        let tool = QuerySubagentTool::new(bus);
        let ctx = make_tool_context();
        let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();
        match &result.model_content[0] {
            DataBlock::Text { text, .. } => {
                assert!(text.contains("Main: running"));
                assert!(text.contains("sub-1: running"));
                assert!(text.contains("sub-2: running"));
            }
            _ => panic!("expected text"),
        }
        let agents = result.details.get("agents").unwrap().as_array().unwrap();
        assert_eq!(agents.len(), 3);
    }

    #[tokio::test]
    async fn query_subagent_single_agent() {
        let bus = message_bus_pair();
        register_main(&bus);
        register_sub(&bus, "sub-1");

        let tool = QuerySubagentTool::new(bus);
        let ctx = make_tool_context();
        let result = tool
            .execute(serde_json::json!({"agent_id": "sub-1"}), &ctx)
            .await
            .unwrap();
        match &result.model_content[0] {
            DataBlock::Text { text, .. } => {
                assert_eq!(text, "sub-1: running");
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn query_subagent_unknown_returns_empty() {
        let bus = message_bus_pair();
        register_main(&bus);

        let tool = QuerySubagentTool::new(bus);
        let ctx = make_tool_context();
        let result = tool
            .execute(serde_json::json!({"agent_id": "ghost"}), &ctx)
            .await
            .unwrap();
        match &result.model_content[0] {
            DataBlock::Text { text, .. } => {
                assert_eq!(text, "No agents found.");
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn query_subagent_shows_done_status() {
        let bus = message_bus_pair();
        bus.register(
            "sub-1",
            AgentEntry {
                status: AgentStatus::Done(Box::new(done_result())),
                harness: None,
                is_main: false,
                role: None,
                description: None,
            },
        );

        let tool = QuerySubagentTool::new(bus);
        let ctx = make_tool_context();
        let result = tool
            .execute(serde_json::json!({"agent_id": "sub-1"}), &ctx)
            .await
            .unwrap();
        match &result.model_content[0] {
            DataBlock::Text { text, .. } => {
                assert_eq!(text, "sub-1: done");
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn query_subagent_includes_role_and_description() {
        let bus = message_bus_pair();
        bus.register(
            "sub-1",
            AgentEntry {
                status: AgentStatus::Running,
                harness: None,
                is_main: false,
                role: Some("code-reviewer".into()),
                description: Some("Review src/auth.rs".into()),
            },
        );

        let tool = QuerySubagentTool::new(bus);
        let ctx = make_tool_context();
        let result = tool
            .execute(serde_json::json!({"agent_id": "sub-1"}), &ctx)
            .await
            .unwrap();

        let text = match &result.model_content[0] {
            DataBlock::Text { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(
            text.contains("code-reviewer"),
            "text must contain role: {text}"
        );
        assert!(
            text.contains("Review src/auth.rs"),
            "text must contain description: {text}"
        );
    }

    // ── Tool 5: MessageMainTool ───────────────────────────────────────────

    #[test]
    fn message_main_schema_fields() {
        let bus = message_bus_pair();
        let tool = MessageMainTool::new(bus, "sub-1");
        assert_eq!(tool.name(), MESSAGE_MAIN_TOOL_NAME);
        assert_eq!(tool.execution_mode(), ToolExecutionMode::Parallel);
        let schema = tool.parameters_schema();
        let props = schema.get("properties").unwrap();
        assert!(props.get("message").is_some());
        assert!(props.get("to").is_none()); // no `to` — always sends to Main
    }

    #[tokio::test]
    async fn message_main_sends_to_main() {
        let bus = message_bus_pair();
        register_main(&bus);

        let tool = MessageMainTool::new(bus.clone(), "sub-1");
        let ctx = make_tool_context();
        let args = serde_json::json!({"message": "reporting in"});
        let result = tool.execute(args, &ctx).await.unwrap();
        match &result.model_content[0] {
            DataBlock::Text { text, .. } => {
                assert_eq!(text, "Message reported to main agent");
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn message_main_missing_message_returns_error() {
        let bus = message_bus_pair();
        register_main(&bus);

        let tool = MessageMainTool::new(bus, "sub-1");
        let ctx = make_tool_context();
        let err = tool.execute(serde_json::json!({}), &ctx).await.unwrap_err();
        assert_eq!(err.code, "invalid_arguments");
    }

    #[tokio::test]
    async fn message_main_main_not_registered_returns_error() {
        let bus = message_bus_pair();
        // Main not registered → send fails.
        let tool = MessageMainTool::new(bus, "sub-1");
        let ctx = make_tool_context();
        let err = tool
            .execute(serde_json::json!({"message": "hi"}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code, "message_delivery_failed");
    }

    // ── Tool 6: AwaitMainMessageTool ──────────────────────────────────────

    #[test]
    fn await_main_message_schema_and_mode() {
        let bus = message_bus_pair();
        let tool = AwaitMainMessageTool::new(bus, "sub-1");
        assert_eq!(tool.name(), AWAIT_MAIN_MESSAGE_TOOL_NAME);
        assert_eq!(tool.execution_mode(), ToolExecutionMode::Sequential);
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").unwrap().get("timeout").is_some());
    }

    #[tokio::test]
    async fn await_main_message_receives_from_main() {
        let bus = message_bus_pair();
        register_sub(&bus, "sub-1");
        register_main(&bus);

        let tool = AwaitMainMessageTool::new(bus.clone(), "sub-1");
        let ctx = make_tool_context();

        let wait_handle =
            tokio::spawn(
                async move { tool.execute(serde_json::json!({"timeout": 5}), &ctx).await },
            );

        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = bus.send(MAIN_AGENT_ID, "sub-1", "do this").await;

        let result = wait_handle.await.unwrap().unwrap();
        match &result.model_content[0] {
            DataBlock::Text { text, .. } => {
                assert_eq!(text, "[from:Main] do this");
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn await_main_message_times_out() {
        let bus = message_bus_pair();
        register_sub(&bus, "sub-1");

        let tool = AwaitMainMessageTool::new(bus, "sub-1");
        let ctx = make_tool_context();
        let args = serde_json::json!({"timeout": 0.05});
        let result = tool.execute(args, &ctx).await.unwrap();
        match &result.model_content[0] {
            DataBlock::Text { text, .. } => {
                assert!(text.starts_with("No message within"));
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn await_main_message_aborts_on_cancel() {
        let bus = message_bus_pair();
        register_sub(&bus, "sub-1");

        let abort = CancellationToken::new();
        let tool = AwaitMainMessageTool::new(bus, "sub-1");
        let ctx = make_ctx_with_abort(abort.clone());

        let abort_clone = abort.clone();
        let wait_handle =
            tokio::spawn(
                async move { tool.execute(serde_json::json!({"timeout": 60}), &ctx).await },
            );

        tokio::time::sleep(Duration::from_millis(50)).await;
        abort_clone.cancel();

        let result = wait_handle.await.unwrap().unwrap();
        match &result.model_content[0] {
            DataBlock::Text { text, .. } => {
                assert!(text.starts_with("No message within"));
            }
            _ => panic!("expected text"),
        }
    }

    // ── format_event unit tests ──────────────────────────────────────────

    #[test]
    fn format_event_message() {
        let ev = SubAgentEvent::Message {
            agent_id: "sub-1".into(),
            body: "hello".into(),
        };
        assert_eq!(format_event(&ev), "[from:sub-1] hello");
    }

    #[test]
    fn format_event_done_ok() {
        let ev = SubAgentEvent::Done {
            agent_id: "sub-1".into(),
            result: Ok(done_result()),
        };
        assert_eq!(format_event(&ev), "[agent 'sub-1' completed] all done");
    }

    #[test]
    fn format_event_done_err() {
        let ev = SubAgentEvent::Done {
            agent_id: "sub-1".into(),
            result: Err(AgentError::Internal("boom".into())),
        };
        assert_eq!(format_event(&ev), "[agent 'sub-1' failed] internal: boom");
    }

    // ── parse_timeout unit tests ─────────────────────────────────────────

    #[test]
    fn parse_timeout_uses_default_when_absent() {
        let t = parse_timeout(&serde_json::json!({}));
        assert_eq!(t, Duration::from_secs(DEFAULT_WAIT_TIMEOUT_SECS));
    }

    #[test]
    fn parse_timeout_reads_explicit_value() {
        let t = parse_timeout(&serde_json::json!({"timeout": 30}));
        assert_eq!(t, Duration::from_secs(30));
    }
    #[test]
    fn parse_timeout_rejects_negative() {
        let t = parse_timeout(&serde_json::json!({"timeout": -1.0}));
        assert_eq!(t, Duration::from_secs(DEFAULT_WAIT_TIMEOUT_SECS));
    }

    #[test]
    fn parse_timeout_rejects_nan() {
        let t = parse_timeout(&serde_json::json!({"timeout": f64::NAN}));
        assert_eq!(t, Duration::from_secs(DEFAULT_WAIT_TIMEOUT_SECS));
    }

    #[test]
    fn parse_timeout_rejects_infinity() {
        let t = parse_timeout(&serde_json::json!({"timeout": f64::INFINITY}));
        assert_eq!(t, Duration::from_secs(DEFAULT_WAIT_TIMEOUT_SECS));
    }
}
