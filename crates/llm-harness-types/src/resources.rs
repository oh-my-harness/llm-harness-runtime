/// Harness runtime resources, provided via `BeforeRunHook` context.
pub struct AgentHarnessResources {
    /// Names of the loaded skills.
    pub skill_names: Vec<String>,
    /// Names of the loaded prompt templates.
    pub template_names: Vec<String>,
}
