use std::sync::Arc;

use llm_harness_agent::{HarnessHooks, Plugin};
use llm_harness_types::Tool;

use crate::prompt::{self, SystemPromptHook};
use crate::tools::{bash, edit, read, write};

pub struct CodingAgentPlugin {
    tools: Vec<Arc<dyn Tool>>,
    context_files: Vec<prompt::ContextFile>,
    cwd: std::path::PathBuf,
}

impl CodingAgentPlugin {
    pub fn new(cwd: &std::path::Path) -> Self {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(read::ReadTool::new()),
            Arc::new(bash::BashTool::new()),
            Arc::new(edit::EditTool::new()),
            Arc::new(write::WriteTool::new()),
        ];
        Self {
            context_files: prompt::find_context_files(cwd),
            tools,
            cwd: cwd.to_path_buf(),
        }
    }

    pub fn system_prompt(&self) -> String {
        prompt::build_system_prompt(&self.tools, &self.cwd, &self.context_files)
    }
}

impl Plugin for CodingAgentPlugin {
    fn name(&self) -> &str {
        "coding-agent"
    }

    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        tools.extend(self.tools.iter().cloned());
    }

    fn register_hooks(&self, hooks: &mut HarnessHooks) {
        hooks
            .before_run
            .push(Arc::new(SystemPromptHook::new(self.system_prompt())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_name_is_coding_agent() {
        let plugin = CodingAgentPlugin::new(std::path::Path::new("."));
        assert_eq!(plugin.name(), "coding-agent");
    }

    #[test]
    fn plugin_registers_four_tools() {
        let plugin = CodingAgentPlugin::new(std::path::Path::new("."));
        let mut tools: Vec<Arc<dyn Tool>> = vec![];
        plugin.register_tools(&mut tools);
        assert_eq!(tools.len(), 4);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"edit"));
        assert!(names.contains(&"write"));
    }

    #[test]
    fn plugin_registers_before_run_hook() {
        let plugin = CodingAgentPlugin::new(std::path::Path::new("."));
        let mut hooks = HarnessHooks::none();
        plugin.register_hooks(&mut hooks);
        assert_eq!(hooks.before_run.len(), 1);
    }

    #[test]
    fn system_prompt_is_not_empty() {
        let plugin = CodingAgentPlugin::new(std::path::Path::new("."));
        let prompt = plugin.system_prompt();
        assert!(!prompt.is_empty());
        assert!(prompt.contains("coding assistant"));
    }
}
