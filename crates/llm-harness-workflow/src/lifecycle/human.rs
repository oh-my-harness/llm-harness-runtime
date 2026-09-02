//! Built-in human interaction tools.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use llm_harness_types::{
    DataBlock, EventStream, REQUEST_HUMAN_APPROVAL_TOOL_NAME, REQUEST_HUMAN_INPUT_TOOL_NAME, Tool,
    ToolContext, ToolExecutionMode, ToolFailure, ToolLevel, ToolProgress, ToolResult,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HumanApprovalDefault {
    Approve,
    Deny,
}

pub struct RequestHumanApprovalTool {
    stream: Arc<Mutex<Box<dyn EventStream>>>,
    timeout: Duration,
    default: HumanApprovalDefault,
}

impl RequestHumanApprovalTool {
    pub fn new(stream: Arc<Mutex<Box<dyn EventStream>>>) -> Self {
        Self {
            stream,
            timeout: Duration::from_secs(300),
            default: HumanApprovalDefault::Deny,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_default(mut self, default: HumanApprovalDefault) -> Self {
        self.default = default;
        self
    }
}

fn approval_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "question": {
                "type": "string",
                "minLength": 1,
                "description": "The specific action or decision requiring approval."
            },
            "context": {
                "description": "Optional structured context for the reviewer."
            }
        },
        "required": ["question"],
        "additionalProperties": false
    })
}

impl Tool for RequestHumanApprovalTool {
    fn name(&self) -> &str {
        REQUEST_HUMAN_APPROVAL_TOOL_NAME
    }

    fn level(&self) -> ToolLevel {
        ToolLevel::BuiltIn
    }

    fn description(&self) -> &str {
        "Request a human approve or deny a specific action before continuing.\n\n\
         Use when: an action is destructive, hard to reverse, security-sensitive, or explicitly requires a human decision.\n\
         Don't use for: ordinary progress updates or input that can be obtained from project files.\n\
         Returns: approved or denied. On timeout, the configured fail-safe decision is returned."
    }

    fn parameters_schema(&self) -> &Value {
        static SCHEMA: std::sync::LazyLock<Value> = std::sync::LazyLock::new(approval_schema);
        &SCHEMA
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let question = args
                .get("question")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|question| !question.is_empty())
                .ok_or_else(|| {
                    ToolFailure::invalid_arguments("question must be a non-empty string")
                })?;
            let context = args.get("context").cloned().unwrap_or(Value::Null);

            request_human_response(
                &ctx.update_tx,
                ctx.tool_use_id.as_str(),
                "human_approval_request",
                json!({"question": question, "context": context}),
                &self.stream,
                "Human approval response must contain decision 'approve' or 'deny'.",
                self.timeout,
                ctx.abort.clone(),
                parse_approval_response,
                || match self.default {
                    HumanApprovalDefault::Approve => {
                        let details = json!({
                            "request_id": ctx.tool_use_id,
                            "decision": "approved",
                            "timed_out": true,
                        });
                        ToolResult::full(
                            vec![DataBlock::text(
                                "Human approval timed out; configured policy approved the action.",
                            )],
                            details,
                            false,
                        )
                    }
                    HumanApprovalDefault::Deny => {
                        let details = json!({
                            "request_id": ctx.tool_use_id,
                            "decision": "denied",
                            "timed_out": true,
                        });
                        ToolResult::full(
                            vec![DataBlock::text(
                                "Human approval timed out; configured policy denied the action.",
                            )],
                            details,
                            false,
                        )
                    }
                },
                |approved| {
                    let decision = if approved { "approved" } else { "denied" };
                    let details = json!({"request_id": ctx.tool_use_id, "decision": decision});
                    ToolResult::full(
                        vec![DataBlock::text(if approved {
                            "Human approved the action."
                        } else {
                            "Human denied the action."
                        })],
                        details,
                        false,
                    )
                },
            )
            .await
        })
    }
}

llm_harness_types::assert_builtin_registered!(RequestHumanApprovalTool => REQUEST_HUMAN_APPROVAL_TOOL_NAME);

