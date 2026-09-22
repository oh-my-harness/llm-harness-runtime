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
    persona: String,
    generated_prompt: Option<String>,
    prompt: Option<String>,
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
            persona: String::new(),
            generated_prompt: None,
            prompt: None,
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

    /// Set the member's persona: its personality and collaboration style.
    ///
    /// If neither [`IndividualSpec::with_prompt`] nor a system prompt supplied
    /// through [`IndividualSpec::configure`] is present, this generates a
    /// member-specific system prompt containing the id, expertise, and persona.
    pub fn with_persona(mut self, persona: impl Into<String>) -> Self {
        let persona = persona.into();
        self.generated_prompt = (!persona.trim().is_empty())
            .then(|| default_persona_prompt(&self.id, &self.expertise, &persona));
        self.persona = persona;
        self
    }

    /// Set the member's complete system prompt.
    ///
    /// This is intentionally a full prompt rather than a fragment. Callers
    /// that use both a persona and a custom prompt should include the persona
    /// in the prompt text. An explicit prompt takes precedence over a prompt
    /// supplied later through [`IndividualSpec::configure`].
    pub fn with_prompt(mut self, prompt: impl Into<String>) -> Self {
        let prompt = prompt.into();
        self.prompt = Some(prompt);
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

    /// The member's personality and collaboration style, if configured.
    pub fn persona(&self) -> &str {
        &self.persona
    }

    /// The prompt that will be used if this spec is built now.
    ///
    /// An explicit [`IndividualSpec::with_prompt`] wins, followed by a system
    /// prompt set through [`IndividualSpec::configure`], followed by a prompt
    /// generated from [`IndividualSpec::with_persona`].
    pub fn prompt(&self) -> Option<&str> {
        self.prompt
            .as_deref()
            .or_else(|| self.builder.configured_system_prompt())
            .or(self.generated_prompt.as_deref())
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
        let Self {
            mut builder,
            generated_prompt,
            prompt,
            ..
        } = self;
        if let Some(prompt) = prompt {
            builder = builder.system_prompt(Some(prompt));
        } else if builder.configured_system_prompt().is_none()
            && let Some(prompt) = generated_prompt
        {
            builder = builder.system_prompt(Some(prompt));
        }
        builder
    }
}

fn default_persona_prompt(id: &AgentId, expertise: &str, persona: &str) -> String {
    format!(
        "You are {id}, a peer member of an AgentTeam.\n\
         Expertise: {expertise}\n\
         Persona: {persona}"
    )
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
    fn persona_generates_member_specific_prompt() {
        let spec = IndividualSpec::new("analyst-1", "data analysis", "gpt-4o")
            .with_persona("cautious, evidence-driven, concise");

        assert_eq!(spec.persona(), "cautious, evidence-driven, concise");
        let prompt = spec.prompt().unwrap();
        assert!(prompt.contains("analyst-1"));
        assert!(prompt.contains("data analysis"));
        assert!(prompt.contains("cautious, evidence-driven, concise"));
    }

    #[test]
    fn full_prompt_overrides_generated_persona_prompt() {
        let spec = IndividualSpec::new("analyst-1", "data analysis", "gpt-4o")
            .with_persona("cautious")
            .with_prompt("custom full prompt");

        assert_eq!(spec.prompt(), Some("custom full prompt"));
    }

    #[test]
    fn persona_does_not_replace_explicit_prompt() {
        let spec = IndividualSpec::new("analyst-1", "data analysis", "gpt-4o")
            .with_prompt("custom full prompt")
            .with_persona("cautious");

        assert_eq!(spec.prompt(), Some("custom full prompt"));
        assert_eq!(spec.persona(), "cautious");
    }

    #[test]
    fn configured_prompt_takes_precedence_over_generated_persona_prompt() {
        let spec = IndividualSpec::new("analyst-1", "data analysis", "gpt-4o")
            .with_persona("cautious")
            .configure(|b| b.system_prompt(Some("custom configured prompt".to_string())));

        assert_eq!(spec.prompt(), Some("custom configured prompt"));
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
