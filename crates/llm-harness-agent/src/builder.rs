//! Builder for constructing [`AgentHarness`] with plugins, LLM providers, and tools.
//!
//! [`HarnessBuilder`] provides a fluent API for:
//! - Installing plugins (tools, hooks, skills, templates)
//! - Registering LLM provider clients with exact and glob matching
//! - Building an [`AgentHarness`] with an in-memory or existing session
//!
//! # Example
//!
//! ```ignore
//! use llm_harness_agent::HarnessBuilder;
//!
//! let harness = HarnessBuilder::new("claude-3-5-sonnet-20241022")
//!     .install(&my_auth_plugin)
//!     .install(&my_tools_plugin)
//!     .provider("claude-*", client)
//!     .build(env)
//!     .await?;
//! ```

use glob::Pattern;
use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use crate::{
    AgentHarness, AgentHarnessOptions, CompactionModelConfig, CompactionPromptSpec, HarnessHooks,
    ModelInfo, Plugin, PromptTemplate, Skill,
};
use llm_harness_loop::{ConvertToLlmHook, FinalAnswerMode, LlmClient, RetryConfig};
use llm_harness_types::{
    AfterProviderResponseHook, AfterToolCallHook, AfterTurnHook, BeforeCompactHook,
    BeforeProviderRequestHook, BeforeRunHook, BeforeToolCallHook, BeforeTurnHook, ExecutionEnv,
    FinalAnswerValidator, PrepareNextTurnHook, PricingProvider, ShouldStopHook, StreamOptions,
    ThinkingLevel, Tool, ToolExecutionMode, TransformContextHook,
};

use crate::harness::{UsageLedger, cost_hook::CostAccumulatorHook};
use thiserror::Error;

// ── Provider matching ─────────────────────────────────────────────────────────

/// Internal representation of an LLM provider matcher.
///
/// Supports exact pattern matching (e.g., "claude-3") or glob patterns
/// (e.g., "claude-*", "*-sonnet", "claude-3-*-sonnet"). Glob patterns are
/// matched using a longest-pattern-match strategy: the most specific glob
/// (longest pattern string) is selected if multiple globs match.
struct ProviderMatcher {
    /// The pattern string (either exact or glob).
    pattern: String,
    /// Whether this pattern contains wildcards.
    is_glob: bool,
    /// Pre-compiled glob pattern (only used when `is_glob`).
    compiled: Pattern,
    /// The LLM client for this provider.
    client: Arc<dyn LlmClient>,
}

impl ProviderMatcher {
    /// Create a new provider matcher.
    ///
    /// # Panics
    ///
    /// Panics if `pattern` contains an invalid glob (e.g. unclosed `[`).
    /// This is a build-time programming error, not a runtime condition:
    /// better to fail fast than silently route requests to the wrong provider.
    fn new(pattern: impl Into<String>, client: Arc<dyn LlmClient>) -> Self {
        let pattern = pattern.into();
        let is_glob = pattern.contains('*');
        let compiled = Pattern::new(&pattern)
            .unwrap_or_else(|e| panic!("invalid glob pattern {pattern:?}: {e}"));
        Self {
            pattern,
            is_glob,
            compiled,
            client,
        }
    }

