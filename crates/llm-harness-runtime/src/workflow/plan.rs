// crates/llm-harness-runtime/src/workflow/plan.rs
//! plan 的来源无关性：validate_workflow 公共校验 + PlanWorkflowTool。
//!
//! Workflow 是 plan 的 runtime 内部表达，来源无关（人类/LLM/外部系统）。
//! 任何来源都过同一套校验、同一套落盘、同一套崩溃恢复。

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{Tool, ToolContext, ToolError, ToolResult};
use serde::Deserialize;

use crate::error::Result;
use crate::task::TaskId;
use crate::task_store::TaskStore;
use crate::workflow::model::{
    ConditionExpr, Edge, EdgeCondition, Step, StepExecutionPolicy, StepId, Workflow,
};

const MAX_STEP_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;
const MAX_STEP_ATTEMPTS: u32 = 10;
const MAX_STEP_RETRY_BACKOFF_MS: u64 = 5 * 60 * 1000;

/// 校验 workflow 定义。任何来源（人类/LLM/外部）的 plan 都过此校验。
///
/// 规则：
/// - step id 非空且唯一（无重复）。
/// - `entry_step` 必须在 `steps` 里。
/// - 每条 edge 的 `from` / `to` 必须都在 `steps` 里。
/// - 每个 step（除 `entry_step`）至少有一条入边。
/// - LLM step: prompt must be non-empty; allowed_tools entries must be non-empty.
/// - Executor step: executor_name must be non-empty.
pub fn validate_workflow(wf: &Workflow) -> Result<()> {
    // step.id 非空（空 id 会导致 entry_step / edge 匹配歧义）。
    for step in &wf.steps {
        if step.id().is_empty() {
            anyhow::bail!(
                "validate_workflow: step with empty id (name='{}')",
                step.name()
            );
        }
    }
    // 重复 id 检测：HashSet 会去重，故 len 不符即有重复。
    let step_ids: std::collections::HashSet<&str> =
        wf.steps.iter().map(|s| s.id().as_str()).collect();
    if step_ids.len() != wf.steps.len() {
        // 找出第一个重复 id 用于报错。
        let mut seen = std::collections::HashSet::new();
        let dup = wf
            .steps
            .iter()
            .find(|s| !seen.insert(s.id().as_str()))
            .map(|s| s.id().as_str())
            .unwrap_or("?");
        anyhow::bail!("validate_workflow: duplicate step id '{}'", dup);
    }

    if !step_ids.contains(wf.entry_step.as_str()) {
        anyhow::bail!(
            "validate_workflow: entry_step '{}' not in steps",
            wf.entry_step
        );
    }
    for edge in &wf.edges {
        if !step_ids.contains(edge.from.as_str()) {
            anyhow::bail!("validate_workflow: edge from '{}' not in steps", edge.from);
        }
        if !step_ids.contains(edge.to.as_str()) {
            anyhow::bail!("validate_workflow: edge to '{}' not in steps", edge.to);
        }
        validate_edge_condition(edge)?;
    }
    let has_incoming = |id: &str| wf.edges.iter().any(|e| e.to == id);
    for step in &wf.steps {
        if step.id() != &wf.entry_step && !has_incoming(step.id()) {
            anyhow::bail!(
                "validate_workflow: step '{}' has no incoming edge (orphan)",
                step.id()
            );
        }
    }
    for step in &wf.steps {
        validate_step_policy(step.id(), step.policy())?;
        match step {
            Step::Llm {
                prompt,
                allowed_tools,
                ..
            } => {
                if prompt.is_empty() {
                    anyhow::bail!("validate_workflow: step '{}' has empty prompt", step.id());
                }
                for tool in allowed_tools {
                    if tool.is_empty() {
                        anyhow::bail!(
                            "validate_workflow: step '{}' has empty tool name in allowed_tools",
                            step.id()
                        );
                    }
                }
            }
            Step::Executor { executor_name, .. } => {
                if executor_name.is_empty() {
                    anyhow::bail!(
                        "validate_workflow: step '{}' has empty executor_name",
                        step.id()
                    );
                }
            }
        }
    }
    // 安全阀：限制 step/edge 数量，防止恶意或失控的 workflow 定义。
    const MAX_STEPS: usize = 1000;
    const MAX_EDGES: usize = 5000;
    if wf.steps.len() > MAX_STEPS {
        anyhow::bail!(
            "validate_workflow: too many steps ({}, max {})",
            wf.steps.len(),
            MAX_STEPS
        );
    }
    if wf.edges.len() > MAX_EDGES {
        anyhow::bail!(
            "validate_workflow: too many edges ({}, max {})",
            wf.edges.len(),
            MAX_EDGES
        );
    }
    Ok(())
}

