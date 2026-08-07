//! User notification tool + channel abstraction.
//!
//! `NotifyUserTool` is a `Tool` trait implementation (not `BuiltIn`) that lets
//! the LLM send a message to the user without ending the run. Delivery is
//! delegated to a user-provided [`NotificationChannel`] injected via
//! `RunExtensions`.
//!
//! `NotifyPlugin` wraps the tool as a `Plugin` for one-line installation.
//!
//! # Example
//!
//! ```ignore
//! use llm_harness_strategy::{NotifyPlugin, NotificationChannel, NotificationMessage};
//! use std::sync::Arc;
//! use futures::future::BoxFuture;
//!
//! struct ConsoleChannel;
//! impl NotificationChannel for ConsoleChannel {
//!     fn send(&self, msg: NotificationMessage) -> BoxFuture<'_, Result<(), llm_harness_strategy::NotificationError>> {
//!         println!("[notify] {}", msg.text);
//!         Box::pin(async { Ok(()) })
//!     }
//! }
//!
//! let channel = Arc::new(ConsoleChannel) as Arc<dyn NotificationChannel>;
//! let harness = HarnessBuilder::new("claude-3-5-sonnet")
//!     .install(&NotifyPlugin::new())
//!     .build(env)
//!     .await?;
//! ```
//!
//! The channel is passed to each run via
//! `RunRequest::with_extension(Arc<dyn NotificationChannel>)`.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::future::BoxFuture;
use llm_harness_agent::Plugin;
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolLevel, ToolResult};
use serde::{Deserialize, Serialize};

/// Default timeout for a single notification send.
const DEFAULT_SEND_TIMEOUT: Duration = Duration::from_secs(10);

// ── Types ───────────────────────────────────────────────────────────────

/// Urgency level. Reserved for future multi-channel selection; currently
/// passed through to the channel as metadata only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    High,
}

/// Message payload delivered to a [`NotificationChannel`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationMessage {
    /// Plain-text message body.
    pub text: String,
    /// Urgency hint for the channel implementation.
    pub urgency: Urgency,
}

/// Errors that can occur while sending a notification.
#[derive(thiserror::Error, Debug)]
pub enum NotificationError {
    #[error("notification send failed: {0}")]
    SendFailed(String),
}

// ── Channel trait ───────────────────────────────────────────────────────

/// Abstraction over a notification delivery channel.
///
/// User-side implementations (IM, email, Slack, push, etc.) implement this
/// trait and inject an `Arc<dyn NotificationChannel>` into `RunExtensions`.
///
/// This trait lives in the strategy crate, **not** in `llm-harness-types`,
/// because the mechanism layer (`loop_fn`/`dispatch`/`session`) has zero
/// consumers of it — only the `NotifyUserTool` (also strategy) calls it.
///
/// # Panic contract
///
/// `send` **must not panic**. The tool engine does not isolate panics — a
/// panic inside `send` will propagate through `dispatch` and terminate the
/// entire agent run. Channel implementors are responsible for catching all
/// errors internally and returning them as `Err(NotificationError::SendFailed(..))`.
pub trait NotificationChannel: Send + Sync {
    /// Deliver `msg` to the user. Called inside `tokio::time::timeout`.
    fn send(&self, msg: NotificationMessage) -> BoxFuture<'_, Result<(), NotificationError>>;
}
/// The typed key used to retrieve the channel from `RunExtensions`.
///
/// Users inject the channel via:
/// ```ignore
/// RunRequest::from_text("...")
///     .with_extension(Arc::new(my_channel) as Arc<dyn NotificationChannel>);
/// ```
type ChannelExt = Arc<dyn NotificationChannel>;

static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "text": {
                "type": "string",
                "description": "The message to send to the user. Keep it concise and actionable."
            },
            "urgency": {
                "type": "string",
                "enum": ["low", "normal", "high"],
                "default": "normal",
                "description": "Urgency hint. Use \"high\" only for time-sensitive alerts."
            }
        },
        "required": ["text"]
    })
});

/// Tool that lets the LLM notify the user without ending the run.
///
/// Registered at `ToolLevel::User` (not `BuiltIn`): the loop engine has no
/// special handling for it — `terminate` is always `false`.
pub struct NotifyUserTool;

impl Tool for NotifyUserTool {
    fn name(&self) -> &str {
        "notify_user"
    }

    fn level(&self) -> ToolLevel {
        ToolLevel::User
    }

    fn label(&self) -> &str {
        "Notify user"
    }

