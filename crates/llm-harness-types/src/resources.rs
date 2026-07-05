use crate::{PromptTemplate, Skill};

/// Harness runtime resources, provided via `BeforeRunHook` context.
pub struct AgentHarnessResources {
    /// Loaded skills (full content available to hooks).
    pub skills: Vec<Skill>,
    /// Loaded prompt templates.
    pub templates: Vec<PromptTemplate>,
}
