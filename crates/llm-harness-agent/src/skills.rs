use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use llm_harness_types::*;
use tokio_util::sync::CancellationToken;

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

// ── Skill validation ──────────────────────────────────────────────────────────

fn is_valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

// ── Frontmatter parser ────────────────────────────────────────────────────────

/// Parse optional YAML frontmatter from a Markdown file.
///
/// Returns `(fields, body)` where `fields` is a map of `kebab-case` keys to
/// their string values. If there is no frontmatter the map is empty and `body`
/// is the full input.
fn parse_frontmatter(text: &str) -> (HashMap<String, String>, &str) {
    let mut fields = HashMap::new();

    // Frontmatter must start with "---" on the first line.
    let after_fence = match text.strip_prefix("---") {
        Some(rest) if rest.starts_with('\n') || rest.is_empty() => {
            &rest[rest.starts_with('\n') as usize..]
        }
        _ => return (fields, text),
    };

    // Find the closing "---".
    let close = match after_fence.find("\n---") {
        Some(pos) => pos,
        None => return (fields, text),
    };

    let fm_text = &after_fence[..close];
    let body_start = close + 4; // skip "\n---"
    let body = after_fence
        .get(body_start..)
        .unwrap_or("")
        .trim_start_matches('\n');

    for line in fm_text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(colon_pos) = line.find(':') {
            let key = line[..colon_pos].trim().to_lowercase();
            let raw_val = line[colon_pos + 1..].trim();
            // Strip surrounding quotes.
            let val = raw_val
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| {
                    raw_val
                        .strip_prefix('\'')
                        .and_then(|s| s.strip_suffix('\''))
                })
                .unwrap_or(raw_val)
                .to_string();
            if !key.is_empty() {
                fields.insert(key, val);
            }
        }
    }

    (fields, body)
}

// ── load_skills ───────────────────────────────────────────────────────────────

/// Scan `dirs` for `SKILL.md` files and return all successfully loaded skills.
///
/// Each directory in `dirs` is treated as a skills root: immediate
/// subdirectories are scanned for a `SKILL.md` file.  Loading failures produce
/// `SkillDiagnostic` entries and do not abort the overall load.
pub async fn load_skills(
    env: &dyn ExecutionEnv,
    dirs: &[PathBuf],
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let tagged_dirs: Vec<(String, PathBuf)> =
        dirs.iter().map(|d| (String::new(), d.clone())).collect();
    let (sourced, diags) = load_sourced_skills(env, &tagged_dirs).await;
    (sourced.into_iter().map(|s| s.skill).collect(), diags)
}

/// Like `load_skills` but attaches a caller-supplied source tag to each skill.
pub async fn load_sourced_skills(
    env: &dyn ExecutionEnv,
    dirs: &[(String, PathBuf)],
) -> (Vec<SourcedSkill>, Vec<SkillDiagnostic>) {
    let mut skills = Vec::new();
    let mut diags = Vec::new();

    for (tag, dir) in dirs {
        let abort = CancellationToken::new();
        let entries = match env.list_dir(dir, abort).await {
            Ok(e) => e,
            Err(err) => {
                diags.push(SkillDiagnostic {
                    source: dir.clone(),
                    level: DiagnosticLevel::Warn,
                    message: format!("failed to list skills directory: {err}"),
                });
                continue;
            }
        };

        for entry in entries {
            if !entry.is_dir {
                continue;
            }
            let skill_file = entry.path.join("SKILL.md");
            let abort = CancellationToken::new();
            let content = match env.read_text_file(&skill_file, abort).await {
                Ok(c) => c,
                Err(_) => continue, // No SKILL.md in this subdirectory — skip silently.
            };

            match parse_skill_file(&skill_file, &content, &entry.path) {
                Ok(skill) => skills.push(SourcedSkill {
                    skill,
                    source_tag: tag.clone(),
                }),
                Err(msg) => diags.push(SkillDiagnostic {
                    source: skill_file,
                    level: DiagnosticLevel::Warn,
                    message: msg,
                }),
            }
        }
    }

    (skills, diags)
}

