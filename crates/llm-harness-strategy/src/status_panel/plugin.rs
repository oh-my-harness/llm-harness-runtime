//! plugin.rs — StatusPanelPlugin (impl Plugin).
//!
//! 一行接入：注册 `todo_write` 工具 + `StatusBarHook` + `ToolCallCounterHook`。

use std::sync::Arc;

use llm_harness_agent::{HarnessHooks, Plugin};
use llm_harness_types::Tool;

use crate::status_panel::collectors::{
    EnvironmentCollector, SideChannelCollector, StatusCollector, TodoCollector, ToolCallCounter,
    ToolCallCounterCollector,
};
use crate::status_panel::counter_hook::ToolCallCounterHook;
use crate::status_panel::hook::{StatusBarHook, UpdateStrategy};
use crate::status_panel::store::TodoStore;
use crate::status_panel::todo_tool::TodoWriteTool;

/// 状态栏 Plugin 配置。
#[derive(Debug, Clone)]
pub struct StatusPanelConfig {
    /// 更新策略。
    pub strategy: UpdateStrategy,
    /// 启用哪些收集器。
    pub enable_todo: bool,
    pub enable_environment: bool,
    pub enable_tool_counter: bool,
    pub enable_side_channel: bool,
    /// 侧信道窗口大小。
    pub side_channel_window: usize,
    /// 覆盖工作目录显示。
    pub working_dir: Option<std::path::PathBuf>,
}

impl Default for StatusPanelConfig {
    fn default() -> Self {
        Self {
            strategy: UpdateStrategy::default(),
            enable_todo: true,
            enable_environment: true,
            enable_tool_counter: true,
            enable_side_channel: false, // 默认关，有侵入性
            side_channel_window: 3,
            working_dir: None,
        }
    }
}

pub struct StatusPanelPlugin {
    config: StatusPanelConfig,
    todo_store: TodoStore,
    tool_counter: ToolCallCounter,
}

impl StatusPanelPlugin {
    pub fn new(config: StatusPanelConfig) -> Self {
        Self {
            config,
            todo_store: TodoStore::new(),
            tool_counter: ToolCallCounter::new(),
        }
    }

    /// 用默认配置创建。
    pub fn with_defaults() -> Self {
        Self::new(StatusPanelConfig::default())
    }

    /// 暴露 TodoStore 给外部（如需在其他 hook 中读取 TODO 状态）。
    pub fn todo_store(&self) -> &TodoStore {
        &self.todo_store
    }

    /// 暴露 ToolCallCounter 给外部。
    pub fn tool_counter(&self) -> &ToolCallCounter {
        &self.tool_counter
    }

    /// 暴露配置。
    pub fn config(&self) -> &StatusPanelConfig {
        &self.config
    }
}

impl Plugin for StatusPanelPlugin {
    fn name(&self) -> &str {
        "status-panel"
    }

    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        if self.config.enable_todo {
            tools.push(Arc::new(TodoWriteTool::new(self.todo_store.clone())));
        }
    }

    fn register_hooks(&self, hooks: &mut HarnessHooks) {
        // 收集器
        let mut collectors: Vec<Box<dyn StatusCollector>> = Vec::new();
        if self.config.enable_todo {
            collectors.push(Box::new(TodoCollector));
        }
        if self.config.enable_environment {
            let env = match &self.config.working_dir {
                Some(dir) => EnvironmentCollector::with_working_dir(dir.clone()),
                None => EnvironmentCollector::new(),
            };
            collectors.push(Box::new(env));
        }
        if self.config.enable_tool_counter {
            collectors.push(Box::new(ToolCallCounterCollector));
        }

        let side_channel = if self.config.enable_side_channel {
            Some(SideChannelCollector::with_window(
                self.config.side_channel_window,
            ))
        } else {
            None
        };

        // StatusBarHook
        hooks.transform_context.push(Arc::new(StatusBarHook::new(
            collectors,
            side_channel,
            self.config.strategy.clone(),
            self.todo_store.clone(),
            self.tool_counter.clone(),
        )));

        // ToolCallCounterHook
        if self.config.enable_tool_counter {
            hooks
                .after_tool_call
                .push(Arc::new(ToolCallCounterHook::new(
                    self.tool_counter.clone(),
                )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status_panel::store::TodoItem;

    #[test]
    fn plugin_name() {
        let plugin = StatusPanelPlugin::with_defaults();
        assert_eq!(plugin.name(), "status-panel");
    }

    #[test]
    fn default_config_enables_core_collectors() {
        let plugin = StatusPanelPlugin::with_defaults();
        assert!(plugin.config.enable_todo);
        assert!(plugin.config.enable_environment);
        assert!(plugin.config.enable_tool_counter);
        assert!(!plugin.config.enable_side_channel);
    }

    #[test]
    fn register_tools_adds_todo_write() {
        let plugin = StatusPanelPlugin::with_defaults();
        let mut tools: Vec<Arc<dyn Tool>> = vec![];
        plugin.register_tools(&mut tools);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "todo_write");
    }

    #[test]
    fn register_tools_skips_todo_when_disabled() {
        let mut config = StatusPanelConfig::default();
        config.enable_todo = false;
        let plugin = StatusPanelPlugin::new(config);
        let mut tools: Vec<Arc<dyn Tool>> = vec![];
        plugin.register_tools(&mut tools);
        assert!(tools.is_empty());
    }

    #[test]
    fn register_hooks_adds_transform_and_counter_hooks() {
        let plugin = StatusPanelPlugin::with_defaults();
        let mut hooks = HarnessHooks::none();
        plugin.register_hooks(&mut hooks);
        assert_eq!(hooks.transform_context.len(), 1);
        assert_eq!(hooks.after_tool_call.len(), 1);
    }

    #[test]
    fn register_hooks_skips_counter_when_disabled() {
        let mut config = StatusPanelConfig::default();
        config.enable_tool_counter = false;
        let plugin = StatusPanelPlugin::new(config);
        let mut hooks = HarnessHooks::none();
        plugin.register_hooks(&mut hooks);
        assert_eq!(hooks.transform_context.len(), 1);
        assert_eq!(hooks.after_tool_call.len(), 0);
    }

    #[test]
    fn todo_store_shared_between_tool_and_hook() {
        let plugin = StatusPanelPlugin::with_defaults();
        let store = plugin.todo_store().clone();
        store.rewrite(vec![TodoItem::new("1", "shared")]);
        assert_eq!(plugin.todo_store().snapshot().len(), 1);
    }
}
