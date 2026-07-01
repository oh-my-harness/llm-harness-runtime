// crates/llm-harness-runtime/src/workflow/model.rs
//! 流程定义与运行时状态的纯数据模型。领域无关，可序列化（走 TaskStore 落盘）。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

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
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Step {
    /// LLM step: construct an AgentHarness and run one prompt.
    Llm {
        /// 步骤唯一标识。
        id: StepId,
        /// 人类可读名称。
        name: String,
        /// step agent 的指令。run_llm_step 用此字段作为 prompt。
        prompt: String,
        /// 本步允许的工具集。空 Vec = 不允许任何工具（字面语义，无 sentinel）。
        #[serde(default)]
        allowed_tools: Vec<String>,
        /// Optional runtime execution policy for this step.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy: Option<StepExecutionPolicy>,
    },
    /// Non-LLM step: execute a registered deterministic executor.
    Executor {
        /// 步骤唯一标识。
        id: StepId,
        /// 人类可读名称。
        name: String,
        /// Executor registry key.
        executor_name: String,
        /// Executor-specific JSON config.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<serde_json::Value>,
        /// Optional runtime execution policy for this step.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy: Option<StepExecutionPolicy>,
    },
}

/// Runtime policy applied uniformly to LLM and executor steps.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepExecutionPolicy {
    /// Per-attempt timeout in milliseconds. Absent means no runtime step timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Maximum execution attempts for step execution failures. Includes the first attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    /// Fixed delay before retrying a failed attempt, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_backoff_ms: Option<u64>,
}

impl<'de> Deserialize<'de> for Step {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum TaggedStep {
            Llm {
                id: StepId,
                name: String,
                prompt: String,
                #[serde(default)]
                allowed_tools: Vec<String>,
                #[serde(default)]
                policy: Option<StepExecutionPolicy>,
            },
            Executor {
                id: StepId,
                name: String,
                executor_name: String,
                #[serde(default)]
                config: Option<serde_json::Value>,
                #[serde(default)]
                policy: Option<StepExecutionPolicy>,
            },
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyLlmStep {
            id: StepId,
            name: String,
            prompt: String,
            #[serde(default)]
            allowed_tools: Vec<String>,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum StepCompat {
            Tagged(TaggedStep),
            LegacyLlm(LegacyLlmStep),
        }

        match StepCompat::deserialize(deserializer)? {
            StepCompat::Tagged(TaggedStep::Llm {
                id,
                name,
                prompt,
                allowed_tools,
                policy,
            }) => Ok(Self::Llm {
                id,
                name,
                prompt,
                allowed_tools,
                policy,
            }),
            StepCompat::Tagged(TaggedStep::Executor {
                id,
                name,
                executor_name,
                config,
                policy,
            }) => Ok(Self::Executor {
                id,
                name,
                executor_name,
                config,
                policy,
            }),
            StepCompat::LegacyLlm(LegacyLlmStep {
                id,
                name,
                prompt,
                allowed_tools,
            }) => Ok(Self::Llm {
                id,
                name,
                prompt,
                allowed_tools,
                policy: None,
            }),
        }
    }
}

impl Step {
    pub fn llm(
        id: impl Into<String>,
        name: impl Into<String>,
        prompt: impl Into<String>,
        allowed_tools: Vec<String>,
    ) -> Self {
        Self::Llm {
            id: id.into(),
            name: name.into(),
            prompt: prompt.into(),
            allowed_tools,
            policy: None,
        }
    }

    pub fn executor(
        id: impl Into<String>,
        name: impl Into<String>,
        executor_name: impl Into<String>,
        config: Option<serde_json::Value>,
    ) -> Self {
        Self::Executor {
            id: id.into(),
            name: name.into(),
            executor_name: executor_name.into(),
            config,
            policy: None,
        }
    }

    #[must_use]
    pub fn with_policy(mut self, policy: StepExecutionPolicy) -> Self {
        match &mut self {
            Self::Llm { policy: slot, .. } | Self::Executor { policy: slot, .. } => {
                *slot = Some(policy);
            }
        }
        self
    }

    pub fn id(&self) -> &StepId {
        match self {
            Self::Llm { id, .. } | Self::Executor { id, .. } => id,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Llm { name, .. } | Self::Executor { name, .. } => name,
        }
    }

    pub fn policy(&self) -> Option<&StepExecutionPolicy> {
        match self {
            Self::Llm { policy, .. } | Self::Executor { policy, .. } => policy.as_ref(),
        }
    }

    pub fn allowed_tools(&self) -> &[String] {
        match self {
            Self::Llm { allowed_tools, .. } => allowed_tools.as_slice(),
            Self::Executor { .. } => &[],
        }
    }

    pub fn prompt(&self) -> Option<&str> {
        match self {
            Self::Llm { prompt, .. } => Some(prompt.as_str()),
            Self::Executor { .. } => None,
        }
    }

    pub fn executor_name(&self) -> Option<&str> {
        match self {
            Self::Executor { executor_name, .. } => Some(executor_name.as_str()),
            Self::Llm { .. } => None,
        }
    }

    pub fn executor_config(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Executor { config, .. } => config.as_ref(),
            Self::Llm { .. } => None,
        }
    }
}

/// 步骤间的跳转边。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    /// 起始步骤 ID。
    pub from: StepId,
    /// 目标步骤 ID。
    pub to: StepId,
    /// Optional transition condition. A string is a legacy/custom judge label;
    /// an object is a runtime-evaluable declarative condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<EdgeCondition>,
}

