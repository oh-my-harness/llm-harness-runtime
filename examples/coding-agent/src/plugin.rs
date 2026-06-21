use std::sync::Arc;

use llm_harness_agent::{HarnessHooks, Plugin};
use llm_harness_types::Tool;

use crate::prompt::{self, SystemPromptHook};
use crate::tools::{bash, edit, read, write};

pub struct CodingAgentPlugin {
    system_prompt: String,
}

impl CodingAgentPlugin {
    pub fn new(cwd: &std::path::Path) -> Self {
        let context_files = prompt::find_context_files(cwd);
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(read::ReadTool::new()),
            Arc::new(bash::BashTool::new()),
            Arc::new(edit::EditTool::new()),
            Arc::new(write::WriteTool::new()),
        ];
        let system_prompt = prompt::build_system_prompt(&tools, cwd, &context_files);
        Self { system_prompt }
    }

    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }
}

impl Plugin for CodingAgentPlugin {
    fn name(&self) -> &str {
        "coding-agent"
    }

    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        tools.push(Arc::new(read::ReadTool::new()));
        tools.push(Arc::new(bash::BashTool::new()));
        tools.push(Arc::new(edit::EditTool::new()));
        tools.push(Arc::new(write::WriteTool::new()));
    }

    fn register_hooks(&self, hooks: &mut HarnessHooks) {
        hooks
            .before_run
            .push(Arc::new(SystemPromptHook::new(self.system_prompt.clone())));
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
        assert!(prompt.contains("read"));
        assert!(prompt.contains("bash"));
        assert!(prompt.contains("edit"));
        assert!(prompt.contains("write"));
    }
}
