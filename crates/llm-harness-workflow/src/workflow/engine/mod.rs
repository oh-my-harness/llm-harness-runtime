mod config;
mod event;
mod json_parse;
mod prompt_context;
mod request;
mod runner;
mod structured;

#[cfg(test)]
mod tests;

pub use config::{BuilderCustomize, WorkflowEngineConfig};
use event::{EVENT_CHANNEL_CAPACITY, step_progress_from};
pub use event::{StepProgress, WorkflowEvent};
pub use request::WorkflowRunRequest;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use llm_harness_agent::{AgentHarnessEvent, HarnessHooks, Plugin, Session};
use llm_harness_types::{AgentError, AgentEvent, RunRequest, SPAWN_AGENT_TOOL_NAME, Tool};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::lifecycle::task::{Checkpoint, TaskError, TaskId, TaskResult};
use crate::lifecycle::task_store::TaskStore;
use crate::workflow::error::WorkflowError;
use crate::workflow::executor::{
    ExecutorCancellationPolicy, ExecutorCtx, ExecutorExecutionId, StepExecutor, builtin_executors,
};
use crate::workflow::judge::{EdgeConditionJudge, NoopJudge, StepCtx, StepTransitionJudge};
use crate::workflow::model::{
    EdgeCondition, Step, StepId, StepRecord, StepResult, StructuredStatus, Transition, Workflow,
    WorkflowContext, WorkflowState, WorkflowStatus,
};
use crate::workflow::plan::validate_workflow;
use llm_harness_agent::{HarnessBuildError, HarnessBuilder};
use llm_harness_subagents::spawner::HarnessSubAgentSpawner;
use llm_harness_types::CostAggregate;

/// 空实现 Plugin：build_step_spawner 的 sub-agent plugin factory 占位。
/// 流程场景下 sub-agent 不需要额外 plugin（工具集由 step 的 allowed_tools 控制）。
struct NoopPlugin;
impl Plugin for NoopPlugin {
    fn name(&self) -> &str {
        "workflow-noop"
    }
}
use prompt_context::{format_context_vars, format_step_history_summary};

struct PersistenceRequest {
    store: Arc<dyn TaskStore>,
    task_id: TaskId,
    state: WorkflowState,
    completion: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    /// Bounded admission for this persistence operation. Held by the worker
    /// until the backend save finishes, then released.
    permit: tokio::sync::OwnedSemaphorePermit,
}

struct PersistenceReceipt {
    completion: tokio::sync::oneshot::Receiver<anyhow::Result<()>>,
}

#[cfg(test)]
use runner::aggregate_cost;

/// 默认步骤数上限。step_history.len() 超限 → Failed。
///
/// 500 覆盖典型 agentic 循环：max_retries(5) × max_attempts(3) = 15 条/逻辑步，
/// 约够 30+ 个带完整重试的步骤。可通过 `with_max_steps` 调整。
const DEFAULT_MAX_STEPS: usize = 500;

/// 默认 Retry 上限。连续 Retry 超限 → Failed。
/// 语义：允许的连续 Retry 次数（不含原始执行）。max_retries=5 时第 6 次触发 Failed。
///
/// 注意：与 StepExecutionPolicy.max_attempts 独立。最坏情况下单步总执行次数
/// = max_retries × max_attempts（judge Retry 触发完整 policy retry 循环）。
/// max_steps 是最终兜底。如需更严格的联合上限，在业务层自行限制。
const DEFAULT_MAX_RETRIES: usize = 5;

/// Maximum time one engine call waits for a workflow-state persistence
/// operation. The owned operation keeps its ordering position after this
/// deadline so a late older save cannot overtake a newer snapshot.
const DEFAULT_PERSISTENCE_TIMEOUT: Duration = Duration::from_secs(30);

/// Fixed number of workflow-state persistence operations that may be active
/// plus queued at once (one permit per request, held until the backend save
/// finishes). Bounds the full WorkflowState snapshots the lane retains.
const DEFAULT_PERSISTENCE_CAPACITY: usize = 2;

fn default_declarative_judge(
    workflow: &Workflow,
    judge: Arc<dyn StepTransitionJudge>,
) -> Arc<dyn StepTransitionJudge> {
    // Activate the built-in EdgeConditionJudge when the workflow has
    // declarative edges (Expr conditions) OR label-based edges (string
    // conditions like "pass"/"fail"/"success"). The latter enables
    // route_key-based routing with fallback semantics, which is the generic
    // equivalent of EDA Agent's resolve_next().
    let has_declarative_edges = workflow.edges.iter().any(|edge| {
        matches!(
            &edge.condition,
            Some(EdgeCondition::Expr(_)) | Some(EdgeCondition::Label(_))
        )
    });
    if has_declarative_edges && judge.is_noop() {
        Arc::new(EdgeConditionJudge::from_workflow(workflow))
    } else {
        judge
    }
}

// ── WorkflowEngine ──────────────────────────────────────────────────────────

