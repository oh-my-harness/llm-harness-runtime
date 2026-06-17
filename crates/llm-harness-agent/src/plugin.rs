use std::sync::Arc;

use crate::harness::HarnessHooks;
use crate::session::types::SessionEntry;
use crate::skills::{PromptTemplate, Skill};
use llm_harness_types::Tool;

// ── Plugin trait ───────────────────────────────────────────────────────────────

/// A pluggable component that extends `AgentHarness` with custom tools, hooks,
/// skills, templates, and state providers.
///
/// The plugin model allows applications to modularly inject functionality at
/// harness construction time. Plugins are registered in declaration order and
/// compose their contributions deterministically:
///
/// - **Tools**: accumulated in registration order via `Vec<Arc<dyn Tool>>`
/// - **Hooks**: accumulated in registration order via hook vectors in `HarnessHooks`
/// - **Skills**: accumulated in registration order
/// - **Templates**: accumulated in registration order
/// - **StateProvider**: last registered provider is active (only one provider at a time)
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
/// Two plugin types can conflict:
///
/// 1. **Tool name conflicts**: If two plugins register tools with the same `name()`,
///    both are added to the registry. The harness tool dispatcher will see both;
///    the behavior depends on the dispatcher implementation (typically: first match,
///    or error on duplicate).
///
/// 2. **StateProvider conflicts**: Only one `StateProvider` can be active. If multiple
///    plugins register a `StateProvider`, the last one wins. Earlier providers are
///    discarded.
///
/// 3. **Skill/template name conflicts**: If two plugins provide the same skill or
///    template name, both are added. Skills/templates are looked up by name; which
///    one is used depends on the lookup order and de-duplication logic.
///
/// Applications should document plugin loading order and test for conflicts.
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
    /// Used for logging and diagnostics. Must be unique across all plugins
    /// loaded in a single harness.
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

    /// Provide a custom state provider for managing plugin-specific session state.
    ///
    /// Called once at harness construction. Return `Some(Arc<dyn StateProvider>)`
    /// to enable custom entry type handling and state reconstruction from branch history.
    /// Default implementation returns `None`.
    ///
    /// If multiple plugins register a `StateProvider`, the last one wins
    /// (earlier providers are discarded).
    ///
    /// # Returns
    ///
    /// `Some(provider)` if this plugin provides custom state management,
    /// `None` otherwise.
    ///
    /// # Example
    ///
    /// ```ignore
    /// fn state_provider(&self) -> Option<Arc<dyn StateProvider>> {
    ///     Some(Arc::new(MyStateProvider))
    /// }
    /// ```
    fn state_provider(&self) -> Option<Arc<dyn StateProvider>> {
        None
    }
}

// ── StateProvider trait ────────────────────────────────────────────────────────

/// Manages plugin-specific session state stored as custom `SessionEntry` items.
///
/// When a session is loaded from disk, the framework reconstructs the session tree
/// by replaying entries in order. For built-in entry types (messages, model changes, etc.),
/// the framework handles reconstruction internally. For plugin-defined custom entry types,
/// the `StateProvider` is responsible for interpreting and integrating them.
///
/// ## Entry Type Identification
///
/// Custom entries are tagged with a `custom_type` field (see `SessionEntryPayload::Custom`).
/// Your `StateProvider` receives entries and must match on `entry_type()` to identify
/// its own entries.
///
/// ## Interior Mutability Requirement
///
/// `StateProvider` implementations must use **interior mutability** (e.g., `Mutex<T>`)
/// to accumulate state from `rebuild()`. The `rebuild()` method receives `&self`, not
/// `&mut self`, so all state updates must go through a non-mutable reference pattern.
///
/// # Example: Auth State Provider
///
/// ```ignore
/// use std::sync::Mutex;
///
/// pub struct AuthStateProvider {
///     current_token: Mutex<Option<String>>,
/// }
///
/// impl AuthStateProvider {
///     pub fn new() -> Self {
///         Self {
///             current_token: Mutex::new(None),
///         }
///     }
///
///     pub fn current_token(&self) -> Option<String> {
///         self.current_token.lock().unwrap().clone()
///     }
/// }
///
/// impl StateProvider for AuthStateProvider {
///     fn entry_type(&self) -> &str {
///         "auth-token"
///     }
///
///     fn rebuild(&self, entries: &[SessionEntry]) {
///         for entry in entries {
///             if let SessionEntryPayload::Custom {
///                 custom_type,
///                 data,
///             } = &entry.payload
///             {
///                 if custom_type == self.entry_type() {
///                     if let Ok(token) = serde_json::from_value::<String>(data.clone()) {
///                         *self.current_token.lock().unwrap() = Some(token);
///                     }
///                 }
///             }
///         }
///     }
/// }
/// ```
///
/// ## Usage Lifecycle
///
/// 1. Plugin registers its `StateProvider` in `register_hooks()` or `state_provider()`
/// 2. At harness construction, the provider is stored
/// 3. When a session is loaded, the framework calls `rebuild(&entries)` to replay state
/// 4. The provider maintains internal state via `Mutex` or similar
/// 5. Application code queries the provider's public API (e.g., `current_token()`)
///
/// The framework does not actively manage the provider's state; the provider owns
/// that responsibility.
pub trait StateProvider: Send + Sync {
    /// Identifies the custom entry type(s) this provider handles.
    ///
    /// When replaying entries, the framework calls this method and compares it
    /// against each entry's `custom_type` field. If they match, the provider's
    /// `rebuild()` method is invoked.
    ///
    /// # Returns
    ///
    /// A string identifying the custom entry type (e.g., `"auth-token"`, `"user-context"`).
    /// Must be consistent across `rebuild()` invocations and session loads.
    fn entry_type(&self) -> &str;

    /// Rebuild internal state from a sequence of session entries.
    ///
    /// Called once per session load (after opening a session from disk). The entries
    /// are provided in order and may include entries from many branches (if called
    /// during session-wide state reconstruction).
    ///
    /// The provider should use interior mutability to accumulate state. For example,
    /// if the last entry of a given type contains a token, set that token; earlier
    /// entries are superseded.
    ///
    /// # Arguments
    ///
    /// * `entries` - all session entries in chronological order
    ///
    /// # Example
    ///
    /// Scan entries for the custom type, extract relevant data, and store in `Mutex`:
    ///
    /// ```ignore
    /// fn rebuild(&self, entries: &[SessionEntry]) {
    ///     for entry in entries {
    ///         if let SessionEntryPayload::Custom {
    ///             custom_type,
    ///             data,
    ///         } = &entry.payload
    ///         {
    ///             if custom_type == self.entry_type() {
    ///                 // parse data and update internal state
    ///                 let _ = /* extract from data */;
    ///             }
    ///         }
    ///     }
    /// }
    /// ```
    fn rebuild(&self, entries: &[SessionEntry]);
}
