//! `PyAgentHarness` — 包装 `AgentHarness`。
//!
//! 暴露 `prompt()`、`events()`、`message_count()`、`phase()`。
//! 使用与 `PyAgent` 相同的 GIL+tokio 模式：`py.detach()` + `runtime.block_on()`。
//!
//! # 事件类型说明
//!
//! `AgentHarness::subscribe()` 返回 `broadcast::Receiver<Arc<AgentHarnessEvent>>`，
//! 与 `Agent::subscribe()` 返回的 `Receiver<Arc<AgentEvent>>` 不同。
//! `AgentHarnessEvent` 是 `AgentEvent` 的超集：`Agent(AgentEvent)` 变体包装底层
//! agent 事件，其余变体是 harness 级事件（phase change、compaction 等）。
//! 本模块实现独立的 `PyHarnessEventIterator` 处理此类型。

use std::sync::Arc;

use llm_harness_agent::{AgentHarness, AgentHarnessEvent};
use llm_harness_types::HarnessPhase;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use tokio::sync::broadcast;

use crate::event_stream::agent_event_to_dict;
use crate::pyagent::runtime;

/// 将 `HarnessPhase` 转为字符串标识。
fn phase_str(phase: HarnessPhase) -> &'static str {
    match phase {
        HarnessPhase::Idle => "idle",
        HarnessPhase::Turning => "turning",
        HarnessPhase::Compacting => "compacting",
        HarnessPhase::Branching => "branching",
        _ => "unknown",
    }
}

