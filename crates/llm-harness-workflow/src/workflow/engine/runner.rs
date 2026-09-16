use super::*;
use crate::workflow::engine::structured::{
    DEFAULT_MAX_UPGRADE_ATTEMPTS, StructuredOutputCoordinator,
};
use crate::workflow::model::LoopConfig;

// ── 自由函数 ─────────────────────────────────────────────────────────────

/// 聚合 step_history 中所有 StepResult 的 cost。
/// 纯函数：run() 的 Abort arm（已持锁）和 total_cost()（异步获锁后）共用。
pub(super) fn aggregate_cost(history: &[StepRecord]) -> CostAggregate {
    let mut total = CostAggregate::default();
    for record in history {
        if let Some(r) = &record.result {
            let total_has_usage = total.total_input_tokens != 0
                || total.total_output_tokens != 0
                || !total.by_model.is_empty();
            let source_has_usage = r.cost.total_input_tokens != 0
                || r.cost.total_output_tokens != 0
                || !r.cost.by_model.is_empty();
            match (
                total.billing_usage_profile.as_ref(),
                r.cost.billing_usage_profile.as_ref(),
            ) {
                (None, Some(profile))
                    if !total_has_usage && total.billing_usage_violations == 0 =>
                {
                    total.billing_usage_profile = Some(*profile);
                }
                (None, Some(_)) if source_has_usage => {
                    total.billing_usage_violations += 1;
                }
                (Some(current), Some(incoming)) if current != incoming => {
                    total.billing_usage_profile = None;
                    total.billing_usage_violations += 1;
                }
                (Some(_), None) if source_has_usage => {
                    total.billing_usage_profile = None;
                    total.billing_usage_violations += 1;
                }
                (None, None) if source_has_usage => {
                    total.billing_usage_violations += 1;
                }
                _ => {}
            }
            total.billing_usage_violations += r.cost.billing_usage_violations;
            total.usage_reports.merge(&r.cost.usage_reports);
            total.total_input_tokens += r.cost.total_input_tokens;
            total.total_output_tokens += r.cost.total_output_tokens;
            total.total_cache_read_tokens += r.cost.total_cache_read_tokens;
            total.total_cache_write_tokens += r.cost.total_cache_write_tokens;
            total.total_reasoning_tokens += r.cost.total_reasoning_tokens;
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
pub(super) fn count_executed_turns(history: &[StepRecord]) -> u32 {
    history.iter().filter(|r| r.result.is_some()).count() as u32
}

/// BeforeRunHook：在 system prompt 末尾追加结构化输出约束（Layer 1）。
///
/// 经 `BeforeRunCtx.system_prompt` 追加，保留 customize_builder 已设的 system prompt。
fn structured_prompt_hook(segment: &'static str) -> Arc<dyn llm_harness_types::BeforeRunHook> {
    Arc::new(StructuredPromptHook { segment })
}

/// 配合 [`structured_prompt_hook`]：把结构化输出约束追加到 system prompt。
struct StructuredPromptHook {
    segment: &'static str,
}

impl llm_harness_types::BeforeRunHook for StructuredPromptHook {
    fn before_run<'a>(
        &'a self,
        ctx: llm_harness_types::BeforeRunCtx<'a>,
    ) -> futures::future::BoxFuture<
        'a,
        std::result::Result<llm_harness_types::BeforeRunResult, llm_harness_types::AgentError>,
    > {
        Box::pin(async move {
            let existing = ctx.system_prompt.take().unwrap_or_default();
            // Idempotent: only append if segment is not already present.
            // This prevents unbounded growth when continue_run calls
            // before_run_hook repeatedly (each sub-agent completion wake).
            if !existing.contains(self.segment) {
                *ctx.system_prompt = Some(format!("{existing}{seg}", seg = self.segment));
            } else {
                *ctx.system_prompt = Some(existing);
            }
            Ok(llm_harness_types::BeforeRunResult {
                additional_messages: vec![],
                system_prompt: None,
            })
        })
    }
}

// ── impl WorkflowEngine ─────────────────────────────────────────────────

impl super::WorkflowEngine {
    // ── 构造 ───────────────────────────────────────────────────────────

    /// 多步 workflow 构造器。task_id 由引擎内部生成。
    /// 内部调 validate_workflow 校验；若有 task_store，调 save_workflow 落盘。
    /// 校验失败 → Err（非法 plan 不进引擎）。
    pub fn new(
        workflow: Workflow,
        config: WorkflowEngineConfig,
        judge: Arc<dyn StepTransitionJudge>,
    ) -> Result<Self, WorkflowError> {
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
    ) -> Result<Self, WorkflowError> {
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
    ) -> Result<Self, WorkflowError> {
        let (engine, _state) = Self::load_and_build(store, task_id, config, judge).await?;
        Ok(engine)
    }

    /// 从指定 step 恢复并重跑（`--restart-from` 场景）。
    ///
    /// 加载 task store 中 task_id 的完整上下文（workflow + state + checkpoints），
    /// 截断 step_history 中目标 step 及其下游所有 record，将 current_step 指向 step，
    /// status 置 Paused。调用方随后调 [`WorkflowEngine::run`] 从 step 重新执行。
    ///
    /// # 语义
    /// - step_history：保留目标 step 之前的上游 record，截断目标 step 及下游。
    /// - context 黑板：不回滚，保留当前累积值（无法恢复到 step 执行前快照）。
    /// - task_id / planned_by / checkpoints：全部从 store 恢复。
    /// - status 置 Paused，run() 不会用历史末尾 transition 覆写 current_step。
    ///
    /// # 错误
    /// - [`WorkflowError::WorkflowNotFound`]：workflow 或 state 不在 store。
    /// - [`WorkflowError::CurrentStepNotFound`]：step 不在 workflow steps。
    /// - [`WorkflowError::StepNotInHistory`]：step 从未执行过（不在 step_history）。
    pub async fn restore_from_step(
        store: Arc<dyn TaskStore>,
        task_id: TaskId,
        step: impl Into<StepId>,
        config: WorkflowEngineConfig,
        judge: Arc<dyn StepTransitionJudge>,
    ) -> Result<Self, WorkflowError> {
        let target: StepId = step.into();
        let (engine, mut state) = Self::load_and_build(store, task_id, config, judge).await?;

        // 校验 target step 存在于 workflow。
        if !engine.workflow.steps.iter().any(|s| s.id() == &target) {
            return Err(WorkflowError::CurrentStepNotFound {
                step_id: target.clone(),
            });
        }

        // 截断 step_history：删除 target step 及其下游所有 record。
        // target 必须在 step_history 中（回退重跑语义要求该步曾执行过）。
        let truncate_pos = state.step_history.iter().rposition(|r| r.step_id == target);
        let pos = truncate_pos.ok_or_else(|| WorkflowError::StepNotInHistory {
            step_id: target.clone(),
        })?;
        state.step_history.truncate(pos);

        // 改写 current_step + status 置 Paused。
        state.current_step = target;
        state.status = WorkflowStatus::Paused;
        state.reason = None;
        state.result = None;
        state.error = None;
        state.ended_at = None;

        // 把改写后的 state 写回 engine 的 workflow_state + persist 落盘。
        {
            let mut ws = engine.workflow_state.lock().await;
            *ws = state.clone();
        }
        engine.persist_state(&state).await?;

        Ok(engine)
    }

    /// restore / restore_from_step 共用的加载逻辑：
    /// load workflow + state + checkpoints，validate，组装 engine。
    /// 返回 (engine, 加载到的 state)。调用方可对 state 做后处理后再 persist。
    async fn load_and_build(
        store: Arc<dyn TaskStore>,
        task_id: TaskId,
        config: WorkflowEngineConfig,
        judge: Arc<dyn StepTransitionJudge>,
    ) -> Result<(Self, WorkflowState), WorkflowError> {
        let workflow = store.load_workflow(&task_id).await?.ok_or_else(|| {
            WorkflowError::WorkflowNotFound {
                task_id: task_id.0.clone(),
            }
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
            state.clone(),
        );
        // 从 store 重载 checkpoints 到内存缓存。
        if let Ok(cps) = store.list_checkpoints(&engine.task_id).await {
            *engine.checkpoints.lock().await = cps;
        }
        Ok((engine, state))
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
            step_builder_customizers: HashMap::new(),
            running: AtomicBool::new(false),
            planned_by: None,
        }
    }

    // ── builder 方法 ───────────────────────────────────────────────────

    pub fn task_id(&self) -> &TaskId {
        &self.task_id
    }

    #[must_use]
    pub fn with_tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.extra_tools.push(tool);
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

    /// 设置 per-step builder 定制闭包。仅对指定 step 生效。
    /// 在共享闭包之后、step plugin 注册之前执行。
    #[must_use]
    pub fn with_step_builder(
        mut self,
        step_id: impl Into<String>,
        customize: impl Fn(HarnessBuilder) -> HarnessBuilder + Send + Sync + 'static,
    ) -> Self {
        self.step_builder_customizers
            .insert(step_id.into(), Arc::new(customize));
        self
    }

    /// 设置共享 builder 定制闭包。在 `run()` 之前调用；`run_llm_step` 每步读取此值。
    ///
    /// 供 Python 绑定等外部封装层在引擎构建后注入 hooks/max_tokens 等配置。
    pub fn set_customize_builder(&mut self, customize: BuilderCustomize) {
        self.config.customize_builder = Some(customize);
    }

    /// 获取共享 builder 定制闭包的引用。
    pub fn config_customize_builder(&self) -> &Option<BuilderCustomize> {
        &self.config.customize_builder
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
    ///
    /// 此上限与 `StepExecutionPolicy.max_attempts` 独立：judge Retry 触发后，
    /// 每次重跑仍会执行完整的 policy retry 循环。最坏情况单步执行次数 = max_retries × max_attempts。
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

    // ── 运行循环 ───────────────────────────────────────────────────────

    /// 启动/恢复执行，不向 LLM steps 注入 typed request extensions。
    pub async fn run(&self) -> Result<TaskResult, WorkflowError> {
        self.run_with_request(WorkflowRunRequest::default()).await
    }

    /// 使用本次调用的可信 typed context 启动/恢复执行。
    ///
    /// 请求只沿当前调用链传给 LLM step 的 `RunContext`，不会写入
    /// `WorkflowEngine`、checkpoint 或持久化状态。
    pub async fn run_with_request(
        &self,
        request: WorkflowRunRequest,
    ) -> Result<TaskResult, WorkflowError> {
        enum StartupAction {
            Continue,
            Return(TaskResult),
            Fail(String),
        }

        // 重入守卫：防止并发 run() 覆盖写。
        if self.running.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Err(WorkflowError::AlreadyRunning);
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
                            Transition::Pause { reason } => {
                                // 上次进程在 judge 返回 Pause 后异常退出。
                                // 状态已为 Paused，无需覆盖。
                                action = StartupAction::Fail(reason);
                            }
                        }
                    }
                }
                _ => {
                    self.running
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    return Err(WorkflowError::InvalidStatus {
                        status: state.status.clone(),
                    });
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
            StartupAction::Continue => self.run_loop(&request).await,
            StartupAction::Return(result) => Ok(result),
            StartupAction::Fail(error) => {
                let _ = self.event_tx.send(WorkflowEvent::Failed {
                    error: error.clone(),
                });
                Err(WorkflowError::TaskFailed(error))
            }
        }
    }

    /// Resolve the cache key for a step from the current `WorkflowContext`.
    /// Returns `None` if the step has no cache policy or the pointer doesn't resolve.
    async fn resolve_cache_key(&self, step: &Step) -> Option<serde_json::Value> {
        let cache = step.policy()?.cache.as_ref()?;
        let ctx = self.context.lock().await;
        let ctx_value = serde_json::Value::Object(ctx.variables.clone());
        ctx_value.pointer(&cache.key_pointer).cloned()
    }

    /// Search `step_history` (reverse) for a cached result matching the given key.
    /// A match requires: same `step_id`, `cache_key` equals `key`, has a `result`,
    /// and within TTL (if configured).
    async fn find_cached_result(&self, step: &Step, key: &serde_json::Value) -> Option<StepResult> {
        let cache = step.policy()?.cache.as_ref()?;
        let now = chrono::Utc::now();
        let state = self.workflow_state.lock().await;
        for record in state.step_history.iter().rev() {
            if record.step_id != *step.id() {
                continue;
            }
            if record.cache_key.as_ref() != Some(key) {
                continue;
            }
            let Some(result) = record.result.as_ref() else {
                continue;
            };
            if let Some(ttl) = cache.ttl_secs {
                let elapsed = now.signed_duration_since(record.ended_at);
                if elapsed.num_seconds() > ttl as i64 {
                    continue;
                }
            }
            return Some(result.clone());
        }
        None
    }

    fn executor_for_step(
        &self,
        step: &Step,
    ) -> Result<Option<Arc<dyn StepExecutor>>, WorkflowError> {
        let Step::Executor { executor_name, .. } = step else {
            return Ok(None);
        };
        self.step_executors
            .get(executor_name)
            .cloned()
            .map(Some)
            .ok_or_else(|| WorkflowError::ExecutorNotFound {
                name: executor_name.clone(),
            })
    }

    fn validate_executor_step(&self, step: &Step) -> Result<(), WorkflowError> {
        if let Some(executor) = self.executor_for_step(step)? {
            executor
                .validate_step(step)
                .map_err(|source| WorkflowError::StepFailed {
                    id: step.id().to_string(),
                    source,
                })?;
        }
        Ok(())
    }

    fn executor_cancellation_policy(
        &self,
        step: &Step,
    ) -> Result<ExecutorCancellationPolicy, WorkflowError> {
        Ok(self
            .executor_for_step(step)?
            .map(|executor| executor.cancellation_policy())
            .unwrap_or_default())
    }

    /// run() 的主循环。拆出以降低 run() 圈复杂度。
    async fn run_loop(&self, request: &WorkflowRunRequest) -> Result<TaskResult, WorkflowError> {
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
                .ok_or_else(|| WorkflowError::CurrentStepNotFound {
                    step_id: current_step_id.clone(),
                })?
                .clone();

            // max_steps 安全阀。
            if history_len >= self.max_steps {
                return self
                    .fail_task(format!("max_steps ({}) exceeded", self.max_steps))
                    .await;
            }

            // Executor-owned policy must be checked before cache lookup because
            // a cache hit skips execute() entirely.
            if let Err(error) = self.validate_executor_step(&step) {
                return self
                    .fail_task(format!("step '{}' failed: {}", step.id(), error))
                    .await;
            }

            let _ = self.event_tx.send(WorkflowEvent::StepStarted {
                step_id: step.id().clone(),
                step_name: step.name().to_string(),
            });

            // 3. 构造 abort token，并与 cancel() 原子交接。
            //
            // current_abort 与 workflow_state 始终按这个顺序加锁。这样 cancel()
            // 要么拿到本 token 并取消它，要么先把状态置为 Cancelled；不会出现
            // cancel 已完成、run_loop 随后才注册一个永远收不到取消信号的 token。
            let step_abort = CancellationToken::new();
            let cancelled_result = {
                let mut ca = self.current_abort.lock().await;
                let state = self.workflow_state.lock().await;
                if state.status == WorkflowStatus::Cancelled {
                    Some(state.result.clone().unwrap_or_else(|| TaskResult {
                        cost: aggregate_cost(&state.step_history),
                        turns: count_executed_turns(&state.step_history),
                        final_message: None,
                    }))
                } else {
                    *ca = Some(step_abort.clone());
                    None
                }
            };
            if let Some(result) = cancelled_result {
                return Ok(result);
            }

            // 4. Resolve cache key + check for a cached result (issue #41).
            //    If the step has a cache policy and a previous successful
            //    execution with the same key value exists in step_history,
            //    reuse its StepResult and skip execution.
            let cache_key = self.resolve_cache_key(&step).await;
            let cached_result = if let Some(ref key) = cache_key {
                self.find_cached_result(&step, key).await
            } else {
                None
            };

            // 5. Execute the step (or reuse cached result on cache hit).
            let step_result = match cached_result {
                Some(cached) => cached,
                None => match self
                    .run_step_with_policy(&step, step_abort.clone(), request)
                    .await
                {
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
                },
            };
            let _ = self.event_tx.send(WorkflowEvent::StepFinished {
                step_id: step.id().clone(),
                result: step_result.clone(),
            });

            // 6. 据 Transition 更新 state + 记录 step_history。
            //    apply_transition 内部检查 status==Cancelled 守卫：
            //    若 cancel() 已抢先置 Cancelled，则不推 record、不覆盖状态。
            let transition = self.decide_transition(&step, &step_result).await;
            let next_step = self
                .apply_transition(&step, step_result, transition.clone(), cache_key)
                .await?;

            // 7. 检查 cancel 守卫：cancel() 可能在 run_step 期间抢先置 Cancelled。
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
                (Transition::To(next), override_next) => {
                    // LoopConfig 续环/退出（apply_transition 返回的 next_step）
                    // 优先于 judge 的原始 next：耗尽时 exit_route 覆盖 judge 路由。
                    let target = override_next.unwrap_or(next);
                    let mut state = self.workflow_state.lock().await;
                    state.current_step = target;
                    let snapshot = state.clone();
                    drop(state);
                    self.persist_state(&snapshot).await?;
                    self.pause_if_requested().await?;
                }
                (Transition::Retry, next) => {
                    // current_step 不变，重跑。LoopConfig 在场时 apply_transition
                    // 可能返回目标 stage（循环跳转/退出路由），需跟随。
                    if let Some(target) = next {
                        let mut state = self.workflow_state.lock().await;
                        state.current_step = target;
                        let snapshot = state.clone();
                        drop(state);
                        self.persist_state(&snapshot).await?;
                    }
                    self.pause_if_requested().await?;
                }
                (Transition::Pause { reason }, _) => {
                    // judge 主动暂停（对齐 LangGraph interrupt）。
                    // 状态已在 apply_transition 中置为 Paused，这里发事件 + bail。
                    let _ = self.event_tx.send(WorkflowEvent::Paused {
                        reason: reason.clone(),
                    });
                    return Err(WorkflowError::Paused(reason));
                }
            }
        }
    }

    async fn pause_if_requested(&self) -> Result<(), WorkflowError> {
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
        Err(WorkflowError::Paused(reason))
    }

    /// 判断 transition：自由 task 直接 Abort，否则调 judge.decide。
    async fn decide_transition(&self, step: &Step, step_result: &StepResult) -> Transition {
        // Terminal steps short-circuit: no judge invocation.
        if let Step::Terminal {
            exit_code, message, ..
        } = step
        {
            if *exit_code == 0 {
                return Transition::Abort {
                    reason: message
                        .clone()
                        .unwrap_or_else(|| "terminal: success".into()),
                };
            }
            return Transition::Fail {
                reason: message
                    .clone()
                    .unwrap_or_else(|| format!("terminal: exit_code {}", exit_code)),
            };
        }

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
    async fn fail_task(&self, error_msg: String) -> Result<TaskResult, WorkflowError> {
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
        Err(WorkflowError::TaskFailed(error_msg))
    }

    // ── 步骤执行 ───────────────────────────────────────────────────────

    /// Dispatch one workflow step by kind.
    async fn run_step(
        &self,
        step: &Step,
        step_abort: CancellationToken,
        policy_attempt: usize,
        request: &WorkflowRunRequest,
    ) -> Result<StepResult, WorkflowError> {
        match step {
            Step::Llm { .. } => {
                self.run_llm_step(step, step_abort, policy_attempt, request)
                    .await
            }
            Step::Executor { .. } => {
                self.run_executor_step(step, step_abort, policy_attempt)
                    .await
            }
            Step::Terminal {
                exit_code, message, ..
            } => {
                // Terminal steps short-circuit: no LLM call, no executor.
                // The actual success/fail transition is handled in
                // decide_transition via the terminal check.
                let _ = step_abort;
                Ok(StepResult {
                    output: message.clone().unwrap_or_default(),
                    structured: Some(serde_json::json!({
                        "exit_code": exit_code,
                        "message": message,
                    })),
                    structured_status: StructuredStatus::Ok,
                    tool_calls_count: 0,
                    session_id: String::new(),
                    cost: CostAggregate::default(),
                    started_at: None,
                    ended_at: None,
                })
            }
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
        request: &WorkflowRunRequest,
    ) -> Result<StepResult, WorkflowError> {
        let policy = step.policy().cloned().unwrap_or_default();
        let max_attempts = policy.max_attempts.unwrap_or(1).max(1);
        let timeout = policy.timeout_ms.map(Duration::from_millis);
        let retry_backoff = Duration::from_millis(policy.retry_backoff_ms.unwrap_or(0));

        for attempt in 1..=max_attempts {
            if step_abort.is_cancelled() {
                return Err(WorkflowError::StepAborted {
                    id: step.id().to_string(),
                });
            }
            let attempt_abort = step_abort.child_token();
            let result = self
                .run_step_attempt(
                    step,
                    attempt_abort.clone(),
                    timeout,
                    attempt as usize,
                    request,
                )
                .await;
            match result {
                Ok(result) => return Ok(result),
                Err(err) => {
                    attempt_abort.cancel();
                    if step_abort.is_cancelled() {
                        return Err(err);
                    }
                    if matches!(&err, WorkflowError::ExecutorRecoveryFailed { .. }) {
                        return Err(err);
                    }
                    if attempt >= max_attempts {
                        if max_attempts == 1 {
                            return Err(err);
                        }
                        return Err(WorkflowError::StepExhausted {
                            id: step.id().to_string(),
                            max_attempts,
                            source: err.into(),
                        });
                    }
                    if !retry_backoff.is_zero() {
                        tokio::select! {
                            biased;
                            _ = step_abort.cancelled() => return Err(WorkflowError::StepAborted { id: step.id().to_string() }),
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
        policy_attempt: usize,
        request: &WorkflowRunRequest,
    ) -> Result<StepResult, WorkflowError> {
        if self.executor_cancellation_policy(step)? == ExecutorCancellationPolicy::Finalize {
            return self
                .run_finalizing_step_attempt(step, attempt_abort, timeout, policy_attempt, request)
                .await;
        }

        let Some(timeout) = timeout else {
            return self
                .run_step(step, attempt_abort, policy_attempt, request)
                .await;
        };
        tokio::select! {
            biased;
            _ = attempt_abort.cancelled() => Err(WorkflowError::StepAborted { id: step.id().to_string() }),
            result = self.run_step(step, attempt_abort.clone(), policy_attempt, request) => result,
            _ = tokio::time::sleep(timeout) => {
                attempt_abort.cancel();
                Err(WorkflowError::StepTimeout {
                    id: step.id().to_string(),
                    timeout_ms: timeout.as_millis() as u64,
                })
            }
        }
    }

    async fn run_finalizing_step_attempt(
        &self,
        step: &Step,
        attempt_abort: CancellationToken,
        timeout: Option<Duration>,
        policy_attempt: usize,
        request: &WorkflowRunRequest,
    ) -> Result<StepResult, WorkflowError> {
        enum Interruption {
            Aborted,
            TimedOut(u64),
        }

        let execution = self.run_step(step, attempt_abort.clone(), policy_attempt, request);
        tokio::pin!(execution);

        let interruption = match timeout {
            Some(timeout) => {
                tokio::select! {
                    biased;
                    _ = attempt_abort.cancelled() => Interruption::Aborted,
                    result = &mut execution => return result,
                    _ = tokio::time::sleep(timeout) => {
                        attempt_abort.cancel();
                        Interruption::TimedOut(timeout.as_millis() as u64)
                    }
                }
            }
            None => {
                tokio::select! {
                    biased;
                    _ = attempt_abort.cancelled() => Interruption::Aborted,
                    result = &mut execution => return result,
                }
            }
        };

        // The abort token stops participant work. The executor owns the scoped
        // finalization boundary and must now resolve before runtime returns.
        let finalized_result = execution.await;
        self.persist_finalized_executor_state(step, finalized_result.as_ref().ok())
            .await?;

        match interruption {
            Interruption::Aborted => Err(WorkflowError::StepAborted {
                id: step.id().to_string(),
            }),
            Interruption::TimedOut(timeout_ms) => Err(WorkflowError::StepTimeout {
                id: step.id().to_string(),
                timeout_ms,
            }),
        }
    }

    async fn persist_finalized_executor_state(
        &self,
        step: &Step,
        finalized_result: Option<&StepResult>,
    ) -> Result<(), WorkflowError> {
        let mut state = self.workflow_state.lock().await;
        if state.status == WorkflowStatus::Cancelled
            && let Some(result) = finalized_result
            && let Some(record) = state
                .step_history
                .iter_mut()
                .rev()
                .find(|record| record.step_id == *step.id() && record.result.is_none())
        {
            record.result = Some(result.clone());
            record.ended_at = chrono::Utc::now();
        }
        let snapshot = state.clone();
        drop(state);
        self.persist_state(&snapshot).await
    }

    /// Run a deterministic non-LLM executor step.
    async fn run_executor_step(
        &self,
        step: &Step,
        step_abort: CancellationToken,
        policy_attempt: usize,
    ) -> Result<StepResult, WorkflowError> {
        let Step::Executor { .. } = step else {
            return Err(WorkflowError::Internal(
                "run_executor_step called with non-executor step".into(),
            ));
        };
        let executor = self
            .executor_for_step(step)?
            .expect("executor step must resolve an executor");
        let started_at = chrono::Utc::now();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let env = self.config.env_factory.create(&cwd)?;
        let history = self.workflow_state.lock().await.step_history.clone();
        let prev_result = history.last().and_then(|record| record.result.as_ref());
        let attempt = history
            .iter()
            .filter(|record| record.step_id == *step.id())
            .count()
            + policy_attempt;
        let ctx = ExecutorCtx {
            execution_id: ExecutorExecutionId {
                task_id: self.task_id.clone(),
                step_id: step.id().clone(),
                attempt,
            },
            current_step: step,
            prev_result,
            step_history: &history,
            env,
            abort: step_abort.clone(),
            context: self.context.clone(),
            event_tx: self.event_tx.clone(),
        };
        let cancellation_policy = executor.cancellation_policy();
        let recovery = executor.recover(&ctx);
        let recovered = match cancellation_policy {
            ExecutorCancellationPolicy::Interruptible => tokio::select! {
                biased;
                _ = step_abort.cancelled() => {
                    return Err(WorkflowError::StepAborted { id: step.id().to_string() });
                }
                result = recovery => result,
            },
            ExecutorCancellationPolicy::Finalize => recovery.await,
        }
        .map_err(|source| WorkflowError::ExecutorRecoveryFailed {
            execution_id: ctx.execution_id.to_string(),
            source,
        })?;
        let mut result = if let Some(recovered) = recovered {
            recovered
        } else {
            let execution = executor.execute(&ctx);
            match cancellation_policy {
                ExecutorCancellationPolicy::Interruptible => tokio::select! {
                    biased;
                    _ = step_abort.cancelled() => {
                        return Err(WorkflowError::StepAborted { id: step.id().to_string() });
                    }
                    result = execution => result,
                },
                ExecutorCancellationPolicy::Finalize => execution.await,
            }
            .map_err(|source| WorkflowError::StepFailed {
                id: step.id().to_string(),
                source,
            })?
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
    async fn run_llm_step(
        &self,
        step: &Step,
        step_abort: CancellationToken,
        policy_attempt: usize,
        request: &WorkflowRunRequest,
    ) -> Result<StepResult, WorkflowError> {
        let Step::Llm {
            prompt,
            allowed_tools,
            structured,
            ..
        } = step
        else {
            return Err(WorkflowError::Internal(
                "run_llm_step called with non-LLM step".into(),
            ));
        };
        // 记录真实开始时间（用于 StepRecord）。
        let started_at = chrono::Utc::now();
        // a. 构造 env + session。
        //    session 路径按 <task_id>/<step_id>-<attempt_seq>/ 隔离:
        //    attempt_seq = step_history 中同 step_id 的 record 数 + policy_attempt
        //    (本次 StepRecord 尚未 push,push 发生在 apply_transition)。
        //    policy_attempt 从 1 开始:首次执行 = +1 (向后兼容);
        //    policy retry 时无 record push,需靠 policy_attempt 递增避免路径碰撞。
        //    judge Retry 会先 push record,故下一轮 history_count 已 +1。
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let env = self.config.env_factory.create(&cwd)?;
        let attempt_seq = {
            let state = self.workflow_state.lock().await;
            state
                .step_history
                .iter()
                .filter(|r| r.step_id == *step.id())
                .count()
                + policy_attempt
        };
        let session_dir = self
            .config
            .session_base_dir
            .join(&self.task_id.0)
            .join(format!("{}-{}", step.id(), attempt_seq));
        let (storage, session_id) = self
            .config
            .session_factory
            .create(&session_dir, &self.config.model)
            .await?;

        // b. 构造 builder。
        let mut builder = HarnessBuilder::new(&self.config.model)
            .provider(&self.config.model, self.config.client.clone());
        // 共享闭包：业务层可链式调用任意 builder 方法。
        if let Some(customize) = &self.config.customize_builder {
            builder = customize(builder);
        }
        // per-step 闭包：可覆盖共享设置。
        if let Some(customize) = self.step_builder_customizers.get(step.id()) {
            builder = customize(builder);
        }

        // c. 注册 step plugin（若有）。
        if let Some(factory) = self.step_plugin_factories.get(step.id()) {
            let plugin = factory();
            builder = builder.install(plugin.as_ref());
        }

        // d. 注册 extra tools + 异步 spawn tool 组（若该步允许）。
        //    收集所有注入的 tool 名（extra_tools + spawn 相关），
        //    传给 gate 让其不被过滤。
        let mut injected_tool_names: HashSet<String> = HashSet::new();
        for tool in &self.extra_tools {
            injected_tool_names.insert(tool.name().to_string());
            builder = builder.tool(tool.clone());
        }
        let post_build_spawn_setup: Option<(
            Arc<llm_harness_subagents::plugin::SpawnPlugin>,
            Arc<llm_harness_subagents::message_bus::MessageBus>,
        )> = if allowed_tools.iter().any(|t| t == SPAWN_AGENT_TOOL_NAME) {
            let bus = llm_harness_subagents::message_bus::MessageBus::new();
            let plugin = llm_harness_subagents::plugin::SpawnPlugin::new(bus.clone());
            let spawner = Arc::new(self.build_step_spawner(bus.clone(), step.id(), attempt_seq));
            bus.register(
                llm_harness_subagents::message_bus::MAIN_AGENT_ID,
                llm_harness_subagents::message_bus::AgentEntry {
                    status: llm_harness_subagents::message_bus::AgentStatus::Running,
                    harness: None,
                    is_main: true,
                    role: None,
                    description: None,
                },
            );
            injected_tool_names.insert(SPAWN_AGENT_TOOL_NAME.to_string());
            injected_tool_names.insert(llm_harness_types::MESSAGE_SUBAGENT_TOOL_NAME.to_string());
            injected_tool_names
                .insert(llm_harness_types::AWAIT_SUBAGENT_REPLY_TOOL_NAME.to_string());
            injected_tool_names.insert(llm_harness_types::ABORT_SUBAGENT_TOOL_NAME.to_string());
            builder = builder.tool(Arc::new(llm_harness_subagents::tools::SpawnAgentTool::new(
                spawner,
            )));
            builder = builder.tool(Arc::new(
                llm_harness_subagents::tools::MessageSubagentTool::new(
                    bus.clone(),
                    llm_harness_subagents::message_bus::MAIN_AGENT_ID,
                ),
            ));
            builder = builder.tool(Arc::new(
                llm_harness_subagents::tools::AwaitSubagentReplyTool::new(
                    bus.clone(),
                    llm_harness_subagents::message_bus::MAIN_AGENT_ID,
                ),
            ));
            builder = builder.tool(Arc::new(
                llm_harness_subagents::tools::QuerySubagentTool::new(bus.clone()),
            ));
            builder = builder.tool(Arc::new(
                llm_harness_subagents::tools::AbortSubagentTool::new(bus.clone()),
            ));
            builder = builder.convert_to_llm(Some(Arc::new(
                llm_harness_loop::convert::DefaultConvertToLlm::new().with_custom_converter(
                    Arc::new(llm_harness_subagents::delivery::SubAgentMessageConverter),
                ),
            )));
            builder = builder.install(plugin.as_ref());
            Some((plugin, bus))
        } else {
            None
        };

        let structured_coordinator: Option<StructuredOutputCoordinator> = match *structured {
            Some(true) => {
                let supports_json_schema = self.config.client.capabilities().supports_json_schema();
                Some(StructuredOutputCoordinator::new(
                    supports_json_schema,
                    DEFAULT_MAX_UPGRADE_ATTEMPTS,
                ))
            }
            _ => None,
        };

        // e. 注入门禁 hook（第一道 PrepareNextTurn + 第二道 BeforeToolCall）。
        //    gate 的 active_tools = step.allowed_tools ∪ injected_tool_names，
        //    确保注入的内置 tool 不被门禁滤掉。

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
        let mut hook_state = HarnessHooks::none();
        hook_state.prepare_next_turn.push(gate_hook);
        hook_state.before_tool_call.push(approval_hook);
        if let Some(ref coord) = structured_coordinator {
            // Layer 1：BeforeRunHook 在 system prompt 追加结构化输出约束。
            hook_state
                .before_run
                .push(structured_prompt_hook(coord.system_prompt_segment()));
            // Layer 2/3：should_stop 提取 JSON / prepare_next_turn 注入 response_format。
            hook_state.should_stop.push(coord.should_stop_hook());
            hook_state
                .prepare_next_turn
                .push(coord.prepare_next_turn_hook());
        }
        builder = builder.hooks(hook_state);

        // f. build + set_abort_token + prompt。
        let harness = Arc::new(
            builder
                .build_with_session(env, Session::new(storage))
                .map_err(|e: HarnessBuildError| WorkflowError::StepFailed {
                    id: step.id().to_string(),
                    source: e.into(),
                })?,
        );
        harness.set_abort_token(step_abort.clone());
        if let Some((plugin, _bus)) = post_build_spawn_setup {
            plugin.set_harness_weak(Arc::downgrade(&harness));
        }

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
        // Spawn prompt() on a separate task so events can be consumed
        // concurrently. Without this, prompt().await blocks until the entire
        // agent loop finishes, overflowing the broadcast channel (capacity 256)
        // before the consumer below starts — causing Lagged errors and missing
        // the Settled sentinel.
        let harness_clone = Arc::clone(&harness);
        let step_request =
            RunRequest::from_text(prompt_text).with_extensions(request.extensions().clone());
        let prompt_task = tokio::spawn(async move { harness_clone.run(step_request).await });

        // h. 收集事件 → StepResult。
        //    哨兵：Settled（所有排队活动结束）或 Aborted（被 cancel）。
        //    channel 关闭（harness 异常退出）视为失败而非成功。
        let mut text_delta_output = String::new();
        let mut final_answer_output: Option<String> = None;
        let mut tool_calls_count = 0u32;
        let mut settled = false;
        let mut aborted = false;
        // AgentEvent::Error（非 Aborted）捕获：循环结束后翻成 StepFailed。
        // Aborted 由 biased select 的 step_abort.cancelled() 路径接管，这里忽略。
        let mut step_error: Option<WorkflowError> = None;
        loop {
            tokio::select! {
                biased;
                _ = step_abort.cancelled() => {
                    // abort：收到的部分 output 保留，但不标记 settled。
                    aborted = true;
                    break;
                }
                event = rx.recv() => {
                    match event {
                        Ok(event) => match event.as_ref() {
                            AgentHarnessEvent::Agent(AgentEvent::TextDelta { text, .. }) => {
                                text_delta_output.push_str(text);
                                let _ = self.event_tx.send(WorkflowEvent::TextDelta {
                                    step_id: step.id().clone(),
                                    text: text.clone(),
                                });
                            }
                            AgentHarnessEvent::Agent(AgentEvent::ThinkingDelta { thinking, .. }) => {
                                let _ = self.event_tx.send(WorkflowEvent::ThinkingDelta {
                                    step_id: step.id().clone(),
                                    thinking: thinking.clone(),
                                });
                            }
                            AgentHarnessEvent::Agent(ev) => match ev {
                                // Aborted：忽略。与 step_abort.cancelled() 同源，
                                // biased select 会让 step_abort.cancelled() 优先返回 StepAborted。
                                AgentEvent::Error(AgentError::Aborted) => {}
                                // 非 Aborted 的 Error：捕获并跳出，循环后翻成 StepFailed。
                                // 含 StreamIncomplete / provider timeout / stream ended 等。
                                AgentEvent::Error(e) => {
                                    step_error = Some(WorkflowError::StepFailed {
                                        id: step.id().to_string(),
                                        source: anyhow::anyhow!("{e}"),
                                    });
                                    break;
                                }
                                _ => {
                                    if let Some((_, _, text)) = ev.as_final_answer() {
                                        final_answer_output = Some(text);
                                    }
                                    // Forward coarse-grained progress to workflow subscribers.
                                    if let Some(progress) = step_progress_from(ev) {
                                        let _ = self.event_tx.send(WorkflowEvent::StepProgress {
                                            step_id: step.id().clone(),
                                            progress,
                                        });
                                    }
                                }
                            },
                            AgentHarnessEvent::ToolCallStart { .. } => tool_calls_count += 1,
                            AgentHarnessEvent::Settled | AgentHarnessEvent::Aborted => {
                                settled = true;
                                break;
                            }
                            _ => {}
                        },
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // Channel is still open; we dropped events.
                            // Continue recv — the prompt task join below is
                            // the authoritative error source if Settled was lost.
                            continue;
                        }
                    }
                }
            }
        }
        // Join the prompt task to surface any errors from the agent loop.
        // If the channel closed without Settled, the prompt task may hold
        // the actual error (e.g. LLM failure, storage error).
        let prompt_result = prompt_task
            .await
            .map_err(|e| WorkflowError::Internal(format!("prompt task panicked: {e}")))?;
        if aborted {
            // step_abort 被 cancel：正常中止，不是 harness 异常退出。
            // prompt() 返回 Ok（drive_loop 将 abort 视为信号而非错误）；
            // 丢弃部分 output，向上报错让调用方决定重试或终止。
            prompt_result?;
            return Err(WorkflowError::StepAborted {
                id: step.id().to_string(),
            });
        }
        if let Some(err) = step_error {
            // 事件流出现 AgentEvent::Error（非 Aborted）：翻成 StepFailed。
            // prompt_result? 优先：若 prompt() 本身也错（如 panic 后 JoinError），
            // 透传更具体的错误；否则返回事件流捕获的错误。
            prompt_result?;
            return Err(err);
        }
        // 兜底：事件流可能因 Lagged 丢失 AgentEvent::Error，但 drive_loop 会把
        // 非 Aborted 错误写进 harness.state().error_message。若 step_error 未捕获
        // 但 state 里记了错误，翻成 StepFailed（与事件流路径一致）。
        // 走到这里 step_error 必为 None：上面的 `if let Some(err) = step_error`
        // 已提前 return。
        if let Some(msg) = harness.state().error_message.clone() {
            prompt_result?;
            return Err(WorkflowError::StepFailed {
                id: step.id().to_string(),
                source: anyhow::anyhow!("{msg}"),
            });
        }
        if !settled {
            // channel 关闭但未收到 Settled 或 Aborted：harness 异常退出。
            // If prompt() returned an error, propagate that (more specific).
            // Otherwise, report the generic sentinel-missing error.
            prompt_result?;
            return Err(WorkflowError::Internal(format!(
                "run_llm_step: event stream closed before Settled (step '{}')",
                step.id()
            )));
        }
        let cost = harness.usage();
        let ended_at = chrono::Utc::now();
        // 从 coordinator 取结构化结果（若有）。先读 status 再 take_structured：
        // take 会把状态重置为 Idle，之后 status() 误报 Failed。
        let (structured, structured_status) = if let Some(ref coord) = structured_coordinator {
            let status = coord.status();
            let value = coord.take_structured();
            (value, status)
        } else {
            (None, StructuredStatus::NotRequired)
        };
        let output = final_answer_output.unwrap_or(text_delta_output);

        Ok(StepResult {
            output,
            structured,
            structured_status,
            tool_calls_count,
            session_id,
            cost,
            started_at: Some(started_at),
            ended_at: Some(ended_at),
        })
    }

    /// 构造 step 内部 fan-out 用的 spawner（局部对象，step 结束丢弃）。
    ///
    /// `step_id` + `attempt_seq` 用于拼 sub-agent session 路径:
    /// `<session_base_dir>/<task_id>/<step_id>-<attempt_seq>/sub-agents/`。
    pub(super) fn build_step_spawner(
        &self,
        bus: Arc<llm_harness_subagents::message_bus::MessageBus>,
        step_id: &StepId,
        attempt_seq: usize,
    ) -> HarnessSubAgentSpawner {
        let sub_agent_base = self
            .config
            .session_base_dir
            .join(&self.task_id.0)
            .join(format!("{}-{}", step_id, attempt_seq))
            .join("sub-agents");
        HarnessSubAgentSpawner::new(
            self.config.model.clone(),
            self.config.client.clone(),
            sub_agent_base,
            bus,
            |_cwd, _bus, _agent_id| Box::new(NoopPlugin),
        )
        .env_factory(self.config.env_factory.clone())
        .session_factory(self.config.session_factory.clone())
        .builder_customize(self.config.customize_builder.clone())
    }

    // ── 生命周期 ───────────────────────────────────────────────────────

    /// 持久化 WorkflowState（若有 task_store）。
    /// 调用方应在 drop 锁后调用（传入 snapshot），避免持锁 await I/O。
    pub(super) async fn persist_state(&self, state: &WorkflowState) -> Result<(), WorkflowError> {
        // 同步共享 context 快照进 state，保证落盘的是最新 context。
        let mut state = state.clone();
        state.context = self.context_snapshot().await;
        if let Some(store) = &self.task_store {
            store.save_workflow_state(&self.task_id, &state).await?;
        }
        Ok(())
    }

    /// 声明式循环（LoopConfig）续环判定 — Retry 与 To 两条路径共用。
    ///
    /// 调用前置条件：调用方已确认这是"续环"转换（Retry，或
    /// To(target_stage)，包括 target 等于当前步）。函数消费
    /// `workflow_state` 锁，并在持久化前释放，避免持锁 await I/O。
    ///
    /// 计数/耗尽语义见 `LoopConfig`（`max_iterations=N` 允许 N 次续环，
    /// 第 N+1 次耗尽）。耗尽时：清零该步计数，跟随 `exit_route` 标签边
    /// 返回其目标；`exit_route` 缺失或无法解析则置 Failed 并返回
    /// `Err(TaskFailed)` —— 调用方（run_loop）无需再兜底。
    async fn step_loop_continuation(
        &self,
        mut state: tokio::sync::MutexGuard<'_, WorkflowState>,
        step: &Step,
        loop_cfg: &LoopConfig,
    ) -> Result<Option<StepId>, WorkflowError> {
        // 声明式循环计数：持久化在 WorkflowState.loop_iterations，
        // 跨节点跳转（check→Retry→target→To(check)）后仍保留。
        // 语义：max_iterations=N 允许 N 次循环继续（跳转），
        // 第 N+1 次续环超限（count > max_iterations）。
        let entry = state
            .loop_iterations
            .entry(step.id().clone())
            .and_modify(|c| *c += 1)
            .or_insert(1);
        let loop_count = *entry;
        if loop_count > loop_cfg.max_iterations {
            // Loop exhausted: the episode ends — clear the counter so a
            // later re-entry of the same loop starts fresh.
            state.loop_iterations.remove(step.id());

            // Follow exit_route edge (a declarative Label edge).
            if let Some(exit_label) = &loop_cfg.exit_route {
                // Strip "next_on_" prefix if present to get the raw route
                // key, then find a matching edge.
                let label = exit_label.strip_prefix("next_on_").unwrap_or(exit_label);
                if let Some(edge) = self.workflow.edges.iter().find(|e| {
                    e.from.as_str() == step.id().as_str()
                        && matches!(
                            &e.condition,
                            Some(EdgeCondition::Label(l)) if l == label
                        )
                }) {
                    let next = edge.to.clone();
                    // Persist the effective transition so Running-state crash
                    // recovery resumes at the exit target, not the loop head.
                    if let Some(record) = state.step_history.last_mut() {
                        record.transition = Transition::To(next.clone());
                    }
                    let snapshot = state.clone();
                    drop(state);
                    self.persist_state(&snapshot).await?;
                    return Ok(Some(next));
                }
                // exit_route set but no matching edge → explicit failure.
                let msg = format!(
                    "LoopConfig for step '{}' exhausted ({} iterations); \
                     exit_route '{}' has no matching edge",
                    step.id(),
                    loop_cfg.max_iterations,
                    label
                );
                state.status = WorkflowStatus::Failed;
                state.error = Some(TaskError::Internal(msg.clone()));
                state.ended_at = Some(chrono::Utc::now());
                let snapshot = state.clone();
                drop(state);
                self.persist_state(&snapshot).await?;
                return Err(WorkflowError::TaskFailed(msg));
            }
            // exit_route missing: To-based loops have no Retry records, so
            // falling through to max_retries can never bound them. Fail
            // explicitly. (The Retry caller treats Err the same way.)
            let msg = format!(
                "LoopConfig for step '{}' exhausted ({} iterations); no exit_route configured",
                step.id(),
                loop_cfg.max_iterations
            );
            state.status = WorkflowStatus::Failed;
            state.error = Some(TaskError::Internal(msg.clone()));
            state.ended_at = Some(chrono::Utc::now());
            let snapshot = state.clone();
            drop(state);
            self.persist_state(&snapshot).await?;
            return Err(WorkflowError::TaskFailed(msg));
        }

        // Loop continues: jump to target_stage or re-run current.
        if let Some(target) = &loop_cfg.target_stage
            && target != step.id()
        {
            if let Some(record) = state.step_history.last_mut() {
                record.transition = Transition::To(target.clone());
            }
            let snapshot = state.clone();
            drop(state);
            self.persist_state(&snapshot).await?;
            return Ok(Some(target.clone()));
        }
        // target_stage is None or same as current: re-run in place.
        let snapshot = state.clone();
        drop(state);
        self.persist_state(&snapshot).await?;
        Ok(None)
    }

    /// 据 Transition 更新 state + 记录 step_history。返回 next_step（Retry/Abort 时为 None）。
    ///
    async fn apply_transition(
        &self,
        step: &Step,
        step_result: StepResult,
        transition: Transition,
        cache_key: Option<serde_json::Value>,
    ) -> Result<Option<StepId>, WorkflowError> {
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
            cache_key,
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
                    return Err(WorkflowError::TaskFailed(msg));
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
                    return Err(WorkflowError::TaskFailed(msg));
                }

                // 声明式续环：仅当"LoopConfig 持有步自身"路由到其
                // target_stage（包括自环）时才计一次循环迭代；
                // 其他步骤的 To(target) 是普通跳转，绝不计入别人的循环。
                if let Some(loop_cfg) = step.policy().and_then(|p| p.r#loop.as_ref()) {
                    let is_loop_continuation = matches!(
                        loop_cfg.target_stage.as_ref(),
                        Some(target) if target == &next
                    );
                    if is_loop_continuation {
                        return self.step_loop_continuation(state, step, loop_cfg).await;
                    }
                    state.loop_iterations.remove(step.id());
                }
                let snapshot = state.clone();
                drop(state);
                self.persist_state(&snapshot).await?;
                Ok(Some(next))
            }
            Transition::Retry => {
                // Check for declarative LoopConfig on this step's policy.
                // If present, the loop has its own counter and exit semantics
                // that supersede the generic max_retries safety valve.
                if let Some(loop_cfg) = step.policy().and_then(|p| p.r#loop.as_ref()) {
                    // 续环计数/耗尽/退出共用判定（To 路径亦走此函数）。
                    // 注意：exit_route 缺失或无法解析时显式失败（原 fall-through
                    // 到 max_retries 只对纯 Retry 循环有界；两者现已合一）。
                    return self.step_loop_continuation(state, step, loop_cfg).await;
                }

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
                    return Err(WorkflowError::TaskFailed(msg));
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
            Transition::Pause { reason } => {
                state.status = WorkflowStatus::Paused;
                state.reason = Some(reason.clone());
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
    pub fn pause(&self, reason: &str) -> Result<(), WorkflowError> {
        self.pause_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut r) = self.pause_reason.lock() {
            *r = Some(reason.to_string());
        }
        Ok(())
    }

    /// Prepare a paused or failed task for continuation.
    ///
    /// Accepts `Paused` (operator-initiated pause) and `Failed` (transient error
    /// such as LLM 403 / network blip). Resets to `Paused` so that `run()` —
    /// which owns the `Paused -> Running` transition — can drive execution
    /// from `current_step`.
    ///
    /// `Succeeded` and `Cancelled` are terminal and cannot be resumed.
    ///
    /// **Side-effect policy**: Runtime only resets the state machine; it does
    /// NOT manage step-level side effects (e.g. cached PanGen results). The
    /// step executor is responsible for deciding whether to recompute — a
    /// resumed `Failed` step will be re-driven by `run_loop`, and the executor
    /// decides to skip/recompute based on its own domain markers.
    pub async fn resume(&self) -> Result<(), WorkflowError> {
        let mut state = self.workflow_state.lock().await;
        match state.status {
            WorkflowStatus::Paused => {}
            WorkflowStatus::Failed => {
                // Reset Failed → Paused so run() can pick it up.
                state.status = WorkflowStatus::Paused;
                state.reason = None;
                state.error = None;
                state.result = None;
                state.ended_at = None;
            }
            ref s => {
                return Err(WorkflowError::InvalidStatus { status: s.clone() });
            }
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
    pub async fn cancel(&self, reason: &str) -> Result<(), WorkflowError> {
        // 与 run_loop 注册 step token 使用同一锁顺序并保持两个 guard，确保
        // "注册 token" 与 "置 Cancelled" 之间不存在丢失取消信号的窗口。
        let mut current_abort = self.current_abort.lock().await;
        let mut state = self.workflow_state.lock().await;
        if let Some(token) = current_abort.take() {
            token.cancel();
        }
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
                cache_key: None,
            });
        }
        state.status = WorkflowStatus::Cancelled;
        state.reason = Some(reason.to_string());
        state.ended_at = Some(now);
        let snapshot = state.clone();
        drop(state);
        drop(current_abort);
        self.persist_state(&snapshot).await?;
        let _ = self.event_tx.send(WorkflowEvent::Cancelled {
            reason: reason.to_string(),
        });
        Ok(())
    }

    pub async fn checkpoint(&self, description: &str, payload: Value) -> Result<(), WorkflowError> {
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
