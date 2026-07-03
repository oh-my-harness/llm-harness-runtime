use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use futures::future::BoxFuture;
use llm_harness_agent::{
    AgentHarnessEvent, ContextMode, HarnessHooks, Plugin, ProviderSelection, Session, SpawnOptions,
    SubAgentResult, sub_agent::SubAgentSpawner,
};
use llm_harness_loop::LlmClient;
use llm_harness_types::{
    AgentError, AgentEvent, ContentBlock, Tool, ToolContext, ToolError, ToolExecutionMode,
    ToolResult,
};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::builder::{HarnessBuildError, HarnessBuilder};
use crate::cost::CostAggregate;
use crate::error::Result;
use crate::spawner::{EnvFactory, HarnessSubAgentSpawner, SessionFactory};
use crate::task::{Checkpoint, TaskError, TaskId, TaskResult};
use crate::task_store::TaskStore;
use crate::workflow::executor::{ExecutorCtx, StepExecutor, builtin_executors};
use crate::workflow::judge::{EdgeConditionJudge, NoopJudge, StepCtx, StepTransitionJudge};
use crate::workflow::model::{
    EdgeCondition, Step, StepId, StepRecord, StepResult, Transition, Workflow, WorkflowContext,
    WorkflowState, WorkflowStatus,
};
use crate::workflow::plan::validate_workflow;

/// 内置 tool 名：让 step agent 提交结构化结果。
pub const SUBMIT_STEP_RESULT_TOOL_NAME: &str = "submit_step_result";

/// 内置 tool 名：同步派发 sub-agent。
pub const SYNC_SPAWN_TOOL_NAME: &str = "spawn_agent";

/// broadcast channel 容量。步骤级事件频率低，overflow 时丢弃旧事件（比阻塞 run 好）。
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// 默认步骤数上限。step_history.len() 超限 → Failed。
const DEFAULT_MAX_STEPS: usize = 100;

/// 默认 Retry 上限。连续 Retry 超限 → Failed。
/// 语义：允许的连续 Retry 次数（不含原始执行）。max_retries=5 时第 6 次触发 Failed。
const DEFAULT_MAX_RETRIES: usize = 5;

/// SubmitStepResultTool 提交的 JSON payload 大小上限（bytes）。
const SUBMIT_RESULT_MAX_BYTES: usize = 256 * 1024;

/// SyncSpawnAgentTool prompt 长度上限（bytes）。
const SYNC_SPAWN_MAX_PROMPT_BYTES: usize = 1024 * 1024;

fn default_declarative_judge(
    workflow: &Workflow,
    judge: Arc<dyn StepTransitionJudge>,
) -> Arc<dyn StepTransitionJudge> {
    let has_declarative_edges = workflow
        .edges
        .iter()
        .any(|edge| matches!(edge.condition, Some(EdgeCondition::Expr(_))));
    if has_declarative_edges && judge.is_noop() {
        Arc::new(EdgeConditionJudge::from_workflow(workflow))
    } else {
        judge
    }
}

// ── SyncSpawnAgentTool ─────────────────────────────────────────────────────

/// 流程场景专用的同步 spawn tool。走 `spawner.spawn()`（非 `spawn_async`）。
///
/// cancel 传播（方案 A：tool 内 `select!`）：
/// `step_abort.cancelled()` → `handle.abort()` → 返回 `Err(ToolError::Aborted)`。
///
/// schema 不含 `wait`；传 `wait` 任意值被忽略。
pub struct SyncSpawnAgentTool {
    spawner: Arc<HarnessSubAgentSpawner>,
    /// step 级 abort token。引擎构造 tool 时注入，与注入到 step harness 的 abort token 同一个 clone。
    step_abort: CancellationToken,
}

impl SyncSpawnAgentTool {
    pub fn new(spawner: Arc<HarnessSubAgentSpawner>, step_abort: CancellationToken) -> Self {
        Self {
            spawner,
            step_abort,
        }
    }
}

fn sync_spawn_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "prompt": { "type": "string", "description": "给 sub-agent 的任务指令" },
            "agent_id": { "type": "string", "description": "sub-agent 标识 ID" },
            "context": { "type": "string", "description": "附加上下文" },
            "provider": { "type": "string", "description": "provider 别名" }
        },
        "required": ["prompt"]
    })
}

impl Tool for SyncSpawnAgentTool {
    fn name(&self) -> &str {
        SYNC_SPAWN_TOOL_NAME
    }

    fn description(&self) -> &str {
        "派发一个 sub-agent 处理子任务，同步等待结果。结果作为 tool 结果返回。"
    }

    fn parameters_schema(&self) -> &'static serde_json::Value {
        static SYNC_SPAWN_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(sync_spawn_schema);
        &SYNC_SPAWN_SCHEMA
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, std::result::Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let prompt = args
                .get("prompt")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::Execution("missing prompt".into()))?
                .to_string();
            if prompt.len() > SYNC_SPAWN_MAX_PROMPT_BYTES {
                return Err(ToolError::Execution(format!(
                    "spawn_agent: prompt exceeds {} bytes",
                    SYNC_SPAWN_MAX_PROMPT_BYTES
                )));
            }
            let context = args
                .get("context")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| ContextMode::Inject(s.to_string()))
                .unwrap_or(ContextMode::None);
            let provider = args
                .get("provider")
                .and_then(|v| v.as_str())
                .map(|s| ProviderSelection::Alias(s.to_string()))
                .unwrap_or(ProviderSelection::Inherit);
            // wait 参数被忽略（schema 不含，但 LLM 可能传）
            let opts = SpawnOptions {
                prompt,
                provider,
                cwd: None,
                context,
            };
            // spawn 前检查 abort：若已 cancelled 则不浪费 spawn。
            if self.step_abort.is_cancelled() {
                return Err(ToolError::Aborted);
            }
            let handle = self
                .spawner
                .spawn(opts)
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;
            // 取出 abort token：select! 的 cancel arm 不持 handle 所有权。
            let abort_token = handle.abort_token();

            // select!：正常 join / step abort
            tokio::select! {
                biased;
                _ = self.step_abort.cancelled() => {
                    abort_token.cancel();
                    Err(ToolError::Aborted)
                }
                result = handle.join() => {
                    match result {
                        Ok(r) => Ok(ToolResult {
                            content: vec![ContentBlock::Text { text: format_sub_agent_result(&r) }],
                            details: serde_json::json!({ "agent_id": r.session_id, "is_error": false }),
                            terminate: false,
                        }),
                        Err(e) => Ok(ToolResult {
                            content: vec![ContentBlock::Text { text: format_sub_agent_failure(&e) }],
                            details: serde_json::json!({ "agent_id": "unknown", "is_error": true }),
                            terminate: false,
                        }),
                    }
                }
            }
        })
    }
}

/// 格式化 sub-agent 成功结果。
fn format_sub_agent_result(r: &SubAgentResult) -> String {
    let output = &r.output;
    let preview = if output.len() > 200 {
        // char 边界安全截取，避免 panic。
        let mut end = 200;
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &output[..end])
    } else {
        output.clone()
    };
    format!("[Sub-agent completed]\n{preview}")
}

/// 格式化 sub-agent 失败。
fn format_sub_agent_failure(e: &AgentError) -> String {
    format!("[Sub-agent failed]\n{e}")
}

// ── WorkflowEngineConfig ───────────────────────────────────────────────────

/// 打包 WorkflowEngine 构造器的共用参数，避免 new/new_free_task/restore 传 5 个散参。
pub struct WorkflowEngineConfig {
    pub client: Arc<dyn LlmClient>,
    pub model: String,
    pub env_factory: Arc<dyn EnvFactory>,
    pub session_factory: Arc<dyn SessionFactory>,
    pub session_base_dir: PathBuf,
}

// ── WorkflowEvent ──────────────────────────────────────────────────────────

/// 引擎级事件。subscribe() 返回的 Receiver 推送此类型。
/// 步骤级事件必须对外可见——长周期任务才能做监控/UI。
///
/// broadcast channel 容量由 `EVENT_CHANNEL_CAPACITY` 控制。
/// overflow 时丢弃旧事件（比阻塞 run 好）。订阅者应容忍丢事件。
#[derive(Debug, Clone)]
pub enum WorkflowEvent {
    StepStarted { step_id: StepId, step_name: String },
    StepFinished { step_id: StepId, result: StepResult },
    Paused { reason: String },
    Resumed,
    Cancelled { reason: String },
    Failed { error: String },
}

// ── SubmitStepResultTool ───────────────────────────────────────────────────

/// 内置 tool：让 step agent 提交结构化结果（JSON）。
/// 引擎在 step 收尾时把提交的 JSON 填入 StepResult.structured，
/// judge 直接读 ctx.last_result.structured，不用自己经 TaskStore 反查 checkpoint。
pub struct SubmitStepResultTool {
    /// 提交的结果经 channel 传给引擎（run_step 在收集事件时读取）。
    tx: Arc<tokio::sync::Mutex<Option<serde_json::Value>>>,
}

impl SubmitStepResultTool {
    pub fn new() -> (Self, Arc<tokio::sync::Mutex<Option<serde_json::Value>>>) {
        let tx = Arc::new(tokio::sync::Mutex::new(None));
        (Self { tx: tx.clone() }, tx)
    }
}

fn submit_step_result_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "result": { "type": "object", "description": "结构化结果（任意 JSON）" }
        },
        "required": ["result"]
    })
}

impl Tool for SubmitStepResultTool {
    fn name(&self) -> &str {
        SUBMIT_STEP_RESULT_TOOL_NAME
    }
    fn description(&self) -> &str {
        "提交本步的结构化结果（JSON）。judge 会直接读此结果做跳转决策。"
    }
    fn parameters_schema(&self) -> &'static serde_json::Value {
        static S: LazyLock<serde_json::Value> = LazyLock::new(submit_step_result_schema);
        &S
    }
    fn execute<'a>(
        &'a self,
        args: Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, std::result::Result<ToolResult, ToolError>> {
        let tx = self.tx.clone();
        Box::pin(async move {
            let result = args.get("result").cloned().unwrap_or(Value::Null);
            // 结构校验：result 必须是 object（结构化结果）。
            if !result.is_object() {
                return Err(ToolError::Execution(
                    "submit_step_result: 'result' must be a JSON object".into(),
                ));
            }
            // 大小限制：防止 LLM 注入巨大 JSON 撑爆内存/落盘。
            let serialized = serde_json::to_string(&result)
                .map_err(|e| ToolError::Execution(format!("invalid JSON: {e}")))?;
            if serialized.len() > SUBMIT_RESULT_MAX_BYTES {
                return Err(ToolError::Execution(format!(
                    "submit_step_result: payload exceeds {} bytes",
                    SUBMIT_RESULT_MAX_BYTES
                )));
            }
            // last-write-wins：多次调用后者覆盖前者。
            *tx.lock().await = Some(result.clone());
            Ok(ToolResult {
                content: vec![ContentBlock::Text {
                    text: "Step result submitted.".into(),
                }],
                details: serde_json::json!({ "submitted": true }),
                terminate: false,
            })
        })
    }
}

/// 格式化 step_history 摘要，供步骤间上下文注入。
const STEP_HISTORY_OUTPUT_PREVIEW_CHARS: usize = 1_000;
const STEP_HISTORY_STRUCTURED_PREVIEW_CHARS: usize = 4_000;

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

fn structured_preview(value: &Value) -> String {
    let serialized = serde_json::to_string(value)
        .unwrap_or_else(|err| format!("\"<failed to serialize structured result: {err}>\""));
    truncate_chars(&serialized, STEP_HISTORY_STRUCTURED_PREVIEW_CHARS)
}

