// crates/llm-harness-runtime/src/workflow/model.rs
//! 流程定义与运行时状态的纯数据模型。领域无关，可序列化（走 TaskStore 落盘）。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cost::CostAggregate;
use crate::task::{TaskError, TaskId, TaskResult};

/// 步骤标识。
pub type StepId = String;

/// 流程定义（单层）。纯数据，可序列化（走 TaskStore 落盘）。
/// 来源无关：人类规划 / LLM 生成（PlanWorkflowTool）/ 外部系统产出，都走此结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    /// 所有步骤。
    pub steps: Vec<Step>,
    /// 步骤间的边（条件跳转 / 无条件顺序）。
    pub edges: Vec<Edge>,
    /// 入口步骤 ID（必须在 steps 里）。
    pub entry_step: StepId,
}

/// 单个步骤定义。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    /// 步骤唯一标识。
    pub id: StepId,
    /// 人类可读名称。
    pub name: String,
    /// step agent 的指令（review-1）。run_step 用此字段作为 prompt，
    /// 不再用 step.name。自由 task 的 prompt 经 new_free_task 传入并写入此字段。
    pub prompt: String,
    /// 本步允许的工具集。空 Vec = 不允许任何工具（字面语义，无 sentinel）。
    pub allowed_tools: Vec<String>,
}

/// 步骤间的跳转边。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    /// 起始步骤 ID。
    pub from: StepId,
    /// 目标步骤 ID。
    pub to: StepId,
    /// 跳转条件标识（judge 识别，如 "thickness_ok"）。None = 无条件顺序跳转。
    /// 注意：这是 judge 识别的标识，不是 runtime 求值的表达式。
    pub condition: Option<String>,
}

/// task 生命周期状态（原 TaskState enum 的变体降为此枚举）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    Idle,
    Running,
    Paused,
    AwaitingHumanInput,
    Succeeded,
    Failed,
    Cancelled,
}

/// 流程运行时状态（统一表达 task 生命周期 + workflow 执行位置）。
/// 走 TaskStore 落盘，覆盖写。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowState {
    /// task 生命周期状态。
    pub status: WorkflowStatus,
    /// 暂停/失败/取消原因（对应 status = Paused/Failed/Cancelled）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// task 结果（status = Succeeded 时有值）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<TaskResult>,
    /// task 错误（status = Failed 时有值）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<TaskError>,
    /// task 启动时间（status != Idle 时有值）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// task 结束时间（status ∈ {Succeeded, Failed, Cancelled} 时有值）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,

    /// 当前执行步骤。
    pub current_step: StepId,
    /// 已完成的步骤历史（审计链）。
    #[serde(default)]
    pub step_history: Vec<StepRecord>,
    /// 规划此 workflow 的 task ID（review-9）。LLM 两阶段场景下记录 plan_task_id，
    /// 业务层恢复时不用自己维护两张 ID 表。人类规划场景为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_by: Option<TaskId>,
}

impl WorkflowState {
    /// 新 task 的初始状态：Idle，current_step = entry_step，空历史。
    pub fn new(entry_step: StepId) -> Self {
        Self {
            status: WorkflowStatus::Idle,
            reason: None,
            result: None,
            error: None,
            started_at: None,
            ended_at: None,
            current_step: entry_step,
            step_history: vec![],
            planned_by: None,
        }
    }
}

/// 单步执行结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    /// 所有 turn 的 TextDelta 拼接的完整文本输出。
    pub output: String,
    /// step agent 经 submit_step_result tool 提交的结构化结果。
    /// judge 直接读 ctx.last_result.structured，不用自己经 TaskStore 反查 checkpoint。
    /// None 表示 step agent 未调 submit_step_result（judge 需 fallback 到解析 output 或读 checkpoint）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<serde_json::Value>,
    /// 本次执行共调用了几次工具。
    pub tool_calls_count: u32,
    /// 本步 session ID（供调试和审计）。
    pub session_id: String,
    /// 本步 cost。
    pub cost: CostAggregate,
    /// 本步开始时间（run_step 入口记录）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// 本步结束时间（run_step 收尾记录）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
}

/// 步骤历史记录（审计链的一环）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    /// 步骤 ID。
    pub step_id: StepId,
    /// 开始时间。
    pub started_at: DateTime<Utc>,
    /// 结束时间。
    pub ended_at: DateTime<Utc>,
    /// 执行结果（若该步正常完成）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<StepResult>,
    /// 该步结束时的跳转决策。
    pub transition: Transition,
}

/// 步骤跳转决策（judge 返回，引擎消费）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transition {
    /// 跳到指定步骤（引擎校验合法性，非法跳转 = 越界，Abort）。
    To(StepId),
    /// 重跑当前步（step agent 重新 spawn，新建 harness）。
    Retry,
    /// 终止流程。
    Abort { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_state_roundtrip() {
        let state = WorkflowState::new("step_1".into());
        let json = serde_json::to_string(&state).unwrap();
        let back: WorkflowState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.status, WorkflowStatus::Idle);
        assert_eq!(back.current_step, "step_1");
        assert!(back.step_history.is_empty());
    }

    #[test]
    fn workflow_roundtrip() {
        let wf = Workflow {
            entry_step: "a".into(),
            steps: vec![Step {
                id: "a".into(),
                name: "Step A".into(),
                prompt: "do step A".into(),
                allowed_tools: vec!["tool1".into()],
            }],
            edges: vec![],
        };
        let json = serde_json::to_string(&wf).unwrap();
        let back: Workflow = serde_json::from_str(&json).unwrap();
        assert_eq!(back.entry_step, "a");
        assert_eq!(back.steps.len(), 1);
    }

    #[test]
    fn transition_serializes_snake_case() {
        let t = Transition::To("next".into());
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains("\"to\""));
        let t2 = Transition::Abort {
            reason: "bad".into(),
        };
        let json2 = serde_json::to_string(&t2).unwrap();
        assert!(json2.contains("\"abort\""));
    }

    #[test]
    fn workflow_status_serializes_snake_case() {
        let json = serde_json::to_string(&WorkflowStatus::Running).unwrap();
        assert_eq!(json, "\"running\"");
    }
}
