//! Python callable 包装为 `StepTransitionJudge` 和 `StepExecutor` trait。
//!
//! 验证风险点：workflow trait callback 可从 Python 驱动。
//! 使用与 `PyTool` 相同的模式：`spawn_blocking` + `Python::attach` + `call1`。
//! `StepCtx`/`ExecutorCtx` 的借用字段在进入 `spawn_blocking` 前序列化为
//! owned 数据，避免跨线程借用和 GIL 下 `.await` 死锁。

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_agent::{HarnessHooks, Plugin};
use llm_harness_runtime::spawn::spawner::{EnvFactory, JsonlSessionFactory};
use llm_harness_runtime::workflow::engine::{WorkflowEngine, WorkflowEngineConfig, WorkflowEvent};
use llm_harness_runtime::workflow::executor::{ExecutorCtx, StepExecutor};
use llm_harness_runtime::workflow::judge::{StepCtx, StepTransitionJudge};
use llm_harness_runtime::workflow::model::{
    Edge, EdgeCondition, Step, StepResult, Transition, Workflow,
};
use llm_harness_types::{AgentError, ExecutionEnv, Tool, UnsupportedEnv};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use crate::pyagent::runtime;
use crate::pyeventstream::PyWaitForExternalEventTool;
use crate::pyhooks::HookKind;
use crate::pyplugin::PyPluginWrapper;
use crate::pyprovider::PyProvider;
use crate::pytool::PyToolWrapper;
use crate::value_conv::{pyobject_to_value, value_to_pyobject};

// ── PyJudge ─────────────────────────────────────────────────────────────────

/// Python callable 包装为 `StepTransitionJudge`。
///
/// callback 签名：`callback(ctx: dict) -> str`
/// 返回值编码：
/// - `"retry"` → Retry
/// - `"to:<step_id>"` → To(step_id)
/// - `"fail:<reason>"` → Fail { reason }
/// - `"abort:<reason>"` → Abort { reason }
pub struct PyJudge {
    callback: Arc<Py<PyAny>>,
}

impl PyJudge {
    pub fn new(callback: Py<PyAny>) -> Self {
        Self {
            callback: Arc::new(callback),
        }
    }
}

impl StepTransitionJudge for PyJudge {
    fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
        let callback = Arc::clone(&self.callback);
        // 在进入 spawn_blocking 前提取 owned 数据，避免跨线程借用
        let step_id = ctx.current_step.id().to_string();
        let structured = ctx.last_result.structured.clone();
        let output = ctx.last_result.output.clone();
        let step_count = ctx.step_history.len();

        Box::pin(async move {
            let result = tokio::task::spawn_blocking(move || {
                Python::attach(|py| {
                    let cb = callback.bind(py);
                    let dict = PyDict::new(py);
                    dict.set_item("step_id", &step_id)?;
                    dict.set_item("output", &output)?;
                    dict.set_item("step_count", step_count)?;
                    if let Some(s) = &structured {
                        dict.set_item("structured", value_to_pyobject(py, s)?)?;
                    } else {
                        dict.set_item("structured", py.None())?;
                    }

                    let raw = cb.call1((dict,))?;
                    let transition_str: String = raw.extract()?;
                    Ok::<_, PyErr>(transition_str)
                })
            })
            .await;

            match result {
                Ok(Ok(s)) => parse_transition(&s),
                Ok(Err(e)) => Transition::Abort {
                    reason: format!("judge callback error: {e}"),
                },
                Err(e) => Transition::Abort {
                    reason: format!("judge join failed: {e}"),
                },
            }
        })
    }

    fn is_noop(&self) -> bool {
        false
    }
}

/// 将字符串解析为 `Transition`。
fn parse_transition(s: &str) -> Transition {
    let s = s.trim();
    match s {
        "retry" => Transition::Retry,
        s if s.starts_with("to:") => {
            let target = s[3..].trim().to_string();
            Transition::To(target)
        }
        s if s.starts_with("fail:") => Transition::Fail {
            reason: s[5..].trim().to_string(),
        },
        s if s.starts_with("abort:") => Transition::Abort {
            reason: s[6..].trim().to_string(),
        },
        _ => Transition::Abort {
            reason: format!("invalid transition: {s}"),
        },
    }
}

