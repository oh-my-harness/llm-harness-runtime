use std::path::Path;
use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{AgentError, BeforeRunCtx, BeforeRunHook, BeforeRunResult, Tool};

/// A pre-loaded project context file (e.g. CLAUDE.md).
#[derive(Debug, Clone)]
pub struct ContextFile {
    pub path: String,
    pub content: String,
}

/// Walk up from `cwd` to find CLAUDE.md and AGENTS.md.
pub fn find_context_files(cwd: &Path) -> Vec<ContextFile> {
    let mut files = Vec::new();
    for dir in cwd.ancestors() {
        for name in &["CLAUDE.md", "AGENTS.md"] {
            let p = dir.join(name);
            if !p.is_file() {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&p) else {
                continue;
            };
            let display = if let Ok(rel) = p.strip_prefix(cwd) {
                rel.to_string_lossy().to_string()
            } else {
                p.to_string_lossy().to_string()
            };
            if !files.iter().any(|cf: &ContextFile| cf.path == display) {
                files.push(ContextFile {
                    path: display,
                    content,
                });
            }
        }
    }
    files
}

/// Build the system prompt string.
pub fn build_system_prompt(
    tools: &[Arc<dyn Tool>],
    cwd: &Path,
    context_files: &[ContextFile],
) -> String {
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let cwd_str = cwd.to_string_lossy().replace('\\', "/");

    let tool_list = if tools.is_empty() {
        "(none)".to_string()
    } else {
        tools
            .iter()
            .map(|t| format!("- {}: {}", t.name(), t.description()))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let has_bash = tools.iter().any(|t| t.name() == "bash");
    let guidelines = if has_bash {
        "- Use bash for file operations like ls, grep, find\n\
         - Be concise in your responses\n\
         - Show file paths clearly when working with files"
    } else {
        "- Be concise in your responses\n\
         - Show file paths clearly when working with files"
    };

    let mut prompt = format!(
        "You are an expert coding assistant. \
         You help users by reading files, executing commands, editing code, and writing new files.\n\n\
         Available tools:\n{tool_list}\n\n\
         Guidelines:\n{guidelines}"
    );

    if !context_files.is_empty() {
        prompt.push_str("\n\n<project_context>\n\n");
        prompt.push_str("Project-specific instructions and guidelines:\n\n");
        for cf in context_files {
            prompt.push_str(&format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                cf.path, cf.content
            ));
        }
        prompt.push_str("</project_context>");
    }

    prompt.push_str(&format!("\nCurrent date: {date}"));
    prompt.push_str(&format!("\nCurrent working directory: {cwd_str}"));

    prompt
}

// ── SystemPromptHook ─────────────────────────────────────────────────────────

/// A `BeforeRunHook` that injects the system prompt into every run.
pub struct SystemPromptHook {
    prompt: String,
}

impl SystemPromptHook {
    pub fn new(prompt: String) -> Self {
        Self { prompt }
    }
}

impl BeforeRunHook for SystemPromptHook {
    fn before_run<'a>(
        &'a self,
        ctx: BeforeRunCtx<'a>,
    ) -> BoxFuture<'a, Result<BeforeRunResult, AgentError>> {
        let prompt = self.prompt.clone();
        Box::pin(async move {
            *ctx.system_prompt = Some(prompt);
            Ok(BeforeRunResult {
                additional_messages: vec![],
                system_prompt: None,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;
    use llm_harness_types::Tool;

    struct MockTool {
        name: &'static str,
        desc: &'static str,
    }

    impl Tool for MockTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            self.desc
        }
        fn parameters_schema(&self) -> &serde_json::Value {
            static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
            S.get_or_init(|| serde_json::json!({}))
        }
        fn execute<'a>(
            &'a self,
            _args: serde_json::Value,
            _ctx: &'a llm_harness_types::ToolContext,
        ) -> BoxFuture<'a, Result<llm_harness_types::ToolResult, llm_harness_types::ToolError>>
        {
            Box::pin(async { Err(llm_harness_types::ToolError::Aborted) })
        }
    }

    #[test]
    fn prompt_contains_tool_names_and_descriptions() {
        let read = MockTool {
            name: "read",
            desc: "Read a file",
        };
        let bash = MockTool {
            name: "bash",
            desc: "Run a command",
        };
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(read), Arc::new(bash)];
        let prompt = build_system_prompt(&tools, std::path::Path::new("/home/user/project"), &[]);
        assert!(prompt.contains("read: Read a file"), "got: {prompt}");
        assert!(prompt.contains("bash: Run a command"), "got: {prompt}");
        assert!(prompt.contains("/home/user/project"), "got: {prompt}");
    }

    #[test]
    fn prompt_appends_context_files() {
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let cfs = vec![ContextFile {
            path: "CLAUDE.md".into(),
            content: "# Rules\nBe careful.".into(),
        }];
        let prompt = build_system_prompt(&tools, std::path::Path::new("/tmp"), &cfs);
        assert!(
            prompt.contains("<project_instructions path=\"CLAUDE.md\">"),
            "got: {prompt}"
        );
        assert!(prompt.contains("Be careful."), "got: {prompt}");
    }

    #[test]
    fn prompt_no_context_files_no_project_context_block() {
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let prompt = build_system_prompt(&tools, std::path::Path::new("/tmp"), &[]);
        assert!(!prompt.contains("<project_instructions"), "got: {prompt}");
        assert!(!prompt.contains("<project_context"), "got: {prompt}");
    }

    #[test]
    fn prompt_includes_date() {
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let prompt = build_system_prompt(&tools, std::path::Path::new("/tmp"), &[]);
        assert!(prompt.contains("Current date:"), "got: {prompt}");
    }

    #[test]
    fn prompt_empty_tools_shows_none() {
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let prompt = build_system_prompt(&tools, std::path::Path::new("/tmp"), &[]);
        assert!(prompt.contains("(none)"), "got: {prompt}");
    }
}