    /// Check if this matcher matches the given model ID.
    fn matches(&self, model: &str) -> bool {
        if !self.is_glob {
            // Exact match.
            self.pattern == model
        } else {
            // Glob match via the compiled pattern.
            self.compiled.matches(model)
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
    /// A business tool uses a framework-reserved name.
    #[error("tool name conflict: {0}")]
    ToolNameConflict(String),
    /// No registered provider matches the specified model.
    #[error("No provider matches model: {model}")]
    NoMatchingProvider { model: String },
}

// ── HarnessBuilder ────────────────────────────────────────────────────────────

/// Fluent builder for constructing an [`AgentHarness`].
///
/// Collects plugins, tools, hooks, skills, templates,
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
    /// 可选的 pricing provider，用于成本计算。
    pricing: Option<Arc<dyn PricingProvider>>,
    /// Optional caller-owned completed-call accounting state.
    usage_ledger: Option<UsageLedger>,
    /// Optional maximum output tokens per provider call.
    max_tokens: Option<u32>,
    /// Optional independent compaction model config (model ID + metadata).
    /// Client is resolved at build time. When set, compaction summaries use
    /// this provider instead of the main conversation client.
    compaction_model: Option<(String, ModelInfo)>,
    /// Initial system prompt.
    system_prompt: Option<String>,
    /// Initial thinking level.
    thinking_level: Option<ThinkingLevel>,
    /// Initial sampling temperature.
    temperature: Option<f32>,
    /// Optional `response_format` for structured output (e.g. `JsonObject`).
    response_format: Option<llm_harness_loop::ResponseFormat>,
    /// Optional model metadata (context_window, etc.).
    model_info: Option<ModelInfo>,
    /// Retry config for transient provider errors.
    retry: Option<RetryConfig>,
    /// Final-answer classification mode.
    final_answer_mode: Option<FinalAnswerMode>,
    /// Default tool execution mode for batches (overrides the `Parallel` default).
    default_execution_mode: Option<ToolExecutionMode>,
    /// Custom context converter.
    convert_to_llm: Option<Arc<dyn ConvertToLlmHook>>,
    /// Stream options for LLM requests.
    stream_options: Option<StreamOptions>,
    /// Compaction reserved response tokens override.
    compaction_reserve_tokens: Option<u32>,
    /// Compaction keep-recent tokens override.
    compaction_keep_recent_tokens: Option<u32>,
    /// Custom compaction prompt spec. `None` → hardcoded constants.
    compaction_prompt: Option<CompactionPromptSpec>,
    /// Current task query intent for context-aware compaction.
    compaction_query: Option<String>,
    /// Max consecutive compaction failures before circuit breaker trips.
    /// `None` → default (3, clamped to minimum 1).
    compaction_max_failures: Option<u32>,
    /// Enable automatic compaction at each turn start.
    auto_compact: bool,
    /// Steer/follow-up queue capacity.
    queue_capacity: Option<usize>,
    /// Whether to auto-register `SkillReadTool` when skills are present.
    /// Defaults to `true`. Set to `false` via [`disable_skill_read_tool`](Self::disable_skill_read_tool).
    skill_read_tool_enabled: bool,
    /// Shared readable-skills store, passed to both `SkillReadTool` and
    /// `AgentHarnessOptions` so runtime skill updates are visible to the tool.
    readable_skills: Option<Arc<RwLock<Vec<Skill>>>>,
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
            pricing: None,
            usage_ledger: None,
            max_tokens: None,
            compaction_model: None,
            system_prompt: None,
            thinking_level: None,
            temperature: None,
            response_format: None,
            model_info: None,
            retry: None,
            final_answer_mode: None,
            default_execution_mode: None,
            convert_to_llm: None,
            stream_options: None,
            compaction_reserve_tokens: None,
            compaction_keep_recent_tokens: None,
            compaction_prompt: None,
            compaction_query: None,
            compaction_max_failures: None,
            skill_read_tool_enabled: true,
            readable_skills: None,
            auto_compact: false,
            queue_capacity: None,
        }
    }

    /// Install a plugin, accumulating its contributions.
    ///
    /// Plugins contribute tools, hooks, skills, and templates. Contributions
    /// are accumulated in registration order:
    ///
    /// - **Tools**: appended to the tools list
    /// - **Hooks**: appended to respective hook vectors
    /// - **Skills**: appended to the skills list
    /// - **Templates**: appended to the templates list
    ///
    /// If two plugins register tools or skills with the same name, both are
    /// added — `install` neither deduplicates nor diagnoses. Tool dispatch
    /// resolves duplicates by last-registration-wins; skill/template lookup
    /// resolves by first-registration-wins. See `Plugin` trait docs for details.
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

    /// Register an `AfterTurnHook` without wrapping it in a Plugin.
    ///
    /// Push semantics: appended to `hooks.after_turn`. Call multiple times to
    /// compose multiple hooks — `CompositeAfterTurnHook` is all-execute, so
    /// registration order does not affect correctness (every hook always runs).
    pub fn after_turn_hook(mut self, hook: Arc<dyn AfterTurnHook>) -> Self {
        self.hooks.after_turn.push(hook);
        self
    }

    /// Register a `BeforeRunHook` without wrapping it in a Plugin.
    ///
    /// Push semantics: appended to `hooks.before_run`.
    pub fn before_run_hook(mut self, hook: Arc<dyn BeforeRunHook>) -> Self {
        self.hooks.before_run.push(hook);
        self
    }

    /// Register a `BeforeTurnHook` without wrapping it in a Plugin.
    ///
    /// Push semantics: appended to `hooks.before_turn`.
    pub fn before_turn_hook(mut self, hook: Arc<dyn BeforeTurnHook>) -> Self {
        self.hooks.before_turn.push(hook);
        self
    }

    /// Register a `BeforeToolCallHook` without wrapping it in a Plugin.
    ///
    /// Push semantics: appended to `hooks.before_tool_call`.
    pub fn before_tool_call_hook(mut self, hook: Arc<dyn BeforeToolCallHook>) -> Self {
        self.hooks.before_tool_call.push(hook);
        self
    }

    /// Register an `AfterToolCallHook` without wrapping it in a Plugin.
    ///
    /// Push semantics: appended to `hooks.after_tool_call`.
    pub fn after_tool_call_hook(mut self, hook: Arc<dyn AfterToolCallHook>) -> Self {
        self.hooks.after_tool_call.push(hook);
        self
    }

    /// Register a `TransformContextHook` without wrapping it in a Plugin.
    ///
    /// Push semantics: appended to `hooks.transform_context`.
    pub fn transform_context_hook(mut self, hook: Arc<dyn TransformContextHook>) -> Self {
        self.hooks.transform_context.push(hook);
        self
    }

    /// Register a `PrepareNextTurnHook` without wrapping it in a Plugin.
    ///
    /// Push semantics: appended to `hooks.prepare_next_turn`.
    pub fn prepare_next_turn_hook(mut self, hook: Arc<dyn PrepareNextTurnHook>) -> Self {
        self.hooks.prepare_next_turn.push(hook);
        self
    }

    /// Register a `BeforeProviderRequestHook` without wrapping it in a Plugin.
    ///
    /// Push semantics: appended to `hooks.before_provider_request`.
    pub fn before_provider_request_hook(
        mut self,
        hook: Arc<dyn BeforeProviderRequestHook>,
    ) -> Self {
        self.hooks.before_provider_request.push(hook);
        self
    }

    /// Register an `AfterProviderResponseHook` without wrapping it in a Plugin.
    ///
    /// Push semantics: appended to `hooks.after_provider_response`.
    pub fn after_provider_response_hook(
        mut self,
        hook: Arc<dyn AfterProviderResponseHook>,
    ) -> Self {
        self.hooks.after_provider_response.push(hook);
        self
    }

    /// Register a `FinalAnswerValidator` without wrapping it in a Plugin.
    ///
    /// Push semantics: appended to `hooks.final_answer_validator`.
    pub fn final_answer_validator_hook(mut self, hook: Arc<dyn FinalAnswerValidator>) -> Self {
        self.hooks.final_answer_validator.push(hook);
        self
    }

    /// Register a `BeforeCompactHook` without wrapping it in a Plugin.
    ///
    /// Push semantics: appended to `hooks.before_compact`.
    pub fn before_compact_hook(mut self, hook: Arc<dyn BeforeCompactHook>) -> Self {
        self.hooks.before_compact.push(hook);
        self
    }

    /// Set hook collections directly. Push semantics: the provided hooks
    /// are appended to the builder's existing hooks. Call multiple times
    /// to compose hooks from multiple sources.
    pub fn hooks(mut self, hooks: HarnessHooks) -> Self {
        self.hooks.before_run.extend(hooks.before_run);
        self.hooks.before_turn.extend(hooks.before_turn);
        self.hooks.after_turn.extend(hooks.after_turn);
        self.hooks.before_tool_call.extend(hooks.before_tool_call);
        self.hooks.after_tool_call.extend(hooks.after_tool_call);
        self.hooks.transform_context.extend(hooks.transform_context);
        self.hooks.prepare_next_turn.extend(hooks.prepare_next_turn);
        self.hooks.should_stop.extend(hooks.should_stop);
        self.hooks
            .before_provider_request
            .extend(hooks.before_provider_request);
        self.hooks
            .after_provider_response
            .extend(hooks.after_provider_response);
        self.hooks
            .final_answer_validator
            .extend(hooks.final_answer_validator);
        self.hooks.before_compact.extend(hooks.before_compact);
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

    /// Use caller-owned accounting state for this harness.
    ///
    /// Retaining a clone lets the caller recover completed provider-call
    /// evidence even if the harness execution future is later cancelled.
    pub fn usage_ledger(mut self, usage_ledger: UsageLedger) -> Self {
        self.usage_ledger = Some(usage_ledger);
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
    /// 2. If no exact match, the longest-pattern glob is used
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
    ///
    /// # Panics
    ///
    /// Panics if `pattern` is an invalid glob (e.g. unclosed `[`).
    pub fn provider(mut self, pattern: impl Into<String>, client: Arc<dyn LlmClient>) -> Self {
        self.providers.push(ProviderMatcher::new(pattern, client));
        self
    }

    /// Set the maximum output tokens per provider call.
    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Configure an independent compaction model. When set, compaction
    /// summaries use a separate provider/model instead of the main
    /// conversation client — e.g. main conversation on Sonnet, compaction
    /// on Haiku.
    ///
    /// `model_info` should reflect the compaction model's real
    /// `context_window` and `max_tokens`, used for prompt budget checks
    /// in `compact()`. The compaction client is resolved at build time.
    pub fn compaction_model(mut self, model: impl Into<String>, model_info: ModelInfo) -> Self {
        self.compaction_model = Some((model.into(), model_info));
        self
    }

    /// Set the initial system prompt. `None` resets to default (no system prompt).
    pub fn system_prompt(mut self, prompt: Option<String>) -> Self {
        self.system_prompt = prompt;
        self
    }

    /// Set the initial thinking level.
    pub fn thinking_level(mut self, level: ThinkingLevel) -> Self {
        self.thinking_level = Some(level);
        self
    }

    /// Set the initial sampling temperature. `None` resets to provider default.
    pub fn temperature(mut self, temp: Option<f32>) -> Self {
        self.temperature = temp;
        self
    }

    /// Set the `response_format` for structured output. `None` resets to default.
    pub fn response_format(mut self, fmt: Option<llm_harness_loop::ResponseFormat>) -> Self {
        self.response_format = fmt;
        self
    }

    /// Set model metadata (context_window, max_tokens). `None` resets to default.
    pub fn model_info(mut self, info: Option<ModelInfo>) -> Self {
        self.model_info = info;
        self
    }

    /// Set retry config for transient provider errors. `None` disables retry.
    pub fn retry(mut self, config: Option<RetryConfig>) -> Self {
        self.retry = config;
        self
    }

    /// Set the final-answer classification mode.
    pub fn final_answer_mode(mut self, mode: FinalAnswerMode) -> Self {
        self.final_answer_mode = Some(mode);
        self
    }

    /// Set the default tool execution mode for batches with multiple tool calls.
    ///
    /// `Parallel` (default) runs tools concurrently via `join_all`.
    /// `Sequential` runs them one-at-a-time with sub-group barriers.
    /// Individual tools can still override via `Tool::execution_mode()`.
    pub fn default_execution_mode(mut self, mode: ToolExecutionMode) -> Self {
        self.default_execution_mode = Some(mode);
        self
    }

    /// Set a custom context converter. `None` uses `DefaultConvertToLlm`.
    pub fn convert_to_llm(mut self, converter: Option<Arc<dyn ConvertToLlmHook>>) -> Self {
        self.convert_to_llm = converter;
        self
    }

    /// Set stream options for LLM requests. `None` resets to default.
    pub fn stream_options(mut self, opts: Option<StreamOptions>) -> Self {
        self.stream_options = opts;
        self
    }

    /// Override compaction reserved response tokens. `None` uses compaction default.
    pub fn compaction_reserve_tokens(mut self, tokens: Option<u32>) -> Self {
        self.compaction_reserve_tokens = tokens;
        self
    }

    /// Override compaction keep-recent tokens. `None` uses compaction default.
    pub fn compaction_keep_recent_tokens(mut self, tokens: Option<u32>) -> Self {
        self.compaction_keep_recent_tokens = tokens;
        self
    }

    /// Set a custom compaction prompt spec. `None` uses hardcoded constants.
    pub fn compaction_prompt(mut self, prompt: Option<CompactionPromptSpec>) -> Self {
        self.compaction_prompt = prompt;
        self
    }

    /// Set the current task query intent for context-aware compaction.
    /// `None` disables task-aware compaction.
    pub fn compaction_query(mut self, query: Option<String>) -> Self {
        self.compaction_query = query;
        self
    }

    /// Override max consecutive compaction failures before circuit breaker trips.
    /// `None` uses default (3, clamped to minimum 1).
    pub fn compaction_max_failures(mut self, max: Option<u32>) -> Self {
        self.compaction_max_failures = max;
        self
    }

    /// Enable automatic compaction at each turn start.
    pub fn auto_compact(mut self, enabled: bool) -> Self {
        self.auto_compact = enabled;
        self
    }

    /// Set steer/follow-up queue capacity. `None` resets to default (32).
    pub fn queue_capacity(mut self, capacity: Option<usize>) -> Self {
        self.queue_capacity = capacity;
        self
    }

    /// Disable auto-registration of `SkillReadTool`.
    ///
    /// By default, `build()` / `build_with_session()` register `SkillReadTool`
    /// when skills are present. Call this to opt out.
    pub fn disable_skill_read_tool(mut self) -> Self {
        self.skill_read_tool_enabled = false;
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
        let builder = self.inject_skill_read_tool(&env);
        let (client, opts) = builder.resolve_and_build()?;
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
        session: crate::Session,
    ) -> Result<AgentHarness, HarnessBuildError> {
        let builder = self.inject_skill_read_tool(&env);
        let (client, opts) = builder.resolve_and_build()?;
        Ok(AgentHarness::with_session(client, env, session, opts))
    }
    pub(crate) fn inject_skill_read_tool(mut self, env: &Arc<dyn ExecutionEnv>) -> Self {
        if self.skill_read_tool_enabled {
            let readable: Vec<Skill> = {
                let mut seen = HashSet::new();
                self.skills
                    .iter()
                    .filter(|s| !s.disable_model_invocation && seen.insert(s.name.clone()))
                    .cloned()
                    .collect()
            };
            if !readable.is_empty() {
                let store = Arc::new(RwLock::new(readable));
                self.tools.push(Arc::new(crate::SkillReadTool::new(
                    store.clone(),
                    env.clone(),
                )));
                self.readable_skills = Some(store);
            }
        }
        self
    }

    /// Resolve provider, compaction config, and build options from builder state.
    fn resolve_and_build(
        self,
    ) -> Result<(Arc<dyn LlmClient>, AgentHarnessOptions), HarnessBuildError> {
        let client = self.resolve_provider(&self.model)?;
        let compaction = self.resolve_compaction()?;
        let opts = self.build_options(compaction);
        // Check tool name conflicts before returning opts to the caller.
        llm_harness_types::check_tool_name_conflicts(&opts.tools)
            .map_err(HarnessBuildError::ToolNameConflict)?;
        Ok((client, opts))
    }

    /// Resolve an LLM provider for the given model ID.
    ///
    /// Selection order:
    /// 1. Exact pattern match
    /// 2. Longest-pattern glob match
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

        // Second pass: longest-pattern glob match. A longer pattern string is
        // generally more specific (e.g., "claude-3-*" beats "claude-*").
        let mut best_match: Option<&ProviderMatcher> = None;
        for matcher in &self.providers {
            if matcher.is_glob
                && matcher.matches(model)
                && (best_match.is_none()
                    || matcher.pattern.len() > best_match.unwrap().pattern.len())
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

    /// Resolve the compaction model config, if set. When compaction_model is
    /// configured, resolves its LLM client via `resolve_provider` and returns
    /// a fully populated `CompactionModelConfig`. Returns `None` when not set
    /// (compaction falls back to the main conversation client).
    fn resolve_compaction(&self) -> Result<Option<CompactionModelConfig>, HarnessBuildError> {
        match &self.compaction_model {
            Some((model, model_info)) => {
                let client = self.resolve_provider(model)?;
                Ok(Some(CompactionModelConfig {
                    client,
                    model: model.clone(),
                    model_info: model_info.clone(),
                }))
            }
            None => Ok(None),
        }
    }

    /// Build [`AgentHarnessOptions`] from accumulated builder state.
    fn build_options(&self, compaction: Option<CompactionModelConfig>) -> AgentHarnessOptions {
        let mut opts = AgentHarnessOptions::new(self.model.clone());
        opts.tools = self.tools.clone();
        opts.hooks = self.hooks.clone();
        opts.skills = self.skills.clone();
        opts.readable_skills = self.readable_skills.clone();
        opts.templates = self.templates.clone();
        if let Some(max_tokens) = self.max_tokens {
            opts.max_tokens = max_tokens;
        }
        opts.compaction_model = compaction;

        // 创建共享 cost 状态。即使没有 pricing/budget，
        // 也注入 CostAccumulatorHook，使 token 计数始终能通过
        // AgentHarness::usage() 读取。
        let usage_ledger = self.usage_ledger.clone().unwrap_or_default();
        let cost = usage_ledger.shared_state();
        opts.cost = cost.clone();

        // 注入内置成本累积器（始终注入，使 usage() 可用）。
        let accumulator: Arc<dyn AfterProviderResponseHook> =
            Arc::new(CostAccumulatorHook::new(cost.clone(), self.pricing.clone()));
        opts.hooks.after_provider_response.insert(0, accumulator);

        // ── 由 setter 配置的可选字段 ──
        if let Some(ref prompt) = self.system_prompt {
            opts.system_prompt = Some(prompt.clone());
        }
        if let Some(level) = self.thinking_level {
            opts.thinking_level = level;
        }
        if let Some(temp) = self.temperature {
            opts.temperature = Some(temp);
        }
        if let Some(ref fmt) = self.response_format {
            opts.response_format = Some(fmt.clone());
        }
        if let Some(ref info) = self.model_info {
            opts.model_info = Some(info.clone());
        }
        if let Some(ref config) = self.retry {
            opts.retry = Some(config.clone());
        }
        if let Some(mode) = &self.final_answer_mode {
            opts.final_answer_mode = mode.clone();
        }
        if let Some(mode) = self.default_execution_mode {
            opts.default_execution_mode = mode;
        }
        if let Some(ref converter) = self.convert_to_llm {
            opts.convert_to_llm = Some(converter.clone());
        }
        if let Some(ref opts_so) = self.stream_options {
            opts.stream_options = opts_so.clone();
        }
        if let Some(tokens) = self.compaction_reserve_tokens {
            opts.compaction_reserve_tokens = Some(tokens);
        }
        if let Some(tokens) = self.compaction_keep_recent_tokens {
            opts.compaction_keep_recent_tokens = Some(tokens);
        }
        if let Some(ref prompt) = self.compaction_prompt {
            opts.compaction_prompt = Some(prompt.clone());
        }
        if let Some(ref query) = self.compaction_query {
            opts.compaction_query = Some(query.clone());
        }
        if let Some(max) = self.compaction_max_failures {
            opts.compaction_max_failures = Some(max);
        }
        opts.auto_compact = self.auto_compact;
        if let Some(cap) = self.queue_capacity {
            opts.queue_capacity = cap;
        }

        opts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Provider matching tests ────────────────────────────────────────────────

    #[test]
    fn test_builder_no_provider() {
        let builder = HarnessBuilder::new("claude-3-5-sonnet");
        let err = builder.resolve_provider("claude-3-5-sonnet");
        assert!(matches!(err, Err(HarnessBuildError::NoProvider)));
    }

    #[cfg(feature = "test-utils")]
    fn mock_client() -> Arc<dyn LlmClient> {
        use llm_harness_loop::test_utils::MockLlmClient;
        Arc::new(MockLlmClient::new(vec![]))
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn provider_exact_match_resolves_exact_client() {
        let exact = mock_client();
        let glob = mock_client();
        let builder = HarnessBuilder::new("claude-3-sonnet")
            .provider("claude-3-sonnet", exact.clone())
            .provider("claude-*", glob);
        let resolved = builder.resolve_provider("claude-3-sonnet").unwrap();
        assert!(Arc::ptr_eq(&resolved, &exact));
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn provider_glob_match_resolves_glob_client() {
        let glob = mock_client();
        let builder = HarnessBuilder::new("claude-3-5-sonnet").provider("claude-*", glob.clone());
        let resolved = builder.resolve_provider("claude-3-5-sonnet").unwrap();
        assert!(Arc::ptr_eq(&resolved, &glob));
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn provider_no_match_returns_error() {
        let client = mock_client();
        let builder = HarnessBuilder::new("gpt-4o").provider("claude-*", client);
        let err = builder.resolve_provider("gpt-4o");
        assert!(matches!(
            err,
            Err(HarnessBuildError::NoMatchingProvider { .. })
        ));
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn provider_longest_pattern_glob_wins() {
        let short = mock_client();
        let long = mock_client();
        let builder = HarnessBuilder::new("claude-3-5-sonnet")
            .provider("c*", short)
            .provider("claude-*", long.clone());
        let resolved = builder.resolve_provider("claude-3-5-sonnet").unwrap();
        assert!(Arc::ptr_eq(&resolved, &long));
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn provider_glob_matches_suffix() {
        let client = mock_client();
        let builder = HarnessBuilder::new("claude-3-5-sonnet").provider("*-sonnet", client.clone());
        let resolved = builder.resolve_provider("claude-3-5-sonnet").unwrap();
        assert!(Arc::ptr_eq(&resolved, &client));

        // Suffix-only glob must NOT match a model without that suffix.
        let err = builder.resolve_provider("claude-3-5-haiku");
        assert!(matches!(
            err,
            Err(HarnessBuildError::NoMatchingProvider { .. })
        ));
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn provider_glob_matches_middle() {
        let client = mock_client();
        let builder =
            HarnessBuilder::new("claude-3-5-sonnet").provider("claude-3-*-sonnet", client.clone());
        // Matches: middle wildcard bridges "claude-3-" and "-sonnet".
        let resolved = builder.resolve_provider("claude-3-5-sonnet").unwrap();
        assert!(Arc::ptr_eq(&resolved, &client));

        // Does NOT match: literal prefix "claude-3-" doesn't match "claude-4-".
        let err = builder.resolve_provider("claude-4-5-sonnet");
        assert!(matches!(
            err,
            Err(HarnessBuildError::NoMatchingProvider { .. })
        ));
    }

    #[cfg(feature = "test-utils")]
    #[test]
    #[should_panic(expected = "invalid glob pattern")]
    fn provider_invalid_glob_panics() {
        let client = mock_client();
        // Unclosed `[` is an invalid glob — must panic rather than silently
        // fall back to a catch-all that routes every model to this client.
        let _ = HarnessBuilder::new("m").provider("claude-[3*", client);
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
            Result<llm_harness_types::ToolResult, llm_harness_types::ToolFailure>,
        > {
            Box::pin(async {
                Ok(llm_harness_types::ToolResult::full(
                    vec![llm_harness_types::DataBlock::text("mock result")],
                    serde_json::Value::Null,
                    false,
                ))
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

    struct AcceptFinalAnswer;

    impl llm_harness_types::FinalAnswerValidator for AcceptFinalAnswer {
        fn validate<'a>(
            &'a self,
            _: llm_harness_types::FinalAnswerValidationCtx<'a>,
        ) -> futures::future::BoxFuture<'a, Result<(), llm_harness_types::FinalAnswerValidationError>>
        {
            Box::pin(async { Ok(()) })
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
                source: std::path::PathBuf::from("/test/SKILL.md"),
                base_dir: std::path::PathBuf::from("/test"),
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

    #[test]
    fn builder_hooks_appends_final_answer_validators() {
        let mut hooks = HarnessHooks::none();
        hooks
            .final_answer_validator
            .push(Arc::new(AcceptFinalAnswer));

        let builder = HarnessBuilder::new("claude-3-sonnet").hooks(hooks);

        assert_eq!(builder.hooks.final_answer_validator.len(), 1);
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
        use llm_harness_types::{Tool, ToolContext, ToolFailure, ToolResult};
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
            ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
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
    fn builder_before_turn_hook_appends_to_hooks() {
        use futures::future::BoxFuture;
        use llm_harness_types::{BeforeTurnCtx, BeforeTurnHook};
        use std::sync::Arc;

        struct Nop;
        impl BeforeTurnHook for Nop {
            fn before_turn<'a>(&'a self, _ctx: BeforeTurnCtx<'a>) -> BoxFuture<'a, ()> {
                Box::pin(async {})
            }
        }

        let builder = HarnessBuilder::new("m")
            .before_turn_hook(Arc::new(Nop))
            .before_turn_hook(Arc::new(Nop));
        assert_eq!(builder.hooks.before_turn.len(), 2);
    }

    #[test]
    fn builder_before_compact_hook_appends_to_hooks() {
        use futures::future::BoxFuture;
        use llm_harness_types::{BeforeCompactCtx, BeforeCompactDecision, BeforeCompactHook};
        use std::sync::Arc;

        struct Allow;
        impl BeforeCompactHook for Allow {
            fn before_compact<'a>(
                &'a self,
                _ctx: BeforeCompactCtx<'a>,
            ) -> BoxFuture<'a, BeforeCompactDecision> {
                Box::pin(async { BeforeCompactDecision::Proceed })
            }
        }

        let builder = HarnessBuilder::new("m")
            .before_compact_hook(Arc::new(Allow))
            .before_compact_hook(Arc::new(Allow));
        assert_eq!(builder.hooks.before_compact.len(), 2);
    }

    #[test]
    fn builder_final_answer_validator_hook_appends_to_hooks() {
        use futures::future::BoxFuture;
        use llm_harness_types::{
            FinalAnswerValidationCtx, FinalAnswerValidationError, FinalAnswerValidator,
        };
        use std::sync::Arc;

        struct Accept;
        impl FinalAnswerValidator for Accept {
            fn validate<'a>(
                &'a self,
                _: FinalAnswerValidationCtx<'a>,
            ) -> BoxFuture<'a, Result<(), FinalAnswerValidationError>> {
                Box::pin(async { Ok(()) })
            }
        }

        let builder = HarnessBuilder::new("m")
            .final_answer_validator_hook(Arc::new(Accept))
            .final_answer_validator_hook(Arc::new(Accept));
        assert_eq!(builder.hooks.final_answer_validator.len(), 2);
    }

    #[test]
    fn build_options_injects_accumulator_without_pricing() {
        let builder = HarnessBuilder::new("test-model");
        let opts = builder.build_options(None);
        assert!(
            !opts.hooks.after_provider_response.is_empty(),
            "CostAccumulatorHook must be injected even without pricing"
        );
        assert!(
            opts.hooks.should_stop.is_empty(),
            "no should_stop hooks injected by default"
        );
    }

    #[test]
    fn build_options_applies_max_tokens_override() {
        let opts = HarnessBuilder::new("test-model")
            .max_tokens(2048)
            .build_options(None);
        assert_eq!(opts.max_tokens, 2048);
    }
    #[test]
    fn builder_setters_exist() {
        // 只验证 setter 存在且返回 Self（链式）；具体值断言见后续测试。
        let _ = HarnessBuilder::new("m")
            .system_prompt(Some("hi".to_string()))
            .thinking_level(ThinkingLevel::High)
            .temperature(Some(0.7))
            .model_info(Some(ModelInfo {
                context_window: 200_000,
                max_tokens: 8192,
            }))
            .retry(Some(RetryConfig {
                max_retries: 3,
                base_delay_ms: 500,
            }))
            .final_answer_mode(FinalAnswerMode::default())
            .convert_to_llm(None)
            .stream_options(Some(StreamOptions::default()))
            .response_format(Some(llm_harness_loop::ResponseFormat::JsonObject))
            .compaction_reserve_tokens(Some(1024))
            .compaction_keep_recent_tokens(Some(2048))
            .compaction_prompt(None)
            .compaction_query(None)
            .compaction_max_failures(Some(5))
            .queue_capacity(Some(64));
    }
    // ── build_options field propagation tests (Task 4) ────────────────────────

    #[test]
    fn build_options_applies_system_prompt() {
        let opts = HarnessBuilder::new("m")
            .system_prompt(Some("be brief".to_string()))
            .build_options(None);
        assert_eq!(opts.system_prompt.as_deref(), Some("be brief"));
    }

    #[test]
    fn build_options_applies_thinking_level() {
        let opts = HarnessBuilder::new("m")
            .thinking_level(ThinkingLevel::High)
            .build_options(None);
        assert_eq!(opts.thinking_level, ThinkingLevel::High);
    }

    #[test]
    fn build_options_applies_temperature() {
        let opts = HarnessBuilder::new("m")
            .temperature(Some(0.7))
            .build_options(None);
        assert_eq!(opts.temperature, Some(0.7));
    }

    #[test]
    fn build_options_temperature_none_resets() {
        let opts = HarnessBuilder::new("m")
            .temperature(Some(0.7))
            .temperature(None)
            .build_options(None);
        assert_eq!(opts.temperature, None);
    }

    #[test]
    fn build_options_applies_response_format() {
        use llm_harness_loop::ResponseFormat;
        let opts = HarnessBuilder::new("m")
            .response_format(Some(ResponseFormat::JsonObject))
            .build_options(None);
        assert_eq!(
            opts.response_format,
            Some(ResponseFormat::JsonObject),
            "response_format setter must propagate to opts"
        );
    }

    #[test]
    fn build_options_response_format_none_is_default() {
        use llm_harness_loop::ResponseFormat;
        let opts = HarnessBuilder::new("m").build_options(None);
        assert_eq!(
            opts.response_format, None,
            "default response_format is None"
        );
        // Confirm type is the loop re-export, not some local alias.
        let _: Option<ResponseFormat> = opts.response_format;
    }

    #[test]
    fn build_options_response_format_none_resets() {
        use llm_harness_loop::ResponseFormat;
        let opts = HarnessBuilder::new("m")
            .response_format(Some(ResponseFormat::JsonObject))
            .response_format(None)
            .build_options(None);
        assert_eq!(
            opts.response_format, None,
            "calling response_format(None) must reset to None"
        );
    }

    #[test]
    fn build_options_applies_model_info() {
        let info = ModelInfo {
            context_window: 200_000,
            max_tokens: 8192,
        };
        let opts = HarnessBuilder::new("m")
            .model_info(Some(info.clone()))
            .build_options(None);
        let got = opts.model_info.expect("model_info should be set");
        assert_eq!(got.context_window, info.context_window);
        assert_eq!(got.max_tokens, info.max_tokens);
    }

    #[test]
    fn build_options_applies_retry() {
        let cfg = RetryConfig {
            max_retries: 3,
            base_delay_ms: 500,
        };
        let opts = HarnessBuilder::new("m")
            .retry(Some(cfg.clone()))
            .build_options(None);
        let got = opts.retry.expect("retry should be set");
        assert_eq!(got.max_retries, cfg.max_retries);
        assert_eq!(got.base_delay_ms, cfg.base_delay_ms);
    }

    #[test]
    fn build_options_applies_final_answer_mode() {
        let mode = FinalAnswerMode::default();
        let opts = HarnessBuilder::new("m")
            .final_answer_mode(mode.clone())
            .build_options(None);
        assert_eq!(opts.final_answer_mode, mode);
    }

    #[test]
    fn build_options_applies_default_execution_mode() {
        let opts = HarnessBuilder::new("m")
            .default_execution_mode(ToolExecutionMode::Sequential)
            .build_options(None);
        assert_eq!(opts.default_execution_mode, ToolExecutionMode::Sequential);
    }

    #[test]
    fn build_options_applies_stream_options() {
        let so = StreamOptions {
            timeout_ms: Some(30_000),
            ..StreamOptions::default()
        };
        let opts = HarnessBuilder::new("m")
            .stream_options(Some(so.clone()))
            .build_options(None);
        assert_eq!(opts.stream_options.timeout_ms, so.timeout_ms);
    }

    #[test]
    fn build_options_applies_compaction_reserve_tokens() {
        let opts = HarnessBuilder::new("m")
            .compaction_reserve_tokens(Some(1024))
            .build_options(None);
        assert_eq!(opts.compaction_reserve_tokens, Some(1024));
    }

    #[test]
    fn build_options_applies_compaction_keep_recent_tokens() {
        let opts = HarnessBuilder::new("m")
            .compaction_keep_recent_tokens(Some(2048))
            .build_options(None);
        assert_eq!(opts.compaction_keep_recent_tokens, Some(2048));
    }

    #[test]
    fn build_options_applies_compaction_max_failures() {
        let opts = HarnessBuilder::new("m")
            .compaction_max_failures(Some(5))
            .build_options(None);
        assert_eq!(opts.compaction_max_failures, Some(5));
    }

    #[test]
    fn build_options_applies_queue_capacity() {
        let opts = HarnessBuilder::new("m")
            .queue_capacity(Some(64))
            .build_options(None);
        assert_eq!(opts.queue_capacity, 64);
    }

    #[test]
    fn build_options_defaults_when_no_setter_called() {
        let opts = HarnessBuilder::new("m").build_options(None);
        assert_eq!(opts.default_execution_mode, ToolExecutionMode::Parallel);
        assert_eq!(opts.system_prompt, None);
        assert_eq!(opts.thinking_level, ThinkingLevel::Off);
        assert_eq!(opts.temperature, None);
        assert_eq!(opts.response_format, None);
        assert!(opts.model_info.is_none());
        assert!(opts.retry.is_none());
        assert_eq!(opts.final_answer_mode, FinalAnswerMode::default());
        assert!(opts.convert_to_llm.is_none());
        assert_eq!(
            opts.stream_options.timeout_ms,
            StreamOptions::default().timeout_ms
        );
        assert_eq!(opts.compaction_reserve_tokens, None);
        assert_eq!(opts.compaction_keep_recent_tokens, None);
        assert_eq!(opts.compaction_max_failures, None);
        assert_eq!(opts.queue_capacity, 32); // DEFAULT_QUEUE_CAPACITY
    }

    #[test]
    fn caller_owned_usage_ledger_shares_the_builder_accounting_state() {
        use llm_harness_types::UsageReportProvenance;

        let ledger = UsageLedger::default();
        let opts = HarnessBuilder::new("m")
            .usage_ledger(ledger.clone())
            .build_options(None);
        opts.cost
            .lock()
            .unwrap()
            .usage_reports
            .record(UsageReportProvenance::Missing);

        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.usage_reports.provider_calls, 1);
        assert_eq!(snapshot.usage_reports.missing, 1);
    }

    #[tokio::test]
    async fn completed_usage_is_recorded_before_custom_response_hooks() {
        use futures::future::BoxFuture;
        use llm_harness_types::{
            AfterProviderResponseCtx, ProviderResponseInfo, RunContext, RunRequest, TokenUsage,
            UsageReportProvenance,
        };
        use std::sync::atomic::{AtomicBool, Ordering};

        struct ObserveLedger {
            ledger: UsageLedger,
            observed: Arc<AtomicBool>,
        }

        impl AfterProviderResponseHook for ObserveLedger {
            fn after_response<'a>(&'a self, _: AfterProviderResponseCtx<'a>) -> BoxFuture<'a, ()> {
                Box::pin(async move {
                    self.observed.store(
                        self.ledger.snapshot().usage_reports.provider_calls == 1,
                        Ordering::SeqCst,
                    );
                })
            }
        }

        let ledger = UsageLedger::default();
        let observed = Arc::new(AtomicBool::new(false));
        let mut hooks = HarnessHooks::none();
        hooks.after_provider_response.push(Arc::new(ObserveLedger {
            ledger: ledger.clone(),
            observed: observed.clone(),
        }));
        let opts = HarnessBuilder::new("m")
            .usage_ledger(ledger)
            .hooks(hooks)
            .build_options(None);
        let run = RunContext::new(RunRequest::default());
        let info = ProviderResponseInfo {
            status_code: None,
            response_headers: vec![],
            usage: Some(TokenUsage {
                provenance: UsageReportProvenance::ReportedValid,
                ..Default::default()
            }),
            latency_ms: 0,
            model: Some("m".into()),
            provider: Some("provider".into()),
        };

        for hook in &opts.hooks.after_provider_response {
            hook.after_response(AfterProviderResponseCtx {
                run: &run,
                turn_index: 0,
                info: &info,
            })
            .await;
        }

        assert!(observed.load(Ordering::SeqCst));
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn build_registers_skill_read_tool_when_skills_present() {
        use llm_harness_loop::test_utils::MockLlmClient;
        use llm_harness_runtime_sandbox_os::OsEnv;

        let tmp = tempfile::TempDir::new().unwrap();
        let env: Arc<dyn llm_harness_types::ExecutionEnv> = Arc::new(OsEnv::new(tmp.path()));
        let client = Arc::new(MockLlmClient::new(vec![]));
        let plugin = MockPlugin::new("p1").with_skill("my-skill");
        let builder = HarnessBuilder::new("m")
            .provider("m", client)
            .install(&plugin)
            .inject_skill_read_tool(&env);
        let (_client, opts) = builder.resolve_and_build().unwrap();
        let has_skill_read = opts.tools.iter().any(|t| t.name() == "skill_read");
        assert!(
            has_skill_read,
            "skill_read tool should be registered when skills are present"
        );
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn build_no_skill_read_tool_when_no_skills() {
        use llm_harness_loop::test_utils::MockLlmClient;

        let client = Arc::new(MockLlmClient::new(vec![]));
        let builder = HarnessBuilder::new("m").provider("m", client);
        let (_client, opts) = builder.resolve_and_build().unwrap();
        let has_skill_read = opts.tools.iter().any(|t| t.name() == "skill_read");
        assert!(
            !has_skill_read,
            "skill_read tool should NOT be registered when no skills"
        );
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn disable_skill_read_tool_opt_out() {
        use llm_harness_loop::test_utils::MockLlmClient;
        use llm_harness_runtime_sandbox_os::OsEnv;

        let tmp = tempfile::TempDir::new().unwrap();
        let env: Arc<dyn llm_harness_types::ExecutionEnv> = Arc::new(OsEnv::new(tmp.path()));
        let client = Arc::new(MockLlmClient::new(vec![]));
        let plugin = MockPlugin::new("p1").with_skill("my-skill");
        let builder = HarnessBuilder::new("m")
            .provider("m", client)
            .install(&plugin)
            .disable_skill_read_tool()
            .inject_skill_read_tool(&env);
        let (_client, opts) = builder.resolve_and_build().unwrap();
        let has_skill_read = opts.tools.iter().any(|t| t.name() == "skill_read");
        assert!(
            !has_skill_read,
            "skill_read should NOT be registered when opted out"
        );
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn build_sets_readable_skills_when_skills_present() {
        use llm_harness_loop::test_utils::MockLlmClient;
        use llm_harness_runtime_sandbox_os::OsEnv;

        let tmp = tempfile::TempDir::new().unwrap();
        let env: Arc<dyn llm_harness_types::ExecutionEnv> = Arc::new(OsEnv::new(tmp.path()));
        let client = Arc::new(MockLlmClient::new(vec![]));
        let plugin = MockPlugin::new("p1").with_skill("my-skill");
        let builder = HarnessBuilder::new("m")
            .provider("m", client)
            .install(&plugin)
            .inject_skill_read_tool(&env);
        let (_client, opts) = builder.resolve_and_build().unwrap();
        assert!(
            opts.readable_skills.is_some(),
            "readable_skills store should be set when skills are present"
        );
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn build_no_readable_skills_when_no_skills() {
        use llm_harness_loop::test_utils::MockLlmClient;

        let client = Arc::new(MockLlmClient::new(vec![]));
        let builder = HarnessBuilder::new("m").provider("m", client);
        let (_client, opts) = builder.resolve_and_build().unwrap();
        assert!(
            opts.readable_skills.is_none(),
            "readable_skills should NOT be set when no skills"
        );
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn disable_skill_read_tool_leaves_readable_skills_none() {
        use llm_harness_loop::test_utils::MockLlmClient;
        use llm_harness_runtime_sandbox_os::OsEnv;

        let tmp = tempfile::TempDir::new().unwrap();
        let env: Arc<dyn llm_harness_types::ExecutionEnv> = Arc::new(OsEnv::new(tmp.path()));
        let client = Arc::new(MockLlmClient::new(vec![]));
        let plugin = MockPlugin::new("p1").with_skill("my-skill");
        let builder = HarnessBuilder::new("m")
            .provider("m", client)
            .install(&plugin)
            .disable_skill_read_tool()
            .inject_skill_read_tool(&env);
        let (_client, opts) = builder.resolve_and_build().unwrap();
        assert!(
            opts.readable_skills.is_none(),
            "readable_skills should NOT be set when skill_read_tool is disabled"
        );
    }
}