// ── PyExecutor ──────────────────────────────────────────────────────────────

/// Python callable 包装为 `StepExecutor`。
///
/// callback 签名：`callback(ctx: dict) -> dict`
/// 返回 dict 期望包含：
/// - `"output"`: str（必填）
/// - `"structured"`: dict（可选，将转为 `serde_json::Value`）
pub struct PyExecutor {
    callback: Arc<Py<PyAny>>,
}

impl PyExecutor {
    pub fn new(callback: Py<PyAny>) -> Self {
        Self {
            callback: Arc::new(callback),
        }
    }
}

impl StepExecutor for PyExecutor {
    fn execute<'a>(&'a self, ctx: &ExecutorCtx<'a>) -> BoxFuture<'a, anyhow::Result<StepResult>> {
        let callback = Arc::clone(&self.callback);
        let step_id = ctx.current_step.id().to_string();
        let step_name = ctx.current_step.name().to_string();
        let config = ctx.current_step.executor_config().cloned();
        let prev_output = ctx.prev_result.map(|r| r.output.clone());
        // clone Arc<Mutex<WorkflowContext>>，在 async 上下文里 lock
        let context = ctx.context.clone();

        Box::pin(async move {
            // 在进入 spawn_blocking 前读取 context 快照，避免 GIL 下 .await 死锁
            let context_snapshot = {
                let guard = context.lock().await;
                guard.variables.clone()
            };

            let result = tokio::task::spawn_blocking(move || {
                Python::attach(|py| {
                    let cb = callback.bind(py);
                    let dict = PyDict::new(py);
                    dict.set_item("step_id", &step_id)?;
                    dict.set_item("step_name", &step_name)?;
                    if let Some(c) = &config {
                        dict.set_item("config", value_to_pyobject(py, c)?)?;
                    } else {
                        dict.set_item("config", py.None())?;
                    }
                    if let Some(o) = &prev_output {
                        dict.set_item("prev_output", o)?;
                    } else {
                        dict.set_item("prev_output", py.None())?;
                    }

                    let ctx_dict = PyDict::new(py);
                    for (k, v) in &context_snapshot {
                        ctx_dict.set_item(k, value_to_pyobject(py, v)?)?;
                    }
                    dict.set_item("context", ctx_dict)?;

                    let raw = cb.call1((dict,))?;
                    let result_dict = raw.cast::<PyDict>()?;
                    let output: String = result_dict
                        .get_item("output")?
                        .ok_or_else(|| {
                            pyo3::exceptions::PyKeyError::new_err("missing 'output' key")
                        })?
                        .extract()?;
                    let structured = result_dict
                        .get_item("structured")
                        .ok()
                        .flatten()
                        .filter(|v| !v.is_none())
                        .map(|v| pyobject_to_value(&v))
                        .transpose()?;

                    Ok::<_, PyErr>((output, structured))
                })
            })
            .await;

            match result {
                Ok(Ok((output, structured))) => Ok(StepResult {
                    output,
                    structured,
                    tool_calls_count: 0,
                    session_id: String::new(),
                    cost: Default::default(),
                    started_at: None,
                    ended_at: None,
                }),
                Ok(Err(e)) => Err(anyhow::anyhow!("executor callback error: {e}")),
                Err(e) => Err(anyhow::anyhow!("executor join failed: {e}")),
            }
        })
    }
}

// ── Python 包装类 ───────────────────────────────────────────────────────────

/// 持有 `PyJudge` 的不透明 Python 包装，供 Python 侧引用已注册的 judge。
#[pyclass(name = "Judge")]
pub struct PyJudgeWrapper {
    pub judge: Arc<PyJudge>,
}

/// 持有 `PyExecutor` 的不透明 Python 包装，供 Python 侧引用已注册的 executor。
#[pyclass(name = "Executor")]
pub struct PyExecutorWrapper {
    pub executor: Arc<PyExecutor>,
}

