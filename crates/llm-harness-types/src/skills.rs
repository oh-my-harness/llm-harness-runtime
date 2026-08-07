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
    /// Absolute path of the source SKILL.md file.
    pub source: PathBuf,
    /// Skill root directory (parent of `source`). Used for resolving
    /// relative sub-file paths in `skill_read` and `resolve_skill_resource`.
    pub base_dir: PathBuf,
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

// ── Skill markup escaping ───────────────────────────────────────────────────

/// Escape tags that could forge skill markup boundaries.
///
/// Handles both bare tags (`<skill>`) and attributed open tags
/// (`<skill name="...">`) by matching the tag prefix.
pub fn escape_skill_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Closing tags: </skill> and </available-skills>
        if s[i..].starts_with("</skill>") {
            out.push_str("</skill\\>");
            i += 8;
        } else if s[i..].starts_with("</available-skills>") {
            out.push_str("</available-skills\\>");
            i += 19;
        } else if s[i..].starts_with("<skill>") {
            out.push_str("<skill\\>");
            i += 7;
        } else if s[i..].starts_with("<skill ") {
            // Attributed open tag: <skill name="..."> — escape the opening.
            out.push_str("<skill\\ ");
            i += 7;
        } else if s[i..].starts_with("<available-skills>") {
            out.push_str("<available-skills\\>");
            i += 18;
        } else if s[i..].starts_with("<available-skills ") {
            out.push_str("<available-skills\\ ");
            i += 18;
        } else {
            // Safe: push one char (handles multi-byte UTF-8 correctly).
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Escape a string for safe use inside an XML attribute value (`"..."`).
pub fn escape_xml_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Escape a string for safe use as XML element text (body).
///
/// Escapes `&`, `<`, `>` — the three characters with special meaning in XML
/// element content. Unlike [`escape_xml_attr`], `"` is left as-is (legal in body).
pub fn escape_xml_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