fn parse_skill_file(source: &Path, content: &str, dir: &Path) -> Result<Skill, String> {
    let (fields, body) = parse_frontmatter(content);

    // Derive the name: frontmatter > parent directory name.
    let dir_name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    let name = fields
        .get("name")
        .cloned()
        .unwrap_or_else(|| dir_name.clone());

    if !is_valid_skill_name(&name) {
        return Err(format!(
            "invalid skill name '{name}': must be lowercase letters/digits/hyphens, ≤ 64 chars"
        ));
    }

    let description = fields.get("description").cloned().unwrap_or_default();
    if description.is_empty() {
        return Err("skill 'description' is required and must not be empty".into());
    }
    if description.len() > 1024 {
        return Err(format!(
            "skill description exceeds 1024 chars ({} chars)",
            description.len()
        ));
    }

    let label = fields.get("label").cloned().filter(|s| !s.is_empty());
    let disable_model_invocation = fields
        .get("disable-model-invocation")
        .map(|v| v == "true")
        .unwrap_or(false);

    Ok(Skill {
        name,
        label,
        description,
        content: body.to_string(),
        source: source.to_path_buf(),
        disable_model_invocation,
    })
}

// ── load_prompt_templates ─────────────────────────────────────────────────────

/// Scan `dirs` for `.md` template files (non-recursive) and return them.
pub async fn load_prompt_templates(
    env: &dyn ExecutionEnv,
    dirs: &[PathBuf],
) -> (Vec<PromptTemplate>, Vec<SkillDiagnostic>) {
    let mut templates = Vec::new();
    let mut diags = Vec::new();

    for dir in dirs {
        let abort = CancellationToken::new();
        let entries = match env.list_dir(dir, abort).await {
            Ok(e) => e,
            Err(err) => {
                diags.push(SkillDiagnostic {
                    source: dir.clone(),
                    level: DiagnosticLevel::Warn,
                    message: format!("failed to list templates directory: {err}"),
                });
                continue;
            }
        };

        for entry in entries {
            if entry.is_dir {
                continue;
            }
            let path = &entry.path;
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }

            let name = match path.file_stem().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };

            let abort = CancellationToken::new();
            let content = match env.read_text_file(path, abort).await {
                Ok(c) => c,
                Err(err) => {
                    diags.push(SkillDiagnostic {
                        source: path.clone(),
                        level: DiagnosticLevel::Warn,
                        message: format!("failed to read template file: {err}"),
                    });
                    continue;
                }
            };

            let (fields, body) = parse_frontmatter(&content);
            let description = fields.get("description").cloned().unwrap_or_default();

            // Fall back to first non-empty body line, truncated to 60 chars.
            let description = if description.is_empty() {
                let first_line = body
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim();
                if first_line.len() > 60 {
                    format!("{}...", &first_line[..60])
                } else {
                    first_line.to_string()
                }
            } else {
                description
            };

            templates.push(PromptTemplate {
                name,
                description,
                content: body.to_string(),
                source: path.clone(),
            });
        }
    }

    (templates, diags)
}

// ── System prompt formatting ──────────────────────────────────────────────────

/// Build the skills section injected into the system prompt.
///
/// Only includes skills with `disable_model_invocation = false`.
pub fn format_skills_for_system_prompt(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();

    if visible.is_empty() {
        return String::new();
    }

    let mut buf = String::from("<available-skills>\n");
    for skill in visible {
        buf.push_str(&format!(
            "  <skill name=\"{}\" location=\"{}\">{}</skill>\n",
            skill.name,
            skill.source.display(),
            skill.description
        ));
    }
    buf.push_str("</available-skills>");
    buf
}

/// Wrap a skill's content for explicit invocation as a user message.
pub fn format_skill_invocation(skill: &Skill, additional_instructions: Option<&str>) -> String {
    let mut buf = format!(
        "<skill name=\"{}\" location=\"{}\">\n{}\n</skill>",
        skill.name,
        skill.source.display(),
        skill.content
    );
    if let Some(extra) = additional_instructions {
        buf.push('\n');
        buf.push_str(extra);
    }
    buf
}

// ── invoke_template ───────────────────────────────────────────────────────────