// ── dict_to_workflow ────────────────────────────────────────────────────────

/// 将 Python dict 解析为 `Workflow` 结构。
///
/// 期望格式：
/// ```python
/// {
///     "entry_step": "step1",
///     "steps": [
///         {"id": "step1", "name": "Step 1", "prompt": "...", "allowed_tools": [...]},
///         # 或 executor step:
///         {"id": "step2", "name": "Exec", "executor": "exec_name", "executor_config": {...}},
///     ],
///     "edges": [{"from": "step1", "to": "step2"}],
/// }
/// ```
fn dict_to_workflow(dict: &Bound<'_, PyDict>) -> PyResult<Workflow> {
    let entry_step: String = dict
        .get_item("entry_step")?
        .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err("missing 'entry_step'"))?
        .extract()?;

    let steps_list = dict
        .get_item("steps")?
        .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err("missing 'steps'"))?;
    let steps_seq = steps_list.cast::<pyo3::types::PyList>()?;

    let mut steps = Vec::with_capacity(steps_seq.len());
    for item in steps_seq.iter() {
        let step_dict = item.cast::<PyDict>()?;
        let id: String = step_dict
            .get_item("id")?
            .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err("missing step 'id'"))?
            .extract()?;
        let name: String = step_dict
            .get_item("name")?
            .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err("missing step 'name'"))?
            .extract()?;

        // 判断是 LLM step 还是 executor step
        if let Some(executor_name_val) = step_dict.get_item("executor")? {
            // Executor step
            let executor_name: String = executor_name_val.extract()?;
            let config = step_dict
                .get_item("executor_config")?
                .filter(|v| !v.is_none())
                .map(|v| pyobject_to_value(&v))
                .transpose()?;
            steps.push(Step::executor(id, name, executor_name, config));
        } else {
            // LLM step
            let prompt: String = step_dict
                .get_item("prompt")?
                .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err("missing step 'prompt'"))?
                .extract()?;
            let allowed_tools: Vec<String> = step_dict
                .get_item("allowed_tools")?
                .map(|v| v.extract())
                .transpose()?
                .unwrap_or_default();
            steps.push(Step::llm(id, name, prompt, allowed_tools));
        }
    }

    let edges_list = dict
        .get_item("edges")?
        .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err("missing 'edges'"))?;
    let edges_seq = edges_list.cast::<pyo3::types::PyList>()?;

    let mut edges = Vec::with_capacity(edges_seq.len());
    for item in edges_seq.iter() {
        let edge_dict = item.cast::<PyDict>()?;
        let from: String = edge_dict
            .get_item("from")?
            .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err("missing edge 'from'"))?
            .extract()?;
        let to: String = edge_dict
            .get_item("to")?
            .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err("missing edge 'to'"))?
            .extract()?;
        // Parse optional "condition" key. A string is treated as a legacy
        // label for custom judges (EdgeCondition::Label). A dict is treated
        // as a declarative ConditionExpr (EdgeCondition::Expr), deserialized
        // via serde_json for parity with the Rust model.
        let condition = if let Some(cond_val) = edge_dict.get_item("condition")? {
            if cond_val.is_none() {
                None
            } else if let Ok(s) = cond_val.extract::<String>() {
                Some(EdgeCondition::Label(s))
            } else {
                let v = pyobject_to_value(&cond_val)?;
                Some(serde_json::from_value::<EdgeCondition>(v).map_err(|e| {
                    pyo3::exceptions::PyValueError::new_err(format!("invalid edge condition: {e}"))
                })?)
            }
        } else {
            None
        };
        edges.push(Edge {
            from,
            to,
            condition,
        });
    }

    Ok(Workflow {
        entry_step,
        steps,
        edges,
    })
}

// ── workflow_event_to_dict ──────────────────────────────────────────────────

