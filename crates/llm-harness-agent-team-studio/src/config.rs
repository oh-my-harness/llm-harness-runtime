//! Config file (`~/.harness/coding-team.toml`) parsing.
//!
//! Deliberately hand-rolled: the format is a fixed line-based subset of
//! TOML (key = value, [models] table, [[project]] array-of-tables), and
//! pulling the `toml` crate in for this is a dependency-tree cost the
//! workspace does not otherwise carry.

use crate::roles::ModelTier;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectCfg {
    pub id: String,
    pub repo_path: String,
    /// Parsed but currently informational (spec §4.1 keeps coder count at 1).
    pub coders: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub data_root: Option<PathBuf>,
    pub port: u16,
    pub models: ModelTier,
    pub scout_interval_secs: u64,
    pub projects: Vec<ProjectCfg>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("parse error at line {line}: {message}")]
    Parse { line: usize, message: String },
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_root: None,
            port: 8080,
            models: ModelTier {
                strong: "default".into(),
                main: "default".into(),
                cheap: "default".into(),
            },
            scout_interval_secs: 3600,
            projects: Vec::new(),
        }
    }
}

impl Config {
    /// Load and parse a config file.
    pub fn load(path: &std::path::Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Parse {
            line: 0,
            message: format!("read {}: {e}", path.display()),
        })?;
        Config::parse(&text)
    }

    /// Parse the config text. Unknown keys are ignored (forward compat).
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        let mut cfg = Config::default();
        let mut section = Section::Top;
        for (idx, raw) in text.lines().enumerate() {
            let line_no = idx + 1;
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == "[data_root]" || line == "[models]" {
                section = if line == "[models]" {
                    Section::Models
                } else {
                    // Kept for backward compat with an earlier table form.
                    Section::Top
                };
                continue;
            }
            if line == "[[project]]" {
                section = Section::Project;
                cfg.projects.push(ProjectCfg {
                    id: String::new(),
                    repo_path: String::new(),
                    coders: 1,
                });
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(ConfigError::Parse {
                    line: line_no,
                    message: format!("expected `key = value`, got: {line}"),
                });
            };
            let key = key.trim();
            let value = unquote(value.trim());
            match (section, key) {
                (Section::Top, "data_root") => cfg.data_root = Some(PathBuf::from(value)),
                (Section::Top, "port") => cfg.port = parse_u16(&value, line_no)?,
                (Section::Top, "scout_interval_secs") => {
                    cfg.scout_interval_secs = parse_u64(&value, line_no)?
                }
                (Section::Models, "strong") => cfg.models.strong = value,
                (Section::Models, "main") => cfg.models.main = value,
                (Section::Models, "cheap") => cfg.models.cheap = value,
                (Section::Project, "id") => last_project(&mut cfg, line_no)?.id = value,
                (Section::Project, "repo_path") => {
                    last_project(&mut cfg, line_no)?.repo_path = value
                }
                (Section::Project, "coders") => {
                    last_project(&mut cfg, line_no)?.coders = parse_u64(&value, line_no)? as usize
                }
                // Project-only keys outside a [[project]] section are a
                // user error, not a forward-compat case.
                (Section::Top | Section::Models, "id" | "repo_path" | "coders") => {
                    return Err(ConfigError::Parse {
                        line: line_no,
                        message: format!("`{key}` must live in a `[[project]]` section"),
                    });
                }
                _ => {} // unknown keys ignored
            }
        }
        Ok(cfg)
    }
}

/// Build one client per distinct model id from an API key (+optional
/// OpenAI-compatible base URL). Shared by the bin's startup factory and
/// the settings API's live re-install.
pub fn build_providers(
    key: &str,
    base_url: &str,
    models: &crate::roles::ModelTier,
) -> Vec<(String, Arc<dyn llm_harness_loop::LlmClient>)> {
    if key.is_empty() {
        return vec![];
    }
    let mut out: Vec<(String, Arc<dyn llm_harness_loop::LlmClient>)> = Vec::new();
    for model in [&models.strong, &models.main, &models.cheap] {
        if model.is_empty() || out.iter().any(|(m, _)| m == model) {
            continue;
        }
        let client: Arc<dyn llm_harness_loop::LlmClient> = if base_url.is_empty() {
            Arc::new(
                llm_harness_loop::OpenAIProvider::builder(key)
                    .max_tokens_scheme(llm_harness_loop::MaxTokensScheme::MaxCompletionTokens)
                    .build(),
            )
        } else {
            Arc::new(llm_harness_loop::openai_compatible(key, base_url))
        };
        out.push((model.clone(), client));
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Section {
    Top,
    Models,
    Project,
}

fn last_project(cfg: &mut Config, line_no: usize) -> Result<&mut ProjectCfg, ConfigError> {
    cfg.projects.last_mut().ok_or(ConfigError::Parse {
        line: line_no,
        message: "`[[project]]` section required before project keys".into(),
    })
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    v.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(v)
        .to_string()
}

fn parse_u16(v: &str, line: usize) -> Result<u16, ConfigError> {
    v.parse().map_err(|e| ConfigError::Parse {
        line,
        message: format!("invalid u16 `{v}`: {e}"),
    })
}

fn parse_u64(v: &str, line: usize) -> Result<u64, ConfigError> {
    v.parse().map_err(|e| ConfigError::Parse {
        line,
        message: format!("invalid u64 `{v}`: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let text = r#"
# comment
port = 9999
scout_interval_secs = 60
data_root = "/tmp/ct"

[models]
strong = "glm-5.2"
main = "glm-4.7"
cheap = "glm-4.5-air"

[[project]]
id = "a"
repo_path = "/tmp/a"
coders = 1

[[project]]
id = "b"
repo_path = "/tmp/b"
"#;
        let cfg = Config::parse(text).unwrap();
        assert_eq!(cfg.port, 9999);
        assert_eq!(cfg.scout_interval_secs, 60);
        assert_eq!(cfg.data_root, Some(PathBuf::from("/tmp/ct")));
        assert_eq!(cfg.models.strong, "glm-5.2");
        assert_eq!(cfg.models.main, "glm-4.7");
        assert_eq!(cfg.models.cheap, "glm-4.5-air");
        assert_eq!(cfg.projects.len(), 2);
        assert_eq!(cfg.projects[0].id, "a");
        assert_eq!(cfg.projects[0].repo_path, "/tmp/a");
        assert_eq!(cfg.projects[0].coders, 1);
        assert_eq!(cfg.projects[1].id, "b");
        assert_eq!(cfg.projects[1].coders, 1, "default coders");
    }

    #[test]
    fn defaults_apply() {
        let cfg = Config::parse("").unwrap();
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.scout_interval_secs, 3600);
        assert_eq!(cfg.models.strong, "default");
        assert!(cfg.projects.is_empty());
        assert!(cfg.data_root.is_none());
    }

    #[test]
    fn unknown_keys_ignored() {
        let cfg = Config::parse("port = 9000\nfuture_key = \"x\"\n").unwrap();
        assert_eq!(cfg.port, 9000);
    }

    #[test]
    fn project_keys_without_section_error() {
        let err = Config::parse("id = \"x\"\n").unwrap_err();
        assert!(err.to_string().contains("[[project]]"), "{err}");
    }

    #[test]
    fn malformed_line_reports_line_number() {
        let err = Config::parse("port = 8080\nwhat is this\n").unwrap_err();
        match err {
            ConfigError::Parse { line, .. } => assert_eq!(line, 2),
        }
    }
}