/// 将 `AgentHarnessEvent` 转换为 Python dict。
///
/// `Agent(AgentEvent)` 变体委托 `agent_event_to_dict`，其余变体生成
/// 带 `"type"` 字段的简单 dict。
fn harness_event_to_dict(py: Python<'_>, event: &AgentHarnessEvent) -> PyResult<Py<PyAny>> {
    match event {
        AgentHarnessEvent::Agent(agent_event) => agent_event_to_dict(py, agent_event),
        AgentHarnessEvent::PhaseChange { from, to } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "phase_change")?;
            dict.set_item("from", phase_str(*from))?;
            dict.set_item("to", phase_str(*to))?;
            Ok(dict.into_any().unbind())
        }
        AgentHarnessEvent::ModelUpdate { from, to } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "model_update")?;
            dict.set_item("from", from.clone())?;
            dict.set_item("to", to.clone())?;
            Ok(dict.into_any().unbind())
        }
        AgentHarnessEvent::ThinkingLevelUpdate { from, to } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "thinking_level_update")?;
            dict.set_item("from", format!("{from:?}"))?;
            dict.set_item("to", format!("{to:?}"))?;
            Ok(dict.into_any().unbind())
        }
        AgentHarnessEvent::ToolsUpdate { added, removed } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "tools_update")?;
            dict.set_item("added", added.clone())?;
            dict.set_item("removed", removed.clone())?;
            Ok(dict.into_any().unbind())
        }
        AgentHarnessEvent::ActiveToolsUpdate { active } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "active_tools_update")?;
            match active {
                Some(tools) => dict.set_item("active", tools.clone())?,
                None => dict.set_item("active", py.None())?,
            }
            Ok(dict.into_any().unbind())
        }
        AgentHarnessEvent::ResourcesUpdate {
            skills,
            templates,
            diagnostics,
        } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "resources_update")?;
            dict.set_item("skills", *skills)?;
            dict.set_item("templates", *templates)?;
            let diag_strs: Vec<String> = diagnostics.iter().map(|d| format!("{d:?}")).collect();
            dict.set_item("diagnostics", diag_strs)?;
            Ok(dict.into_any().unbind())
        }
        AgentHarnessEvent::SessionInfoUpdate { name } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "session_info_update")?;
            dict.set_item("name", name.clone())?;
            Ok(dict.into_any().unbind())
        }
        AgentHarnessEvent::CompactionStart { estimated_tokens } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "compaction_start")?;
            dict.set_item("estimated_tokens", *estimated_tokens)?;
            Ok(dict.into_any().unbind())
        }
        AgentHarnessEvent::CompactionEnd { stats, error } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "compaction_end")?;
            match stats {
                Some(s) => {
                    dict.set_item("stats", format!("{s:?}"))?;
                }
                None => dict.set_item("stats", py.None())?,
            }
            match error {
                Some(e) => dict.set_item("error", e.clone())?,
                None => dict.set_item("error", py.None())?,
            }
            Ok(dict.into_any().unbind())
        }
        AgentHarnessEvent::QueueUpdate {
            steer_len,
            follow_up_len,
        } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "queue_update")?;
            dict.set_item("steer_len", *steer_len)?;
            dict.set_item("follow_up_len", *follow_up_len)?;
            Ok(dict.into_any().unbind())
        }
        AgentHarnessEvent::SavePoint { entries_flushed } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "savepoint")?;
            dict.set_item("entries_flushed", *entries_flushed)?;
            Ok(dict.into_any().unbind())
        }
        AgentHarnessEvent::BranchForked {
            from,
            new_leaf,
            label,
        } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "branch_forked")?;
            dict.set_item("from", format!("{from:?}"))?;
            dict.set_item("new_leaf", format!("{new_leaf:?}"))?;
            match label {
                Some(l) => dict.set_item("label", l.clone())?,
                None => dict.set_item("label", py.None())?,
            }
            Ok(dict.into_any().unbind())
        }
        AgentHarnessEvent::BranchSwitched { from, to } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "branch_switched")?;
            dict.set_item("from", format!("{from:?}"))?;
            dict.set_item("to", format!("{to:?}"))?;
            Ok(dict.into_any().unbind())
        }
        AgentHarnessEvent::BranchDeleted { leaf } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "branch_deleted")?;
            dict.set_item("leaf", format!("{leaf:?}"))?;
            Ok(dict.into_any().unbind())
        }
        AgentHarnessEvent::BranchSummarized { leaf, summary } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "branch_summarized")?;
            dict.set_item("leaf", format!("{leaf:?}"))?;
            dict.set_item("summary", summary.clone())?;
            Ok(dict.into_any().unbind())
        }
        AgentHarnessEvent::ToolCallStart {
            tool_use_id,
            tool_name,
            args,
        } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "tool_call_start")?;
            dict.set_item("tool_use_id", tool_use_id.clone())?;
            dict.set_item("tool_name", tool_name.clone())?;
            dict.set_item("args", args.to_string())?;
            Ok(dict.into_any().unbind())
        }
        AgentHarnessEvent::ToolCallEnd {
            tool_use_id,
            tool_name,
            result,
        } => {
            let dict = PyDict::new(py);
            dict.set_item("type", "tool_call_end")?;
            dict.set_item("tool_use_id", tool_use_id.clone())?;
            dict.set_item("tool_name", tool_name.clone())?;
            dict.set_item("result", format!("{result:?}"))?;
            Ok(dict.into_any().unbind())
        }
        AgentHarnessEvent::Settled => {
            let dict = PyDict::new(py);
            dict.set_item("type", "settled")?;
            Ok(dict.into_any().unbind())
        }
        AgentHarnessEvent::Aborted => {
            let dict = PyDict::new(py);
            dict.set_item("type", "aborted")?;
            Ok(dict.into_any().unbind())
        }
    }
}

/// Python 迭代器，包装 `broadcast::Receiver<Arc<AgentHarnessEvent>>`。
///
/// 与 `PyEventIterator` 对称，但处理 harness 级事件类型。
#[pyclass(name = "HarnessEventIterator")]
pub struct PyHarnessEventIterator {
    rx: Option<broadcast::Receiver<Arc<AgentHarnessEvent>>>,
    timeout_ms: u64,
    handle: tokio::runtime::Handle,
}

impl PyHarnessEventIterator {
    pub fn new(
        rx: broadcast::Receiver<Arc<AgentHarnessEvent>>,
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
impl PyHarnessEventIterator {
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
                let dict = harness_event_to_dict(py, &event)?;
                Ok(Some(dict))
            }
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                let warning = PyDict::new(py);
                warning.set_item("type", "lagged")?;
                warning.set_item("skipped", n)?;
                Ok(Some(warning.into_any().unbind()))
            }
            Ok(Err(_)) => Ok(None),
            // timeout elapsed：正常终止迭代（py.detach 不捕获 panic，
            // panic 会直接传播；Err 分支只匹配 Elapsed）
            Err(_) => Ok(None),
        }
    }
}