fn validate_step_policy(step_id: &str, policy: Option<&StepExecutionPolicy>) -> Result<()> {
    let Some(policy) = policy else {
        return Ok(());
    };
    if let Some(timeout_ms) = policy.timeout_ms
        && (timeout_ms == 0 || timeout_ms > MAX_STEP_TIMEOUT_MS)
    {
        anyhow::bail!(
            "validate_workflow: step '{}' policy.timeout_ms must be between 1 and {}",
            step_id,
            MAX_STEP_TIMEOUT_MS
        );
    }
    if let Some(max_attempts) = policy.max_attempts
        && (max_attempts == 0 || max_attempts > MAX_STEP_ATTEMPTS)
    {
        anyhow::bail!(
            "validate_workflow: step '{}' policy.max_attempts must be between 1 and {}",
            step_id,
            MAX_STEP_ATTEMPTS
        );
    }
    if let Some(retry_backoff_ms) = policy.retry_backoff_ms
        && retry_backoff_ms > MAX_STEP_RETRY_BACKOFF_MS
    {
        anyhow::bail!(
            "validate_workflow: step '{}' policy.retry_backoff_ms must be <= {}",
            step_id,
            MAX_STEP_RETRY_BACKOFF_MS
        );
    }
    Ok(())
}

fn validate_edge_condition(edge: &Edge) -> Result<()> {
    let Some(condition) = &edge.condition else {
        return Ok(());
    };
    match condition {
        EdgeCondition::Label(label) => {
            if label.is_empty() {
                anyhow::bail!(
                    "validate_workflow: edge '{} -> {}' has empty condition label",
                    edge.from,
                    edge.to
                );
            }
        }
        EdgeCondition::Expr(expr) => {
            validate_condition_pointer(edge, expr.pointer())?;
            match expr {
                ConditionExpr::Gt { value, .. }
                | ConditionExpr::Gte { value, .. }
                | ConditionExpr::Lt { value, .. }
                | ConditionExpr::Lte { value, .. }
                    if !value.is_finite() =>
                {
                    anyhow::bail!(
                        "validate_workflow: edge '{} -> {}' numeric condition value must be finite",
                        edge.from,
                        edge.to
                    );
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn validate_condition_pointer(edge: &Edge, pointer: &str) -> Result<()> {
    if pointer.is_empty() || pointer.starts_with('/') {
        return Ok(());
    }
    anyhow::bail!(
        "validate_workflow: edge '{} -> {}' condition pointer must be empty or start with '/'",
        edge.from,
        edge.to
    )
}

/// LLM 提交 workflow 计划的 tool。注入到规划阶段的自由 task。
///
/// LLM 调用：`submit_workflow(steps, edges, entry_step)` → 校验 + 落盘 + 返回 task_id。
/// 业务层据返回值起执行引擎（`WorkflowEngine::new`）。
pub struct PlanWorkflowTool {
    store: Arc<dyn TaskStore>,
    task_id: TaskId,
}

#[derive(Deserialize)]
struct PlanInput {
    steps: Vec<Step>,
    edges: Vec<Edge>,
    entry_step: StepId,
}

impl PlanWorkflowTool {
    pub fn new(store: Arc<dyn TaskStore>, task_id: TaskId) -> Self {
        Self { store, task_id }
    }
}

fn plan_workflow_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "steps": { "type": "array", "items": {
                "oneOf": [
                    {
                        "type": "object",
                        "properties": {
                            "kind": { "type": "string", "const": "llm" },
                            "id": { "type": "string" },
                            "name": { "type": "string" },
                            "prompt": { "type": "string", "description": "step agent 的指令（必填）" },
                            "allowed_tools": { "type": "array", "items": { "type": "string" } },
                            "policy": step_policy_schema()
                        },
                        "required": ["kind", "id", "name", "prompt"]
                    },
                    {
                        "type": "object",
                        "properties": {
                            "kind": { "type": "string", "const": "executor" },
                            "id": { "type": "string" },
                            "name": { "type": "string" },
                            "executor_name": { "type": "string" },
                            "config": { "type": "object" },
                            "policy": step_policy_schema()
                        },
                        "required": ["kind", "id", "name", "executor_name"]
                    }
                ]
            }},
            "edges": { "type": "array", "items": edge_schema() },
            "entry_step": { "type": "string" }
        },
        "required": ["steps", "edges", "entry_step"]
    })
}

fn edge_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "from": { "type": "string" },
            "to": { "type": "string" },
            "condition": condition_schema()
        },
        "required": ["from", "to"]
    })
}

fn condition_schema() -> serde_json::Value {
    serde_json::json!({
        "oneOf": [
            {
                "type": "string",
                "description": "Legacy/custom judge condition label"
            },
            condition_expr_schema()
        ]
    })
}