pub struct RequestHumanInputTool {
    stream: Arc<Mutex<Box<dyn EventStream>>>,
    timeout: Duration,
    default: Value,
}

impl RequestHumanInputTool {
    pub fn new(stream: Arc<Mutex<Box<dyn EventStream>>>) -> Self {
        Self {
            stream,
            timeout: Duration::from_secs(300),
            default: Value::Null,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_default(mut self, default: Value) -> Self {
        self.default = default;
        self
    }
}

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "question": {
                "type": "string",
                "minLength": 1,
                "description": "The information requested from the human."
            },
            "context": {
                "description": "Optional constraints, choices, or background."
            }
        },
        "required": ["question"],
        "additionalProperties": false
    })
}

impl Tool for RequestHumanInputTool {
    fn name(&self) -> &str {
        REQUEST_HUMAN_INPUT_TOOL_NAME
    }

    fn level(&self) -> ToolLevel {
        ToolLevel::BuiltIn
    }

    fn description(&self) -> &str {
        "Request missing information or a choice from a human.\n\n\
         Use when: required context is unavailable and the human is the correct source.\n\
         Don't use for: destructive-action approval (use request_human_approval).\n\
         Returns: the supplied value. On timeout, the configured default value is returned."
    }

    fn parameters_schema(&self) -> &Value {
        static SCHEMA: std::sync::LazyLock<Value> = std::sync::LazyLock::new(input_schema);
        &SCHEMA
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let question = args
                .get("question")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|question| !question.is_empty())
                .ok_or_else(|| {
                    ToolFailure::invalid_arguments("question must be a non-empty string")
                })?;
            let context = args.get("context").cloned().unwrap_or(Value::Null);
            let default = self.default.clone();

            request_human_response(
                &ctx.update_tx,
                ctx.tool_use_id.as_str(),
                "human_input_request",
                json!({"question": question, "context": context}),
                &self.stream,
                "Human input response must contain a value field.",
                self.timeout,
                ctx.abort.clone(),
                parse_input_response,
                move || {
                    let details = json!({
                        "request_id": ctx.tool_use_id,
                        "value": default,
                        "timed_out": true,
                    });
                    ToolResult::full(
                        vec![DataBlock::text(format!(
                            "Human input timed out; using default value: {default}"
                        ))],
                        details,
                        false,
                    )
                },
                move |value| {
                    let details = json!({"request_id": ctx.tool_use_id, "value": value});
                    ToolResult::full(
                        vec![DataBlock::text(format!("Human supplied: {value}"))],
                        details,
                        false,
                    )
                },
            )
            .await
        })
    }
}

llm_harness_types::assert_builtin_registered!(RequestHumanInputTool => REQUEST_HUMAN_INPUT_TOOL_NAME);

type RequestDetails = Value;

#[allow(clippy::too_many_arguments)]
#[allow(clippy::result_large_err)]
async fn request_human_response<T>(
    update_tx: &tokio::sync::mpsc::Sender<ToolProgress>,
    request_id: &str,
    request_type: &'static str,
    request_payload: RequestDetails,
    stream: &Arc<Mutex<Box<dyn EventStream>>>,
    invalid_response_message: &'static str,
    timeout: Duration,
    abort: CancellationToken,
    parse_event: impl Fn(&Value) -> Option<T>,
    timeout_result: impl FnOnce() -> ToolResult,
    event_result: impl FnOnce(T) -> ToolResult,
) -> Result<ToolResult, ToolFailure> {
    let details = json!({
        "type": request_type,
        "request_id": request_id,
        "payload": request_payload,
    });
    update_tx
        .send(ToolProgress {
            content: Vec::new(),
            details,
        })
        .await
        .map_err(|_| {
            ToolFailure::new(
                "human_request_channel_closed",
                "No consumer is listening for the human interaction request.",
            )
        })?;

    let mut stream = stream.lock().await;
    loop {
        tokio::select! {
            biased;
            _ = abort.cancelled() => {
                return Err(ToolFailure::aborted());
            }
            _ = tokio::time::sleep(timeout) => {
                return Ok(timeout_result());
            }
            event = stream.next() => {
                let Some(event) = event else {
                    return Err(ToolFailure::new(
                        "human_response_stream_closed",
                        "The human response stream closed before a response arrived.",
                    ));
                };
                if event.details.get("request_id").and_then(Value::as_str) != Some(request_id) {
                    continue;
                }
                let Some(parsed) = parse_event(&event.details) else {
                    return Err(ToolFailure::new(
                        "invalid_human_response",
                        invalid_response_message,
                    ));
                };
                return Ok(event_result(parsed));
            }
        }
    }
}

