use std::path::PathBuf;

use crate::DiagnosticLevel;

// ── Skill ─────────────────────────────────────────────────────────────────────

/// A skill loaded from a `SKILL.md` file.
#[derive(Debug, Clone)]
pub struct Skill {
    /// Unique skill name (lowercase letters + digits + hyphens, ≤ 64 chars).
    pub name: String,
    /// Optional UI-friendly label; falls back to `name` when absent.
    pub label: Option<String>,
    /// Short description shown to the LLM (≤ 1024 chars).
    pub description: String,
    /// Full Markdown body of the skill (everything after the frontmatter).
    pub content: String,
    /// Absolute path of the source SKILL.md file.
    pub source: PathBuf,
    /// When `true`, the skill is excluded from the system prompt and must be
    /// invoked explicitly via `AgentHarness::invoke_skill()`.
    pub disable_model_invocation: bool,
}

/// A skill with its load-time source tag.
#[derive(Debug, Clone)]
pub struct SourcedSkill {
    /// The loaded skill.
    pub skill: Skill,
    /// Caller-supplied tag identifying where the skill came from
    /// (e.g. `"user-config"`, `"project-local"`, `"plugin:foo"`).
    pub source_tag: String,
}

/// Diagnostic produced during skill or template loading.
#[derive(Debug, Clone)]
pub struct SkillDiagnostic {
    /// Source file that triggered this diagnostic.
    pub source: PathBuf,
    /// Severity.
    pub level: DiagnosticLevel,
    /// Human-readable message.
    pub message: String,
}

// ── PromptTemplate ────────────────────────────────────────────────────────────

/// A prompt template loaded from a `.md` file.
#[derive(Debug, Clone)]
pub struct PromptTemplate {
    /// Template name derived from the file name (without `.md` extension).
    pub name: String,
    /// UI description from frontmatter; falls back to first non-empty body line.
    pub description: String,
    /// Template body with `$1`, `$@`, `${@:N}` placeholders.
    pub content: String,
    /// Absolute path of the source file.
    pub source: PathBuf,
}
