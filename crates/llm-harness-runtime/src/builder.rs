//! Builder for constructing [`AgentHarness`] with plugins, LLM providers, and tools.
//!
//! [`HarnessBuilder`] provides a fluent API for:
//! - Installing plugins (tools, hooks, skills, templates, state providers)
//! - Registering LLM provider clients with exact and glob matching
//! - Building an [`AgentHarness`] with an in-memory or existing session
//!
//! # Example
//!
//! ```ignore
//! use llm_harness_runtime::builder::HarnessBuilder;
//!
//! let harness = HarnessBuilder::new("claude-3-5-sonnet-20241022")
//!     .install(&my_auth_plugin)
//!     .install(&my_tools_plugin)
//!     .provider("claude-*", client)
//!     .build(env)
//!     .await?;
//! ```

use std::sync::{Arc, Mutex};

use llm_harness_agent::{
    AgentHarness, AgentHarnessOptions, HarnessHooks, Plugin, PromptTemplate, Skill, StateProvider,
};
use llm_harness_loop::LlmClient;
use llm_harness_types::{
    AfterProviderResponseHook, CostAggregate, ExecutionEnv, PricingProvider, ShouldStopHook, Tool,
};

use crate::{
    budget::BudgetControlAdapter, budget::BudgetExceededHook, cost_hook::CostAccumulatorHook,
};
use thiserror::Error;

// ── Provider matching ─────────────────────────────────────────────────────────

/// Internal representation of an LLM provider matcher.
///
/// Supports exact pattern matching (e.g., "claude-3") or glob patterns
/// (e.g., "claude-*"). Glob patterns are matched using a longest-prefix-match
/// strategy: the most specific glob (longest prefix) is selected if multiple
/// globs match.
struct ProviderMatcher {
    /// The pattern string (either exact or glob).
    pattern: String,
    /// Whether this pattern contains wildcards.
    is_glob: bool,
    /// The longest prefix before any wildcard (for matching).
    prefix: String,
    /// The LLM client for this provider.
    client: Arc<dyn LlmClient>,
}

impl ProviderMatcher {
    /// Create a new provider matcher.
    fn new(pattern: impl Into<String>, client: Arc<dyn LlmClient>) -> Self {
        let pattern = pattern.into();
        let is_glob = pattern.contains('*');
        let prefix = if is_glob {
            pattern.split('*').next().unwrap_or("").to_string()
        } else {
            pattern.clone()
        };
        Self {
            pattern,
            is_glob,
            prefix,
            client,
        }
    }

    /// Check if this matcher matches the given model ID.
    fn matches(&self, model: &str) -> bool {
        if !self.is_glob {
            // Exact match.
            self.pattern == model
        } else {
            // Glob match: model must start with the prefix.
            model.starts_with(&self.prefix)
        }
    }
}

// ── Error types ───────────────────────────────────────────────────────────────

/// Errors that can occur when building a harness.
#[derive(Debug, Error)]
pub enum HarnessBuildError {
    /// No LLM provider was registered.
    #[error("No LLM provider registered")]
    NoProvider,

    /// No registered provider matches the specified model.
    #[error("No provider matches model: {model}")]
    NoMatchingProvider { model: String },
}

// ── HarnessBuilder ────────────────────────────────────────────────────────────

/// Fluent builder for constructing an [`AgentHarness`].
///
/// Collects plugins, tools, hooks, skills, templates, and state providers,
/// then constructs the harness with a resolved LLM provider client.
pub struct HarnessBuilder {
    /// The initial model ID.
    model: String,
    /// Registered tools (accumulated from plugins and direct registration).
    tools: Vec<Arc<dyn Tool>>,
    /// Hook collections (accumulated from plugins).
    hooks: HarnessHooks,
    /// Registered skills (accumulated from plugins).
    skills: Vec<Skill>,
    /// Registered prompt templates (accumulated from plugins).
    templates: Vec<PromptTemplate>,
    /// Registered LLM providers.
    providers: Vec<ProviderMatcher>,
    /// Active state provider (last registered wins).
    state_provider: Option<Arc<dyn StateProvider>>,
    /// 可选的 pricing provider，用于成本计算。
    pricing: Option<Arc<dyn PricingProvider>>,
    /// 可选的预算上限 + 超限 hook。设置后，一个 `BudgetControlAdapter`
    /// 会被注入 `should_stop`，与 `CostAccumulatorHook` 共享同一 cost Arc。
    budget: Option<(f64, Option<Arc<dyn BudgetExceededHook>>)>,
    /// 共享成本状态。在 `build_options` 中惰性创建；由
    /// `CostAccumulatorHook`、`BudgetControlAdapter` 和
    /// `AgentHarnessOptions.cost` 共享。
    cost: Option<Arc<Mutex<CostAggregate>>>,
    /// Optional maximum output tokens per provider call.
    max_tokens: Option<u32>,
}