/// Edge condition. String labels remain supported for custom judges; structured
/// expressions can be evaluated by the built-in `EdgeConditionJudge`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EdgeCondition {
    Label(String),
    Expr(ConditionExpr),
}

/// Declarative condition evaluated against `StepResult.structured`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ConditionExpr {
    Exists {
        pointer: String,
    },
    Missing {
        pointer: String,
    },
    Eq {
        pointer: String,
        value: serde_json::Value,
    },
    Ne {
        pointer: String,
        value: serde_json::Value,
    },
    Gt {
        pointer: String,
        value: f64,
    },
    Gte {
        pointer: String,
        value: f64,
    },
    Lt {
        pointer: String,
        value: f64,
    },
    Lte {
        pointer: String,
        value: f64,
    },
}

impl ConditionExpr {
    pub fn pointer(&self) -> &str {
        match self {
            Self::Exists { pointer }
            | Self::Missing { pointer }
            | Self::Eq { pointer, .. }
            | Self::Ne { pointer, .. }
            | Self::Gt { pointer, .. }
            | Self::Gte { pointer, .. }
            | Self::Lt { pointer, .. }
            | Self::Lte { pointer, .. } => pointer,
        }
    }
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
    /// Fail the workflow with a reason.
    Fail { reason: String },
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
            steps: vec![Step::llm("a", "Step A", "do step A", vec!["tool1".into()])],
            edges: vec![],
        };
        let json = serde_json::to_string(&wf).unwrap();
        let back: Workflow = serde_json::from_str(&json).unwrap();
        assert_eq!(back.entry_step, "a");
        assert_eq!(back.steps.len(), 1);
    }

    #[test]
    fn edge_condition_legacy_label_roundtrip() {
        let edge = Edge {
            from: "a".into(),
            to: "b".into(),
            condition: Some(EdgeCondition::Label("ok".into())),
        };
        let json = serde_json::to_string(&edge).unwrap();
        assert!(json.contains("\"condition\":\"ok\""));

        let back: Edge = serde_json::from_str(&json).unwrap();

        assert_eq!(back.condition, Some(EdgeCondition::Label("ok".into())));
    }

    #[test]
    fn edge_condition_expression_roundtrip() {
        let edge = Edge {
            from: "a".into(),
            to: "b".into(),
            condition: Some(EdgeCondition::Expr(ConditionExpr::Eq {
                pointer: "/status".into(),
                value: serde_json::json!("ok"),
            })),
        };
        let json = serde_json::to_string(&edge).unwrap();
        assert!(json.contains("\"op\":\"eq\""));

        let back: Edge = serde_json::from_str(&json).unwrap();

        assert_eq!(
            back.condition,
            Some(EdgeCondition::Expr(ConditionExpr::Eq {
                pointer: "/status".into(),
                value: serde_json::json!("ok"),
            }))
        );
    }

    #[test]
    fn legacy_llm_step_without_kind_deserializes() {
        let json = r#"{
            "entry_step": "a",
            "steps": [{
                "id": "a",
                "name": "Step A",
                "prompt": "do step A",
                "allowed_tools": ["tool1"]
            }],
            "edges": []
        }"#;
        let wf: Workflow = serde_json::from_str(json).unwrap();
        assert_eq!(wf.steps.len(), 1);
        assert!(matches!(wf.steps[0], Step::Llm { .. }));
        assert_eq!(wf.steps[0].id(), "a");
    }

    #[test]
    fn executor_step_roundtrip() {
        let step = Step::executor(
            "exec",
            "Executor",
            "json_transform",
            Some(serde_json::json!({ "path": "$.value" })),
        );
        let json = serde_json::to_string(&step).unwrap();
        assert!(json.contains("\"kind\":\"executor\""));
        let back: Step = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id(), "exec");
        assert_eq!(back.executor_name(), Some("json_transform"));
    }

    #[test]
    fn step_execution_policy_roundtrip() {
        let step = Step::executor("exec", "Executor", "json_transform", None).with_policy(
            StepExecutionPolicy {
                timeout_ms: Some(1_000),
                max_attempts: Some(3),
                retry_backoff_ms: Some(25),
            },
        );
        let json = serde_json::to_string(&step).unwrap();
        assert!(json.contains("\"policy\""));

        let back: Step = serde_json::from_str(&json).unwrap();

        assert_eq!(
            back.policy(),
            Some(&StepExecutionPolicy {
                timeout_ms: Some(1_000),
                max_attempts: Some(3),
                retry_backoff_ms: Some(25),
            })
        );
    }

    #[test]
    fn invalid_step_kind_is_rejected() {
        let json = r#"{
            "kind": "unknown",
            "id": "a",
            "name": "Step A",
            "prompt": "do step A"
        }"#;
        let err = serde_json::from_str::<Step>(json).unwrap_err();
        assert!(!err.to_string().is_empty());
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
        let t3 = Transition::Fail {
            reason: "bad".into(),
        };
        let json3 = serde_json::to_string(&t3).unwrap();
        assert!(json3.contains("\"fail\""));
    }

    #[test]
    fn workflow_status_serializes_snake_case() {
        let json = serde_json::to_string(&WorkflowStatus::Running).unwrap();
        assert_eq!(json, "\"running\"");
    }
}
