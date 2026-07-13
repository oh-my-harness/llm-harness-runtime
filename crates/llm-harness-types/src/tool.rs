use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{AssistantMessage, ContentBlock, ExecutionEnv, ToolError};

/// 工具执行结果。
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// 发送给 LLM 的内容块列表。
    pub content: Vec<ContentBlock>,
    /// 不发送给 LLM 的结构化数据，用于 UI 渲染或审计日志。
    pub details: serde_json::Value,
    /// 当 batch 中所有 tool 均返回 `true` 时，agent loop 提前停止。
    pub terminate: bool,
}

/// 工具执行模式——决定 tool 在同一 batch 中的并发策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    /// 与同 batch 中的其他 `Parallel` tool 并发执行（`join_all`）。
    Parallel,
    /// 作为子组分割点——前一子组结束后才开始新子组。
    Sequential,
}

/// 工具的注册层级——区分框架内置 tool 与业务 tool。
///
/// 同名 tool 冲突时，`User` 级别的 tool 不允许覆盖 `BuiltIn`
/// 级别的保留名；内置 tool 之间的同名冲突由编译期检查保证不会发生
/// （见 [`BUILTIN_TOOL_NAMES`] 的 const 断言）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ToolLevel {
    /// 框架内置 tool（如 `final_answer`、`spawn_agent` 等），由框架注入。
    BuiltIn,
    /// 业务层 tool，用户代码注册。
    User,
}

/// 内置 tool 名字常量。所有内置 tool 的 `name()` / `assert_builtin_registered!`
/// 必须引用这些常量，避免多处硬编码字符串不一致。
pub const FINAL_ANSWER_TOOL_NAME: &str = "final_answer";
pub const WAIT_FOR_EXTERNAL_EVENT_TOOL_NAME: &str = "wait_for_external_event";
pub const SPAWN_AGENT_TOOL_NAME: &str = "spawn_agent";
pub const SUBMIT_STEP_RESULT_TOOL_NAME: &str = "submit_step_result";
pub const SUBMIT_WORKFLOW_TOOL_NAME: &str = "submit_workflow";
pub const SKILL_READ_TOOL_NAME: &str = "skill_read";

// Main↔subagent communication tools
pub const MESSAGE_SUBAGENT_TOOL_NAME: &str = "message_subagent";
pub const AWAIT_SUBAGENT_REPLY_TOOL_NAME: &str = "await_subagent_reply";
pub const QUERY_SUBAGENT_TOOL_NAME: &str = "query_subagent";
pub const ABORT_SUBAGENT_TOOL_NAME: &str = "abort_subagent";
pub const MESSAGE_MAIN_TOOL_NAME: &str = "message_main";
pub const AWAIT_MAIN_MESSAGE_TOOL_NAME: &str = "await_main_message";

pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    FINAL_ANSWER_TOOL_NAME,
    WAIT_FOR_EXTERNAL_EVENT_TOOL_NAME,
    SPAWN_AGENT_TOOL_NAME,
    SUBMIT_STEP_RESULT_TOOL_NAME,
    SUBMIT_WORKFLOW_TOOL_NAME,
    SKILL_READ_TOOL_NAME,
    MESSAGE_SUBAGENT_TOOL_NAME,
    AWAIT_SUBAGENT_REPLY_TOOL_NAME,
    QUERY_SUBAGENT_TOOL_NAME,
    ABORT_SUBAGENT_TOOL_NAME,
    MESSAGE_MAIN_TOOL_NAME,
    AWAIT_MAIN_MESSAGE_TOOL_NAME,
];

/// 编译期断言：`BUILTIN_TOOL_NAMES` 内无重复项。重复则编译失败。
const _: () = {
    builtin_names::__assert_no_duplicates(BUILTIN_TOOL_NAMES);
};

/// 编译期校验：`level() == BuiltIn` 的 tool 必须在 `BUILTIN_TOOL_NAMES` 中登记其名字。
///
/// 用法：在每个 `BuiltIn` 级别 tool 的 `impl Tool` 块之后调用
/// `assert_builtin_registered!(ToolType => "tool_name")`。若名字未登记则编译失败。
#[macro_export]
macro_rules! assert_builtin_registered {
    ($ty:ty => $name:expr) => {
        const _: () = {
            // 引用类型以触发编译期关联校验：确保该 tool 类型确实存在。
            const _TY: fn() = || {
                let _: ::core::marker::PhantomData<$ty>;
            };
            $crate::builtin_names::__check_registered($name);
        };
    };
}

