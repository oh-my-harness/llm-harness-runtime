use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ── StopReason ────────────────────────────────────────────────────────────────

/// LLM 停止生成的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    /// 模型自然结束。
    EndTurn,
    /// 达到 max_tokens 限制被截断。
    MaxTokens,
    /// 匹配到停止序列。
    StopSequence,
    /// 模型请求调用工具。
    ToolUse,
    /// 未知或未分类的停止原因。
    Other,
}

// ── DiagnosticLevel ───────────────────────────────────────────────────────────

/// Skills / PromptTemplates 加载过程中产生的诊断级别。
#[derive(Debug, Clone, Copy)]
pub enum DiagnosticLevel {
    /// 可继续的警告（如单个文件格式非法）。
    Warn,
    /// 整体操作失败。
    Error,
}

// ── HarnessPhase ──────────────────────────────────────────────────────────────

/// AgentHarness 的运行阶段。
///
/// 提升到 types crate 是因为 `HarnessError::NotIdle` 需要携带它，
/// 而 `HarnessError` 在 types 中定义。
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum HarnessPhase {
    /// 无正在进行的操作，接受新请求。
    Idle,
    /// 正在执行 agent loop（LLM 调用 + tool 执行）。
    Turning,
    /// 正在执行 compaction。
    Compacting,
    /// 正在执行分支导航。
    Branching,
}

// ── ToolError ─────────────────────────────────────────────────────────────────

/// Tool 执行失败。
#[derive(thiserror::Error, Debug)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("tool aborted")]
    Aborted,
    #[error("tool execution failed: {0}")]
    Execution(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

// ── AgentError ────────────────────────────────────────────────────────────────

/// Agent 运行时错误。
#[derive(thiserror::Error, Debug, Clone)]
pub enum AgentError {
    #[error("llm provider error: {0}")]
    Provider(String),
    #[error("tool error: {tool_name}: {message}")]
    Tool { tool_name: String, message: String },
    #[error("aborted")]
    Aborted,
    #[error("agent is not idle")]
    NotIdle,
    #[error("internal: {0}")]
    Internal(String),
}

// ── EnvError ──────────────────────────────────────────────────────────────────

/// 执行环境错误。
#[derive(thiserror::Error, Debug)]
pub enum EnvError {
    #[error("path not found: {0}")]
    NotFound(PathBuf),
    #[error("permission denied: {0}")]
    PermissionDenied(PathBuf),
    #[error("path already exists: {0}")]
    AlreadyExists(PathBuf),
    #[error("not a directory: {0}")]
    NotADirectory(PathBuf),
    #[error("is a directory: {0}")]
    IsADirectory(PathBuf),
    #[error("operation aborted")]
    Aborted,
    #[error("invalid utf-8 in {0}")]
    InvalidUtf8(PathBuf),
    #[error("shell command failed: exit {exit_code}")]
    ShellFailed { exit_code: i32, stderr: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("other: {0}")]
    Other(String),
}

// ── SessionError ──────────────────────────────────────────────────────────────

/// Session 操作错误。
#[derive(thiserror::Error, Debug)]
pub enum SessionError {
    #[error("entry not found: {0}")]
    EntryNotFound(crate::EntryId),
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("session already exists: {0}")]
    SessionAlreadyExists(String),
    #[error("not a leaf: {0}")]
    NotALeaf(crate::EntryId),
    #[error("invalid parent: {0}")]
    InvalidParent(crate::EntryId),
    #[error("storage io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization: {0}")]
    Serialization(String),
    #[error("concurrent modification")]
    ConcurrentModification,
}

// ── CompactionError ───────────────────────────────────────────────────────────

/// Compaction 操作错误。
#[derive(thiserror::Error, Debug)]
pub enum CompactionError {
    #[error("not enough tokens to compact")]
    InsufficientTokens,
    #[error("summary model call failed: {0}")]
    SummaryFailed(String),
    #[error("first_kept_entry {0} not found in compaction path")]
    InvalidFirstKeptEntry(crate::EntryId),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Agent(#[from] AgentError),
}

// ── TemplateError ─────────────────────────────────────────────────────────────

/// PromptTemplate 错误。
#[derive(thiserror::Error, Debug)]
pub enum TemplateError {
    #[error("template not found: {0}")]
    NotFound(String),
    #[error("missing required argument at position {0}")]
    MissingArg(usize),
    #[error("invalid argument syntax: {0}")]
    InvalidSyntax(String),
}

// ── HarnessError ──────────────────────────────────────────────────────────────

/// AgentHarness 顶层错误。
#[derive(thiserror::Error, Debug)]
pub enum HarnessError {
    #[error("harness is not idle (current phase: {0:?})")]
    NotIdle(HarnessPhase),
    #[error("skill not found: {0}")]
    SkillNotFound(String),
    #[error("template not found: {0}")]
    TemplateNotFound(String),
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Compaction(#[from] CompactionError),
    #[error(transparent)]
    Env(#[from] EnvError),
    #[error(transparent)]
    Template(#[from] TemplateError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_error_display() {
        let e = ToolError::InvalidArguments("bad arg".into());
        assert!(e.to_string().contains("bad arg"));
        let e2 = ToolError::Aborted;
        assert_eq!(e2.to_string(), "tool aborted");
    }

    #[test]
    fn agent_error_is_clone() {
        let e = AgentError::Provider("rate limit".into());
        let e2 = e.clone();
        assert_eq!(e.to_string(), e2.to_string());
    }

    #[test]
    fn harness_error_from_agent_error() {
        let ae = AgentError::Aborted;
        let he: HarnessError = ae.into();
        assert!(he.to_string().contains("aborted"));
    }

    #[test]
    fn compaction_error_from_session_error() {
        use crate::EntryId;
        let se = SessionError::EntryNotFound(EntryId::new());
        let ce: CompactionError = se.into();
        assert!(ce.to_string().contains("entry not found"));
    }

    #[test]
    fn stop_reason_copy() {
        let r = StopReason::ToolUse;
        let r2 = r;
        assert_eq!(r, r2);
    }

    #[test]
    fn harness_phase_not_idle_in_error() {
        let e = HarnessError::NotIdle(HarnessPhase::Turning);
        assert!(e.to_string().contains("Turning"));
    }
}