/// Python 侧的 `AgentHarness` 包装类。
#[pyclass(name = "AgentHarness")]
pub struct PyAgentHarness {
    pub(crate) harness: Arc<AgentHarness>,
}

impl PyAgentHarness {
    pub fn new(harness: Arc<AgentHarness>) -> Self {
        Self { harness }
    }
}

#[pymethods]
impl PyAgentHarness {
    /// 同步执行 prompt，阻塞直到完成。
    ///
    /// `AgentHarness::prompt()` 返回 `Result<(), HarnessError>`——
    /// 不返回回复文本（回复通过事件流或 `build_context()` 获取）。
    /// 成功时返回 None。
    fn prompt(&self, py: Python<'_>, text: &str) -> PyResult<()> {
        let harness = self.harness.clone();
        let text = text.to_string();
        let rt = runtime(py);
        let result = py.detach(move || rt.block_on(async move { harness.prompt(&text).await }));
        match result {
            Ok(()) => Ok(()),
            Err(e) => Err(pyo3::exceptions::PyRuntimeError::new_err(e.to_string())),
        }
    }

    /// 获取当前会话中的消息数量。
    ///
    /// 通过 `build_context()` 获取消息列表后计数。需释放 GIL 执行 async 调用。
    fn message_count(&self, py: Python<'_>) -> PyResult<usize> {
        let harness = self.harness.clone();
        let rt = runtime(py);
        let result = py.detach(move || rt.block_on(async move { harness.build_context().await }));
        match result {
            Ok(ctx) => Ok(ctx.messages.len()),
            Err(e) => Err(pyo3::exceptions::PyRuntimeError::new_err(e.to_string())),
        }
    }

    /// 获取当前 phase（"idle" / "turning" / "compacting" / "branching"）。
    fn phase(&self) -> &'static str {
        phase_str(self.harness.state().phase)
    }

    /// 返回 harness 事件迭代器。`timeout_ms` 为单次 `__next__` 等待超时（毫秒）。
    #[pyo3(signature = (timeout_ms=5000))]
    fn events(&self, py: Python<'_>, timeout_ms: u64) -> PyResult<Py<PyHarnessEventIterator>> {
        let rx = self.harness.subscribe();
        let handle = runtime(py).handle().clone();
        Py::new(py, PyHarnessEventIterator::new(rx, timeout_ms, handle))
    }

    /// 取消当前正在运行的 prompt（如果有）。不阻塞。
    ///
    /// 发出取消信号后，正在执行的 `prompt()` 会在最近的取消点抛出
    /// `RuntimeError`。可以安全地在非 prompt 期间调用（无操作）。
    fn abort(&self) {
        self.harness.abort();
    }

    /// 收集事件直到 harness 进入 settled 状态（phase=idle 且无更多事件）。
    ///
    /// 便捷方法，等价于：
    /// ```python
    /// events = []
    /// for event in harness.events(timeout_ms=timeout_ms):
    ///     events.append(event)
    ///     if event.get("type") in ("settled", "aborted"):
    ///         break
    /// ```
    ///
    /// 返回事件列表。若超时未收到 settled/aborted，返回已收集的事件。
    #[pyo3(signature = (timeout_ms=30000))]
    fn collect_until_settled(&self, py: Python<'_>, timeout_ms: u64) -> PyResult<Vec<Py<PyAny>>> {
        let rx = self.harness.subscribe();
        let handle = runtime(py).handle().clone();
        let timeout = std::time::Duration::from_millis(timeout_ms);

        let events: Vec<Py<PyAny>> = py.detach(move || {
            let mut events = Vec::new();
            let mut rx = rx;
            loop {
                let recv =
                    handle.block_on(async { tokio::time::timeout(timeout, rx.recv()).await });
                match recv {
                    Ok(Ok(event)) => {
                        let is_settled = matches!(
                            &*event,
                            AgentHarnessEvent::Settled | AgentHarnessEvent::Aborted
                        );
                        Python::attach(|py| {
                            if let Ok(dict) = harness_event_to_dict(py, &event) {
                                events.push(dict);
                            }
                        });
                        if is_settled {
                            break;
                        }
                    }
                    _ => break,
                }
            }
            events
        });

        Ok(events)
    }
}