/// Expand positional placeholders in a template with the supplied arguments.
///
/// Supported syntax:
/// - `$N` — N-th argument (1-based)
/// - `$@` or `$ARGUMENTS` — all arguments joined with a space
/// - `${@:N}` — arguments from position N onwards
/// - `${@:N:L}` — L arguments starting at position N
pub fn invoke_template(template: &PromptTemplate, args: &[String]) -> String {
    expand_placeholders(&template.content, args)
}

fn expand_placeholders(content: &str, args: &[String]) -> String {
    let bytes = content.as_bytes();
    let mut result = String::with_capacity(content.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'$' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'$' {
                i += 1;
            }
            result.push_str(&content[start..i]);
            continue;
        }

        i += 1; // skip '$'
        if i >= bytes.len() {
            result.push('$');
            break;
        }

        match bytes[i] {
            b'{' => {
                i += 1; // skip '{'
                let start = i;
                while i < bytes.len() && bytes[i] != b'}' {
                    i += 1;
                }
                let expr = &content[start..i];
                if i < bytes.len() {
                    i += 1; // skip '}'
                }
                result.push_str(&expand_brace(expr, args));
            }
            b'@' => {
                i += 1;
                result.push_str(&args.join(" "));
            }
            b'A' if content[i..].starts_with("ARGUMENTS") => {
                i += "ARGUMENTS".len();
                result.push_str(&args.join(" "));
            }
            b'0'..=b'9' => {
                let num_start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                let n: usize = content[num_start..i].parse().unwrap_or(0);
                if n > 0 {
                    result.push_str(args.get(n - 1).map(|s| s.as_str()).unwrap_or(""));
                }
            }
            _ => {
                result.push('$');
            }
        }
    }

    result
}

/// Expand `${@:N}` and `${@:N:L}` brace expressions.
fn expand_brace(expr: &str, args: &[String]) -> String {
    if let Some(rest) = expr.strip_prefix("@:") {
        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        let n: usize = parts[0].parse().unwrap_or(1);
        let start = n.saturating_sub(1).min(args.len());
        let subset = if parts.len() == 2 {
            let l: usize = parts[1].parse().unwrap_or(0);
            &args[start..][..l.min(args.len().saturating_sub(start))]
        } else {
            &args[start..]
        };
        subset.join(" ")
    } else {
        // Unknown brace expression — leave verbatim.
        format!("${{{expr}}}")
    }
}

// ── parse_command_args ────────────────────────────────────────────────────────