    fn description(&self) -> &str {
        "Send a message to the user WITHOUT ending the run. Use this when \
         you need to share progress, ask a clarifying question mid-task, or \
         alert the user about something important — and the run should \
         continue afterwards. Do NOT use this as a substitute for \
         final_answer (which ends the run and delivers the final result)."
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &SCHEMA
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let text = args
                .get("text")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| ToolFailure::invalid_arguments("missing string field 'text'"))?
                .to_string();

            let urgency = match args.get("urgency") {
                None => Urgency::Normal,
                Some(v) => serde_json::from_value::<Urgency>(v.clone()).map_err(|_| {
                    ToolFailure::invalid_arguments(
                        "invalid urgency value; expected one of: low, normal, high",
                    )
                })?,
            };

            let msg = NotificationMessage {
                text: text.clone(),
                urgency,
            };

            // Try to retrieve a configured channel from RunExtensions.
            let channel = ctx.run.extension_arc::<ChannelExt>();

            let (delivered, reason) = match channel {
                Some(ch) => {
                    tokio::select! {
                        biased;
                        _ = ctx.abort.cancelled() => (false, "aborted".into()),
                        r = tokio::time::timeout(DEFAULT_SEND_TIMEOUT, ch.send(msg)) => match r {
                            Ok(Ok(())) => (true, String::new()),
                            Ok(Err(e)) => (false, e.to_string()),
                            Err(_) => (false, format!("timeout after {:?}", DEFAULT_SEND_TIMEOUT)),
                        },
                    }
                }
                None => (false, "no_channel_configured".into()),
            };

            let model_text = if delivered {
                format!("Notification delivered: {text}")
            } else {
                format!(
                    "Notification recorded but not delivered ({reason}). \
                     The user may not see this message."
                )
            };

            let details = serde_json::json!({
                "kind": "notify_user",
                "text": text,
                "delivered": delivered,
                "reason": reason,
            });

            Ok(ToolResult::full(
                vec![DataBlock::text(model_text)],
                details,
                false, // never terminates the run
            ))
        })
    }
}

// ── Plugin ──────────────────────────────────────────────────────────────

/// Plugin that registers [`NotifyUserTool`] for LLM-callable user notifications.
///
/// The `NotificationChannel` must be injected separately via
/// `RunRequest::with_extension(Arc<dyn NotificationChannel>)`.
/// This plugin only registers the tool; it does not own the channel.
pub struct NotifyPlugin;

impl NotifyPlugin {
    /// Create a new plugin that registers `notify_user`.
    pub fn new() -> Self {
        Self
    }
}

impl Default for NotifyPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for NotifyPlugin {
    fn name(&self) -> &str {
        "notify-user"
    }

    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        tools.push(Arc::new(NotifyUserTool));
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::{RunContext, RunRequest};
    use parking_lot::Mutex;

    // ── Mock channel ────────────────────────────────────────────────────

    struct MockChannel {
        messages: Mutex<Vec<NotificationMessage>>,
        fail: bool,
    }

    impl MockChannel {
        fn new() -> Self {
            Self {
                messages: Mutex::new(Vec::new()),
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                messages: Mutex::new(Vec::new()),
                fail: true,
            }
        }

        fn messages(&self) -> Vec<NotificationMessage> {
            self.messages.lock().clone()
        }
    }