/// 查询某个 tool 名字是否为框架内置保留名。
pub fn is_builtin_tool_name(name: &str) -> bool {
    BUILTIN_TOOL_NAMES.contains(&name)
}

/// 编译期辅助模块：供 [`assert_builtin_registered!`] 宏调用。
#[doc(hidden)]
pub mod builtin_names {
    use super::BUILTIN_TOOL_NAMES;

    const fn str_eq(a: &str, b: &str) -> bool {
        let a_bytes = a.as_bytes();
        let b_bytes = b.as_bytes();
        if a_bytes.len() != b_bytes.len() {
            return false;
        }
        let mut k = 0;
        while k < a_bytes.len() {
            if a_bytes[k] != b_bytes[k] {
                return false;
            }
            k += 1;
        }
        true
    }

    /// 编译期校验：`name` 必须出现在 `BUILTIN_TOOL_NAMES` 中。
    pub const fn __check_registered(name: &str) {
        let mut i = 0;
        let mut found = false;
        while i < BUILTIN_TOOL_NAMES.len() {
            if str_eq(BUILTIN_TOOL_NAMES[i], name) {
                found = true;
            }
            i += 1;
        }
        assert!(
            found,
            "BuiltIn tool name not registered in BUILTIN_TOOL_NAMES"
        );
    }

    /// 编译期断言：列表内无重复项。
    pub const fn __assert_no_duplicates(names: &[&str]) {
        let mut i = 0;
        while i < names.len() {
            let mut j = i + 1;
            while j < names.len() {
                assert!(!str_eq(names[i], names[j]), "duplicate builtin tool name");
                j += 1;
            }
            i += 1;
        }
    }
}

/// Check tool name conflicts in a batch of tools.
///
/// Rules (any violation returns `Err`):
/// - A `User`-level tool whose name is in [`BUILTIN_TOOL_NAMES`] is rejected
///   (business tool using a framework-reserved name), even in isolation.
/// - Any two tools sharing a name is a conflict:
///   - `User` vs `BuiltIn` → "reserved by the framework"
///   - `User` vs `User` → "duplicate tool name"
///   - `BuiltIn` vs `BuiltIn` → "duplicate builtin tool name"
pub fn check_tool_name_conflicts(tools: &[Arc<dyn Tool>]) -> Result<(), String> {
    use std::collections::HashMap;
    let mut seen: HashMap<String, ToolLevel> = HashMap::new();
    for tool in tools {
        let name = tool.name();
        let level = tool.level();
        if name.trim().is_empty() {
            return Err("tool name must not be empty".into());
        }
        // User tool using a framework-reserved name — reject even if no
        // BuiltIn tool with that name is present in this batch.
        if level == ToolLevel::User && is_builtin_tool_name(name) {
            return Err(format!(
                "tool name '{}' is reserved by the framework; \
                 business tools must not use this name",
                name
            ));
        }
        if let Some(prev_level) = seen.get(name) {
            let kind = match (level, *prev_level) {
                (ToolLevel::User, ToolLevel::User) => "duplicate tool name",
                (ToolLevel::BuiltIn, ToolLevel::BuiltIn) => "duplicate builtin tool name",
                _ => "tool name reserved by the framework",
            };
            return Err(format!(
                "{kind}: '{}' (level {:?} vs {:?})",
                name, prev_level, level
            ));
        } else {
            seen.insert(name.to_string(), level);
        }
    }
    Ok(())
}

/// Tool 执行上下文，每次调用时由 loop 构造后传入。
pub struct ToolContext {
    /// 执行环境（文件系统 + shell）。
    pub env: Arc<dyn ExecutionEnv>,
    /// 用户取消信号。
    pub abort: CancellationToken,
    /// 当前 tool call 在 LLM 输出中的唯一 ID。
    pub tool_use_id: String,
    /// 当前轮次索引（从 0 开始）。
    pub turn_index: u32,
    /// 触发本次 tool call 的完整 LLM 响应；同一消息的多个 tool call 共享同一 Arc。
    pub assistant_message: Arc<AssistantMessage>,
    /// 长时间运行的 tool 通过此 channel 推送部分结果；接收端转发为 `AgentEvent::ToolExecutionUpdate`。
    pub update_tx: mpsc::Sender<ToolResult>,
}

