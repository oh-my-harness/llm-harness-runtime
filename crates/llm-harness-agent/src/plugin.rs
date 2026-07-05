use std::sync::Arc;

use crate::harness::HarnessHooks;
use crate::skills::{PromptTemplate, Skill};
use llm_harness_types::Tool;

// ── Plugin trait ───────────────────────────────────────────────────────────────

/// A pluggable component that extends `AgentHarness` with custom tools, hooks,
/// skills, and templates.
/// The plugin model allows applications to modularly inject functionality at
/// harness construction time. Plugins are registered in declaration order and
/// compose their contributions deterministically:
///
/// - **Tools**: accumulated in registration order via `Vec<Arc<dyn Tool>>`
/// - **Hooks**: accumulated in registration order via hook vectors in `HarnessHooks`
/// - **Skills**: accumulated in registration order
/// - **Templates**: accumulated in registration order
///
/// ## Plugin Installation Order Matters
///
/// Plugins should document their dependencies explicitly. For example:
/// - A plugin requiring auth should register after the auth plugin
/// - A plugin that wraps tool execution should register before domain-specific tools
///
/// The harness will not validate or reorder plugins; registration order is the
/// caller's responsibility.
///
/// ## Conflict Resolution
///
/// `install` does not deduplicate or diagnose name conflicts at registration
/// time; the four contribution vectors are appended as-is. Behavior when two
/// plugins contribute the same name differs by resource type:
///
/// 1. **Tool name conflicts**: both tools are kept in the `Vec<Arc<dyn Tool>>`.
///    At dispatch time the loop builds a `HashMap<&str, Arc<dyn Tool>>` keyed by
///    `name()`, so the **last-registered** tool with a given name wins (the
///    earlier entry is silently shadowed). No diagnostic is emitted.
///
/// 2. **Skill / template name conflicts**: both entries are kept. Lookup uses
///    `iter().find(|s| s.name == name)`, so the **first-registered** entry wins.
///    No diagnostic is emitted on the `install` path. (By contrast, the
///    directory-loading path — `load_skills` / `load_prompt_templates` — emits
///    a `duplicate ... name` warning diagnostic; that path is not taken for
///    plugin-contributed skills/templates.)
///
/// Applications that care about conflict visibility should validate plugin
/// contributions before calling `install`, or rely on directory loading (which
/// does diagnose duplicates).
///
/// ## Minimal Example
///
/// ```ignore
/// struct MyPlugin;
///
/// impl Plugin for MyPlugin {
///     fn name(&self) -> &str {
///         "my-plugin"
///     }
///
///     fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
///         // tools.push(Arc::new(MyTool));
///     }
///
///     fn register_hooks(&self, hooks: &mut HarnessHooks) {
///         // hooks.before_turn.push(Arc::new(MyBeforeTurnHook));
///     }
/// }
/// ```
pub trait Plugin: Send + Sync {
    /// Unique identifier for this plugin (e.g., `"auth-google"`, `"tool-web-search"`).
    ///
    /// Used for logging and diagnostics. Should be unique across all plugins
    /// loaded in a single harness; duplicates are accepted but may cause
    /// confusion in diagnostics.
    fn name(&self) -> &str;

    /// Register tools to be available in this harness.
    ///
    /// Called once at harness construction. Append tools to `tools`.
    /// Default implementation does nothing.
    ///
    /// # Arguments
    ///
    /// * `tools` - mutable vector of registered tools; append new tools here
    fn register_tools(&self, _tools: &mut Vec<Arc<dyn Tool>>) {}

    /// Register hooks to observe and control harness execution.
    ///
    /// Called once at harness construction. Add hooks to the appropriate vector
    /// in `hooks`. Default implementation does nothing.
    ///
    /// # Arguments
    ///
    /// * `hooks` - mutable collection of hook vectors; add hooks to desired vectors
    ///
    /// # Example
    ///
    /// ```ignore
    /// fn register_hooks(&self, hooks: &mut HarnessHooks) {
    ///     hooks.before_turn.push(Arc::new(MyBeforeTurnHook));
    ///     hooks.after_turn.push(Arc::new(MyAfterTurnHook));
    /// }
    /// ```
    fn register_hooks(&self, _hooks: &mut HarnessHooks) {}

    /// Register skills (runnable procedures) to be available in this harness.
    ///
    /// Called once at harness construction. Append skills to `skills`.
    /// Default implementation does nothing.
    ///
    /// # Arguments
    ///
    /// * `skills` - mutable vector of loaded skills; append new skills here
    fn register_skills(&self, _skills: &mut Vec<Skill>) {}

    /// Register prompt templates to be available in this harness.
    ///
    /// Called once at harness construction. Append templates to `templates`.
    /// Default implementation does nothing.
    ///
    /// # Arguments
    ///
    /// * `templates` - mutable vector of loaded templates; append new templates here
    fn register_templates(&self, _templates: &mut Vec<PromptTemplate>) {}
}