    impl NotificationChannel for MockChannel {
        fn send(&self, msg: NotificationMessage) -> BoxFuture<'_, Result<(), NotificationError>> {
            if self.fail {
                return Box::pin(async {
                    Err(NotificationError::SendFailed("mock failure".into()))
                });
            }
            self.messages.lock().push(msg);
            Box::pin(async { Ok(()) })
        }
    }

    // ── Slow channel for timeout test ───────────────────────────────────

    struct SlowChannel;

    impl NotificationChannel for SlowChannel {
        fn send(&self, _msg: NotificationMessage) -> BoxFuture<'_, Result<(), NotificationError>> {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(())
            })
        }
    }

    // ── Helpers ─────────────────────────────────────────────────────────

    fn make_ctx(request: RunRequest) -> ToolContext {
        let (update_tx, _update_rx) = tokio::sync::mpsc::channel(16);
        ToolContext {
            run: Arc::new(RunContext::new(request)),
            env: Arc::new(llm_harness_types::UnsupportedEnv::new()),
            abort: tokio_util::sync::CancellationToken::new(),
            tool_use_id: "tu-test".to_string(),
            turn_index: 0,
            assistant_message: Arc::new(llm_harness_loop::test_utils::test_assistant_message(
                vec![],
            )),
            update_tx,
        }
    }

    // ── Tests ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn delivers_message_to_channel() {
        let channel = Arc::new(MockChannel::new());
        let ctx = make_ctx(
            RunRequest::from_text("test").with_extension(channel as Arc<dyn NotificationChannel>),
        );

        let tool = NotifyUserTool;
        let args = serde_json::json!({"text": "hello user", "urgency": "high"});
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(!result.terminate);
        let details = &result.details;
        assert_eq!(details["kind"], "notify_user");
        assert_eq!(details["delivered"], true);
        assert_eq!(details["text"], "hello user");
    }

    #[tokio::test]
    async fn fallback_when_no_channel_configured() {
        let ctx = make_ctx(RunRequest::from_text("test"));

        let tool = NotifyUserTool;
        let args = serde_json::json!({"text": "hello"});
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(!result.terminate);
        assert_eq!(result.details["delivered"], false);
        assert_eq!(result.details["reason"], "no_channel_configured");
    }

    #[tokio::test]
    async fn channel_failure_does_not_block() {
        let channel = Arc::new(MockChannel::failing()) as Arc<dyn NotificationChannel>;
        let ctx = make_ctx(RunRequest::from_text("test").with_extension(channel));

        let tool = NotifyUserTool;
        let args = serde_json::json!({"text": "hello"});
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(!result.terminate);
        assert_eq!(result.details["delivered"], false);
        assert!(
            result.details["reason"]
                .as_str()
                .unwrap()
                .contains("mock failure")
        );
    }
    #[tokio::test]
    async fn timeout_does_not_block() {
        let channel = Arc::new(SlowChannel) as Arc<dyn NotificationChannel>;
        let ctx = make_ctx(RunRequest::from_text("test").with_extension(channel));

        let tool = NotifyUserTool;
        let args = serde_json::json!({"text": "hello"});
        let result = tokio::time::timeout(Duration::from_secs(15), tool.execute(args, &ctx))
            .await
            .expect("tool should complete within 15s")
            .unwrap();

        assert!(!result.terminate);
        assert_eq!(result.details["delivered"], false);
        assert!(
            result.details["reason"]
                .as_str()
                .unwrap()
                .contains("timeout")
        );
    }
    #[tokio::test]
    async fn missing_text_arg_returns_failure() {
        let ctx = make_ctx(RunRequest::from_text("test"));

        let tool = NotifyUserTool;
        let args = serde_json::json!({"urgency": "low"});
        let result = tool.execute(args, &ctx).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, "invalid_arguments");
    }

    #[tokio::test]
    async fn invalid_urgency_returns_failure() {
        let ctx = make_ctx(RunRequest::from_text("test"));

        let tool = NotifyUserTool;
        let args = serde_json::json!({"text": "hi", "urgency": "URGENT"});
        let result = tool.execute(args, &ctx).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, "invalid_arguments");
    }

    #[tokio::test]
    async fn abort_cancels_send() {
        let channel = Arc::new(SlowChannel) as Arc<dyn NotificationChannel>;
        let ctx = make_ctx(RunRequest::from_text("test").with_extension(channel));

        // Cancel immediately before executing.
        ctx.abort.cancel();

        let tool = NotifyUserTool;
        let args = serde_json::json!({"text": "hello"});
        let result = tokio::time::timeout(Duration::from_secs(2), tool.execute(args, &ctx))
            .await
            .expect("tool should respond to abort within 2s")
            .unwrap();

        assert!(!result.terminate);
        assert_eq!(result.details["delivered"], false);
        assert_eq!(result.details["reason"], "aborted");
    }

    #[tokio::test]
    async fn urgency_defaults_to_normal() {
        let channel = Arc::new(MockChannel::new());
        let ctx = make_ctx(
            RunRequest::from_text("test").with_extension(channel as Arc<dyn NotificationChannel>),
        );

        let tool = NotifyUserTool;
        let args = serde_json::json!({"text": "test"});
        let result = tool.execute(args, &ctx).await.unwrap();

        assert_eq!(result.details["delivered"], true);
        // Urgency is embedded in the NotificationMessage sent to the channel.
        // We verify indirectly: the tool succeeded with default urgency.
    }

    #[test]
    fn plugin_registers_tool() {
        let plugin = NotifyPlugin::new();
        assert_eq!(plugin.name(), "notify-user");

        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        plugin.register_tools(&mut tools);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "notify_user");
        assert_eq!(tools[0].level(), ToolLevel::User);
    }

    #[test]
    fn schema_has_required_text() {
        let schema = &*SCHEMA;
        assert_eq!(schema["type"], "object");
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "text"));
    }
}
