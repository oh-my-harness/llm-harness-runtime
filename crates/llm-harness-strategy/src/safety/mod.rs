//! Safety defaults plugin — bash command blacklist + path traversal protection.

pub mod command;
pub mod deny;
pub mod path;

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{
    BeforeToolCallCtx, BeforeToolCallDecision, BeforeToolCallHook, ExecutionEnv,
};

use crate::safety::command::{CommandSafetyPredicate, DefaultCommandBlacklist, SafetyVerdict};
use crate::safety::deny::{deny_message, path_deny_message};
use crate::safety::path::{PathSafetyConfig, canonicalize_check, static_check};
use llm_harness_agent::{HarnessHooks, Plugin};

/// The hook that dispatches safety checks by tool name.
///
/// - `bash` → command safety check (synchronous)
/// - `write`/`edit`/`read` → static + canonicalize path check
/// - other tools → Allow
///
/// `command` is held as `Arc<dyn CommandSafetyPredicate>` rather than `Box`
/// because `Plugin::register_hooks` takes `&self` and must clone the predicate
/// into each registered hook instance — it cannot move out of a `Box`.
pub struct SafetyHook {
    /// Command safety predicate.
    command: Arc<dyn CommandSafetyPredicate>,
    /// Path safety configuration.
    path_config: PathSafetyConfig,
}

impl SafetyHook {
    /// Create a new safety hook with the given command predicate and path config.
    pub fn new(command: Arc<dyn CommandSafetyPredicate>, path_config: PathSafetyConfig) -> Self {
        Self {
            command,
            path_config,
        }
    }
}

impl BeforeToolCallHook for SafetyHook {
    fn on_call<'a>(&'a self, ctx: BeforeToolCallCtx<'a>) -> BoxFuture<'a, BeforeToolCallDecision> {
        Box::pin(async move {
            match ctx.tool_name {
                "bash" => {
                    let command_str = ctx
                        .args
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let verdict = self.command.check(command_str);
                    match verdict {
                        SafetyVerdict::Allow => BeforeToolCallDecision::Allow,
                        d @ SafetyVerdict::Deny { .. } => {
                            BeforeToolCallDecision::Deny(deny_message(&d, "bash", command_str))
                        }
                    }
                }
                "write" | "edit" | "read" => {
                    let path = ctx.args.get("path").and_then(|v| v.as_str()).unwrap_or("");

                    // Determine root: config override or env working_dir.
                    let (root, env_opt) = match &self.path_config.root {
                        Some(r) => (r.clone(), None),
                        None => {
                            // Try to get ExecutionEnv from RunExtensions.
                            match ctx.run.extension_arc::<Arc<dyn ExecutionEnv>>() {
                                Some(env) => (env.working_dir().to_path_buf(), Some(env)),
                                None => {
                                    tracing::warn!(
                                        "ExecutionEnv not in RunExtensions; \
                                         skipping canonicalize check, using '.' as root"
                                    );
                                    (std::path::PathBuf::from("."), None)
                                }
                            }
                        }
                    };

                    // Static check (synchronous).
                    if let Err(violation) = static_check(path, &root, &self.path_config) {
                        return BeforeToolCallDecision::Deny(path_deny_message(&violation, path));
                    }

                    // Canonicalize check (async, only if env available and root came from env).
                    if let Some(env) = env_opt
                        && let Err(violation) =
                            canonicalize_check(path, &root, &**env, &self.path_config).await
                    {
                        return BeforeToolCallDecision::Deny(path_deny_message(&violation, path));
                    }

                    BeforeToolCallDecision::Allow
                }
                _ => BeforeToolCallDecision::Allow,
            }
        })
    }
}

/// Safety defaults plugin: bash command blacklist + path traversal protection.
///
/// Install with `builder.install(&SafetyDefaultsPlugin::new())`.
///
/// Holds the command predicate as `Arc<dyn CommandSafetyPredicate>` so that
/// `Plugin::register_hooks(&self, ...)` can clone the predicate into each
/// registered [`SafetyHook`] without moving out of `&self`.
pub struct SafetyDefaultsPlugin {
    command: Arc<dyn CommandSafetyPredicate>,
    path_config: PathSafetyConfig,
}

impl SafetyDefaultsPlugin {
    /// Create with default blacklist rules and default path config.
    pub fn new() -> Self {
        Self {
            command: Arc::new(DefaultCommandBlacklist::new()),
            path_config: PathSafetyConfig::new(),
        }
    }