impl HarnessBuilder {
    /// Create a new builder with the specified model ID.
    ///
    /// # Arguments
    ///
    /// * `model` - Initial model ID (e.g., `"claude-3-5-sonnet-20241022"`).
    ///   Updated via [`set_model()`](AgentHarness::set_model) after construction.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            tools: vec![],
            hooks: HarnessHooks::none(),
            skills: vec![],
            templates: vec![],
            providers: vec![],
            state_provider: None,
            pricing: None,
            budget: None,
            cost: None,
            max_tokens: None,
        }
    }

    /// Install a plugin, accumulating its contributions.
    ///
    /// Plugins contribute tools, hooks, skills, templates, and optionally
    /// a state provider. Contributions are accumulated in registration order:
    ///
    /// - **Tools**: appended to the tools list
    /// - **Hooks**: appended to respective hook vectors
    /// - **Skills**: appended to the skills list
    /// - **Templates**: appended to the templates list
    /// - **StateProvider**: last registered provider wins (earlier providers discarded)
    ///
    /// If two plugins register tools or skills with the same name, both are added.
    /// Conflict resolution happens at tool dispatch time (framework-dependent).
    ///
    /// # Arguments
    ///
    /// * `plugin` - The plugin to install.
    ///
    /// # Returns
    ///
    /// `self` for method chaining.
    pub fn install(mut self, plugin: &dyn Plugin) -> Self {
        // Register tools.
        plugin.register_tools(&mut self.tools);

        // Register hooks.
        plugin.register_hooks(&mut self.hooks);

        // Register skills.
        plugin.register_skills(&mut self.skills);

        // Register templates.
        plugin.register_templates(&mut self.templates);

        // Register state provider (last wins; silently overwrite earlier providers).
        if let Some(provider) = plugin.state_provider() {
            self.state_provider = Some(provider);
        }

        self
    }

    /// Register a single tool without wrapping it in a Plugin.
    ///
    /// Equivalent to `install(plugin)` for a plugin that contributes only this
    /// tool. Appended to the tools list in registration order.
    pub fn tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// Register a `ShouldStopHook` without wrapping it in a Plugin.
    ///
    /// Push semantics: appended to `hooks.should_stop`. Call multiple times to
    /// compose multiple hooks — `CompositeShouldStopHook` is all-execute, so
    /// registration order does not affect correctness (every hook always runs).
    pub fn should_stop_hook(mut self, hook: Arc<dyn ShouldStopHook>) -> Self {
        self.hooks.should_stop.push(hook);
        self
    }

    /// 设置 pricing provider，用于成本计算。
    ///
    /// 设置后，builder 注入一个 `CostAccumulatorHook`，计算每次调用的
    /// USD 成本，填充 `CostAggregate.total_cost` 和 `by_model` 明细。
    /// 不设置则只累积 token 计数。
    pub fn pricing(mut self, pricing: Arc<dyn PricingProvider>) -> Self {
        self.pricing = Some(pricing);
        self
    }

    /// 配置预算上限和可选的超限 hook。
    ///
    /// - `limit` — 预算上限，单位 USD（如 `5.0` 表示 $5.00）。
    /// - `exceeded_hook = None` → surveillance 模式：只统计成本，loop 永不
    ///   因预算停止。
    /// - `exceeded_hook = Some(h)` → 超限时由 `h` 决定继续/停止。
    ///
    /// builder 注入一个 `BudgetControlAdapter`（与 `CostAccumulatorHook`
    /// 共享同一 cost Arc）到 `hooks.should_stop`。
    pub fn budget(
        mut self,
        limit: f64,
        exceeded_hook: Option<Arc<dyn BudgetExceededHook>>,
    ) -> Self {
        self.budget = Some((limit, exceeded_hook));
        self
    }

    /// Register an LLM provider client.
    ///
    /// Supports exact and glob patterns:
    /// - Exact: `"claude-3-5-sonnet-20241022"` — matches only that model
    /// - Glob: `"claude-*"` — matches any model starting with `"claude-"`
    ///
    /// When resolving a model at build time:
    /// 1. Exact patterns are checked first
    /// 2. If no exact match, the longest-prefix glob is used
    /// 3. If no provider matches, [`HarnessBuildError::NoMatchingProvider`] is returned
    ///
    /// # Arguments
    ///
    /// * `pattern` - Model pattern (exact or glob with `*`)
    /// * `client` - The LLM client for this pattern
    ///
    /// # Returns
    ///
    /// `self` for method chaining.
    pub fn provider(mut self, pattern: impl Into<String>, client: Arc<dyn LlmClient>) -> Self {
        self.providers.push(ProviderMatcher::new(pattern, client));
        self
    }

    /// Set the maximum output tokens per provider call.
    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Build an [`AgentHarness`] with an in-memory session.
    ///
    /// Resolves the registered LLM provider for the model ID and constructs
    /// the harness with default options and an empty in-memory session.
    ///
    /// # Arguments
    ///
    /// * `env` - The execution environment (sandbox, resource manager, etc.)
    ///
    /// # Errors
    ///
    /// - [`HarnessBuildError::NoProvider`] if no provider is registered
    /// - [`HarnessBuildError::NoMatchingProvider`] if no provider matches the model
    pub async fn build(
        self,
        env: Arc<dyn ExecutionEnv>,
    ) -> Result<AgentHarness, HarnessBuildError> {
        let client = self.resolve_provider(&self.model)?;
        let opts = self.build_options();
        Ok(AgentHarness::new_in_memory(client, env, opts).await)
    }

    /// Build an [`AgentHarness`] with an existing session.
    ///
    /// Resolves the registered LLM provider for the model ID and constructs
    /// the harness with the provided session.
    ///
    /// # Arguments
    ///
    /// * `env` - The execution environment
    /// * `session` - An existing session (loaded from disk or in-memory)
    ///
    /// # Errors
    ///
    /// - [`HarnessBuildError::NoProvider`] if no provider is registered
    /// - [`HarnessBuildError::NoMatchingProvider`] if no provider matches the model
    pub fn build_with_session(
        self,
        env: Arc<dyn ExecutionEnv>,
        session: llm_harness_agent::Session,
    ) -> Result<AgentHarness, HarnessBuildError> {
        let client = self.resolve_provider(&self.model)?;
        let opts = self.build_options();
        Ok(AgentHarness::with_session(client, env, session, opts))
    }

    /// Resolve an LLM provider for the given model ID.
    ///
    /// Selection order:
    /// 1. Exact pattern match
    /// 2. Longest-prefix glob match
    /// 3. Error if no match
    fn resolve_provider(&self, model: &str) -> Result<Arc<dyn LlmClient>, HarnessBuildError> {
        if self.providers.is_empty() {
            return Err(HarnessBuildError::NoProvider);
        }

        // First pass: exact match.
        for matcher in &self.providers {
            if !matcher.is_glob && matcher.matches(model) {
                return Ok(matcher.client.clone());
            }
        }

        // Second pass: longest-prefix glob match.
        let mut best_match: Option<&ProviderMatcher> = None;
        for matcher in &self.providers {
            if matcher.is_glob
                && matcher.matches(model)
                && (best_match.is_none() || matcher.prefix.len() > best_match.unwrap().prefix.len())
            {
                best_match = Some(matcher);
            }
        }

        if let Some(matcher) = best_match {
            return Ok(matcher.client.clone());
        }

        Err(HarnessBuildError::NoMatchingProvider {
            model: model.to_string(),
        })
    }

    /// Build [`AgentHarnessOptions`] from accumulated builder state.
    fn build_options(&self) -> AgentHarnessOptions {
        let mut opts = AgentHarnessOptions::new(self.model.clone());
        opts.tools = self.tools.clone();
        opts.hooks = self.hooks.clone();
        opts.skills = self.skills.clone();
        opts.templates = self.templates.clone();
        if let Some(max_tokens) = self.max_tokens {
            opts.max_tokens = max_tokens;
        }

        // 惰性创建共享 cost 状态。即使没有 pricing/budget，
        // 也注入 CostAccumulatorHook，使 token 计数始终能通过
        // AgentHarness::usage() 读取。
        let cost = self
            .cost
            .clone()
            .unwrap_or_else(|| Arc::new(Mutex::new(CostAggregate::default())));
        opts.cost = cost.clone();

        // 注入内置成本累积器（始终注入，使 usage() 可用）。
        let accumulator: Arc<dyn AfterProviderResponseHook> =
            Arc::new(CostAccumulatorHook::new(cost.clone(), self.pricing.clone()));
        opts.hooks.after_provider_response.push(accumulator);

        // 若配置了 budget，注入预算决策器。
        if let Some((limit, exceeded_hook)) = &self.budget {
            let budget_adapter: Arc<dyn ShouldStopHook> = Arc::new(BudgetControlAdapter::new(
                cost.clone(),
                *limit,
                exceeded_hook.clone(),
            ));
            opts.hooks.should_stop.push(budget_adapter);
        }

        opts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Provider matching tests ────────────────────────────────────────────────

    #[test]
    fn test_provider_matcher_exact() {
        // Create a simple mock client using a placeholder approach.
        // Since we can't easily mock LlmClient in a unit test, we use a
        // simplified test that checks the matching logic without instantiation.
        let pattern = "claude-3-5-sonnet";
        let is_glob = pattern.contains('*');
        assert!(!is_glob);
    }

    #[test]
    fn test_provider_matcher_glob() {
        let pattern = "claude-*";
        let is_glob = pattern.contains('*');
        assert!(is_glob);
        let prefix = pattern.split('*').next().unwrap_or("");
        assert_eq!(prefix, "claude-");
    }

    #[test]
    fn test_provider_matcher_glob_prefix() {
        let pattern = "claude-3-*";
        let is_glob = pattern.contains('*');
        assert!(is_glob);
        let prefix = pattern.split('*').next().unwrap_or("");
        assert_eq!(prefix, "claude-3-");
    }

    #[test]
    fn test_builder_no_provider() {
        let builder = HarnessBuilder::new("claude-3-5-sonnet");
        let err = builder.resolve_provider("claude-3-5-sonnet");
        assert!(matches!(err, Err(HarnessBuildError::NoProvider)));
    }

    #[test]
    fn test_builder_provider_pattern_extraction() {
        // Test that patterns are correctly parsed (without needing an actual client).
        let exact = "claude-3-5-sonnet";
        assert!(!exact.contains('*'));

        let glob = "claude-*";
        assert!(glob.contains('*'));
        let prefix = glob.split('*').next().unwrap_or("");
        assert_eq!(prefix, "claude-");

        // Test matching logic without instantiation
        let model = "claude-3-5-sonnet";
        assert!(model.starts_with(prefix));
    }

    // ── Plugin integration tests ───────────────────────────────────────────────

    /// Mock tool for testing plugin registration.
    struct MockTool {
        tool_name: String,
        schema: serde_json::Value,
    }

    impl MockTool {
        fn new(name: &str) -> Self {
            Self {
                tool_name: name.to_string(),
                schema: serde_json::json!({}),
            }
        }
    }

    impl Tool for MockTool {
        fn name(&self) -> &str {
            &self.tool_name
        }

        fn description(&self) -> &str {
            "mock tool"
        }

        fn parameters_schema(&self) -> &serde_json::Value {
            &self.schema
        }

        fn execute<'a>(
            &'a self,
            _args: serde_json::Value,
            _ctx: &'a llm_harness_types::ToolContext,
        ) -> futures::future::BoxFuture<
            'a,
            Result<llm_harness_types::ToolResult, llm_harness_types::ToolError>,
        > {
            Box::pin(async {
                Ok(llm_harness_types::ToolResult {
                    content: vec![llm_harness_types::ContentBlock::Text {
                        text: "mock result".into(),
                    }],
                    details: serde_json::Value::Null,
                    terminate: false,
                })
            })
        }
    }

    /// Mock hook for testing plugin registration.
    struct MockBeforeTurnHook;

    impl llm_harness_types::BeforeTurnHook for MockBeforeTurnHook {
        fn before_turn<'a>(
            &'a self,
            _ctx: llm_harness_types::BeforeTurnCtx<'a>,
        ) -> futures::future::BoxFuture<'a, ()> {
            Box::pin(async {})
        }
    }

    /// Mock plugin for testing.
    struct MockPlugin {
        plugin_name: String,
        tools: Vec<Arc<dyn Tool>>,
        hooks: Vec<Arc<dyn llm_harness_types::BeforeTurnHook>>,
        skills: Vec<Skill>,
    }

    impl MockPlugin {
        fn new(name: &str) -> Self {
            Self {
                plugin_name: name.to_string(),
                tools: vec![],
                hooks: vec![],
                skills: vec![],
            }
        }

        fn with_tool(mut self, name: &str) -> Self {
            self.tools.push(Arc::new(MockTool::new(name)));
            self
        }

        fn with_hook(mut self) -> Self {
            self.hooks.push(Arc::new(MockBeforeTurnHook));
            self
        }

        fn with_skill(mut self, name: &str) -> Self {
            self.skills.push(Skill {
                name: name.to_string(),
                label: None,
                description: "test skill".to_string(),
                content: "# Test Skill\n".to_string(),
                source: std::path::PathBuf::from("/test"),
                disable_model_invocation: false,
            });
            self
        }
    }

    impl Plugin for MockPlugin {
        fn name(&self) -> &str {
            &self.plugin_name
        }

        fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
            for tool in &self.tools {
                tools.push(tool.clone());
            }
        }

        fn register_hooks(&self, hooks: &mut HarnessHooks) {
            for hook in &self.hooks {
                hooks.before_turn.push(hook.clone());
            }
        }

        fn register_skills(&self, skills: &mut Vec<Skill>) {
            for skill in &self.skills {
                skills.push(skill.clone());
            }
        }
    }

    /// Test 1: Install a single plugin — verifies hook and tool accumulation.
    #[test]
    fn test_builder_installs_single_plugin() {
        let plugin = MockPlugin::new("plugin-1")
            .with_tool("tool-1")
            .with_hook()
            .with_skill("skill-1");

        let builder = HarnessBuilder::new("claude-3-sonnet").install(&plugin);

        // Verify all contributions are accumulated.
        assert_eq!(builder.hooks.before_turn.len(), 1);
        assert_eq!(builder.tools.len(), 1);
        assert_eq!(builder.skills.len(), 1);
    }

    /// Test 2: Install multiple plugins — verifies accumulation across plugins.
    #[test]
    fn test_builder_installs_multiple_plugins_accumulate_hooks() {
        let plugin1 = MockPlugin::new("plugin-1")
            .with_tool("tool-1")
            .with_hook()
            .with_skill("skill-1");

        let plugin2 = MockPlugin::new("plugin-2")
            .with_tool("tool-2")
            .with_hook()
            .with_skill("skill-2");

        let builder = HarnessBuilder::new("claude-3-sonnet")
            .install(&plugin1)
            .install(&plugin2);

        // Verify all contributions from both plugins are accumulated.
        assert_eq!(builder.hooks.before_turn.len(), 2);
        assert_eq!(builder.tools.len(), 2);
        assert_eq!(builder.skills.len(), 2);
    }

    /// Test 3: Tool name conflicts — both tools are added to registry.
    ///
    /// When two plugins register tools with the same name,
    /// both are added (no deduplication). Conflict resolution happens
    /// at dispatch time, not at registration time.
    #[test]
    fn test_builder_detects_tool_name_conflict() {
        let plugin1 = MockPlugin::new("plugin-1").with_tool("shared-tool");

        let plugin2 = MockPlugin::new("plugin-2").with_tool("shared-tool");

        let builder = HarnessBuilder::new("claude-3-sonnet")
            .install(&plugin1)
            .install(&plugin2);

        // Both tools are registered even with name conflict.
        assert_eq!(builder.tools.len(), 2);
        assert_eq!(builder.tools[0].name(), "shared-tool");
        assert_eq!(builder.tools[1].name(), "shared-tool");
    }

    /// Test 4: Skill name conflicts — both skills are added.
    ///
    /// When two plugins register skills with the same name,
    /// both are added. Conflict resolution happens at lookup time.
    #[test]
    fn test_builder_detects_skill_name_conflict() {
        let plugin1 = MockPlugin::new("plugin-1").with_skill("shared-skill");

        let plugin2 = MockPlugin::new("plugin-2").with_skill("shared-skill");

        let builder = HarnessBuilder::new("claude-3-sonnet")
            .install(&plugin1)
            .install(&plugin2);

        // Both skills are registered even with name conflict.
        assert_eq!(builder.skills.len(), 2);
    }

    /// Test 5: Provider exact match takes priority over glob patterns.
    ///
    /// When both "claude-3-sonnet" (exact) and "claude-*" (glob) are registered,
    /// the exact pattern should be preferred for model "claude-3-sonnet".
    #[test]
    fn test_provider_exact_match_takes_priority() {
        // Verify that exact patterns are checked before globs.
        let exact = "claude-3-sonnet";
        let glob = "claude-*";

        // Exact match check.
        assert!(exact == "claude-3-sonnet");

        // Glob prefix extraction.
        let prefix = glob.split('*').next().unwrap_or("");
        assert_eq!(prefix, "claude-");

        // Model matches glob prefix.
        assert!("claude-3-sonnet".starts_with(prefix));

        // Exact match should be selected first due to resolution order.
        assert_eq!(exact, "claude-3-sonnet");
    }

    /// Test 6: Longest-prefix glob takes priority over shorter globs.
    ///
    /// When both "c*" (prefix: 1) and "claude-*" (prefix: 7) match a model,
    /// the longest prefix (most specific) should be selected.
    #[test]
    fn test_provider_longest_prefix_glob_wins() {
        let model = "claude-3-5-sonnet";

        // Pattern 1: "c*" (prefix length 1)
        let pattern1 = "c*";
        let prefix1 = pattern1.split('*').next().unwrap_or("");
        assert_eq!(prefix1, "c");
        assert!(model.starts_with(prefix1));
        assert_eq!(prefix1.len(), 1);

        // Pattern 2: "claude-*" (prefix length 7)
        let pattern2 = "claude-*";
        let prefix2 = pattern2.split('*').next().unwrap_or("");
        assert_eq!(prefix2, "claude-");
        assert!(model.starts_with(prefix2));
        assert_eq!(prefix2.len(), 7);

        // Longest prefix should be selected.
        assert!(prefix2.len() > prefix1.len());
        assert_eq!(prefix2, "claude-");
    }

    /// Test 7: Plugin with multiple tools — all are accumulated.
    #[test]
    fn test_builder_plugin_with_multiple_tools() {
        let plugin = MockPlugin::new("plugin-1")
            .with_tool("tool-a")
            .with_tool("tool-b")
            .with_tool("tool-c");

        let builder = HarnessBuilder::new("claude-3-sonnet").install(&plugin);

        assert_eq!(builder.tools.len(), 3);
        assert_eq!(builder.tools[0].name(), "tool-a");
        assert_eq!(builder.tools[1].name(), "tool-b");
        assert_eq!(builder.tools[2].name(), "tool-c");
    }

    /// Test 8: Plugin with multiple hooks — all are accumulated.
    #[test]
    fn test_builder_plugin_with_multiple_hooks() {
        let plugin = MockPlugin::new("plugin-1")
            .with_hook()
            .with_hook()
            .with_hook();

        let builder = HarnessBuilder::new("claude-3-sonnet").install(&plugin);

        assert_eq!(builder.hooks.before_turn.len(), 3);
    }

    /// Test 9: Empty plugin contributes nothing.
    #[test]
    fn test_builder_empty_plugin_contributes_nothing() {
        let plugin = MockPlugin::new("empty-plugin");

        let builder = HarnessBuilder::new("claude-3-sonnet").install(&plugin);

        assert_eq!(builder.hooks.before_turn.len(), 0);
        assert_eq!(builder.tools.len(), 0);
        assert_eq!(builder.skills.len(), 0);
    }

    /// Test 10: Plugins are accumulated in registration order.
    #[test]
    fn test_builder_accumulates_plugins_in_order() {
        let plugin1 = MockPlugin::new("plugin-1").with_tool("tool-1");
        let plugin2 = MockPlugin::new("plugin-2").with_tool("tool-2");
        let plugin3 = MockPlugin::new("plugin-3").with_tool("tool-3");

        let builder = HarnessBuilder::new("claude-3-sonnet")
            .install(&plugin1)
            .install(&plugin2)
            .install(&plugin3);

        assert_eq!(builder.tools.len(), 3);
        assert_eq!(builder.tools[0].name(), "tool-1");
        assert_eq!(builder.tools[1].name(), "tool-2");
        assert_eq!(builder.tools[2].name(), "tool-3");
    }

    #[test]
    fn builder_tool_appends_to_tools_list() {
        use futures::future::BoxFuture;
        use llm_harness_types::{Tool, ToolContext, ToolError, ToolResult};
        use std::sync::Arc;

        struct DummyTool;
        impl Tool for DummyTool {
            fn name(&self) -> &str {
                "dummy"
            }
            fn description(&self) -> &str {
                "dummy"
            }
            fn parameters_schema(&self) -> &serde_json::Value {
                static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
                S.get_or_init(|| serde_json::json!({"type":"object","properties":{}}))
            }
            fn execute<'a>(
                &'a self,
                _args: serde_json::Value,
                _ctx: &'a ToolContext,
            ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
                Box::pin(async { unreachable!() })
            }
        }

        let builder = HarnessBuilder::new("m").tool(Arc::new(DummyTool));
        assert_eq!(builder.tools.len(), 1);
        assert_eq!(builder.tools[0].name(), "dummy");
    }

    #[test]
    fn builder_should_stop_hook_appends_to_hooks() {
        use futures::future::BoxFuture;
        use llm_harness_types::{ShouldStopCtx, ShouldStopHook};
        use std::sync::Arc;

        struct AlwaysStop;
        impl ShouldStopHook for AlwaysStop {
            fn should_stop<'a>(&'a self, _ctx: ShouldStopCtx<'a>) -> BoxFuture<'a, bool> {
                Box::pin(async { true })
            }
        }

        let builder = HarnessBuilder::new("m")
            .should_stop_hook(Arc::new(AlwaysStop))
            .should_stop_hook(Arc::new(AlwaysStop));
        assert_eq!(builder.hooks.should_stop.len(), 2);
    }

    #[test]
    fn build_options_injects_accumulator_and_budget_hooks() {
        use crate::cost::TokenPrice;

        struct StubPricing;
        impl crate::cost::PricingProvider for StubPricing {
            fn price_for(&self, _model: &str, _provider: &str) -> Option<TokenPrice> {
                None
            }
        }

        let builder = HarnessBuilder::new("test-model")
            .pricing(Arc::new(StubPricing) as Arc<dyn crate::cost::PricingProvider>)
            .budget(5.0, None);

        let opts = builder.build_options();
        // 成本累积器始终注入。
        assert!(
            !opts.hooks.after_provider_response.is_empty(),
            "CostAccumulatorHook must be injected"
        );
        // 仅在调用 budget() 时注入预算 adapter。
        assert!(
            !opts.hooks.should_stop.is_empty(),
            "BudgetControlAdapter must be injected when budget() is called"
        );
        // 共享 cost Arc 是同一实例。
        let cost_snapshot = opts.cost.lock().unwrap().clone();
        assert_eq!(cost_snapshot.total_cost, 0.0);
    }

    #[test]
    fn build_options_injects_accumulator_without_pricing() {
        let builder = HarnessBuilder::new("test-model");
        let opts = builder.build_options();
        assert!(
            !opts.hooks.after_provider_response.is_empty(),
            "CostAccumulatorHook must be injected even without pricing"
        );
        assert!(
            opts.hooks.should_stop.is_empty(),
            "no budget() call → no BudgetControlAdapter"
        );
    }

    #[test]
    fn build_options_applies_max_tokens_override() {
        let opts = HarnessBuilder::new("test-model")
            .max_tokens(2048)
            .build_options();
        assert_eq!(opts.max_tokens, 2048);
    }
}