/// 将 `WorkflowEvent` 转换为 Python dict。
///
/// 每个 dict 包含 `"type"` 字段标识事件类型。
/// 镜像 `blender-scene-generator/src/server/events.rs` 的序列化逻辑。
fn workflow_event_to_dict(py: Python<'_>, event: &WorkflowEvent) -> PyResult<Py<PyAny>> {
    use llm_harness_runtime::workflow::engine::StepProgress;

    match event {
        WorkflowEvent::StepStarted { step_id, step_name } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "step_started")?;
            dict.set_item("step_id", step_id.clone())?;
            dict.set_item("step_name", step_name.clone())?;
            Ok(dict.into_any().unbind())
        }
        WorkflowEvent::StepFinished { step_id, result } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "step_finished")?;
            dict.set_item("step_id", step_id.clone())?;
            dict.set_item("output", result.output.clone())?;
            match &result.structured {
                Some(v) => dict.set_item("structured", value_to_pyobject(py, v)?)?,
                None => dict.set_item("structured", py.None())?,
            }
            dict.set_item("tool_calls_count", result.tool_calls_count)?;
            Ok(dict.into_any().unbind())
        }
        WorkflowEvent::Paused { reason } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "paused")?;
            dict.set_item("reason", reason.clone())?;
            Ok(dict.into_any().unbind())
        }
        WorkflowEvent::Resumed => {
            let dict = PyDict::new(py);
            dict.set_item("type", "resumed")?;
            Ok(dict.into_any().unbind())
        }
        WorkflowEvent::Cancelled { reason } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "cancelled")?;
            dict.set_item("reason", reason.clone())?;
            Ok(dict.into_any().unbind())
        }
        WorkflowEvent::Failed { error } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "failed")?;
            dict.set_item("error", error.clone())?;
            Ok(dict.into_any().unbind())
        }
        WorkflowEvent::StepProgress { step_id, progress } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "step_progress")?;
            dict.set_item("step_id", step_id.clone())?;

            let prog_dict = PyDict::new(py);
            match progress {
                StepProgress::ToolCallStart { tool_use_id, name } => {
                    prog_dict.set_item("type", "tool_call_start")?;
                    prog_dict.set_item("tool_use_id", tool_use_id.clone())?;
                    prog_dict.set_item("name", name.clone())?;
                }
                StepProgress::ToolCallEnd { tool_use_id, args } => {
                    prog_dict.set_item("type", "tool_call_end")?;
                    prog_dict.set_item("tool_use_id", tool_use_id.clone())?;
                    prog_dict.set_item("args", args.to_string())?;
                }
                StepProgress::ToolExecutionStart {
                    tool_use_id,
                    tool_name,
                } => {
                    prog_dict.set_item("type", "tool_execution_start")?;
                    prog_dict.set_item("tool_use_id", tool_use_id.clone())?;
                    prog_dict.set_item("tool_name", tool_name.clone())?;
                }
                StepProgress::ToolExecutionEnd {
                    tool_use_id,
                    ok,
                    error,
                } => {
                    prog_dict.set_item("type", "tool_execution_end")?;
                    prog_dict.set_item("tool_use_id", tool_use_id.clone())?;
                    prog_dict.set_item("ok", *ok)?;
                    match error {
                        Some(e) => prog_dict.set_item("error", e.clone())?,
                        None => prog_dict.set_item("error", py.None())?,
                    }
                }
                StepProgress::TurnEnd { index } => {
                    prog_dict.set_item("type", "turn_end")?;
                    prog_dict.set_item("index", *index)?;
                }
                StepProgress::MessageEnd { message_id, kind } => {
                    prog_dict.set_item("type", "message_end")?;
                    prog_dict.set_item("message_id", message_id.clone())?;
                    prog_dict.set_item("kind", format!("{kind:?}"))?;
                }
            }
            dict.set_item("progress", prog_dict)?;
            Ok(dict.into_any().unbind())
        }
    }
}

// ── UnsupportedEnvFactory ───────────────────────────────────────────────────

/// `EnvFactory` 返回 `UnsupportedEnv`（无文件系统/shell 能力）。
///
/// Tools needing filesystem access should use the bridge pattern
/// (their own `ExecutionEnv`) rather than the execution env provided
/// by the engine.
struct UnsupportedEnvFactory;

