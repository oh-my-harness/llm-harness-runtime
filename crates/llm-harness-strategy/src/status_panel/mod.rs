//! Agent 状态栏 Plugin —— 对应阶段三 #4/#5。
//!
//! 提供 `<agent_status>` XML 注入（TransformContextHook）、TODO write 工具、
//! 工具调用计数器、环境状态收集器。通过 `Plugin` trait 一行接入。

mod collectors;
mod counter_hook;
mod format;
mod hook;
mod store;
mod todo_tool;

pub use collectors::{
    CollectorCtx, EnvironmentCollector, SideChannelCollector, StatusCollector, TodoCollector,
    ToolCallCounter, ToolCallCounterCollector,
};
pub use counter_hook::ToolCallCounterHook;
pub use hook::StatusBarHook;
pub use store::{TodoItem, TodoStatus, TodoStore};
pub use todo_tool::TodoWriteTool;

pub use self::plugin::{StatusPanelConfig, StatusPanelPlugin};
pub use hook::UpdateStrategy;

mod plugin;
