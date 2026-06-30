use std::path::PathBuf;

use futures::future::BoxFuture;
use llm_harness_types::{AgentError, CostAggregate};
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
}

/// 选择 sub-agent 使用的 provider。
pub enum ProviderSelection {
    /// 使用与父 agent 相同的 client + model。
    Inherit,
    /// 使用在 HarnessSubAgentSpawner 上注册的命名别名。
    Alias(String),
}

/// 向 sub-agent 传递父 agent 上下文的方式。
pub enum ContextMode {
    /// Sub-agent 从零开始，无父 context。
    None,
    /// 将上下文文本作为前缀注入 sub-agent 的 prompt。
    Inject(String),
}

/// 派发 sub-agent 的核心 trait —— 仅定义**同步 spawn** 语义：
/// `spawn()` 返回 `SubAgentHandle`，由调用方自行 `join()` 等待结果。
///
/// 异步投递（结果通过 completion channel 回流主 agent）和 sub↔sub peer 通信
/// 依赖 runtime 层的 `AsyncSpawnRegistry` / `PeerDirectory` 等基础设施，
/// 不在本 trait 抽象范围内——见 `HarnessSubAgentSpawner`（llm-harness-runtime）
/// 的 `spawn_async` / `spawn_async_with_peer` / `spawn_with_peer` 扩展方法。
pub trait SubAgentSpawner: Send + Sync {
    /// 创建 sub-agent 的初始配置（session、harness、tokio task），不等待执行。
    /// 实际执行在返回的 `SubAgentHandle::join()` 中等待。
    fn spawn(&self, opts: SpawnOptions) -> BoxFuture<'_, Result<SubAgentHandle, AgentError>>;
}

/// sub-agent 运行中的句柄。
#[derive(Debug)]
pub struct SubAgentHandle {
    pub(crate) join: Option<tokio::task::JoinHandle<Result<SubAgentResult, AgentError>>>,
    pub(crate) abort: CancellationToken,
}

impl SubAgentHandle {
    /// 创建一个新的 handle（仅供 `SubAgentSpawner` 实现使用）。
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
    pub async fn join(mut self) -> Result<SubAgentResult, AgentError> {
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

impl Drop for SubAgentHandle {
    fn drop(&mut self) {
        // Drop 时取消，防止孤儿 tokio task 继续消耗 LLM token。
        // abort.cancel() 发信号；但若 task 阻塞在 LLM 请求中，信号不会立即生效。
        // join.abort() 直接中止 tokio task，避免在 LLM 请求返回前泄漏。
        self.abort.cancel();
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

/// sub-agent 完成后的结果。
#[derive(Clone, Debug)]
pub struct SubAgentResult {
    /// 所有 turn 的 TextDelta 拼接的完整文本输出。
    pub output: String,
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
            output: "hi".into(),
            tool_calls_count: 1,
            session_id: "s1".into(),
            cost: CostAggregate::default(),
        };
        assert_eq!(r.output, "hi");
        // CostAggregate 未 impl PartialEq，用 Debug 字符串断言默认值。
        assert_eq!(format!("{:?}", r.cost), format!("{:?}", CostAggregate::default()));
    }
}