impl EnvFactory for UnsupportedEnvFactory {
    fn create(&self, cwd: &std::path::Path) -> Result<Arc<dyn ExecutionEnv>, AgentError> {
        Ok(Arc::new(UnsupportedEnv::with_working_dir(
            cwd.to_path_buf(),
        )))
    }
}

/// `Arc<PyPlugin>` 的 `Plugin` 适配器。
///
/// `PyPlugin` 实现了 `Plugin`，但 `Arc<PyPlugin>` 没有。
/// `with_step_plugin` 的工厂闭包需要返回 `Box<dyn Plugin>`，
/// 此适配器让 `Arc<PyPlugin>` 可作为 `Plugin` 使用。
struct PyPluginAdapter(Arc<crate::pyplugin::PyPlugin>);

impl Plugin for PyPluginAdapter {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        self.0.register_tools(tools);
    }

    fn register_hooks(&self, hooks: &mut HarnessHooks) {
        self.0.register_hooks(hooks);
    }
}

// ── PyWorkflowEngine ────────────────────────────────────────────────────────

/// Python 侧的 `WorkflowEngine` 包装类。
///
/// 暴露 fluent API：`with_tool()` / `with_hooks()` / `with_step_plugin()` /
/// `with_executor()` / `run()` / `subscribe()` / `task_id()`。
///
/// 内部持有 `Option<WorkflowEngine>`（非 `Arc`），因为 `with_*` 方法
/// 消费 `self`。`run()` / `subscribe()` 在执行时按需创建 `Arc`。
#[pyclass(name = "WorkflowEngine")]
pub struct PyWorkflowEngine {
    pub(crate) engine: Option<WorkflowEngine>,
}

#[pymethods]
impl PyWorkflowEngine {
    /// 从 workflow dict 构造 `WorkflowEngine`。
    ///
    /// `workflow_dict` 包含 `entry_step`/`steps`/`edges`。
    /// `provider` 提供底层 LLM client；`model` 为模型标识；
    /// `judge` 决定步骤间跳转。
    #[new]
    #[pyo3(signature = (workflow_dict, provider, model, judge, session_base_dir="sessions"))]
    fn new(
        py: Python<'_>,
        workflow_dict: &Bound<'_, PyDict>,
        provider: &Bound<'_, PyProvider>,
        model: &str,
        judge: &Bound<'_, PyJudgeWrapper>,
        session_base_dir: &str,
    ) -> PyResult<Self> {
        let workflow = dict_to_workflow(workflow_dict)?;
        let client = provider.borrow().client.clone();
        let judge_arc: Arc<dyn StepTransitionJudge> = judge.borrow().judge.clone();

        let config = WorkflowEngineConfig {
            client,
            model: model.to_string(),
            env_factory: Arc::new(UnsupportedEnvFactory),
            session_factory: Arc::new(JsonlSessionFactory),
            session_base_dir: std::path::PathBuf::from(session_base_dir),
        };

        let engine = py.detach(|| {
            WorkflowEngine::new(workflow, config, judge_arc)
                .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
        })?;

        Ok(Self {
            engine: Some(engine),
        })
    }