fn parse_approval_response(event: &Value) -> Option<bool> {
    match event.get("decision").and_then(Value::as_str) {
        Some("approve" | "approved") => Some(true),
        Some("deny" | "denied") => Some(false),
        _ => None,
    }
}

fn parse_input_response(event: &Value) -> Option<Value> {
    if event.get("value").is_some() {
        Some(event["value"].clone())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;
    use llm_harness_types::Event;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    struct ChannelStream {
        rx: mpsc::Receiver<Event>,
    }

    impl EventStream for ChannelStream {
        fn next<'a>(&'a mut self) -> BoxFuture<'a, Option<Event>> {
            Box::pin(async move { self.rx.recv().await })
        }
    }

    fn stream() -> (mpsc::Sender<Event>, Arc<Mutex<Box<dyn EventStream>>>) {
        let (tx, rx) = mpsc::channel::<Event>(8);
        (tx, Arc::new(Mutex::new(Box::new(ChannelStream { rx }))))
    }

    fn make_tool_ctx(abort: CancellationToken) -> (ToolContext, mpsc::Receiver<ToolProgress>) {
        let (update_tx, update_rx) = mpsc::channel(1);
        (
            ToolContext {
                run: Arc::new(llm_harness_types::RunContext::new(
                    llm_harness_types::RunRequest::default(),
                )),
                env: Arc::new(llm_harness_loop::test_utils::NoOpEnv),
                abort,
                tool_use_id: "tu_1".into(),
                turn_index: 0,
                assistant_message: Arc::new(llm_harness_loop::test_utils::test_assistant_message(
                    vec![],
                )),
                update_tx,
            },
            update_rx,
        )
    }

    fn human_event(details: Value) -> Event {
        Event {
            content: vec![DataBlock::text("human response")],
            details,
        }
    }

    #[tokio::test]
    async fn approval_accepts_matching_response() {
        let (tx, stream) = stream();
        let tool = RequestHumanApprovalTool::new(stream);
        let (ctx, mut updates) = make_tool_ctx(CancellationToken::new());
        let task = tokio::spawn({
            let tool = tool;
            async move {
                tool.execute(
                    json!({"question": "delete the directory?", "context": {"path": "/tmp/x"}}),
                    &ctx,
                )
                .await
            }
        });

        let request = updates.recv().await.unwrap();
        assert_eq!(request.details["type"], "human_approval_request");
        assert_eq!(request.details["request_id"], "tu_1");
        tx.send(human_event(json!({
            "request_id": "tu_1",
            "decision": "deny",
        })))
        .await
        .unwrap();

        let result = task.await.unwrap().unwrap();
        assert_eq!(result.details["decision"], "denied");
        assert_eq!(
            result.model_content[0],
            DataBlock::text("Human denied the action.")
        );
    }

    #[tokio::test]
    async fn approval_ignores_other_requests_and_accepts_late_response() {
        let (tx, stream) = stream();
        let tool = RequestHumanApprovalTool::new(stream);
        let (ctx, mut updates) = make_tool_ctx(CancellationToken::new());
        let task =
            tokio::spawn(async move { tool.execute(json!({"question": "proceed?"}), &ctx).await });
        assert!(updates.recv().await.is_some());

        tx.send(human_event(json!({
            "request_id": "other",
            "decision": "deny",
        })))
        .await
        .unwrap();
        tx.send(human_event(json!({
            "request_id": "tu_1",
            "decision": "approve",
        })))
        .await
        .unwrap();

        assert_eq!(task.await.unwrap().unwrap().details["decision"], "approved");
    }

    #[tokio::test]
    async fn approval_timeout_applies_fail_closed_default() {
        let (_tx, stream) = stream();
        let tool = RequestHumanApprovalTool::new(stream).with_timeout(Duration::from_millis(25));
        let (ctx, _updates) = make_tool_ctx(CancellationToken::new());
        let result = tool
            .execute(json!({"question": "proceed?"}), &ctx)
            .await
            .unwrap();
        assert_eq!(result.details["decision"], "denied");
        assert_eq!(result.details["timed_out"], true);
    }

    #[tokio::test]
    async fn approval_invalid_response_fails_closed() {
        let (tx, stream) = stream();
        let tool = RequestHumanApprovalTool::new(stream);
        let (ctx, mut updates) = make_tool_ctx(CancellationToken::new());
        let task =
            tokio::spawn(async move { tool.execute(json!({"question": "proceed?"}), &ctx).await });
        assert!(updates.recv().await.is_some());
        tx.send(human_event(
            json!({"request_id": "tu_1", "decision": "maybe"}),
        ))
        .await
        .unwrap();

        let failure = task.await.unwrap().unwrap_err();
        assert_eq!(failure.code, "invalid_human_response");
    }

    #[tokio::test]
    async fn input_returns_supplied_value() {
        let (tx, stream) = stream();
        let tool = RequestHumanInputTool::new(stream);
        let (ctx, mut updates) = make_tool_ctx(CancellationToken::new());
        let task = tokio::spawn(async move {
            tool.execute(
                json!({"question": "Which database?", "context": ["postgres", "sqlite"]}),
                &ctx,
            )
            .await
        });
        let request = updates.recv().await.unwrap();
        assert_eq!(request.details["type"], "human_input_request");
        tx.send(human_event(
            json!({"request_id": "tu_1", "value": "postgres"}),
        ))
        .await
        .unwrap();

        let result = task.await.unwrap().unwrap();
        assert_eq!(result.details["value"], "postgres");
        assert_eq!(
            result.model_content[0],
            DataBlock::text("Human supplied: \"postgres\"")
        );
    }

    #[tokio::test]
    async fn input_timeout_uses_configured_default() {
        let (_tx, stream) = stream();
        let tool = RequestHumanInputTool::new(stream)
            .with_timeout(Duration::from_millis(25))
            .with_default(json!({"name": "fallback"}));
        let (ctx, _updates) = make_tool_ctx(CancellationToken::new());
        let result = tool
            .execute(json!({"question": "name?"}), &ctx)
            .await
            .unwrap();
        assert_eq!(result.details["value"], json!({"name": "fallback"}));
    }

    #[tokio::test]
    async fn closed_stream_fails_closed() {
        let (tx, rx) = mpsc::channel::<Event>(1);
        drop(tx);
        let stream: Arc<Mutex<Box<dyn EventStream>>> =
            Arc::new(Mutex::new(Box::new(ChannelStream { rx })));
        let tool = RequestHumanApprovalTool::new(stream);
        let (ctx, _updates) = make_tool_ctx(CancellationToken::new());
        let failure = tool
            .execute(json!({"question": "proceed?"}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(failure.code, "human_response_stream_closed");
    }

    #[tokio::test]
    async fn cancellation_is_reported_as_aborted() {
        let (_tx, stream) = stream();
        let tool = RequestHumanApprovalTool::new(stream);
        let abort = CancellationToken::new();
        let (ctx, _updates) = make_tool_ctx(abort.clone());
        let task =
            tokio::spawn(async move { tool.execute(json!({"question": "proceed?"}), &ctx).await });
        abort.cancel();
        let failure = task.await.unwrap().unwrap_err();
        assert_eq!(failure.code, "aborted");
    }
}