/// 唯一执行引擎。自由 task = 单步 workflow。
///
/// 与 `HarnessSubAgentSpawner` 同级（都在 runtime 层）。承担两职责：
/// (1) workflow 步骤推进（每步直接用 `HarnessBuilder` 构造 `AgentHarness`，
///     step 内 fan-out 走 `build_step_spawner` 构造的局部 spawner）；
/// (2) task 生命周期管理。
pub struct WorkflowEngine {
    workflow: Workflow,
    config: WorkflowEngineConfig,
    judge: Arc<dyn StepTransitionJudge>,
    /// step_id → plugin 工厂闭包。引擎每步据此选 plugin。
    step_plugin_factories: HashMap<StepId, Box<dyn Fn() -> Box<dyn Plugin> + Send + Sync>>,
    /// step_id → builder 定制闭包。引擎每步据此定制 builder（per-step 层）。
    step_builder_customizers: HashMap<StepId, BuilderCustomize>,
    /// executor_name → deterministic non-LLM step executor.
    step_executors: HashMap<String, Arc<dyn StepExecutor>>,
    /// 缓存：step_id → allowed_tools。构造时从 workflow 提取，避免每步重建。
    step_tools_cache: HashMap<StepId, Vec<String>>,
    /// 额外 tool（对 new/new_free_task 路径）。run() 内部构造每步 harness 时注册。
    extra_tools: Vec<Arc<dyn Tool>>,
    task_store: Option<Arc<dyn TaskStore>>,
    /// FIFO lane for workflow-state commits. Requests stay owned by the lane
    /// after a caller timeout, so older saves cannot finish after newer ones.
    persistence_tx:
        std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<PersistenceRequest>>>,
    /// Maximum caller wait for persistence; the owned save may finish later.
    persistence_timeout: Duration,
    /// Fixed-capacity admission for workflow-state persistence. One permit
    /// travels with each PersistenceRequest; the worker holds it until the
    /// backend save finishes, so capacity bounds active + queued saves and
    /// the retained full WorkflowState snapshots they carry.
    persistence_permits: Arc<tokio::sync::Semaphore>,
    /// Test-only gate after the run loop's provisional cancellation check.
    #[cfg(test)]
    abort_finalization_gate: Option<Arc<(tokio::sync::Notify, tokio::sync::Notify)>>,
    /// Test-only gate inside run_loop's To continuation branch, after the
    /// provisional cancellation check and before persistence admission.
    #[cfg(test)]
    to_continuation_gate: Option<Arc<(tokio::sync::Notify, tokio::sync::Notify)>>,
    /// Test-only gate inside run_loop's Retry-with-target continuation branch
    /// (LoopConfig jump), before persistence admission.
    #[cfg(test)]
    retry_continuation_gate: Option<Arc<(tokio::sync::Notify, tokio::sync::Notify)>>,
    /// Test-only gate inside pause_if_requested, after the pause flag check
    /// and before persistence admission.
    #[cfg(test)]
    pause_admission_gate: Option<Arc<(tokio::sync::Notify, tokio::sync::Notify)>>,
    /// Test-only gate before fail_task persistence admission.
    #[cfg(test)]
    fail_task_admission_gate: Option<Arc<(tokio::sync::Notify, tokio::sync::Notify)>>,
    /// Test-only override for fail_task's persistence deadline. The normal
    /// startup save keeps the production timeout even when a test needs to
    /// exercise fail_task's degraded admission path quickly.
    #[cfg(test)]
    fail_task_persistence_timeout: Option<Duration>,
    /// task 标识。由引擎内部生成（uuid）。
    task_id: TaskId,
    /// workflow 状态（步骤推进 + 生命周期状态，统一表达）。
    /// Arc<Mutex<...>>：gate hook 需共享同一份 state。
    workflow_state: Arc<Mutex<WorkflowState>>,
    /// 共享可变上下文。executor 写入、judge/LLM step 读取。
    /// 持久化时 snapshot 合并进 workflow_state.context。
    context: Arc<Mutex<WorkflowContext>>,
    /// task 级 checkpoint 的内存缓存（仅运行时快速访问，非权威源）。
    checkpoints: Mutex<Vec<Checkpoint>>,
    /// pause 请求标志。pause() 设 true，run() 在步边界检查并消费。
    pause_requested: AtomicBool,
    /// pause 请求的原因。pause() 设值，run() 在步边界消费。用 std Mutex（pause 是同步方法）。
    pause_reason: std::sync::Mutex<Option<String>>,
    /// 当前步的 abort token（cancel() 用）。
    current_abort: Mutex<Option<CancellationToken>>,
    /// 引擎事件广播。subscribe() 返回 Receiver。
    event_tx: tokio::sync::broadcast::Sender<WorkflowEvent>,
    /// 步骤数上限，默认 `DEFAULT_MAX_STEPS`。step_history.len() 超限 → Failed。
    max_steps: usize,
    /// Retry 上限，默认 `DEFAULT_MAX_RETRIES`。连续 Retry 超限 → Failed。
    max_retries: usize,
    /// run() 重入守卫。防止并发 run() 覆盖写。
    running: AtomicBool,
    /// 规划此 workflow 的 task ID。由 with_planned_by 设置，
    /// run() 启动时写入 workflow_state。避免 blocking_lock。
    planned_by: Option<TaskId>,
}
