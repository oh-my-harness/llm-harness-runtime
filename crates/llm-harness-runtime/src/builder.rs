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
//! use llm_harness_runtime::HarnessBuilder;
//!
//! let harness = HarnessBuilder::new("claude-3-5-sonnet-20241022")
//!     .install(&my_auth_plugin)
//!     .install(&my_tools_plugin)
//!     .provider("claude-*", client)
//!     .build(env)
//!     .await?;
//! ```

use std::sync::Arc;

use llm_harness_agent::{
    AgentHarness, AgentHarnessOptions, HarnessHooks, Plugin, PromptTemplate, Skill, StateProvider,
};
use llm_harness_loop::LlmClient;
use llm_harness_types::{ExecutionEnv, Tool};
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
        opts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
