use llm_harness_agent::HarnessBuilder;

use crate::AgentId;

/// Declarative specification for a single team member.
///
/// Wraps a [`HarnessBuilder`] plus social metadata (`id`, `expertise`,
/// optional reflection) so an [`AgentTeam`](crate::team::AgentTeam) can
/// build the harness later with the social infrastructure attached.
/// Builder configuration goes through [`IndividualSpec::configure`].
pub struct IndividualSpec {
    id: AgentId,
    expertise: String,
    builder: HarnessBuilder,
    reflection_interval: usize,
    allow_subagents: bool,
}

impl IndividualSpec {
    /// Create a new spec for an agent with the given id, expertise, and model.
    pub fn new(
        id: impl Into<String>,
        expertise: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            id: AgentId::new(id),
            expertise: expertise.into(),
            builder: HarnessBuilder::new(model),
            reflection_interval: 0,
            allow_subagents: false,
        }
    }

    /// Allow this agent to spawn sub-agents (`spawn_agent` and the
    /// companion sub-agent tools from `llm-harness-subagents`).
    ///
    /// Off by default: spawning is an expensive capability that multiplies
    /// LLM traffic and widens the blast radius of a runaway task, so it is
    /// granted per member instead of being given to every team agent.
    pub fn allow_subagents(mut self, allow: bool) -> Self {
        self.allow_subagents = allow;
        self
    }

    /// Configure the inner [`HarnessBuilder`].
    ///
    /// Builder methods are consuming (`fn xxx(mut self) -> Self`), so they
    /// cannot be called through a shared reference — pass a closure instead:
    ///
    /// ```ignore
    /// let spec = spec.configure(|b| b.system_prompt(Some(prompt)).max_tokens(1024));
    /// ```
    pub fn configure(mut self, f: impl FnOnce(HarnessBuilder) -> HarnessBuilder) -> Self {
        self.builder = f(self.builder);
        self
    }

    /// The agent's identifier.
    pub fn id(&self) -> &AgentId {
        &self.id
    }

    /// The agent's area of expertise.
    pub fn expertise(&self) -> &str {
        &self.expertise
    }

    /// The model ID this agent will use (delegates to the inner builder).
    pub fn model(&self) -> &str {
        self.builder.model()
    }

    /// Set the reflection interval: after every `n` completed runs, the agent
    /// receives a reflection task in its inbox with accumulated transcripts.
    ///
    /// The agent then reviews past runs and persists lessons using
    /// `memory_write` (if available). Set to 0 (default) to disable.
    pub fn reflection_interval(mut self, n: usize) -> Self {
        self.reflection_interval = n;
        self
    }

    /// Consume the spec and return the reflection interval.
    pub(crate) fn take_reflection_interval(&mut self) -> usize {
        std::mem::replace(&mut self.reflection_interval, 0)
    }

    /// Consume the spec and return whether sub-agent spawning is allowed.
    pub(crate) fn take_allow_subagents(&mut self) -> bool {
        std::mem::replace(&mut self.allow_subagents, false)
    }

    /// Consume the spec and return the inner [`HarnessBuilder`].
    pub(crate) fn into_builder(self) -> HarnessBuilder {
        self.builder
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_creates_with_id_expertise_model() {
        let spec = IndividualSpec::new("analyst-1", "data analysis", "gpt-4o");
        assert_eq!(spec.id().as_str(), "analyst-1");
        assert_eq!(spec.expertise(), "data analysis");
        assert_eq!(spec.model(), "gpt-4o");
    }

    #[test]
    fn spec_configure_configures_inner_builder() {
        let spec = IndividualSpec::new("analyst-1", "data analysis", "gpt-4o")
            .configure(|b| b.system_prompt(Some("you are an analyst".to_string())));
        // Metadata readable after configure.
        assert_eq!(spec.id().as_str(), "analyst-1");
        assert_eq!(spec.expertise(), "data analysis");
        assert_eq!(spec.model(), "gpt-4o");
        // The closure's builder changes are preserved through into_builder.
        let _builder: HarnessBuilder = spec.into_builder().max_tokens(16384);
    }

    #[test]
    fn reflection_disabled_by_default() {
        let spec = IndividualSpec::new("a1", "testing", "gpt-4o");
        assert_eq!(spec.reflection_interval, 0);
    }

    #[test]
    fn reflection_interval_builder() {
        let spec = IndividualSpec::new("a1", "testing", "gpt-4o").reflection_interval(5);
        assert_eq!(spec.reflection_interval, 5);
        // take_reflection_interval consumes the value.
        let mut spec = spec;
        assert_eq!(spec.take_reflection_interval(), 5);
        assert_eq!(spec.reflection_interval, 0);
    }

    #[test]
    fn subagents_disabled_by_default() {
        let spec = IndividualSpec::new("a1", "testing", "gpt-4o");
        assert!(!spec.allow_subagents);
    }

    #[test]
    fn allow_subagents_builder() {
        let spec = IndividualSpec::new("a1", "testing", "gpt-4o").allow_subagents(true);
        assert!(spec.allow_subagents);
        // take consumes the flag.
        let mut spec = spec;
        assert!(spec.take_allow_subagents());
        assert!(!spec.allow_subagents);
    }
}
