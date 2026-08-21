use std::path::PathBuf;

use llm_harness_types::{AgentError, CostAggregate, ThinkingLevel};
use tokio_util::sync::CancellationToken;

/// 启动 sub-agent 的完整配置。
pub struct SpawnOptions {
    /// 派发给 sub-agent 的初始 prompt。
    pub prompt: String,
    /// 使用哪个 provider；Inherit 表示继承父 agent。
    pub provider: ProviderSelection,
    /// 工作目录；None 继承父 agent 的 cwd。
    pub cwd: Option<PathBuf>,
    /// 向 sub-agent 传递父 agent 上下文的方式。
    pub context: ContextMode,
    /// 覆盖子 Agent 的 system prompt。None = 继承 builder_customize 全局默认。
    pub system_prompt: Option<String>,
    /// 覆盖子 Agent 的 thinking level。None = 继承全局默认。
    /// 工程配置，不暴露给 LLM。
    pub thinking_level: Option<ThinkingLevel>,
    /// 机器可读的角色标签（如 "code-reviewer"）。纯元数据，不影响行为控制流。
    /// None = 未设置（主 agent 无 role，不继承）。
    pub role: Option<String>,
    /// 人类可读的职责描述。纯元数据，不影响行为控制流。
    /// None = 未设置（主 agent 无 description，不继承）。
    pub description: Option<String>,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
            system_prompt: None,
            thinking_level: None,
            role: None,
            description: None,
        }
    }
}

/// 选择 sub-agent 使用的 provider。
pub enum ProviderSelection {
    /// 使用与父 agent 相同的 client + model。
    Inherit,
    /// 使用在 HarnessSubAgentSpawner 上注册的命名别名。
    Alias(String),
}

/// 向 sub-agent 传递父 agent 上下文的方式。
/// 上下文块：带来源标签的内容片段。
#[derive(Debug, Clone)]
pub struct ContextBlock {
    /// 内容来源。File 变体触发文件读取，其他变体直接使用 content。
    pub source: ContextSource,
    /// 上下文内容。File 变体时此字段被忽略（由 spawner 从 path 读取）。
    pub content: String,
}

/// 上下文来源类型。
#[derive(Debug, Clone)]
pub enum ContextSource {
    /// 父 Agent 的对话摘要或历史片段。content 由调用方提供。
    ParentConversation,
    /// 文件内容。spawner 在渲染时读取 path 指向的文件，content 字段被忽略。
    File(PathBuf),
    /// 用户额外指令。content 由调用方提供。
    UserInstruction,
    /// 自定义来源。content 由调用方提供。
    Custom(String),
}

/// 向 sub-agent 传递父 agent 上下文的方式。
pub enum ContextMode {
    /// Sub-agent 从零开始，无父 context。
    None,
    /// 将结构化上下文块注入 sub-agent 的 prompt。
    Inject(Vec<ContextBlock>),
}

/// sub-agent 运行中的句柄。
#[derive(Debug)]
pub struct SubAgentHandle {
    pub(crate) join: Option<tokio::task::JoinHandle<Result<SubAgentResult, AgentError>>>,
    pub(crate) abort: CancellationToken,
}
impl SubAgentHandle {
    /// 创建一个新的 handle（仅供 `HarnessSubAgentSpawner` 使用）。
    pub fn new(
        join: tokio::task::JoinHandle<Result<SubAgentResult, AgentError>>,
        abort: CancellationToken,
    ) -> Self {
        Self {
            join: Some(join),
            abort,
        }
    }

    /// 返回 abort token 的克隆，用于外部取消（不 move self）。
    pub fn abort_token(&self) -> CancellationToken {
        self.abort.clone()
    }

    /// 取消 sub-agent（发 abort 信号，不阻塞）。
    pub fn abort(&self) {
        self.abort.cancel();
    }

    /// 等待 sub-agent 完成并拿到结果。
    pub(crate) async fn join(mut self) -> Result<SubAgentResult, AgentError> {
        let join = self
            .join
            .take()
            .unwrap_or_else(|| unreachable!("join consumed once"));
        match join.await {
            Ok(result) => result,
            Err(e) if e.is_cancelled() => Err(AgentError::Aborted),
            Err(e) => Err(AgentError::Internal(format!(
                "sub-agent task panicked: {e}"
            ))),
        }
    }
}

/// sub-agent 完成后的结果。
#[derive(Clone, Debug)]
pub struct SubAgentResult {
    /// Sub-agent 的最终答案。优先取最后一个 `kind=FinalAnswer` 的 assistant
    /// message 的文本；若 sub-agent 全程未产生 FinalAnswer 消息（如 abort、
    /// `FinalAnswerMode::required_tool()` strict 模式下模型未调用 final-answer
    /// tool、或 broadcast Lagged 丢失尾部事件等退化路径），回退到 TextDelta 拼接。
    pub final_answer: String,
    /// 本次执行共调用了几次工具。
    pub tool_calls_count: u32,
    /// Sub-agent 的 session ID（供调试和审计）。
    pub session_id: String,
    /// 本次执行的 cost（由 spawn_inner 调 harness.usage() 填充）。
    pub cost: CostAggregate,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_agent_result_has_cost_field() {
        let r = SubAgentResult {
            final_answer: "hi".into(),
            tool_calls_count: 1,
            session_id: "s1".into(),
            cost: CostAggregate::default(),
        };
        assert_eq!(r.final_answer, "hi");
        // CostAggregate 未 impl PartialEq，用 Debug 字符串断言默认值。
        assert_eq!(
            format!("{:?}", r.cost),
            format!("{:?}", CostAggregate::default())
        );
    }

    #[test]
    fn context_block_source_variants() {
        let b1 = ContextBlock {
            source: ContextSource::ParentConversation,
            content: "prev chat".into(),
        };
        let b2 = ContextBlock {
            source: ContextSource::File(PathBuf::from("/src/main.rs")),
            content: String::new(),
        };
        let b3 = ContextBlock {
            source: ContextSource::UserInstruction,
            content: "be careful".into(),
        };
        let b4 = ContextBlock {
            source: ContextSource::Custom("task_constraints".into()),
            content: "must use async".into(),
        };
        assert!(matches!(b1.source, ContextSource::ParentConversation));
        assert!(matches!(b2.source, ContextSource::File(_)));
        assert!(matches!(b3.source, ContextSource::UserInstruction));
        assert!(matches!(b4.source, ContextSource::Custom(_)));
    }

    #[test]
    fn context_mode_inject_holds_blocks() {
        let mode = ContextMode::Inject(vec![ContextBlock {
            source: ContextSource::ParentConversation,
            content: "chat".into(),
        }]);
        assert!(matches!(mode, ContextMode::Inject(_)));
    }

    #[test]
    fn spawn_options_has_per_spawn_fields() {
        let opts = SpawnOptions {
            prompt: "do task".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
            system_prompt: Some("You are a reviewer.".into()),
            thinking_level: Some(ThinkingLevel::High),
            role: Some("code-reviewer".into()),
            description: Some("Review src/auth.rs".into()),
        };
        assert_eq!(opts.system_prompt.as_deref(), Some("You are a reviewer."));
        assert_eq!(opts.role.as_deref(), Some("code-reviewer"));
        assert_eq!(opts.description.as_deref(), Some("Review src/auth.rs"));
    }
}