/// 工具 trait——框架调用工具的唯一接口。
pub trait Tool: Send + Sync {
    /// 工具的稳定程序标识符；在 session log 和 LLM tool definition 中使用。
    fn name(&self) -> &str;

    /// 工具注册层级；默认 `User`。框架内置 tool override 为 `BuiltIn`。
    fn level(&self) -> ToolLevel {
        ToolLevel::User
    }

    /// 工具的人类可读 UI 显示名；默认回退到 `name()`。
    fn label(&self) -> &str {
        self.name()
    }

    /// 工具功能的自然语言描述，用于 LLM tool definition。
    fn description(&self) -> &str;

    /// 工具参数的 JSON Schema。
    fn parameters_schema(&self) -> &serde_json::Value;

    /// 工具在同一 batch 中的执行模式；默认 `Parallel`。
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    /// 在 schema 校验前对 LLM 原始参数做兼容转换；默认 identity（不转换）。
    fn prepare_arguments(&self, raw: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        Ok(raw)
    }

    /// 执行工具；返回结果或错误。
    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContentBlock;

    #[test]
    fn tool_result_defaults_terminate_false() {
        let r = ToolResult {
            content: vec![ContentBlock::Text {
                text: "done".into(),
            }],
            details: serde_json::Value::Null,
            terminate: false,
        };
        assert!(!r.terminate);
    }

    #[test]
    fn tool_execution_mode_default_is_parallel() {
        struct MyTool;
        impl Tool for MyTool {
            fn name(&self) -> &str {
                "my_tool"
            }
            fn description(&self) -> &str {
                "does stuff"
            }
            fn parameters_schema(&self) -> &serde_json::Value {
                &serde_json::Value::Null
            }
            fn execute<'a>(
                &'a self,
                _args: serde_json::Value,
                _ctx: &'a ToolContext,
            ) -> BoxFuture<'a, Result<ToolResult, crate::ToolError>> {
                Box::pin(async {
                    Ok(ToolResult {
                        content: vec![],
                        details: serde_json::Value::Null,
                        terminate: false,
                    })
                })
            }
        }
        let t = MyTool;
        assert!(matches!(t.execution_mode(), ToolExecutionMode::Parallel));
        assert_eq!(t.label(), "my_tool");
    }

    #[test]
    fn tool_is_object_safe() {
        fn _accepts(_: &dyn Tool) {}
    }

    #[test]
    fn builtin_tool_names_no_duplicates() {
        // The const assertion already guarantees this at compile time,
        // but check at runtime too for clarity.
        let mut seen = std::collections::HashSet::new();
        for name in BUILTIN_TOOL_NAMES {
            assert!(seen.insert(*name), "duplicate builtin tool name: {name}");
        }
    }

    #[test]
    fn is_builtin_tool_name_recognizes_known_names() {
        assert!(is_builtin_tool_name("final_answer"));
        assert!(is_builtin_tool_name("spawn_agent"));
        assert!(!is_builtin_tool_name("my_business_tool"));
        assert!(!is_builtin_tool_name(""));
    }

    #[test]
    fn tool_level_default_is_user() {
        struct MyTool;
        impl Tool for MyTool {
            fn name(&self) -> &str {
                "my"
            }
            fn description(&self) -> &str {
                "d"
            }
            fn parameters_schema(&self) -> &serde_json::Value {
                &serde_json::Value::Null
            }
            fn execute<'a>(
                &'a self,
                _: serde_json::Value,
                _: &'a ToolContext,
            ) -> BoxFuture<'a, Result<ToolResult, crate::ToolError>> {
                Box::pin(async {
                    Ok(ToolResult {
                        content: vec![],
                        details: serde_json::Value::Null,
                        terminate: false,
                    })
                })
            }
        }
        assert_eq!(MyTool.level(), ToolLevel::User);
    }
}
