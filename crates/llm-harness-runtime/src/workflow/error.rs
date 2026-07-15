//! Workflow 模块结构化错误类型。

use crate::workflow::model::WorkflowStatus;

// ── EdgeEndpoint ──────────────────────────────────────────────────────────

/// Edge 端点方向，用于 `EdgeEndpointNotFound` 报错。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeEndpoint {
    From,
    To,
}

impl std::fmt::Display for EdgeEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::From => write!(f, "from"),
            Self::To => write!(f, "to"),
        }
    }
}

// ── WorkflowValidationError ───────────────────────────────────────────────

/// `validate_workflow` 及其 helper 产生的校验错误。
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum WorkflowValidationError {
    #[error("step with empty id (name='{name}')")]
    EmptyStepId { name: String },

    #[error("duplicate step id '{id}'")]
    DuplicateStepId { id: String },

    #[error("entry_step '{entry_step}' not in steps")]
    EntryStepNotFound { entry_step: String },

    #[error("edge {which} '{endpoint}' not in steps")]
    EdgeEndpointNotFound {
        which: EdgeEndpoint,
        endpoint: String,
    },

    #[error("edge '{from} -> {to}' has empty condition label")]
    EmptyConditionLabel { from: String, to: String },

    #[error("edge '{from} -> {to}' condition pointer must be empty or start with '/'")]
    InvalidConditionPointer { from: String, to: String },

    #[error("edge '{from} -> {to}' numeric condition value must be finite")]
    NonFiniteConditionValue { from: String, to: String },

    #[error("step '{id}' has no incoming edge (orphan)")]
    OrphanStep { id: String },

    #[error("step '{id}' has empty prompt")]
    EmptyPrompt { id: String },

    #[error("step '{id}' has empty tool name in allowed_tools")]
    EmptyToolName { id: String },

    #[error("step '{id}' has empty executor_name")]
    EmptyExecutorName { id: String },

    #[error("too many steps ({count}, max {max})")]
    TooManySteps { count: usize, max: usize },

    #[error("too many edges ({count}, max {max})")]
    TooManyEdges { count: usize, max: usize },

    #[error("step '{step_id}' policy.{field} must be between {min} and {max}")]
    PolicyOutOfRange {
        step_id: String,
        field: &'static str,
        min: u64,
        max: u64,
    },

    #[error("step '{step_id}' policy.{field} must be <= {max}")]
    PolicyOverMax {
        step_id: String,
        field: &'static str,
        max: u64,
    },
}

// ── WorkflowError ─────────────────────────────────────────────────────────

/// `WorkflowEngine` 公共 API 和内部执行的错误。
#[derive(thiserror::Error, Debug)]
pub enum WorkflowError {
    #[error(transparent)]
    Validation(#[from] WorkflowValidationError),

    #[error("run: task is already running (concurrent run() detected)")]
    AlreadyRunning,

    #[error("run: task is not Idle/Paused/Running (status={status:?})")]
    InvalidStatus { status: WorkflowStatus },

    #[error("restore: workflow not found in store for task {task_id}")]
    WorkflowNotFound { task_id: String },

    #[error("run: current_step '{step_id}' not in workflow")]
    CurrentStepNotFound { step_id: String },

    #[error("step '{id}' aborted")]
    StepAborted { id: String },

    #[error("step '{id}' timed out after {timeout_ms} ms")]
    StepTimeout { id: String, timeout_ms: u64 },

    #[error("step '{id}' failed after {max_attempts} attempts: {source}")]
    StepExhausted {
        id: String,
        max_attempts: u32,
        source: anyhow::Error,
    },

    #[error("step '{id}' failed: {source}")]
    StepFailed { id: String, source: anyhow::Error },

    #[error("executor '{name}' not registered")]
    ExecutorNotFound { name: String },

    /// task_store I/O 失败（load/save workflow/state/checkpoint）。
    #[error("task store error: {0}")]
    StoreError(#[from] anyhow::Error),

    /// env_factory / session_factory create() 失败。
    #[error("factory error: {0}")]
    Factory(#[from] llm_harness_types::AgentError),

    /// prompt task panic 或 event stream 异常关闭等引擎内部故障。
    #[error("{0}")]
    Internal(String),

    /// 引擎主动 fail_task（apply_transition 中的非法跳转、max_retries 超限等）。
    /// 携带人类可读的失败原因，与 state.error 同步。
    #[error("task failed: {0}")]
    TaskFailed(String),

    #[error(transparent)]
    Harness(#[from] llm_harness_types::HarnessError),

    #[error("task paused: {0}")]
    Paused(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_error_display() {
        let e = WorkflowValidationError::EmptyStepId { name: "foo".into() };
        assert!(e.to_string().contains("empty id"));
        assert!(e.to_string().contains("foo"));
    }

    #[test]
    fn workflow_error_from_validation() {
        let ve = WorkflowValidationError::DuplicateStepId { id: "s1".into() };
        let we: WorkflowError = ve.into();
        assert!(we.to_string().contains("duplicate step id"));
        assert!(we.to_string().contains("s1"));
    }

    #[test]
    fn edge_endpoint_display() {
        assert_eq!(EdgeEndpoint::From.to_string(), "from");
        assert_eq!(EdgeEndpoint::To.to_string(), "to");
    }
}