fn condition_expr_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "oneOf": [
            {
                "properties": {
                    "op": { "type": "string", "const": "exists" },
                    "pointer": { "type": "string" }
                },
                "required": ["op", "pointer"]
            },
            {
                "properties": {
                    "op": { "type": "string", "const": "missing" },
                    "pointer": { "type": "string" }
                },
                "required": ["op", "pointer"]
            },
            {
                "properties": {
                    "op": { "type": "string", "enum": ["eq", "ne"] },
                    "pointer": { "type": "string" },
                    "value": {}
                },
                "required": ["op", "pointer", "value"]
            },
            {
                "properties": {
                    "op": { "type": "string", "enum": ["gt", "gte", "lt", "lte"] },
                    "pointer": { "type": "string" },
                    "value": { "type": "number" }
                },
                "required": ["op", "pointer", "value"]
            }
        ]
    })
}

fn step_policy_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "timeout_ms": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_STEP_TIMEOUT_MS,
                "description": "Per-attempt timeout in milliseconds"
            },
            "max_attempts": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_STEP_ATTEMPTS,
                "description": "Maximum execution attempts for step failures, including the first attempt"
            },
            "retry_backoff_ms": {
                "type": "integer",
                "minimum": 0,
                "maximum": MAX_STEP_RETRY_BACKOFF_MS,
                "description": "Fixed delay before retrying a failed attempt"
            }
        }
    })
}

impl Tool for PlanWorkflowTool {
    fn name(&self) -> &str {
        "submit_workflow"
    }