    /// 注册一个额外 `Tool`。返回 self 以支持链式调用。
    fn with_tool<'a>(
        mut slf: PyRefMut<'a, Self>,
        tool: &Bound<'_, PyToolWrapper>,
    ) -> PyRefMut<'a, Self> {
        if let Some(engine) = slf.engine.take() {
            let t: Arc<dyn Tool> = tool.borrow().tool.clone();
            slf.engine = Some(engine.with_tool(t));
        }
        slf
    }

    /// 注册一个外部事件 tool（`PyWaitForExternalEventTool`）。返回 self 以支持链式调用。
    ///
    /// 与 `with_tool` 不同，此方法接受 `create_event_channel()` 返回的
    /// `WaitForExternalEventTool`，提取其内部 `Arc<dyn Tool>` 注册到引擎。
    fn with_external_tool<'a>(
        mut slf: PyRefMut<'a, Self>,
        tool: &Bound<'_, PyWaitForExternalEventTool>,
    ) -> PyRefMut<'a, Self> {
        if let Some(engine) = slf.engine.take() {
            let t: Arc<dyn Tool> = tool.borrow().tool.clone();
            slf.engine = Some(engine.with_tool(t));
        }
        slf
    }

    /// 注入额外 hooks。返回 self 以支持链式调用。
    ///
    /// `hooks_list` 为 `create_*_hook` 创建的 Hook 列表。
    /// 按 `HookKind` 变体分发到 `HarnessHooks` 对应字段。
    fn with_hooks<'a>(
        mut slf: PyRefMut<'a, Self>,
        hooks_list: &Bound<'_, PyList>,
    ) -> PyResult<PyRefMut<'a, Self>> {
        let engine = slf
            .engine
            .take()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("engine already consumed"))?;

        let mut harness_hooks = HarnessHooks::none();
        for item in hooks_list.iter() {
            let wrapper = item.cast::<crate::pyhooks::PyHookWrapper>()?;
            let kind = &wrapper.borrow().kind;
            match kind {
                HookKind::BeforeTurn(h) => harness_hooks.before_turn.push(h.clone()),
                HookKind::AfterTurn(h) => harness_hooks.after_turn.push(h.clone()),
                HookKind::BeforeRun(h) => harness_hooks.before_run.push(h.clone()),
                HookKind::AfterProviderResponse(h) => {
                    harness_hooks.after_provider_response.push(h.clone())
                }
                HookKind::BeforeProviderRequest(h) => {
                    harness_hooks.before_provider_request.push(h.clone())
                }
                HookKind::BeforeToolCall(h) => harness_hooks.before_tool_call.push(h.clone()),
                HookKind::AfterToolCall(h) => harness_hooks.after_tool_call.push(h.clone()),
                HookKind::ShouldStop(h) => harness_hooks.should_stop.push(h.clone()),
                HookKind::BeforeCompact(h) => harness_hooks.before_compact.push(h.clone()),
                HookKind::TransformContext(h) => harness_hooks.transform_context.push(h.clone()),
                HookKind::PrepareNextTurn(h) => harness_hooks.prepare_next_turn.push(h.clone()),
            }
        }

        slf.engine = Some(engine.with_hooks(harness_hooks));
        Ok(slf)
    }

    /// 为指定 step 注册一个 plugin 工厂。返回 self 以支持链式调用。
    fn with_step_plugin<'a>(
        mut slf: PyRefMut<'a, Self>,
        step_id: &str,
        plugin: &Bound<'_, PyPluginWrapper>,
    ) -> PyRefMut<'a, Self> {
        if let Some(engine) = slf.engine.take() {
            let plugin_arc = plugin.borrow().plugin.clone();
            slf.engine = Some(engine.with_step_plugin(step_id, move || {
                Box::new(PyPluginAdapter(plugin_arc.clone())) as Box<dyn Plugin>
            }));
        }
        slf
    }

    /// 注册一个命名 executor。返回 self 以支持链式调用。
    fn with_executor<'a>(
        mut slf: PyRefMut<'a, Self>,
        name: &str,
        executor: &Bound<'_, PyExecutorWrapper>,
    ) -> PyRefMut<'a, Self> {
        if let Some(engine) = slf.engine.take() {
            let exec: Arc<dyn StepExecutor> = executor.borrow().executor.clone();
            slf.engine = Some(engine.with_executor(name, exec));
        }
        slf
    }

    /// 设置 workflow context 变量。必须在 `run()` 之前调用。
    ///
    /// 将 `key` → `value` 写入引擎共享 context 的 `variables` 表。
    /// `value` 通过 `pyobject_to_value` 转换为 `serde_json::Value`。
    /// 使用 `try_lock()` 避免在 GIL 下阻塞（run() 启动前锁无竞争）。
    fn set_context_variable(&self, key: &str, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let engine = self
            .engine
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("engine not available"))?;
        let json_val = pyobject_to_value(value)?;
        let ctx = engine.context();
        let mut guard = ctx.try_lock().map_err(|e| {
            pyo3::exceptions::PyRuntimeError::new_err(format!(
                "failed to lock workflow context: {e}"
            ))
        })?;
        guard.variables.insert(key.to_string(), json_val);
        Ok(())
    }

    /// 启动/恢复执行。阻塞直到 workflow 完成。
    ///
    /// 释放 GIL 后在 tokio runtime 上运行 `engine.run()`。
    fn run(&mut self, py: Python<'_>) -> PyResult<()> {
        let engine = self
            .engine
            .take()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("engine not available"))?;
        let engine = Arc::new(engine);
        let rt = runtime(py);
        let engine_clone = engine.clone();
        let result = py.detach(move || {
            rt.block_on(async move { engine_clone.run().await })
                .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
        });
        // After run(), engine_clone (the second Arc) is dropped when the
        // block_on closure exits. Arc::try_unwrap should succeed.
        // If it fails, the engine is left unusable — surface a clear error.
        match Arc::try_unwrap(engine) {
            Ok(e) => self.engine = Some(e),
            Err(_) => {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "workflow engine has outstanding references after run(); \
                     the engine is no longer usable",
                ));
            }
        }
        result?;
        Ok(())
    }

    /// 返回当前 task ID（`task-<uuid>` 格式）。
    fn task_id(&self) -> PyResult<String> {
        let engine = self
            .engine
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("engine not available"))?;
        Ok(engine.task_id().0.clone())
    }

    /// 订阅引擎事件流，返回 `WorkflowEventIterator`。
    ///
    /// 每次调用返回一个独立的 receiver；多个订阅者各自接收事件。
    /// `timeout_ms` 为单次 `__next__` 等待超时（毫秒），默认 5000。
    #[pyo3(signature = (timeout_ms=5000))]
    fn subscribe(&self, py: Python<'_>, timeout_ms: u64) -> PyResult<Py<PyWorkflowEventIterator>> {
        let engine = self
            .engine
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("engine not available"))?;
        let rx = engine.subscribe();
        let handle = runtime(py).handle().clone();
        let iter = PyWorkflowEventIterator::new(rx, timeout_ms, handle);
        Py::new(py, iter)
    }
}