/// Split a user input string into arguments using shell-style quote parsing.
///
/// - Whitespace separates arguments.
/// - Single-quoted and double-quoted regions are treated as one argument.
/// - Quote characters themselves are stripped from the result.
pub fn parse_command_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;

    for ch in input.chars() {
        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                current.push(ch);
            }
        } else if in_double {
            if ch == '"' {
                in_double = false;
            } else {
                current.push(ch);
            }
        } else {
            match ch {
                '\'' => in_single = true,
                '"' => in_double = true,
                ' ' | '\t' | '\n' => {
                    if !current.is_empty() {
                        args.push(current.clone());
                        current.clear();
                    }
                }
                _ => current.push(ch),
            }
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;
    use crate::env::OsEnv;

    // ── parse_command_args ────────────────────────────────────────────────────

    #[test]
    fn parse_args_simple_words() {
        assert_eq!(parse_command_args("a b c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_args_double_quotes() {
        let args = parse_command_args(r#"staging "update API endpoint" --dry-run"#);
        assert_eq!(args, vec!["staging", "update API endpoint", "--dry-run"]);
    }

    #[test]
    fn parse_args_single_quotes() {
        let args = parse_command_args("foo 'hello world' bar");
        assert_eq!(args, vec!["foo", "hello world", "bar"]);
    }

    #[test]
    fn parse_args_empty() {
        assert_eq!(parse_command_args(""), Vec::<String>::new());
    }

    // ── invoke_template ───────────────────────────────────────────────────────

    fn tmpl(content: &str) -> PromptTemplate {
        PromptTemplate {
            name: "test".into(),
            description: "".into(),
            content: content.into(),
            source: PathBuf::from("test.md"),
        }
    }

    #[test]
    fn invoke_positional() {
        let t = tmpl("Deploy to $1. Task: $2.");
        let result = invoke_template(&t, &["staging".into(), "fix bugs".into()]);
        assert_eq!(result, "Deploy to staging. Task: fix bugs.");
    }

    #[test]
    fn invoke_dollar_at() {
        let t = tmpl("Args: $@");
        let result = invoke_template(&t, &["a".into(), "b".into(), "c".into()]);
        assert_eq!(result, "Args: a b c");
    }

    #[test]
    fn invoke_dollar_arguments() {
        let t = tmpl("All: $ARGUMENTS");
        let result = invoke_template(&t, &["x".into(), "y".into()]);
        assert_eq!(result, "All: x y");
    }

    #[test]
    fn invoke_brace_slice_from() {
        let t = tmpl("Rest: ${@:2}");
        let result = invoke_template(&t, &["a".into(), "b".into(), "c".into()]);
        assert_eq!(result, "Rest: b c");
    }

    #[test]
    fn invoke_brace_slice_range() {
        let t = tmpl("Slice: ${@:2:2}");
        let result = invoke_template(&t, &["a".into(), "b".into(), "c".into(), "d".into()]);
        assert_eq!(result, "Slice: b c");
    }

    #[test]
    fn invoke_missing_arg_is_empty() {
        let t = tmpl("$1 and $3");
        let result = invoke_template(&t, &["only".into()]);
        assert_eq!(result, "only and ");
    }

    // ── format_skills_for_system_prompt ───────────────────────────────────────

    fn make_skill(name: &str, desc: &str, disable: bool) -> Skill {
        Skill {
            name: name.into(),
            label: None,
            description: desc.into(),
            content: "skill body".into(),
            source: PathBuf::from(format!("/skills/{name}/SKILL.md")),
            disable_model_invocation: disable,
        }
    }

    #[test]
    fn format_skills_excludes_disabled() {
        let skills = vec![
            make_skill("public-skill", "Do stuff", false),
            make_skill("private-skill", "Hidden", true),
        ];
        let prompt = format_skills_for_system_prompt(&skills);
        assert!(prompt.contains("public-skill"));
        assert!(!prompt.contains("private-skill"));
    }

    #[test]
    fn format_skill_invocation_wraps_content() {
        let skill = make_skill("deploy", "Deploy the app", false);
        let out = format_skill_invocation(&skill, Some("Use staging only."));
        assert!(out.contains("<skill name=\"deploy\""));
        assert!(out.contains("skill body"));
        assert!(out.contains("Use staging only."));
    }

    // ── File-based loading (tempfile + OsEnv) ─────────────────────────────────

    #[tokio::test]
    async fn load_skill_from_temp_dir() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: Does things\n---\nDo the thing.",
        )
        .unwrap();

        let env = OsEnv::new(tmp.path());
        let (skills, diags) = load_skills(&env, &[tmp.path().to_path_buf()]).await;
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "my-skill");
        assert_eq!(skills[0].description, "Does things");
        assert_eq!(skills[0].content.trim(), "Do the thing.");
    }

    #[tokio::test]
    async fn load_skill_missing_description_produces_diagnostic() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join("bad-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: bad-skill\n---\nBody.",
        )
        .unwrap();

        let env = OsEnv::new(tmp.path());
        let (skills, diags) = load_skills(&env, &[tmp.path().to_path_buf()]).await;
        assert!(skills.is_empty());
        assert!(
            !diags.is_empty(),
            "should have a diagnostic for missing description"
        );
    }

    #[tokio::test]
    async fn load_prompt_template_from_temp_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("deploy.md"),
            "---\ndescription: Deploy to an env\n---\nDeploy to $1.",
        )
        .unwrap();

        let env = OsEnv::new(tmp.path());
        let (templates, diags) = load_prompt_templates(&env, &[tmp.path().to_path_buf()]).await;
        assert!(diags.is_empty());
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].name, "deploy");
        assert_eq!(templates[0].description, "Deploy to an env");
        assert_eq!(templates[0].content.trim(), "Deploy to $1.");
    }

    #[tokio::test]
    async fn load_template_description_inferred_from_body() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("hello.md"), "Run the hello task").unwrap();

        let env = OsEnv::new(tmp.path());
        let (templates, _) = load_prompt_templates(&env, &[tmp.path().to_path_buf()]).await;
        assert_eq!(templates[0].description, "Run the hello task");
    }
}