fn format_step_history_summary(history: &[StepRecord]) -> String {
    history
        .iter()
        .map(|r| {
            let Some(result) = r.result.as_ref() else {
                return format!(
                    "- step '{}' → {:?}\n  output: (no result)",
                    r.step_id, r.transition
                );
            };

            let mut lines = vec![format!("- step '{}' → {:?}", r.step_id, r.transition)];
            if let Some(structured) = result.structured.as_ref() {
                lines.push(format!("  structured: {}", structured_preview(structured)));
            }
            lines.push(format!(
                "  output: {}",
                truncate_chars(&result.output, STEP_HISTORY_OUTPUT_PREVIEW_CHARS)
            ));
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 把共享 context 的 KV 格式化为 prompt 摘要。空 context 返回空字符串。
fn format_context_vars(context: &WorkflowContext) -> String {
    if context.variables.is_empty() {
        return String::new();
    }
    let mut entries: Vec<(&String, &Value)> = context.variables.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
        .iter()
        .map(|(k, v)| format!("  {k}: {v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ── NoopPlugin（build_step_spawner 的 plugin factory 占位）───────────────────

/// 空实现 Plugin：build_step_spawner 构建 step 内 spawner 时的 sub-agent plugin factory 占位。
/// 流程场景下 sub-agent 不需要额外 plugin（工具集由 step 的 allowed_tools 控制）。
struct NoopPlugin;
impl Plugin for NoopPlugin {
    fn name(&self) -> &str {
        "workflow-noop"
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
    /// executor_name → deterministic non-LLM step executor.
    step_executors: HashMap<String, Arc<dyn StepExecutor>>,
    /// 缓存：step_id → allowed_tools。构造时从 workflow 提取，避免每步重建。
    step_tools_cache: HashMap<StepId, Vec<String>>,
    /// 额外 tool（对 new/new_free_task 路径）。run() 内部构造每步 harness 时注册。
    extra_tools: Vec<Arc<dyn Tool>>,
    /// 额外 hooks（对 new/new_free_task 路径）。run_step 内部构造每步 harness 时注册。
    extra_hooks: HarnessHooks,
    task_store: Option<Arc<dyn TaskStore>>,
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

impl WorkflowEngine {
    /// 多步 workflow 构造器。task_id 由引擎内部生成。
    /// 内部调 validate_workflow 校验；若有 task_store，调 save_workflow 落盘。
    /// 校验失败 → Err（非法 plan 不进引擎）。
    pub fn new(
        workflow: Workflow,
        config: WorkflowEngineConfig,
        judge: Arc<dyn StepTransitionJudge>,
    ) -> Result<Self> {
        validate_workflow(&workflow)?;
        let judge = default_declarative_judge(&workflow, judge);
        Ok(Self::from_parts(workflow, config, judge, None))
    }

    /// 自由 task 构造器 = 单步 workflow。
    /// 1 个 step（id="run"，prompt 作为 step.prompt）、无 edges、allowed_tools 由业务层显式传入。
    /// judge 用 NoopJudge（流程走完即结束）。
    /// prompt 直接写入 Step.prompt 字段。
    ///
    /// 返回 Result：allowed_tools 含空字符串等校验失败时返回 Err 而非 panic。
    pub fn new_free_task(
        prompt: String,
        allowed_tools: Vec<String>,
        config: WorkflowEngineConfig,
    ) -> Result<Self> {
        let workflow = Workflow {
            entry_step: "run".into(),
            steps: vec![Step::llm("run", "run", prompt, allowed_tools)],
            edges: vec![],
        };
        Self::new(workflow, config, Arc::new(NoopJudge))
    }

    /// 从 TaskStore 恢复引擎。workflow 也从 store load。
    /// store 是信任边界外数据源，恢复后同样过 validate_workflow 校验。
    pub async fn restore(
        store: Arc<dyn TaskStore>,
        task_id: TaskId,
        config: WorkflowEngineConfig,
        judge: Arc<dyn StepTransitionJudge>,
    ) -> Result<Self> {
        let workflow = store.load_workflow(&task_id).await?.ok_or_else(|| {
            anyhow::anyhow!(
                "restore: workflow not found in store for task {}",
                task_id.0
            )
        })?;
        // 信任边界外数据：恢复后同样校验。
        validate_workflow(&workflow)?;
        let judge = default_declarative_judge(&workflow, judge);
        let saved_state = store.load_workflow_state(&task_id).await?;
        let state = saved_state.unwrap_or_else(|| WorkflowState::new(workflow.entry_step.clone()));
        let engine = Self::from_parts_with_state(
            workflow,
            config,
            judge,
            Some(store.clone()),
            task_id,
            state,
        );
        // 从 store 重载 checkpoints 到内存缓存。
        if let Ok(cps) = store.list_checkpoints(&engine.task_id).await {
            *engine.checkpoints.lock().await = cps;
        }
        Ok(engine)
    }

    /// 公共构造：从已校验的 workflow + config 组装 engine（new 路径，新 task_id）。
    fn from_parts(
        workflow: Workflow,
        config: WorkflowEngineConfig,
        judge: Arc<dyn StepTransitionJudge>,
        task_store: Option<Arc<dyn TaskStore>>,
    ) -> Self {
        let task_id = TaskId(format!("task-{}", uuid::Uuid::new_v4()));
        let state = WorkflowState::new(workflow.entry_step.clone());
        Self::from_parts_with_state(workflow, config, judge, task_store, task_id, state)
    }

    /// 公共构造：从已校验的 workflow + config + 指定 state 组装 engine。
    /// 消除 new/restore 的字段初始化重复。
    fn from_parts_with_state(
        workflow: Workflow,
        config: WorkflowEngineConfig,
        judge: Arc<dyn StepTransitionJudge>,
        task_store: Option<Arc<dyn TaskStore>>,
        task_id: TaskId,
        state: WorkflowState,
    ) -> Self {
        let (event_tx, _) = tokio::sync::broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let context = Arc::new(Mutex::new(state.context.clone()));
        let step_tools_cache: HashMap<StepId, Vec<String>> = workflow
            .steps
            .iter()
            .map(|s| (s.id().clone(), s.allowed_tools().to_vec()))
            .collect();
        Self {
            workflow,
            config,
            judge,
            step_plugin_factories: HashMap::new(),
            step_executors: builtin_executors()
                .map(|(name, executor)| (name.to_string(), executor))
                .collect(),
            step_tools_cache,
            extra_tools: vec![],
            extra_hooks: HarnessHooks::none(),
            task_store,
            task_id,
            workflow_state: Arc::new(Mutex::new(state)),
            context,
            checkpoints: Mutex::new(vec![]),
            pause_requested: AtomicBool::new(false),
            pause_reason: std::sync::Mutex::new(None),
            current_abort: Mutex::new(None),
            event_tx,
            max_steps: DEFAULT_MAX_STEPS,
            max_retries: DEFAULT_MAX_RETRIES,
            running: AtomicBool::new(false),
            planned_by: None,
        }
    }

    pub fn task_id(&self) -> &TaskId {
        &self.task_id
    }

    #[must_use]
    pub fn with_tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.extra_tools.push(tool);
        self
    }

    /// 注入额外 hooks。每步构造 harness 时，这些 hooks 会经 HookPlugin 桥接到 builder。
    #[must_use]
    pub fn with_hooks(mut self, hooks: HarnessHooks) -> Self {
        self.extra_hooks = hooks;
        self
    }

    #[must_use]
    pub fn with_step_plugin(
        mut self,
        step_id: impl Into<String>,
        factory: impl Fn() -> Box<dyn Plugin> + Send + Sync + 'static,
    ) -> Self {
        self.step_plugin_factories
            .insert(step_id.into(), Box::new(factory));
        self
    }

    #[must_use]
    pub fn with_executor(
        mut self,
        name: impl Into<String>,
        executor: Arc<dyn StepExecutor>,
    ) -> Self {
        self.step_executors.insert(name.into(), executor);
        self
    }

    #[must_use]
    pub fn with_task_store(mut self, store: Arc<dyn TaskStore>) -> Self {
        self.task_store = Some(store);
        self
    }

    #[must_use]
    pub fn with_max_steps(mut self, max: usize) -> Self {
        self.max_steps = max;
        self
    }

    /// 设置 Retry 上限。默认 `DEFAULT_MAX_RETRIES`，连续 Retry 超限 → Failed。
    #[must_use]
    pub fn with_max_retries(mut self, max: usize) -> Self {
        self.max_retries = max;
        self
    }

    /// 设置 planned_by。LLM 两阶段场景下记录 plan_task_id。
    /// 存为 struct 字段，run() 启动时异步写入 workflow_state。避免 blocking_lock。
    #[must_use]
    pub fn with_planned_by(mut self, plan_task_id: TaskId) -> Self {
        self.planned_by = Some(plan_task_id);
        self
    }

    /// 启动/恢复执行。
    pub async fn run(&self) -> Result<TaskResult> {
        enum StartupAction {
            Continue,
            Return(TaskResult),
            Fail(String),
        }

        // 重入守卫：防止并发 run() 覆盖写。
        if self.running.swap(true, std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("run: task is already running (concurrent run() detected)");
        }

        // 1. 翻转 status → Running，落盘。
        //    允许 Running → Running：崩溃恢复场景下 status 仍为 Running（已落盘），
        //    重启后需从这里恢复。回退 current_step 到最后一个 completed step。
        let startup_action = {
            let mut state = self.workflow_state.lock().await;
            let mut action = StartupAction::Continue;
            // 将 planned_by（struct 字段）写入 state（若有）。
            if let Some(plan_id) = &self.planned_by {
                state.planned_by = Some(plan_id.clone());
            }
            match state.status {
                WorkflowStatus::Idle | WorkflowStatus::Paused => {
                    state.status = WorkflowStatus::Running;
                    state.reason = None;
                    if state.started_at.is_none() {
                        state.started_at = Some(chrono::Utc::now());
                    }
                }
                WorkflowStatus::Running => {
                    // 崩溃恢复：status 仍为 Running（上次进程异常退出未清理）。
                    // 对已落盘的 terminal transition 直接收敛到终态；对 To transition
                    // 回退 current_step 到目标 step，避免重跑已完成步。
                    if let Some(last_record) = state
                        .step_history
                        .iter()
                        .rev()
                        .find(|r| r.result.is_some())
                        .cloned()
                    {
                        match last_record.transition {
                            Transition::Fail { reason } => {
                                state.status = WorkflowStatus::Failed;
                                state.error = Some(TaskError::Internal(reason.clone()));
                                state.ended_at = Some(chrono::Utc::now());
                                action = StartupAction::Fail(reason);
                            }
                            Transition::Abort { .. } => {
                                let result = TaskResult {
                                    cost: aggregate_cost(&state.step_history),
                                    turns: count_executed_turns(&state.step_history),
                                    final_message: last_record.result.map(|r| r.output),
                                };
                                state.status = WorkflowStatus::Succeeded;
                                state.result = Some(result.clone());
                                state.ended_at = Some(chrono::Utc::now());
                                action = StartupAction::Return(result);
                            }
                            Transition::To(next) => {
                                state.current_step = next;
                            }
                            Transition::Retry => {}
                        }
                    }
                }
                _ => {
                    self.running
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    anyhow::bail!(
                        "run: task is not Idle/Paused/Running (status={:?})",
                        state.status
                    );
                }
            }
            // 持锁期间只构造快照，drop 锁后再做文件 I/O。
            let snapshot = state.clone();
            drop(state);
            self.persist_state(&snapshot).await?;
            action
        };
        // 若有 task_store，保存 workflow 定义（供 restore 重建）。
        if let Some(store) = &self.task_store {
            store.save_workflow(&self.task_id, &self.workflow).await?;
        }

        // RAII 守卫：无论 run_loop 正常返回、Err 还是 panic，都清除重入标志。
        struct RunningGuard<'a>(&'a AtomicBool);
        impl Drop for RunningGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _guard = RunningGuard(&self.running);

        match startup_action {
            StartupAction::Continue => self.run_loop().await,
            StartupAction::Return(result) => Ok(result),
            StartupAction::Fail(error) => {
                let _ = self.event_tx.send(WorkflowEvent::Failed {
                    error: error.clone(),
                });
                anyhow::bail!(error)
            }
        }
    }

    /// run() 的主循环。拆出以降低 run() 圈复杂度。
    async fn run_loop(&self) -> Result<TaskResult> {
        loop {
            // 2. 取 current_step + 检查 max_steps（合并为一次锁获取）。
            let (current_step_id, history_len) = {
                let state = self.workflow_state.lock().await;
                (state.current_step.clone(), state.step_history.len())
            };
            let step = self
                .workflow
                .steps
                .iter()
                .find(|s| s.id() == &current_step_id)
                .ok_or_else(|| {
                    anyhow::anyhow!("run: current_step '{}' not in workflow", current_step_id)
                })?
                .clone();

            // max_steps 安全阀。
            if history_len >= self.max_steps {
                return self
                    .fail_task(format!("max_steps ({}) exceeded", self.max_steps))
                    .await;
            }

            let _ = self.event_tx.send(WorkflowEvent::StepStarted {
                step_id: step.id().clone(),
                step_name: step.name().to_string(),
            });

            // 3. 构造 abort token。
            let step_abort = CancellationToken::new();
            {
                let mut ca = self.current_abort.lock().await;
                *ca = Some(step_abort.clone());
            }

            // 4. Execute the step.
            let step_result = match self.run_step_with_policy(&step, step_abort.clone()).await {
                Ok(r) => r,
                Err(e) => {
                    let state = self.workflow_state.lock().await;
                    if state.status == WorkflowStatus::Cancelled {
                        return Ok(state.result.clone().unwrap_or_else(|| TaskResult {
                            cost: aggregate_cost(&state.step_history),
                            turns: count_executed_turns(&state.step_history),
                            final_message: None,
                        }));
                    }
                    drop(state);
                    // step agent 失败 → task Failed。
                    return self
                        .fail_task(format!("step '{}' failed: {}", step.id(), e))
                        .await;
                }
            };
            let _ = self.event_tx.send(WorkflowEvent::StepFinished {
                step_id: step.id().clone(),
                result: step_result.clone(),
            });

            // 5. 据 Transition 更新 state + 记录 step_history。
            //    apply_transition 内部检查 status==Cancelled 守卫：
            //    若 cancel() 已抢先置 Cancelled，则不推 record、不覆盖状态。
            let transition = self.decide_transition(&step, &step_result).await;
            let next_step = self
                .apply_transition(&step, step_result, transition.clone())
                .await?;

            // 6. 检查 cancel 守卫：cancel() 可能在 run_step 期间抢先置 Cancelled。
            //    apply_transition 已跳过 record 推入，此处确认并返回。
            {
                let state = self.workflow_state.lock().await;
                if state.status == WorkflowStatus::Cancelled {
                    return Ok(state.result.clone().unwrap_or_else(|| TaskResult {
                        cost: aggregate_cost(&state.step_history),
                        turns: count_executed_turns(&state.step_history),
                        final_message: None,
                    }));
                }
            }

            // 7. 流程走完？
            match (transition, next_step) {
                (Transition::Fail { reason }, _) => {
                    return self.fail_task(reason).await;
                }
                (Transition::Abort { .. }, _) => {
                    // 流程终止（正常结束或 abort）。
                    let mut state = self.workflow_state.lock().await;
                    let cost = aggregate_cost(&state.step_history);
                    let final_message = state
                        .step_history
                        .last()
                        .and_then(|r| r.result.as_ref())
                        .map(|r| r.output.clone());
                    let result = TaskResult {
                        cost,
                        turns: count_executed_turns(&state.step_history),
                        final_message,
                    };
                    state.status = WorkflowStatus::Succeeded;
                    state.result = Some(result.clone());
                    state.ended_at = Some(chrono::Utc::now());
                    let snapshot = state.clone();
                    drop(state);
                    self.persist_state(&snapshot).await?;
                    return Ok(result);
                }
                (Transition::To(next), _) => {
                    let mut state = self.workflow_state.lock().await;
                    state.current_step = next;
                    let snapshot = state.clone();
                    drop(state);
                    self.persist_state(&snapshot).await?;
                    self.pause_if_requested().await?;
                }
                (Transition::Retry, _) => {
                    // current_step 不变，重跑。
                    self.pause_if_requested().await?;
                }
            }
        }
    }

    async fn pause_if_requested(&self) -> Result<()> {
        if !self
            .pause_requested
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(());
        }

        let reason = self
            .pause_reason
            .lock()
            .map(|r| r.clone().unwrap_or_else(|| "pause requested".into()))
            .unwrap_or_else(|_| "pause requested".into());
        let mut state = self.workflow_state.lock().await;
        state.status = WorkflowStatus::Paused;
        state.reason = Some(reason.clone());
        let snapshot = state.clone();
        drop(state);
        self.persist_state(&snapshot).await?;
        self.pause_requested
            .store(false, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut r) = self.pause_reason.lock() {
            *r = None;
        }
        let _ = self.event_tx.send(WorkflowEvent::Paused {
            reason: reason.clone(),
        });
        Err(TaskError::Paused(reason).into())
    }

    /// 判断 transition：自由 task 直接 Abort，否则调 judge.decide。
    async fn decide_transition(&self, step: &Step, step_result: &StepResult) -> Transition {
        let is_free_task = self.workflow.edges.is_empty() && self.workflow.steps.len() == 1;
        if is_free_task {
            return Transition::Abort {
                reason: "free task complete".into(),
            };
        }
        let history = self.workflow_state.lock().await.step_history.clone();
        let context = self.context.lock().await.clone();
        let ctx = StepCtx {
            current_step: step,
            last_result: step_result,
            step_history: &history,
            context: &context,
        };
        self.judge.decide(&ctx).await
    }

    /// 统一的 task 失败处理：置 Failed + error + ended_at + persist + 事件 + bail。
    /// 消除 run()/apply_transition 中重复的错误处理模式。
    async fn fail_task(&self, error_msg: String) -> Result<TaskResult> {
        let mut state = self.workflow_state.lock().await;
        state.status = WorkflowStatus::Failed;
        state.error = Some(TaskError::Internal(error_msg.clone()));
        state.ended_at = Some(chrono::Utc::now());
        let snapshot = state.clone();
        drop(state);
        self.persist_state(&snapshot).await?;
        let _ = self.event_tx.send(WorkflowEvent::Failed {
            error: error_msg.clone(),
        });
        anyhow::bail!(error_msg)
    }

    /// Dispatch one workflow step by kind.
    async fn run_step(&self, step: &Step, step_abort: CancellationToken) -> Result<StepResult> {
        match step {
            Step::Llm { .. } => self.run_llm_step(step, step_abort).await,
            Step::Executor { .. } => self.run_executor_step(step, step_abort).await,
        }
    }

    /// Run a step with its optional runtime execution policy.
    ///
    /// Policy retries cover step execution failures only. A judge-produced
    /// `Transition::Retry` remains a workflow transition and is still governed
    /// by `with_max_retries`.
    async fn run_step_with_policy(
        &self,
        step: &Step,
        step_abort: CancellationToken,
    ) -> Result<StepResult> {
        let policy = step.policy().cloned().unwrap_or_default();
        let max_attempts = policy.max_attempts.unwrap_or(1).max(1);
        let timeout = policy.timeout_ms.map(Duration::from_millis);
        let retry_backoff = Duration::from_millis(policy.retry_backoff_ms.unwrap_or(0));

        for attempt in 1..=max_attempts {
            if step_abort.is_cancelled() {
                anyhow::bail!("step '{}' aborted", step.id());
            }
            let attempt_abort = step_abort.child_token();
            let result = self
                .run_step_attempt(step, attempt_abort.clone(), timeout)
                .await;
            match result {
                Ok(result) => return Ok(result),
                Err(err) => {
                    attempt_abort.cancel();
                    if step_abort.is_cancelled() {
                        return Err(err);
                    }
                    if attempt >= max_attempts {
                        if max_attempts == 1 {
                            return Err(err);
                        }
                        anyhow::bail!(
                            "step '{}' failed after {} attempts: {}",
                            step.id(),
                            max_attempts,
                            err
                        );
                    }
                    if !retry_backoff.is_zero() {
                        tokio::select! {
                            biased;
                            _ = step_abort.cancelled() => anyhow::bail!("step '{}' aborted", step.id()),
                            _ = tokio::time::sleep(retry_backoff) => {}
                        }
                    }
                }
            }
        }

        unreachable!("attempt loop must return on success or final failure")
    }

    async fn run_step_attempt(
        &self,
        step: &Step,
        attempt_abort: CancellationToken,
        timeout: Option<Duration>,
    ) -> Result<StepResult> {
        let Some(timeout) = timeout else {
            return self.run_step(step, attempt_abort).await;
        };
        tokio::select! {
            biased;
            _ = attempt_abort.cancelled() => anyhow::bail!("step '{}' aborted", step.id()),
            result = self.run_step(step, attempt_abort.clone()) => result,
            _ = tokio::time::sleep(timeout) => {
                attempt_abort.cancel();
                anyhow::bail!(
                    "step '{}' timed out after {} ms",
                    step.id(),
                    timeout.as_millis()
                )
            }
        }
    }

    /// Run a deterministic non-LLM executor step.
    async fn run_executor_step(
        &self,
        step: &Step,
        step_abort: CancellationToken,
    ) -> Result<StepResult> {
        let Step::Executor { executor_name, .. } = step else {
            anyhow::bail!("run_executor_step called with non-executor step");
        };
        let executor = self
            .step_executors
            .get(executor_name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("executor '{}' not registered", executor_name))?;
        let started_at = chrono::Utc::now();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let env = self
            .config
            .env_factory
            .create(&cwd)
            .map_err(|e| anyhow::anyhow!("env_factory: {e}"))?;
        let history = self.workflow_state.lock().await.step_history.clone();
        let prev_result = history.last().and_then(|record| record.result.as_ref());
        let ctx = ExecutorCtx {
            current_step: step,
            prev_result,
            step_history: &history,
            env,
            abort: step_abort.clone(),
            context: self.context.clone(),
        };
        let mut result = tokio::select! {
            biased;
            _ = step_abort.cancelled() => {
                anyhow::bail!("executor step aborted");
            }
            r = executor.execute(&ctx) => r?,
        };
        let ended_at = chrono::Utc::now();
        if result.started_at.is_none() {
            result.started_at = Some(started_at);
        }
        if result.ended_at.is_none() {
            result.ended_at = Some(ended_at);
        }
        if result.session_id.is_empty() {
            result.session_id = format!("executor:{}", step.id());
        }
        Ok(result)
    }

    /// 跑 LLM step：构造 harness + prompt + 收集结果。
    async fn run_llm_step(&self, step: &Step, step_abort: CancellationToken) -> Result<StepResult> {
        let Step::Llm {
            prompt,
            allowed_tools,
            ..
        } = step
        else {
            anyhow::bail!("run_llm_step called with non-LLM step");
        };
        // 记录真实开始时间（用于 StepRecord）。
        let started_at = chrono::Utc::now();

        // a. 构造 env + session。
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let env = self
            .config
            .env_factory
            .create(&cwd)
            .map_err(|e| anyhow::anyhow!("env_factory: {e}"))?;
        let (storage, session_id) = self
            .config
            .session_factory
            .create(&self.config.session_base_dir, &self.config.model)
            .await
            .map_err(|e| anyhow::anyhow!("session_factory: {e}"))?;

        // b. 构造 builder。
        let mut builder = HarnessBuilder::new(&self.config.model)
            .provider(&self.config.model, self.config.client.clone());

        // c. 注册 step plugin（若有）。
        if let Some(factory) = self.step_plugin_factories.get(step.id()) {
            let plugin = factory();
            builder = builder.install(plugin.as_ref());
        }

        // d. 注册 extra tools + SyncSpawnAgentTool（若该步允许）+ SubmitStepResultTool。
        //    收集所有注入的 tool 名（extra_tools + submit_step_result），
        //    传给 gate 让其不被过滤。
        let mut injected_tool_names: HashSet<String> = HashSet::new();
        for tool in &self.extra_tools {
            injected_tool_names.insert(tool.name().to_string());
            builder = builder.tool(tool.clone());
        }
        if allowed_tools.iter().any(|t| t == SYNC_SPAWN_TOOL_NAME) {
            let spawner = self.build_step_spawner();
            let sync_tool = Arc::new(SyncSpawnAgentTool::new(
                Arc::new(spawner),
                step_abort.clone(),
            ));
            builder = builder.tool(sync_tool);
        }
        // 每步注入 SubmitStepResultTool，让 step agent 能提交结构化结果。
        let (submit_tool, structured_rx) = SubmitStepResultTool::new();
        injected_tool_names.insert(SUBMIT_STEP_RESULT_TOOL_NAME.to_string());
        builder = builder.tool(Arc::new(submit_tool));

        // e. 注入门禁 hook（第一道 PrepareNextTurn + 第二道 BeforeToolCall）。
        //    HarnessBuilder 无直接 hooks setter，用 HookPlugin 桥接（install 调 register_hooks）。
        //    gate 的 active_tools = step.allowed_tools ∪ injected_tool_names，
        //    确保 submit_step_result 等内置 tool 不被门禁滤掉。
        let state_arc = self.workflow_state.clone();
        let step_tools: HashMap<StepId, Vec<String>> = self.step_tools_cache.clone();
        let injected_vec: Vec<String> = injected_tool_names.iter().cloned().collect();
        let gate_hook: Arc<dyn llm_harness_types::PrepareNextTurnHook> =
            Arc::new(crate::workflow::gate::WorkflowGateHook::new(
                state_arc,
                step_tools,
                injected_tool_names,
            ));
        let approval_hook =
            crate::workflow::gate::build_gate_approval_hook(allowed_tools, &injected_vec);
        let mut hook_state = self.extra_hooks.clone();
        hook_state.prepare_next_turn.push(gate_hook);
        hook_state.before_tool_call.push(approval_hook);
        builder = builder.install(&HookPlugin { hooks: hook_state });

        // f. build + set_abort_token + prompt。
        let harness = builder
            .build_with_session(env, Session::new(storage))
            .map_err(|e: HarnessBuildError| anyhow::anyhow!("build harness: {e}"))?;
        harness.set_abort_token(step_abort.clone());

        // g. step 指令。步骤间上下文注入：在 step.prompt 前拼接 step_history 摘要。
        //    若共享 context 非空，也注入 KV 摘要，让 LLM step 能读到上游 executor 写入的变量。
        let prompt_text = {
            let state = self.workflow_state.lock().await;
            let context_snapshot = self.context.lock().await.clone();
            let history_ctx = if state.step_history.is_empty() {
                String::new()
            } else {
                format_step_history_summary(&state.step_history)
            };
            let vars_ctx = format_context_vars(&context_snapshot);
            if history_ctx.is_empty() && vars_ctx.is_empty() {
                prompt.clone()
            } else {
                let mut parts = Vec::new();
                if !vars_ctx.is_empty() {
                    parts.push(vars_ctx);
                }
                if !history_ctx.is_empty() {
                    parts.push(history_ctx);
                }
                format!("Context:\n{}\n\n---\n\n{}", parts.join("\n"), prompt)
            }
        };

        let mut rx = harness.subscribe();
        harness.prompt(&prompt_text).await?;

        // h. 收集事件 → StepResult。
        //    哨兵：Settled（所有排队活动结束）或 Aborted（被 cancel）。
        //    channel 关闭（harness 异常退出）视为失败而非成功。
        let mut text_delta_output = String::new();
        let mut final_answer_output: Option<String> = None;
        let mut tool_calls_count = 0u32;
        let mut settled = false;
        loop {
            tokio::select! {
                biased;
                _ = step_abort.cancelled() => {
                    // abort：收到的部分 output 保留，但不标记 settled。
                    break;
                }
                event = rx.recv() => {
                    match event {
                        Ok(event) => match event.as_ref() {
                            AgentHarnessEvent::Agent(AgentEvent::TextDelta { text, .. }) => {
                                text_delta_output.push_str(text);
                            }
                            AgentHarnessEvent::Agent(ev) => {
                                if let Some((_, _, text)) = ev.as_final_answer() {
                                    final_answer_output = Some(text);
                                }
                            }
                            AgentHarnessEvent::ToolCallStart { .. } => tool_calls_count += 1,
                            AgentHarnessEvent::Settled | AgentHarnessEvent::Aborted => {
                                settled = true;
                                break;
                            }
                            _ => {}
                        },
                        Err(_) => break, // channel 关闭
                    }
                }
            }
        }
        if !settled {
            // channel 关闭但未收到 Settled：harness 异常退出。
            anyhow::bail!(
                "run_llm_step: event stream closed before Settled (step '{}')",
                step.id()
            );
        }
        let cost = harness.usage();
        let ended_at = chrono::Utc::now();
        // 从 SubmitStepResultTool 读取结构化结果。
        let structured = structured_rx.lock().await.take();
        let output = final_answer_output.unwrap_or(text_delta_output);

        Ok(StepResult {
            output,
            structured,
            tool_calls_count,
            session_id,
            cost,
            started_at: Some(started_at),
            ended_at: Some(ended_at),
        })
    }

    /// 构造 step 内部 fan-out 用的 spawner（局部对象，step 结束丢弃）。
    fn build_step_spawner(&self) -> HarnessSubAgentSpawner {
        HarnessSubAgentSpawner::new(
            self.config.model.clone(),
            self.config.client.clone(),
            self.config.session_base_dir.clone(),
            |_cwd, _peer_rx, _agent_id| Box::new(NoopPlugin),
        )
        .env_factory(self.config.env_factory.clone())
        .session_factory(self.config.session_factory.clone())
    }

    /// 持久化 WorkflowState（若有 task_store）。
    /// 调用方应在 drop 锁后调用（传入 snapshot），避免持锁 await I/O。
    async fn persist_state(&self, state: &WorkflowState) -> Result<()> {
        // 同步共享 context 快照进 state，保证落盘的是最新 context。
        let mut state = state.clone();
        state.context = self.context_snapshot().await;
        if let Some(store) = &self.task_store {
            store.save_workflow_state(&self.task_id, &state).await?;
        }
        Ok(())
    }

    /// 据 Transition 更新 state + 记录 step_history。返回 next_step（Retry/Abort 时为 None）。
    ///
    /// cancel 守卫：若 status 已被 cancel() 置为 Cancelled，跳过 record 推入，
    /// 直接返回 None（run() 会检查 status 并退出）。
    async fn apply_transition(
        &self,
        step: &Step,
        step_result: StepResult,
        transition: Transition,
    ) -> Result<Option<StepId>> {
        let mut state = self.workflow_state.lock().await;

        // cancel 守卫：cancel() 已抢先置 Cancelled，不推 record、不覆盖状态。
        if state.status == WorkflowStatus::Cancelled {
            return Ok(None);
        }

        let now = chrono::Utc::now();
        let record = StepRecord {
            step_id: step.id().clone(),
            started_at: step_result.started_at.unwrap_or(now),
            ended_at: step_result.ended_at.unwrap_or(now),
            result: Some(step_result),
            transition: transition.clone(),
        };
        state.step_history.push(record);

        match transition {
            Transition::To(next) => {
                // 校验合法性：next 必须在 steps 里 + 从 current_step 可达（有 edge 或是 entry）。
                // 注：To(entry_step) 无条件放行，允许流程返工重跑入口。
                if !self.workflow.steps.iter().any(|s| s.id() == &next) {
                    let msg = format!("InvalidTransition: step '{}' not in workflow", next);
                    state.status = WorkflowStatus::Failed;
                    state.error = Some(TaskError::Internal(msg.clone()));
                    state.ended_at = Some(now);
                    let snapshot = state.clone();
                    drop(state);
                    self.persist_state(&snapshot).await?;
                    anyhow::bail!(msg);
                }
                let reachable = self
                    .workflow
                    .edges
                    .iter()
                    .any(|e| e.from.as_str() == step.id().as_str() && e.to == next)
                    || next == self.workflow.entry_step;
                if !reachable {
                    let msg = format!(
                        "InvalidTransition: no edge from '{}' to '{}'",
                        step.id(),
                        next
                    );
                    state.status = WorkflowStatus::Failed;
                    state.error = Some(TaskError::Internal(msg.clone()));
                    state.ended_at = Some(now);
                    let snapshot = state.clone();
                    drop(state);
                    self.persist_state(&snapshot).await?;
                    anyhow::bail!(msg);
                }
                let snapshot = state.clone();
                drop(state);
                self.persist_state(&snapshot).await?;
                Ok(Some(next))
            }
            Transition::Retry => {
                // max_retries 安全阀。统计当前步连续 Retry 次数。
                // 语义：max_retries=N 允许 N 次 Retry。count 含刚 push 的这条，
                // 故 count > max_retries 时超限（第 N+1 次触发 Failed）。
                let retry_count = state
                    .step_history
                    .iter()
                    .rev()
                    .take_while(|r| {
                        r.step_id.as_str() == step.id().as_str()
                            && matches!(r.transition, Transition::Retry)
                    })
                    .count();
                if retry_count > self.max_retries {
                    let msg = format!(
                        "max_retries ({}) exceeded for step '{}'",
                        self.max_retries,
                        step.id()
                    );
                    state.status = WorkflowStatus::Failed;
                    state.error = Some(TaskError::Internal(msg.clone()));
                    state.ended_at = Some(now);
                    let snapshot = state.clone();
                    drop(state);
                    self.persist_state(&snapshot).await?;
                    anyhow::bail!(msg);
                }
                let snapshot = state.clone();
                drop(state);
                self.persist_state(&snapshot).await?;
                Ok(None)
            }
            Transition::Fail { .. } => {
                drop(state);
                Ok(None)
            }
            Transition::Abort { .. } => {
                let snapshot = state.clone();
                drop(state);
                self.persist_state(&snapshot).await?;
                Ok(None)
            }
        }
    }

    pub async fn state(&self) -> WorkflowStatus {
        self.workflow_state.lock().await.status.clone()
    }

    /// 订阅引擎事件流。步骤级事件（StepStarted/StepFinished/Paused/...）对外可见。
    /// 长周期任务可据此做监控/UI。step 内部的 text_delta 不透传。
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<WorkflowEvent> {
        self.event_tx.subscribe()
    }

    /// 当前执行步骤 ID。轮询进度用（subscribe 是主推方案，此为兜底）。
    pub async fn current_step(&self) -> StepId {
        self.workflow_state.lock().await.current_step.clone()
    }

    /// 步骤历史（审计链）。轮询进度用。
    pub async fn step_history(&self) -> Vec<StepRecord> {
        self.workflow_state.lock().await.step_history.clone()
    }

    /// 非阻塞：设 pause_requested 标志即返回。run() 在步边界检查并消费。
    pub fn pause(&self, reason: &str) -> Result<()> {
        self.pause_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut r) = self.pause_reason.lock() {
            *r = Some(reason.to_string());
        }
        Ok(())
    }

    /// Prepare a paused task for continuation.
    ///
    /// `run()` owns the `Paused -> Running` transition and drives execution.
    /// Keeping that single state-machine entry point prevents an explicit
    /// resume from being mistaken for crash recovery of an already-running task.
    pub async fn resume(&self) -> Result<()> {
        let mut state = self.workflow_state.lock().await;
        if state.status != WorkflowStatus::Paused {
            anyhow::bail!("resume: task is not Paused (status={:?})", state.status);
        }
        self.pause_requested
            .store(false, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut r) = self.pause_reason.lock() {
            *r = None;
        }
        state.reason = None;
        let snapshot = state.clone();
        drop(state);
        self.persist_state(&snapshot).await?;
        let _ = self.event_tx.send(WorkflowEvent::Resumed);
        Ok(())
    }

    /// 立刻 Abort 当前 step。预算耗尽走此路径（止损）。
    ///
    /// cancel 后当前步的 StepRecord 记为 { result: None, transition: Abort }——
    /// partial 结果不保留（step agent 被 abort，输出不完整无意义）。
    /// step_history 里有这条记录（审计链完整），但 result: None 标明无完整结果。
    /// 竞态处理：cancel() 与 apply_transition() 可能交错执行。
    /// 若 apply_transition 已为 current_step 推入 record（result: Some），
    /// cancel 不再推入第二条 record——审计链已有该步执行记录，
    /// cancel 只需置 Cancelled 状态 + reason，避免双记录导致 cost 双计。
    /// 若 apply_transition 尚未执行（record 最后一条不是 current_step），
    /// cancel 推入 { result: None, transition: Abort } 记录保证审计链完整。
    pub async fn cancel(&self, reason: &str) -> Result<()> {
        if let Some(token) = self.current_abort.lock().await.take() {
            token.cancel();
        }
        let mut state = self.workflow_state.lock().await;
        let now = chrono::Utc::now();
        let cancelled_step_id = state.current_step.clone();

        // 检查 apply_transition 是否已为当前步推入 record。
        let already_recorded = state
            .step_history
            .last()
            .is_some_and(|r| r.step_id == cancelled_step_id && r.result.is_some());
        if !already_recorded {
            state.step_history.push(StepRecord {
                step_id: cancelled_step_id,
                started_at: now,
                ended_at: now,
                result: None,
                transition: Transition::Abort {
                    reason: format!("cancelled: {}", reason),
                },
            });
        }
        state.status = WorkflowStatus::Cancelled;
        state.reason = Some(reason.to_string());
        state.ended_at = Some(now);
        let snapshot = state.clone();
        drop(state);
        self.persist_state(&snapshot).await?;
        let _ = self.event_tx.send(WorkflowEvent::Cancelled {
            reason: reason.to_string(),
        });
        Ok(())
    }

    pub async fn checkpoint(&self, description: &str, payload: Value) -> Result<()> {
        let cp = {
            let mut cps = self.checkpoints.lock().await;
            let seq = cps.len();
            let cp = Checkpoint {
                seq,
                saved_at: chrono::Utc::now(),
                description: description.to_string(),
                payload,
            };
            cps.push(cp.clone());
            cp
        };
        // I/O 在锁外执行。
        if let Some(store) = &self.task_store {
            store.save_checkpoint(&self.task_id, &cp).await?;
        }
        Ok(())
    }

    pub async fn total_cost(&self) -> CostAggregate {
        let state = self.workflow_state.lock().await;
        aggregate_cost(&state.step_history)
    }

    /// 共享 context 的 Arc 句柄。executor / 外部代码可通过此句柄读写 KV。
    pub fn context(&self) -> Arc<Mutex<WorkflowContext>> {
        self.context.clone()
    }

    /// 取 context 快照（用于持久化）。
    async fn context_snapshot(&self) -> WorkflowContext {
        self.context.lock().await.clone()
    }
}

/// 聚合 step_history 中所有 StepResult 的 cost。
/// 纯函数：run() 的 Abort arm（已持锁）和 total_cost()（异步获锁后）共用。
fn aggregate_cost(history: &[StepRecord]) -> CostAggregate {
    let mut total = CostAggregate::default();
    for record in history {
        if let Some(r) = &record.result {
            total.total_input_tokens += r.cost.total_input_tokens;
            total.total_output_tokens += r.cost.total_output_tokens;
            total.total_cache_read_tokens += r.cost.total_cache_read_tokens;
            total.total_cache_write_tokens += r.cost.total_cache_write_tokens;
            total.total_cost += r.cost.total_cost;
            for (k, v) in &r.cost.by_model {
                let entry = total.by_model.entry(k.clone()).or_default();
                entry.input_tokens += v.input_tokens;
                entry.output_tokens += v.output_tokens;
                entry.cache_read_tokens += v.cache_read_tokens;
                entry.cache_write_tokens += v.cache_write_tokens;
                entry.cost += v.cost;
                entry.call_count += v.call_count;
            }
        }
    }
    total
}

/// 统计已执行的 step turn 数（不含 Retry/cancel 记录）。
/// 语义：只计 result: Some 的记录（正常完成的 step），Retry 和 cancel 不计。
fn count_executed_turns(history: &[StepRecord]) -> u32 {
    history.iter().filter(|r| r.result.is_some()).count() as u32
}

// ── HookPlugin（把 HarnessHooks 注入 builder 的桥接 plugin）───────────────────

/// 把 HarnessHooks 注入 HarnessBuilder 的桥接 plugin。
/// HarnessBuilder 无直接 `.hooks()` setter，但 `install(plugin)` 调 `register_hooks`，
/// 故用此 plugin 在构造期把 hooks 追加进 builder 的 hooks 集合。
struct HookPlugin {
    hooks: HarnessHooks,
}

impl Plugin for HookPlugin {
    fn name(&self) -> &str {
        "workflow-hook-bridge"
    }
    fn register_hooks(&self, target: &mut HarnessHooks) {
        target
            .before_run
            .extend(self.hooks.before_run.iter().cloned());
        target
            .before_turn
            .extend(self.hooks.before_turn.iter().cloned());
        target
            .after_turn
            .extend(self.hooks.after_turn.iter().cloned());
        target
            .before_tool_call
            .extend(self.hooks.before_tool_call.iter().cloned());
        target
            .after_tool_call
            .extend(self.hooks.after_tool_call.iter().cloned());
        target
            .transform_context
            .extend(self.hooks.transform_context.iter().cloned());
        target
            .prepare_next_turn
            .extend(self.hooks.prepare_next_turn.iter().cloned());
        target
            .should_stop
            .extend(self.hooks.should_stop.iter().cloned());
        target
            .before_provider_request
            .extend(self.hooks.before_provider_request.iter().cloned());
        target
            .after_provider_response
            .extend(self.hooks.after_provider_response.iter().cloned());
        target
            .before_compact
            .extend(self.hooks.before_compact.iter().cloned());
    }
}

#[cfg(test)]
mod sync_tool_tests {
    use super::*;
    use crate::spawner::JsonlSessionFactory;
    use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
    use llm_harness_types::ExecutionEnv;

    struct NoopPlugin;
    impl llm_harness_agent::Plugin for NoopPlugin {
        fn name(&self) -> &str {
            "noop"
        }
    }

    struct NoopEnvFactory;
    impl crate::spawner::EnvFactory for NoopEnvFactory {
        fn create(
            &self,
            _cwd: &std::path::Path,
        ) -> std::result::Result<Arc<dyn ExecutionEnv>, AgentError> {
            Ok(Arc::new(llm_harness_loop::test_utils::NoOpEnv))
        }
    }

    /// LLM client that never returns from `chat_stream` —— 用于测试 abort 路径。
    /// sub-agent 会阻塞在 LLM 调用上，直到 step abort 触发 select! 的 cancel arm。
    struct BlockingClient;

    #[async_trait::async_trait]
    impl llm_harness_loop::LlmClient for BlockingClient {
        fn capabilities(&self) -> llm_harness_loop::ProviderCapabilities {
            llm_harness_loop::ProviderCapabilities::new(false, false, false)
        }
        async fn chat(
            &self,
            _: &llm_harness_loop::ChatRequest,
        ) -> std::result::Result<llm_harness_loop::ChatResponse, llm_harness_loop::LlmError>
        {
            std::future::pending().await
        }
        async fn chat_stream(
            &self,
            _: &llm_harness_loop::ChatRequest,
        ) -> std::result::Result<llm_harness_loop::StreamHandle, llm_harness_loop::LlmError>
        {
            std::future::pending().await
        }
    }

    fn make_spawner(client: Arc<dyn llm_harness_loop::LlmClient>) -> Arc<HarnessSubAgentSpawner> {
        Arc::new(
            HarnessSubAgentSpawner::new(
                "test-model",
                client,
                std::env::temp_dir().join("sync-tool-test"),
                |_cwd, _rx, _id| Box::new(NoopPlugin),
            )
            .session_factory(Arc::new(JsonlSessionFactory))
            .env_factory(Arc::new(NoopEnvFactory)),
        )
    }

    fn make_ctx(abort: CancellationToken) -> ToolContext {
        ToolContext {
            env: Arc::new(llm_harness_loop::test_utils::NoOpEnv),
            abort,
            tool_use_id: "tu_1".into(),
            turn_index: 0,
            assistant_message: Arc::new(empty_msg()),
            update_tx: tokio::sync::mpsc::channel(1).0,
        }
    }

    fn empty_msg() -> llm_harness_types::AssistantMessage {
        llm_harness_loop::test_utils::test_assistant_message(vec![])
    }

    #[tokio::test]
    async fn sync_tool_returns_result_on_normal_join() {
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text(
            "sub-agent done",
        )]));
        let spawner = make_spawner(client);
        let step_abort = CancellationToken::new();
        let tool = SyncSpawnAgentTool::new(spawner, step_abort);
        let ctx = make_ctx(CancellationToken::new());
        let result = tool
            .execute(serde_json::json!({"prompt": "do work"}), &ctx)
            .await
            .unwrap();
        assert!(!result.terminate);
        match &result.content[0] {
            ContentBlock::Text { text } => assert!(text.contains("Sub-agent completed")),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn sync_tool_aborts_subagent_on_step_abort() {
        let client = Arc::new(BlockingClient);
        let spawner = make_spawner(client);
        let step_abort = CancellationToken::new();
        let tool = SyncSpawnAgentTool::new(spawner, step_abort.clone());
        let ctx = make_ctx(CancellationToken::new());
        let abort_clone = step_abort.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            abort_clone.cancel();
        });
        let result = tool
            .execute(serde_json::json!({"prompt": "long work"}), &ctx)
            .await;
        assert!(matches!(result, Err(ToolError::Aborted)));
    }

    #[tokio::test]
    async fn sync_tool_ignores_wait_param() {
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
        let spawner = make_spawner(client);
        let step_abort = CancellationToken::new();
        let tool = SyncSpawnAgentTool::new(spawner, step_abort);
        let ctx = make_ctx(CancellationToken::new());
        let result = tool
            .execute(serde_json::json!({"prompt": "work", "wait": true}), &ctx)
            .await
            .unwrap();
        assert!(!result.terminate);
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;
    use crate::spawner::JsonlSessionFactory;
    use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
    use llm_harness_types::ExecutionEnv;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestEnvFactory;
    impl EnvFactory for TestEnvFactory {
        fn create(
            &self,
            _cwd: &std::path::Path,
        ) -> std::result::Result<Arc<dyn ExecutionEnv>, AgentError> {
            Ok(Arc::new(llm_harness_loop::test_utils::NoOpEnv))
        }
    }

    #[tokio::test]
    async fn free_task_smoke_succeeds() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text(
            "The answer is 42.",
        )]));
        let config = WorkflowEngineConfig {
            client,
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let engine = WorkflowEngine::new_free_task("what is 6*7?".into(), vec![], config).unwrap();
        assert_eq!(engine.state().await, WorkflowStatus::Idle);
        let result = engine.run().await.unwrap();
        assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
        assert!(result.final_message.as_deref().unwrap_or("").contains("42"));
    }

    use crate::task_store::JsonlTaskStore;
    use crate::workflow::judge::NoopJudge;
    use crate::workflow::model::{ConditionExpr, Edge, EdgeCondition};

    struct FixedExecutor;
    impl StepExecutor for FixedExecutor {
        fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
            Box::pin(async move {
                let prev = ctx
                    .prev_result
                    .expect("executor should see previous result");
                let suffix = ctx
                    .current_step
                    .executor_config()
                    .and_then(|config| config.get("suffix"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("done");
                Ok(StepResult {
                    output: format!("executor {suffix} after {}", prev.output),
                    structured: Some(serde_json::json!({
                        "executor": true,
                        "previous_output": prev.output,
                        "history_len": ctx.step_history.len(),
                    })),
                    tool_calls_count: 0,
                    session_id: String::new(),
                    cost: CostAggregate::default(),
                    started_at: None,
                    ended_at: None,
                })
            })
        }
    }

    struct StructuredExecutor;
    impl StepExecutor for StructuredExecutor {
        fn execute<'a>(&'a self, _ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
            Box::pin(async move {
                Ok(StepResult {
                    output: "structured ok".into(),
                    structured: Some(serde_json::json!({"status": "ok"})),
                    tool_calls_count: 0,
                    session_id: String::new(),
                    cost: CostAggregate::default(),
                    started_at: None,
                    ended_at: None,
                })
            })
        }
    }

    struct BlockingExecutor;
    impl StepExecutor for BlockingExecutor {
        fn execute<'a>(&'a self, _ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
            Box::pin(async move { std::future::pending::<Result<StepResult>>().await })
        }
    }

    struct FlakyExecutor {
        attempts: Arc<AtomicUsize>,
    }

    impl StepExecutor for FlakyExecutor {
        fn execute<'a>(&'a self, _ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
            Box::pin(async move {
                let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
                if attempt == 1 {
                    anyhow::bail!("transient executor failure");
                }
                Ok(StepResult {
                    output: format!("ok on attempt {attempt}"),
                    structured: Some(serde_json::json!({ "attempt": attempt })),
                    tool_calls_count: 0,
                    session_id: String::new(),
                    cost: CostAggregate::default(),
                    started_at: None,
                    ended_at: None,
                })
            })
        }
    }

    fn completed_result(output: &str) -> StepResult {
        StepResult {
            output: output.into(),
            structured: None,
            tool_calls_count: 0,
            session_id: format!("session-{output}"),
            cost: CostAggregate::default(),
            started_at: Some(chrono::Utc::now()),
            ended_at: Some(chrono::Utc::now()),
        }
    }

    fn completed_record(step_id: &str, output: &str, transition: Transition) -> StepRecord {
        let now = chrono::Utc::now();
        StepRecord {
            step_id: step_id.into(),
            started_at: now,
            ended_at: now,
            result: Some(completed_result(output)),
            transition,
        }
    }

    #[test]
    fn step_history_summary_includes_structured_before_output() {
        let mut record = completed_record(
            "measure",
            "measurement complete",
            Transition::To("decide".into()),
        );
        record.result.as_mut().unwrap().structured = Some(serde_json::json!({
            "voltage": 3.3,
            "passed": true,
        }));

        let summary = format_step_history_summary(&[record]);

        assert!(summary.contains("- step 'measure'"));
        assert!(summary.contains("structured:"));
        assert!(summary.contains("\"voltage\":3.3"));
        assert!(summary.contains("\"passed\":true"));
        assert!(summary.contains("output: measurement complete"));
        assert!(
            summary.find("structured:").unwrap() < summary.find("output:").unwrap(),
            "structured result should be shown before output preview"
        );
    }

    #[test]
    fn step_history_summary_uses_longer_output_preview() {
        let output = "x".repeat(350);
        let record = completed_record(
            "answer",
            &output,
            Transition::Abort {
                reason: "done".into(),
            },
        );

        let summary = format_step_history_summary(&[record]);

        assert!(
            summary.contains(&output),
            "output under the 1000 character preview limit should not be truncated"
        );
    }

    struct BlockingTool;

    impl Tool for BlockingTool {
        fn name(&self) -> &str {
            "block_forever"
        }

        fn description(&self) -> &str {
            "blocks until the tool context is aborted"
        }

        fn parameters_schema(&self) -> &serde_json::Value {
            static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| serde_json::json!({}));
            &SCHEMA
        }

        fn execute<'a>(
            &'a self,
            _args: serde_json::Value,
            ctx: &'a ToolContext,
        ) -> BoxFuture<'a, std::result::Result<ToolResult, ToolError>> {
            Box::pin(async move {
                ctx.abort.cancelled().await;
                Err(ToolError::Aborted)
            })
        }
    }

    struct TwoStepJudge;

    impl StepTransitionJudge for TwoStepJudge {
        fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
            let cur = ctx.current_step.id().clone();
            Box::pin(async move {
                if cur == "s1" {
                    Transition::To("s2".into())
                } else {
                    Transition::Abort {
                        reason: "done".into(),
                    }
                }
            })
        }
    }

    #[tokio::test]
    async fn free_task_crash_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("done")]));
        let config = WorkflowEngineConfig {
            client: client.clone(),
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let engine = WorkflowEngine::new_free_task("do work".into(), vec![], config)
            .unwrap()
            .with_task_store(store.clone());
        let task_id = engine.task_id().clone();
        engine.run().await.unwrap();
        assert_eq!(engine.state().await, WorkflowStatus::Succeeded);

        // restore：workflow + state 从 store load。
        let config2 = WorkflowEngineConfig {
            client,
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let restored = WorkflowEngine::restore(store, task_id, config2, Arc::new(NoopJudge))
            .await
            .unwrap();
        assert_eq!(restored.state().await, WorkflowStatus::Succeeded);
    }

    #[tokio::test]
    async fn running_restore_continues_from_transition_target() {
        let dir = tempfile::TempDir::new().unwrap();
        let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
        let task_id = TaskId("restore-linear".into());
        let workflow = Workflow {
            entry_step: "s1".into(),
            steps: vec![
                Step::llm("s1", "Step 1", "do step 1", vec![]),
                Step::llm("s2", "Step 2", "do step 2", vec![]),
            ],
            edges: vec![Edge {
                from: "s1".into(),
                to: "s2".into(),
                condition: None,
            }],
        };
        let mut state = WorkflowState::new("s1".into());
        state.status = WorkflowStatus::Running;
        state.started_at = Some(chrono::Utc::now());
        // Simulates a crash after s1's transition record was persisted but
        // before current_step was updated to s2.
        state.current_step = "s1".into();
        state.step_history = vec![completed_record(
            "s1",
            "step1 done",
            Transition::To("s2".into()),
        )];
        store.save_workflow(&task_id, &workflow).await.unwrap();
        store.save_workflow_state(&task_id, &state).await.unwrap();

        struct RestoreJudge;
        impl StepTransitionJudge for RestoreJudge {
            fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
                let step_id = ctx.current_step.id().clone();
                Box::pin(async move {
                    if step_id == "s1" {
                        Transition::To("s2".into())
                    } else {
                        Transition::Abort {
                            reason: "done".into(),
                        }
                    }
                })
            }
        }
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("step2 done")]));
        let config = WorkflowEngineConfig {
            client: client.clone(),
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let restored = WorkflowEngine::restore(store, task_id, config, Arc::new(RestoreJudge))
            .await
            .unwrap();

        let result = restored.run().await.unwrap();
        let history = restored.step_history().await;

        assert_eq!(restored.state().await, WorkflowStatus::Succeeded);
        assert_eq!(client.call_count.load(Ordering::SeqCst), 1);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].step_id, "s1");
        assert_eq!(history[1].step_id, "s2");
        assert_eq!(result.final_message.as_deref(), Some("step2 done"));
    }

    #[tokio::test]
    async fn running_restore_continues_from_rework_target() {
        let dir = tempfile::TempDir::new().unwrap();
        let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
        let task_id = TaskId("restore-rework".into());
        let workflow = Workflow {
            entry_step: "s1".into(),
            steps: vec![
                Step::llm("s1", "Step 1", "do step 1", vec![]),
                Step::llm("s2", "Step 2", "do step 2", vec![]),
            ],
            edges: vec![
                Edge {
                    from: "s1".into(),
                    to: "s2".into(),
                    condition: None,
                },
                Edge {
                    from: "s2".into(),
                    to: "s1".into(),
                    condition: None,
                },
            ],
        };
        let mut state = WorkflowState::new("s1".into());
        state.status = WorkflowStatus::Running;
        state.started_at = Some(chrono::Utc::now());
        // Simulates a crash after s2 decided to rework s1 but before
        // current_step was persisted as s1.
        state.current_step = "s2".into();
        state.step_history = vec![
            completed_record("s1", "step1 done", Transition::To("s2".into())),
            completed_record("s2", "needs rework", Transition::To("s1".into())),
        ];
        store.save_workflow(&task_id, &workflow).await.unwrap();
        store.save_workflow_state(&task_id, &state).await.unwrap();

        struct AbortAfterRestoreJudge;
        impl StepTransitionJudge for AbortAfterRestoreJudge {
            fn decide<'a>(&'a self, _ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
                Box::pin(async {
                    Transition::Abort {
                        reason: "done".into(),
                    }
                })
            }
        }
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text(
            "reworked step1",
        )]));
        let config = WorkflowEngineConfig {
            client: client.clone(),
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let restored =
            WorkflowEngine::restore(store, task_id, config, Arc::new(AbortAfterRestoreJudge))
                .await
                .unwrap();

        let result = restored.run().await.unwrap();
        let history = restored.step_history().await;

        assert_eq!(restored.state().await, WorkflowStatus::Succeeded);
        assert_eq!(client.call_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            history
                .iter()
                .map(|record| record.step_id.as_str())
                .collect::<Vec<_>>(),
            vec!["s1", "s2", "s1"]
        );
        assert_eq!(result.final_message.as_deref(), Some("reworked step1"));
    }

    #[tokio::test]
    async fn running_restore_terminal_fail_does_not_rerun_step() {
        let dir = tempfile::TempDir::new().unwrap();
        let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
        let task_id = TaskId("restore-fail".into());
        let workflow = Workflow {
            entry_step: "s1".into(),
            steps: vec![Step::llm("s1", "Step 1", "do step 1", vec![])],
            edges: vec![],
        };
        let mut state = WorkflowState::new("s1".into());
        state.status = WorkflowStatus::Running;
        state.started_at = Some(chrono::Utc::now());
        state.step_history = vec![completed_record(
            "s1",
            "step1 done",
            Transition::Fail {
                reason: "route failed".into(),
            },
        )];
        store.save_workflow(&task_id, &workflow).await.unwrap();
        store.save_workflow_state(&task_id, &state).await.unwrap();

        let client = Arc::new(MockLlmClient::new(vec![]));
        let config = WorkflowEngineConfig {
            client: client.clone(),
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let restored = WorkflowEngine::restore(store, task_id, config, Arc::new(NoopJudge))
            .await
            .unwrap();

        let err = restored.run().await.unwrap_err();
        let history = restored.step_history().await;

        assert!(err.to_string().contains("route failed"));
        assert_eq!(restored.state().await, WorkflowStatus::Failed);
        assert_eq!(client.call_count.load(Ordering::SeqCst), 0);
        assert_eq!(history.len(), 1);
        assert!(matches!(history[0].transition, Transition::Fail { .. }));
    }

    #[tokio::test]
    async fn running_restore_terminal_abort_does_not_rerun_step() {
        let dir = tempfile::TempDir::new().unwrap();
        let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
        let task_id = TaskId("restore-abort".into());
        let workflow = Workflow {
            entry_step: "s1".into(),
            steps: vec![Step::llm("s1", "Step 1", "do step 1", vec![])],
            edges: vec![],
        };
        let mut state = WorkflowState::new("s1".into());
        state.status = WorkflowStatus::Running;
        state.started_at = Some(chrono::Utc::now());
        state.step_history = vec![completed_record(
            "s1",
            "final answer",
            Transition::Abort {
                reason: "done".into(),
            },
        )];
        store.save_workflow(&task_id, &workflow).await.unwrap();
        store.save_workflow_state(&task_id, &state).await.unwrap();

        let client = Arc::new(MockLlmClient::new(vec![]));
        let config = WorkflowEngineConfig {
            client: client.clone(),
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let restored = WorkflowEngine::restore(store, task_id, config, Arc::new(NoopJudge))
            .await
            .unwrap();

        let result = restored.run().await.unwrap();
        let history = restored.step_history().await;

        assert_eq!(restored.state().await, WorkflowStatus::Succeeded);
        assert_eq!(client.call_count.load(Ordering::SeqCst), 0);
        assert_eq!(history.len(), 1);
        assert!(matches!(history[0].transition, Transition::Abort { .. }));
        assert_eq!(result.turns, 1);
        assert_eq!(result.final_message.as_deref(), Some("final answer"));
    }

    #[tokio::test]
    async fn multi_step_linear_workflow() {
        // 2 步线性流程：s1 → s2。
        let dir = tempfile::TempDir::new().unwrap();
        let client = Arc::new(MockLlmClient::new(vec![
            MockResponse::text("step1 done"),
            MockResponse::text("step2 done"),
        ]));
        let workflow = Workflow {
            entry_step: "s1".into(),
            steps: vec![
                Step::llm("s1", "Step 1", "do step 1", vec![]),
                Step::llm("s2", "Step 2", "do step 2", vec![]),
            ],
            edges: vec![Edge {
                from: "s1".into(),
                to: "s2".into(),
                condition: None,
            }],
        };
        // judge：s1 → s2 → Abort。
        struct LinearJudge;
        impl StepTransitionJudge for LinearJudge {
            fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
                let cur = ctx.current_step.id().clone();
                Box::pin(async move {
                    if cur == "s1" {
                        Transition::To("s2".into())
                    } else {
                        Transition::Abort {
                            reason: "done".into(),
                        }
                    }
                })
            }
        }
        let config = WorkflowEngineConfig {
            client,
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let engine = WorkflowEngine::new(workflow, config, Arc::new(LinearJudge)).unwrap();
        let result = engine.run().await.unwrap();
        assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
        assert_eq!(result.turns, 2); // 2 步历史
    }

    #[tokio::test]
    async fn noop_judge_uses_declarative_edge_conditions_by_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("step2 done")]));
        let workflow = Workflow {
            entry_step: "s1".into(),
            steps: vec![
                Step::executor("s1", "Structured Step", "structured", None),
                Step::llm("s2", "Step 2", "do step 2", vec![]),
            ],
            edges: vec![Edge {
                from: "s1".into(),
                to: "s2".into(),
                condition: Some(EdgeCondition::Expr(ConditionExpr::Eq {
                    pointer: "/status".into(),
                    value: serde_json::json!("ok"),
                })),
            }],
        };
        let config = WorkflowEngineConfig {
            client: client.clone(),
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge))
            .unwrap()
            .with_executor("structured", Arc::new(StructuredExecutor));

        let result = engine.run().await.unwrap();
        let history = engine.step_history().await;

        assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
        assert_eq!(client.call_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            history
                .iter()
                .map(|record| record.step_id.as_str())
                .collect::<Vec<_>>(),
            vec!["s1", "s2"]
        );
        assert_eq!(result.final_message.as_deref(), Some("step2 done"));
    }

    #[tokio::test]
    async fn restore_noop_judge_uses_declarative_edge_conditions_by_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
        let task_id = TaskId("restore-declarative".into());
        let workflow = Workflow {
            entry_step: "s1".into(),
            steps: vec![
                Step::executor("s1", "Structured Step", "structured", None),
                Step::llm("s2", "Step 2", "do step 2", vec![]),
            ],
            edges: vec![Edge {
                from: "s1".into(),
                to: "s2".into(),
                condition: Some(EdgeCondition::Expr(ConditionExpr::Eq {
                    pointer: "/status".into(),
                    value: serde_json::json!("ok"),
                })),
            }],
        };
        store.save_workflow(&task_id, &workflow).await.unwrap();

        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("step2 done")]));
        let config = WorkflowEngineConfig {
            client: client.clone(),
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let restored = WorkflowEngine::restore(store, task_id, config, Arc::new(NoopJudge))
            .await
            .unwrap()
            .with_executor("structured", Arc::new(StructuredExecutor));

        let result = restored.run().await.unwrap();
        let history = restored.step_history().await;

        assert_eq!(restored.state().await, WorkflowStatus::Succeeded);
        assert_eq!(client.call_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            history
                .iter()
                .map(|record| record.step_id.as_str())
                .collect::<Vec<_>>(),
            vec!["s1", "s2"]
        );
        assert_eq!(result.final_message.as_deref(), Some("step2 done"));
    }

    #[tokio::test]
    async fn pause_then_run_continues_from_next_step() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = Arc::new(MockLlmClient::new(vec![
            MockResponse::text("step1 done"),
            MockResponse::text("step2 done"),
        ]));
        let workflow = Workflow {
            entry_step: "s1".into(),
            steps: vec![
                Step::llm("s1", "Step 1", "do step 1", vec![]),
                Step::llm("s2", "Step 2", "do step 2", vec![]),
            ],
            edges: vec![Edge {
                from: "s1".into(),
                to: "s2".into(),
                condition: None,
            }],
        };
        let config = WorkflowEngineConfig {
            client: client.clone(),
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let engine = WorkflowEngine::new(workflow, config, Arc::new(TwoStepJudge)).unwrap();
        engine.pause("operator checkpoint").unwrap();

        let err = engine.run().await.unwrap_err();
        let paused = err
            .downcast_ref::<TaskError>()
            .expect("pause should return TaskError::Paused");

        assert!(matches!(paused, TaskError::Paused(reason) if reason == "operator checkpoint"));
        assert_eq!(engine.state().await, WorkflowStatus::Paused);
        assert_eq!(engine.current_step().await, "s2");
        assert_eq!(engine.step_history().await.len(), 1);

        let result = engine.run().await.unwrap();
        let history = engine.step_history().await;

        assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
        assert_eq!(client.call_count.load(Ordering::SeqCst), 2);
        assert_eq!(
            history
                .iter()
                .map(|record| record.step_id.as_str())
                .collect::<Vec<_>>(),
            vec!["s1", "s2"]
        );
        assert_eq!(result.final_message.as_deref(), Some("step2 done"));
    }

    #[tokio::test]
    async fn resume_then_run_does_not_enter_crash_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = Arc::new(MockLlmClient::new(vec![
            MockResponse::text("step1 done"),
            MockResponse::text("step2 done"),
        ]));
        let workflow = Workflow {
            entry_step: "s1".into(),
            steps: vec![
                Step::llm("s1", "Step 1", "do step 1", vec![]),
                Step::llm("s2", "Step 2", "do step 2", vec![]),
            ],
            edges: vec![Edge {
                from: "s1".into(),
                to: "s2".into(),
                condition: None,
            }],
        };
        let config = WorkflowEngineConfig {
            client: client.clone(),
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let engine = WorkflowEngine::new(workflow, config, Arc::new(TwoStepJudge)).unwrap();
        engine.pause("operator checkpoint").unwrap();
        engine.run().await.unwrap_err();

        engine.resume().await.unwrap();

        assert_eq!(
            engine.state().await,
            WorkflowStatus::Paused,
            "resume should not pre-set Running; run() owns the transition"
        );

        let result = engine.run().await.unwrap();
        let history = engine.step_history().await;

        assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
        assert_eq!(client.call_count.load(Ordering::SeqCst), 2);
        assert_eq!(
            history
                .iter()
                .map(|record| record.step_id.as_str())
                .collect::<Vec<_>>(),
            vec!["s1", "s2"]
        );
        assert_eq!(result.final_message.as_deref(), Some("step2 done"));
    }

    #[tokio::test]
    async fn transition_fail_marks_task_failed() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("step1 done")]));
        let workflow = Workflow {
            entry_step: "s1".into(),
            steps: vec![
                Step::llm("s1", "Step 1", "do step 1", vec![]),
                Step::llm("s2", "Step 2", "do step 2", vec![]),
            ],
            edges: vec![Edge {
                from: "s1".into(),
                to: "s2".into(),
                condition: None,
            }],
        };
        struct FailingJudge;
        impl StepTransitionJudge for FailingJudge {
            fn decide<'a>(&'a self, _ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
                Box::pin(async {
                    Transition::Fail {
                        reason: "route failed".into(),
                    }
                })
            }
        }
        let config = WorkflowEngineConfig {
            client,
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let engine = WorkflowEngine::new(workflow, config, Arc::new(FailingJudge)).unwrap();

        let err = engine.run().await.unwrap_err();
        let history = engine.step_history().await;

        assert_eq!(engine.state().await, WorkflowStatus::Failed);
        assert!(err.to_string().contains("route failed"));
        assert_eq!(history.len(), 1);
        assert!(matches!(history[0].transition, Transition::Fail { .. }));
    }

    #[tokio::test]
    async fn mixed_llm_and_executor_workflow() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("step1 done")]));
        let workflow = Workflow {
            entry_step: "s1".into(),
            steps: vec![
                Step::llm("s1", "Step 1", "do step 1", vec![]),
                Step::executor(
                    "s2",
                    "Executor Step",
                    "fixed",
                    Some(serde_json::json!({"suffix": "done"})),
                ),
            ],
            edges: vec![Edge {
                from: "s1".into(),
                to: "s2".into(),
                condition: None,
            }],
        };
        struct LinearJudge;
        impl StepTransitionJudge for LinearJudge {
            fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
                let cur = ctx.current_step.id().clone();
                Box::pin(async move {
                    if cur == "s1" {
                        Transition::To("s2".into())
                    } else {
                        Transition::Abort {
                            reason: "done".into(),
                        }
                    }
                })
            }
        }
        let config = WorkflowEngineConfig {
            client: client.clone(),
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let engine = WorkflowEngine::new(workflow, config, Arc::new(LinearJudge))
            .unwrap()
            .with_executor("fixed", Arc::new(FixedExecutor));

        let result = engine.run().await.unwrap();

        assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
        assert_eq!(result.turns, 2);
        assert!(
            result
                .final_message
                .as_deref()
                .unwrap_or("")
                .contains("executor done after step1 done")
        );
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn builtin_json_transform_executor_runs_without_manual_registration() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = Arc::new(MockLlmClient::new(vec![]));
        let workflow = Workflow {
            entry_step: "transform".into(),
            steps: vec![Step::executor(
                "transform",
                "Transform",
                "json_transform",
                Some(serde_json::json!({
                    "source": "config.input",
                    "input": {
                        "measurement": {
                            "voltage": 3.3,
                            "unit": "V"
                        }
                    },
                    "fields": {
                        "voltage": "/measurement/voltage",
                        "unit": "/measurement/unit"
                    }
                })),
            )],
            edges: vec![],
        };
        let config = WorkflowEngineConfig {
            client: client.clone(),
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge)).unwrap();

        let result = engine.run().await.unwrap();
        let history = engine.step_history().await;

        assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
        assert_eq!(result.turns, 1);
        assert_eq!(
            history[0]
                .result
                .as_ref()
                .and_then(|r| r.structured.clone()),
            Some(serde_json::json!({"unit": "V", "voltage": 3.3}))
        );
        assert_eq!(
            client.call_count.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[tokio::test]
    async fn step_policy_retries_executor_failure() {
        let dir = tempfile::TempDir::new().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let client = Arc::new(MockLlmClient::new(vec![]));
        let workflow = Workflow {
            entry_step: "flaky".into(),
            steps: vec![Step::executor("flaky", "Flaky", "flaky", None).with_policy(
                crate::workflow::model::StepExecutionPolicy {
                    timeout_ms: Some(1_000),
                    max_attempts: Some(2),
                    retry_backoff_ms: Some(0),
                },
            )],
            edges: vec![],
        };
        let config = WorkflowEngineConfig {
            client,
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge))
            .unwrap()
            .with_executor(
                "flaky",
                Arc::new(FlakyExecutor {
                    attempts: attempts.clone(),
                }),
            );

        let result = engine.run().await.unwrap();
        let history = engine.step_history().await;

        assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0].result.as_ref().unwrap().output,
            "ok on attempt 2"
        );
        assert_eq!(result.turns, 1);
    }

    #[tokio::test]
    async fn step_policy_times_out_blocking_executor() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = Arc::new(MockLlmClient::new(vec![]));
        let workflow = Workflow {
            entry_step: "blocked".into(),
            steps: vec![
                Step::executor("blocked", "Blocked", "blocking", None).with_policy(
                    crate::workflow::model::StepExecutionPolicy {
                        timeout_ms: Some(20),
                        max_attempts: Some(1),
                        retry_backoff_ms: Some(0),
                    },
                ),
            ],
            edges: vec![],
        };
        let config = WorkflowEngineConfig {
            client,
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge))
            .unwrap()
            .with_executor("blocking", Arc::new(BlockingExecutor));

        let err = engine.run().await.unwrap_err();

        assert_eq!(engine.state().await, WorkflowStatus::Failed);
        assert!(err.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn unregistered_executor_fails_task() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = Arc::new(MockLlmClient::new(vec![]));
        let workflow = Workflow {
            entry_step: "exec".into(),
            steps: vec![Step::executor("exec", "Executor Step", "missing", None)],
            edges: vec![],
        };
        let config = WorkflowEngineConfig {
            client,
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let engine = WorkflowEngine::new(workflow, config, Arc::new(NoopJudge)).unwrap();

        let err = engine.run().await.unwrap_err();

        assert_eq!(engine.state().await, WorkflowStatus::Failed);
        assert!(
            err.to_string()
                .contains("executor 'missing' not registered")
        );
    }

    #[tokio::test]
    async fn cancel_executor_step_marks_task_cancelled() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = Arc::new(MockLlmClient::new(vec![]));
        let workflow = Workflow {
            entry_step: "exec".into(),
            steps: vec![Step::executor("exec", "Executor Step", "blocking", None)],
            edges: vec![],
        };
        let config = WorkflowEngineConfig {
            client,
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let engine = Arc::new(
            WorkflowEngine::new(workflow, config, Arc::new(NoopJudge))
                .unwrap()
                .with_executor("blocking", Arc::new(BlockingExecutor)),
        );
        let runner = {
            let engine = engine.clone();
            tokio::spawn(async move { engine.run().await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        engine.cancel("stop").await.unwrap();

        let result = runner.await.unwrap().unwrap();

        assert_eq!(engine.state().await, WorkflowStatus::Cancelled);
        assert_eq!(result.turns, 0);
        assert!(result.final_message.is_none());
    }

    #[tokio::test]
    async fn cancel_then_restore_remains_cancelled() {
        let dir = tempfile::TempDir::new().unwrap();
        let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
        let client = Arc::new(MockLlmClient::new(vec![]));
        let workflow = Workflow {
            entry_step: "exec".into(),
            steps: vec![Step::executor("exec", "Executor Step", "blocking", None)],
            edges: vec![],
        };
        let config = WorkflowEngineConfig {
            client: client.clone(),
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let engine = Arc::new(
            WorkflowEngine::new(workflow, config, Arc::new(NoopJudge))
                .unwrap()
                .with_task_store(store.clone())
                .with_executor("blocking", Arc::new(BlockingExecutor)),
        );
        let task_id = engine.task_id().clone();
        let runner = {
            let engine = engine.clone();
            tokio::spawn(async move { engine.run().await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        engine.cancel("stop").await.unwrap();

        let result = runner.await.unwrap().unwrap();

        assert_eq!(engine.state().await, WorkflowStatus::Cancelled);
        assert_eq!(result.turns, 0);

        let restored = WorkflowEngine::restore(
            store,
            task_id,
            WorkflowEngineConfig {
                client,
                model: "mock-model".into(),
                env_factory: Arc::new(TestEnvFactory),
                session_factory: Arc::new(JsonlSessionFactory),
                session_base_dir: dir.path().join("sessions-restore"),
            },
            Arc::new(NoopJudge),
        )
        .await
        .unwrap();
        let history = restored.step_history().await;

        assert_eq!(restored.state().await, WorkflowStatus::Cancelled);
        assert_eq!(history.len(), 1);
        assert!(history[0].result.is_none());
        assert!(matches!(history[0].transition, Transition::Abort { .. }));
    }

    #[tokio::test]
    async fn cancel_llm_step_discards_partial_result() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut response = MockResponse::tool_use("block_1", "block_forever", "{}");
        response.events.insert(
            0,
            Ok(llm_harness_loop::AdapterStreamEvent::TextDelta {
                index: 0,
                text: "partial output".into(),
            }),
        );
        let client = Arc::new(MockLlmClient::new(vec![response]));
        let call_count = client.call_count.clone();
        let config = WorkflowEngineConfig {
            client,
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let engine = Arc::new(
            WorkflowEngine::new_free_task(
                "stream partial then block".into(),
                vec!["block_forever".into()],
                config,
            )
            .unwrap()
            .with_tool(Arc::new(BlockingTool)),
        );
        let runner = {
            let engine = engine.clone();
            tokio::spawn(async move { engine.run().await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while call_count.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        engine.cancel("stop").await.unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), runner)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let history = engine.step_history().await;

        assert_eq!(engine.state().await, WorkflowStatus::Cancelled);
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
        assert_eq!(result.turns, 0);
        assert!(result.final_message.is_none());
        assert_eq!(history.len(), 1);
        assert!(history[0].result.is_none());
        assert!(matches!(history[0].transition, Transition::Abort { .. }));
    }

    // ── WorkflowContext 共享状态测试 ──────────────────────────────

    /// executor 写 context，后续 executor 读到写入的值。
    struct ContextWriter;
    impl StepExecutor for ContextWriter {
        fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
            Box::pin(async move {
                let mut ctx_data = ctx.context.lock().await;
                ctx_data
                    .variables
                    .insert("focus".into(), serde_json::json!(0.42));
                ctx_data
                    .variables
                    .insert("metro_p".into(), serde_json::json!(1.5));
                drop(ctx_data);
                Ok(StepResult {
                    output: "wrote context".into(),
                    structured: None,
                    tool_calls_count: 0,
                    session_id: String::new(),
                    cost: CostAggregate::default(),
                    started_at: None,
                    ended_at: None,
                })
            })
        }
    }

    /// executor 读 context，验证上游 executor 写入的值。
    struct ContextReader;
    impl StepExecutor for ContextReader {
        fn execute<'a>(&'a self, ctx: &'a ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>> {
            Box::pin(async move {
                let ctx_data = ctx.context.lock().await;
                let focus = ctx_data.variables.get("focus").cloned();
                let metro_p = ctx_data.variables.get("metro_p").cloned();
                drop(ctx_data);
                Ok(StepResult {
                    output: format!("focus={focus:?} metro_p={metro_p:?}"),
                    structured: Some(serde_json::json!({
                        "focus": focus,
                        "metro_p": metro_p,
                    })),
                    tool_calls_count: 0,
                    session_id: String::new(),
                    cost: CostAggregate::default(),
                    started_at: None,
                    ended_at: None,
                })
            })
        }
    }

    #[tokio::test]
    async fn executor_writes_context_and_downstream_reads_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = Arc::new(MockLlmClient::new(vec![]));
        let workflow = Workflow {
            entry_step: "writer".into(),
            steps: vec![
                Step::executor("writer", "Writer", "ctx_writer", None),
                Step::executor("reader", "Reader", "ctx_reader", None),
            ],
            edges: vec![Edge {
                from: "writer".into(),
                to: "reader".into(),
                condition: None,
            }],
        };
        struct LinearJudge;
        impl StepTransitionJudge for LinearJudge {
            fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
                let id = ctx.current_step.id().clone();
                Box::pin(async move {
                    if id == "writer" {
                        Transition::To("reader".into())
                    } else {
                        Transition::Abort {
                            reason: "done".into(),
                        }
                    }
                })
            }
        }
        let config = WorkflowEngineConfig {
            client,
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let engine = WorkflowEngine::new(workflow, config, Arc::new(LinearJudge))
            .unwrap()
            .with_executor("ctx_writer", Arc::new(ContextWriter))
            .with_executor("ctx_reader", Arc::new(ContextReader));

        let result = engine.run().await.unwrap();
        let history = engine.step_history().await;

        assert_eq!(engine.state().await, WorkflowStatus::Succeeded);
        // reader 的 structured 应包含 writer 写入的值。
        use serde_json::Value;
        let reader_structured: Value = history[1]
            .result
            .as_ref()
            .and_then(|r| r.structured.clone())
            .unwrap();
        assert_eq!(reader_structured["focus"], serde_json::json!(0.42));
        assert_eq!(reader_structured["metro_p"], serde_json::json!(1.5));
        assert!(result.final_message.unwrap().contains("focus"));
    }

    #[tokio::test]
    async fn context_survives_crash_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
        let client = Arc::new(MockLlmClient::new(vec![]));
        let workflow = Workflow {
            entry_step: "writer".into(),
            steps: vec![
                Step::executor("writer", "Writer", "ctx_writer", None),
                Step::executor("reader", "Reader", "ctx_reader", None),
            ],
            edges: vec![Edge {
                from: "writer".into(),
                to: "reader".into(),
                condition: None,
            }],
        };
        struct LinearJudge;
        impl StepTransitionJudge for LinearJudge {
            fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
                let id = ctx.current_step.id().clone();
                Box::pin(async move {
                    if id == "writer" {
                        Transition::To("reader".into())
                    } else {
                        Transition::Abort {
                            reason: "done".into(),
                        }
                    }
                })
            }
        }
        let config = WorkflowEngineConfig {
            client,
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let engine = WorkflowEngine::new(workflow, config, Arc::new(LinearJudge))
            .unwrap()
            .with_executor("ctx_writer", Arc::new(ContextWriter))
            .with_executor("ctx_reader", Arc::new(ContextReader))
            .with_task_store(store.clone());
        let task_id = engine.task_id().clone();

        // 從獨立 context 寫入，再手動觸發 persist。
        // persist_state 內部會從 self.context 取 snapshot 覆蓋 state.context。
        engine
            .context()
            .lock()
            .await
            .variables
            .insert("survives".into(), serde_json::json!(true));
        // save_workflow + persist_state（模拟 run() 启动时的落盘）。
        store
            .save_workflow(&task_id, &engine.workflow)
            .await
            .unwrap();
        // 手动触发一次 persist（模拟 run() 启动时的 persist）。
        let snapshot = engine.workflow_state.lock().await.clone();
        engine.persist_state(&snapshot).await.unwrap();

        // restore 后 context 应包含写入的值。
        let config2 = WorkflowEngineConfig {
            client: Arc::new(MockLlmClient::new(vec![])),
            model: "mock-model".into(),
            env_factory: Arc::new(TestEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: dir.path().join("sessions"),
        };
        let restored = WorkflowEngine::restore(store, task_id, config2, Arc::new(LinearJudge))
            .await
            .unwrap();
        let ctx_arc = restored.context();
        let ctx = ctx_arc.lock().await;
        assert_eq!(
            ctx.variables.get("survives"),
            Some(&serde_json::json!(true))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_cost_merges_per_model_cache_tokens() {
        use crate::workflow::model::{StepRecord, StepResult, Transition};
        use llm_harness_types::{CostAggregate, ModelCost};

        let now = chrono::Utc::now();
        let mk_record = |read: u64, write: u64| StepRecord {
            step_id: "s1".to_string(),
            started_at: now,
            ended_at: now,
            result: Some(StepResult {
                output: String::new(),
                structured: None,
                tool_calls_count: 0,
                session_id: String::new(),
                cost: CostAggregate {
                    total_cache_read_tokens: read,
                    total_cache_write_tokens: write,
                    by_model: {
                        let mut m = std::collections::HashMap::new();
                        m.insert(
                            "anthropic/claude-sonnet".to_string(),
                            ModelCost {
                                input_tokens: 0,
                                output_tokens: 0,
                                cache_read_tokens: read,
                                cache_write_tokens: write,
                                cost: 0.0,
                                call_count: 1,
                            },
                        );
                        m
                    },
                    ..Default::default()
                },
                started_at: None,
                ended_at: None,
            }),
            transition: Transition::To("s2".to_string()),
        };

        let history = vec![mk_record(200, 100), mk_record(300, 50)];
        let total = aggregate_cost(&history);

        let mc = total
            .by_model
            .get("anthropic/claude-sonnet")
            .expect("merged by_model entry");
        assert_eq!(
            mc.cache_read_tokens, 500,
            "per-model cache_read must sum across steps"
        );
        assert_eq!(
            mc.cache_write_tokens, 150,
            "per-model cache_write must sum across steps"
        );
        assert_eq!(mc.call_count, 2);
        // Top-level totals must also reflect the merge.
        assert_eq!(total.total_cache_read_tokens, 500);
        assert_eq!(total.total_cache_write_tokens, 150);
    }
}