// ── PyWorkflowEventIterator ─────────────────────────────────────────────────

/// Python 迭代器，包装 `broadcast::Receiver<WorkflowEvent>`。
///
/// 与 `PyHarnessEventIterator` 对称，但处理 `WorkflowEvent` 类型。
/// 释放 GIL 后阻塞等待事件，超时或 channel 关闭时返回 `None`。
#[pyclass(name = "WorkflowEventIterator")]
pub struct PyWorkflowEventIterator {
    rx: Option<tokio::sync::broadcast::Receiver<WorkflowEvent>>,
    timeout_ms: u64,
    handle: tokio::runtime::Handle,
}

impl PyWorkflowEventIterator {
    pub fn new(
        rx: tokio::sync::broadcast::Receiver<WorkflowEvent>,
        timeout_ms: u64,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            rx: Some(rx),
            timeout_ms,
            handle,
        }
    }
}

#[pymethods]
impl PyWorkflowEventIterator {
    fn __iter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// 阻塞等待下一个事件，超时或 channel 关闭时返回 None。
    fn __next__(&mut self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        let rx = match &mut self.rx {
            Some(rx) => rx,
            None => return Ok(None),
        };

        let timeout = std::time::Duration::from_millis(self.timeout_ms);
        let handle = self.handle.clone();

        let recv_result = py.detach(move || {
            handle.block_on(async move { tokio::time::timeout(timeout, rx.recv()).await })
        });

        match recv_result {
            Ok(Ok(event)) => {
                let dict = workflow_event_to_dict(py, &event)?;
                Ok(Some(dict))
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                let warning = PyDict::new(py);
                warning.set_item("type", "lagged")?;
                warning.set_item("skipped", n)?;
                Ok(Some(warning.into_any().unbind()))
            }
            Ok(Err(_)) => Ok(None),
            // timeout elapsed：正常终止迭代
            Err(_) => Ok(None),
        }
    }
}