    fn description(&self) -> &str {
        "提交一个 workflow 计划（steps + edges + entry_step）。校验通过后落盘，返回 task_id。"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static PLAN_WORKFLOW_SCHEMA: std::sync::LazyLock<serde_json::Value> =
            std::sync::LazyLock::new(plan_workflow_schema);
        &PLAN_WORKFLOW_SCHEMA
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, std::result::Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let plan: PlanInput = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            let workflow = Workflow {
                steps: plan.steps,
                edges: plan.edges,
                entry_step: plan.entry_step,
            };
            validate_workflow(&workflow)
                .map_err(|e| ToolError::InvalidArguments(format!("invalid workflow: {e}")))?;
            self.store
                .save_workflow(&self.task_id, &workflow)
                .await
                .map_err(|e| ToolError::Execution(format!("save_workflow failed: {e}")))?;
            Ok(ToolResult {
                content: vec![llm_harness_types::ContentBlock::Text {
                    text: format!(
                        "Workflow plan accepted ({} steps). task_id={}",
                        workflow.steps.len(),
                        self.task_id.0
                    ),
                }],
                details: serde_json::json!({ "task_id": self.task_id.0, "step_count": workflow.steps.len() }),
                terminate: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_valid_workflow() -> Workflow {
        Workflow {
            entry_step: "s1".into(),
            steps: vec![
                Step::llm("s1", "A", "do A", vec!["t1".into()]),
                Step::llm("s2", "B", "do B", vec![]),
            ],
            edges: vec![Edge {
                from: "s1".into(),
                to: "s2".into(),
                condition: None,
            }],
        }
    }

    #[test]
    fn valid_workflow_passes() {
        assert!(validate_workflow(&make_valid_workflow()).is_ok());
    }

    #[test]
    fn entry_not_in_steps_fails() {
        let mut wf = make_valid_workflow();
        wf.entry_step = "nope".into();
        assert!(validate_workflow(&wf).is_err());
    }

    #[test]
    fn edge_endpoint_not_in_steps_fails() {
        let mut wf = make_valid_workflow();
        wf.edges[0].to = "ghost".into();
        assert!(validate_workflow(&wf).is_err());
    }

    #[test]
    fn orphan_step_fails() {
        let mut wf = make_valid_workflow();
        wf.steps.push(Step::llm("s3", "C", "do C", vec![]));
        assert!(validate_workflow(&wf).is_err());
    }

    #[test]
    fn empty_allowed_tools_means_no_tools_allowed() {
        let wf = Workflow {
            entry_step: "s1".into(),
            steps: vec![Step::llm("s1", "A", "do A", vec![])],
            edges: vec![],
        };
        // 空 Vec 合法（表示该步不允许任何工具），校验通过。
        assert!(validate_workflow(&wf).is_ok());
    }

    #[test]
    fn empty_tool_name_fails() {
        let mut wf = make_valid_workflow();
        if let Step::Llm { allowed_tools, .. } = &mut wf.steps[0] {
            *allowed_tools = vec!["".into()];
        }
        assert!(validate_workflow(&wf).is_err());
    }

    #[test]
    fn empty_prompt_fails() {
        let mut wf = make_valid_workflow();
        if let Step::Llm { prompt, .. } = &mut wf.steps[0] {
            *prompt = "".into();
        }
        assert!(validate_workflow(&wf).is_err());
    }

    #[test]
    fn single_step_no_edges_ok() {
        let wf = Workflow {
            entry_step: "run".into(),
            steps: vec![Step::llm("run", "run", "run the task", vec![])],
            edges: vec![],
        };
        assert!(validate_workflow(&wf).is_ok());
    }

    #[test]
    fn duplicate_step_id_fails() {
        let mut wf = make_valid_workflow();
        // 再加一个 id="s1" 的 step（重复）。
        wf.steps.push(Step::llm("s1", "dup", "dup", vec![]));
        assert!(validate_workflow(&wf).is_err());
    }

    #[test]
    fn executor_step_requires_executor_name() {
        let wf = Workflow {
            entry_step: "s1".into(),
            steps: vec![Step::executor("s1", "Executor", "", None)],
            edges: vec![],
        };
        assert!(validate_workflow(&wf).is_err());
    }

    #[test]
    fn invalid_edge_condition_pointer_fails() {
        let mut wf = make_valid_workflow();
        wf.edges[0].condition = Some(EdgeCondition::Expr(ConditionExpr::Exists {
            pointer: "status".into(),
        }));
        let err = validate_workflow(&wf).unwrap_err();
        assert!(err.to_string().contains("condition pointer"));
    }

    #[test]
    fn invalid_step_policy_fails() {
        let wf = Workflow {
            entry_step: "s1".into(),
            steps: vec![
                Step::executor("s1", "Executor", "json_transform", None).with_policy(
                    StepExecutionPolicy {
                        timeout_ms: Some(1_000),
                        max_attempts: Some(0),
                        retry_backoff_ms: Some(0),
                    },
                ),
            ],
            edges: vec![],
        };
        let err = validate_workflow(&wf).unwrap_err();
        assert!(err.to_string().contains("policy.max_attempts"));
    }

    // ── PlanWorkflowTool 测试 ──────────────────────────────────────────────

    use crate::task_store::JsonlTaskStore;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    fn make_tool_ctx() -> ToolContext {
        ToolContext {
            env: Arc::new(llm_harness_loop::test_utils::NoOpEnv),
            abort: CancellationToken::new(),
            tool_use_id: "tu_1".into(),
            turn_index: 0,
            assistant_message: Arc::new(empty_msg()),
            update_tx: tokio::sync::mpsc::channel(1).0,
        }
    }

    fn empty_msg() -> llm_harness_types::AssistantMessage {
        use chrono::Utc;
        llm_harness_types::AssistantMessage {
            kind: llm_harness_types::AssistantMessageKind::FinalAnswer,
            message_id: None,
            turn_id: None,
            content: vec![],
            stop_reason: None,
            timestamp: Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        }
    }

    #[tokio::test]
    async fn plan_tool_accepts_valid_workflow() {
        let dir = TempDir::new().unwrap();
        let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
        let task_id = TaskId("plan-1".into());
        let tool = PlanWorkflowTool::new(store.clone(), task_id.clone());
        let args = serde_json::json!({
            "steps": [
                {"kind": "llm", "id": "s1", "name": "A", "prompt": "do A", "allowed_tools": ["t1"]},
                {"kind": "llm", "id": "s2", "name": "B", "prompt": "do B", "allowed_tools": []}
            ],
            "edges": [{"from": "s1", "to": "s2", "condition": {"op": "eq", "pointer": "/status", "value": "ok"}}],
            "entry_step": "s1"
        });
        let result = tool.execute(args, &make_tool_ctx()).await.unwrap();
        assert!(!result.terminate);
        let wf = store.load_workflow(&task_id).await.unwrap().unwrap();
        assert_eq!(wf.entry_step, "s1");
        assert!(matches!(
            wf.edges[0].condition,
            Some(EdgeCondition::Expr(ConditionExpr::Eq { .. }))
        ));
    }

    #[tokio::test]
    async fn plan_tool_rejects_invalid_workflow() {
        let dir = TempDir::new().unwrap();
        let store: Arc<dyn TaskStore> = Arc::new(JsonlTaskStore::new(dir.path().to_path_buf()));
        let task_id = TaskId("plan-2".into());
        let tool = PlanWorkflowTool::new(store.clone(), task_id.clone());
        let args = serde_json::json!({
            "steps": [{"kind": "llm", "id": "s1", "name": "A", "prompt": "do A", "allowed_tools": []}],
            "edges": [],
            "entry_step": "nope"
        });
        assert!(tool.execute(args, &make_tool_ctx()).await.is_err());
        assert!(store.load_workflow(&task_id).await.unwrap().is_none());
    }
}
