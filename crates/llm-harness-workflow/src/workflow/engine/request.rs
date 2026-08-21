use llm_harness_types::RunExtensions;

/// Per-invocation trusted context for a workflow run.
///
/// The request is not serialized, logged, or stored in `WorkflowState`. Its
/// extensions are copied by shared ownership into every LLM step attempt, where
/// they become the step harness's immutable `RunContext` extensions.
///
/// Deterministic executor steps and spawned sub-agents do not receive these
/// values implicitly. Those boundaries require their own explicit delegation
/// contract.
#[derive(Clone, Default)]
pub struct WorkflowRunRequest {
    extensions: RunExtensions,
}

impl WorkflowRunRequest {
    /// Creates an empty workflow invocation request.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces one typed extension for every LLM step in this run.
    #[must_use]
    pub fn with_extension<T>(mut self, extension: T) -> Self
    where
        T: Send + Sync + 'static,
    {
        self.extensions.insert(extension);
        self
    }

    pub(super) fn extensions(&self) -> &RunExtensions {
        &self.extensions
    }
}