    /// Replace the command safety predicate (extension point for custom
    /// semantic parsers).
    pub fn command_predicate(mut self, p: Arc<dyn CommandSafetyPredicate>) -> Self {
        self.command = p;
        self
    }

    /// Set custom path safety configuration.
    pub fn path_config(mut self, cfg: PathSafetyConfig) -> Self {
        self.path_config = cfg;
        self
    }
}

impl Default for SafetyDefaultsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for SafetyDefaultsPlugin {
    fn name(&self) -> &str {
        "safety-defaults"
    }

    fn register_hooks(&self, hooks: &mut HarnessHooks) {
        hooks.before_tool_call.push(Arc::new(SafetyHook::new(
            Arc::clone(&self.command),
            self.path_config.clone(),
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::{
        AssistantMessage, BeforeToolCallCtx, BeforeToolCallDecision, RunContext, RunRequest,
    };

    fn test_run() -> &'static RunContext {
        static RUN: std::sync::LazyLock<RunContext> =
            std::sync::LazyLock::new(|| RunContext::new(RunRequest::default()));
        &RUN
    }

    fn make_ctx<'a>(
        msg: &'a AssistantMessage,
        tool_name: &'a str,
        args: &'a serde_json::Value,
    ) -> BeforeToolCallCtx<'a> {
        BeforeToolCallCtx {
            run: test_run(),
            assistant_message: msg,
            tool_use_id: "tu_1",
            tool_name,
            args,
            turn_index: 0,
        }
    }

    fn empty_message() -> AssistantMessage {
        llm_harness_loop::test_utils::test_assistant_message(vec![])
    }

    #[tokio::test]
    async fn plugin_registers_hook() {
        let plugin = SafetyDefaultsPlugin::new();
        let mut hooks = HarnessHooks::none();
        plugin.register_hooks(&mut hooks);
        assert!(!hooks.before_tool_call.is_empty());
    }

    #[tokio::test]
    async fn hook_denies_dangerous_bash() {
        let hook = SafetyHook::new(
            Arc::new(DefaultCommandBlacklist::new()),
            PathSafetyConfig::new(),
        );
        let msg = empty_message();
        let args = serde_json::json!({"command": "rm -rf /"});
        let ctx = make_ctx(&msg, "bash", &args);
        let decision = hook.on_call(ctx).await;
        assert!(matches!(decision, BeforeToolCallDecision::Deny(_)));
    }

    #[tokio::test]
    async fn hook_allows_safe_bash() {
        let hook = SafetyHook::new(
            Arc::new(DefaultCommandBlacklist::new()),
            PathSafetyConfig::new(),
        );
        let msg = empty_message();
        let args = serde_json::json!({"command": "ls -la"});
        let ctx = make_ctx(&msg, "bash", &args);
        let decision = hook.on_call(ctx).await;
        assert!(matches!(decision, BeforeToolCallDecision::Allow));
    }

    #[tokio::test]
    async fn hook_denies_path_traversal() {
        let hook = SafetyHook::new(
            Arc::new(DefaultCommandBlacklist::new()),
            PathSafetyConfig::new(),
        );
        let msg = empty_message();
        let args = serde_json::json!({"path": "../../etc/passwd"});
        let ctx = make_ctx(&msg, "write", &args);
        let decision = hook.on_call(ctx).await;
        assert!(matches!(decision, BeforeToolCallDecision::Deny(_)));
    }

    #[tokio::test]
    async fn hook_allows_safe_path() {
        let hook = SafetyHook::new(
            Arc::new(DefaultCommandBlacklist::new()),
            PathSafetyConfig::new(),
        );
        let msg = empty_message();
        let args = serde_json::json!({"path": "src/main.rs"});
        let ctx = make_ctx(&msg, "write", &args);
        let decision = hook.on_call(ctx).await;
        assert!(matches!(decision, BeforeToolCallDecision::Allow));
    }

    #[tokio::test]
    async fn hook_allows_unknown_tool() {
        let hook = SafetyHook::new(
            Arc::new(DefaultCommandBlacklist::new()),
            PathSafetyConfig::new(),
        );
        let msg = empty_message();
        let args = serde_json::json!({});
        let ctx = make_ctx(&msg, "some_custom_tool", &args);
        let decision = hook.on_call(ctx).await;
        assert!(matches!(decision, BeforeToolCallDecision::Allow));
    }
}
