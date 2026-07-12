use crate::{PromptTemplate, Skill};

/// Harness runtime resources, provided via `BeforeRunHook` context.
pub struct AgentHarnessResources {
    /// Loaded skills (metadata only; full content via `skill_read` tool).
    pub skills: Vec<Skill>,
    /// Loaded prompt templates.
    pub templates: Vec<PromptTemplate>,
}
