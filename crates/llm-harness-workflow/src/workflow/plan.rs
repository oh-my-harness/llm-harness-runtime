// crates/llm-harness-workflow/src/workflow/plan.rs
//! plan 的来源无关性：validate_workflow 公共校验 + PlanWorkflowTool。
//!
//! Workflow 是 plan 的 runtime 内部表达，来源无关（人类/LLM/外部系统）。
//! 任何来源都过同一套校验、同一套落盘、同一套崩溃恢复。

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{
    SUBMIT_WORKFLOW_TOOL_NAME, Tool, ToolContext, ToolError, ToolFailure, ToolLevel, ToolResult,
};
use serde::Deserialize;

use crate::lifecycle::task::TaskId;
use crate::lifecycle::task_store::TaskStore;
use crate::workflow::error::{EdgeEndpoint, WorkflowValidationError};
#[allow(unused_imports)]
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
pub fn validate_workflow(wf: &Workflow) -> Result<(), WorkflowValidationError> {
    // step.id 非空（空 id 会导致 entry_step / edge 匹配歧义）。
    for step in &wf.steps {
        if step.id().is_empty() {
            return Err(WorkflowValidationError::EmptyStepId {
                name: step.name().to_string(),
            });
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
        return Err(WorkflowValidationError::DuplicateStepId {
            id: dup.to_string(),
        });
    }

    if !step_ids.contains(wf.entry_step.as_str()) {
        return Err(WorkflowValidationError::EntryStepNotFound {
            entry_step: wf.entry_step.to_string(),
        });
    }
    for edge in &wf.edges {
        if !step_ids.contains(edge.from.as_str()) {
            return Err(WorkflowValidationError::EdgeEndpointNotFound {
                which: EdgeEndpoint::From,
                endpoint: edge.from.to_string(),
            });
        }
        if !step_ids.contains(edge.to.as_str()) {
            return Err(WorkflowValidationError::EdgeEndpointNotFound {
                which: EdgeEndpoint::To,
                endpoint: edge.to.to_string(),
            });
        }
        validate_edge_condition(edge)?;
    }
    let has_incoming = |id: &str| wf.edges.iter().any(|e| e.to == id);
    for step in &wf.steps {
        if step.id() != &wf.entry_step && !has_incoming(step.id()) {
            return Err(WorkflowValidationError::OrphanStep {
                id: step.id().to_string(),
            });
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
                    return Err(WorkflowValidationError::EmptyPrompt {
                        id: step.id().to_string(),
                    });
                }
                for tool in allowed_tools {
                    if tool.is_empty() {
                        return Err(WorkflowValidationError::EmptyToolName {
                            id: step.id().to_string(),
                        });
                    }
                }
            }
            Step::Executor { executor_name, .. } => {
                if executor_name.is_empty() {
                    return Err(WorkflowValidationError::EmptyExecutorName {
                        id: step.id().to_string(),
                    });
                }
            }
            Step::Terminal { .. } => {
                // Terminal steps are always valid: no prompt, no executor,
                // no tools required. exit_code and message are optional.
            }
        }
    }
    // 安全阀：限制 step/edge 数量，防止恶意或失控的 workflow 定义。
    const MAX_STEPS: usize = 1000;
    const MAX_EDGES: usize = 5000;
    if wf.steps.len() > MAX_STEPS {
        return Err(WorkflowValidationError::TooManySteps {
            count: wf.steps.len(),
            max: MAX_STEPS,
        });
    }
    if wf.edges.len() > MAX_EDGES {
        return Err(WorkflowValidationError::TooManyEdges {
            count: wf.edges.len(),
            max: MAX_EDGES,
        });
    }

    // Warn: 同一步的出边不应混用 Expr 和 Label 条件。
    // Expr 边由 EdgeConditionJudge 声明式求值，Label 边需要自定义 judge 处理。
    // 混用容易误用：Label 边在 EdgeConditionJudge 中不参与匹配，仅作报错信息。
    use std::collections::HashMap;
    let mut edges_by_from: HashMap<&str, Vec<&EdgeCondition>> = HashMap::new();
    for edge in &wf.edges {
        if let Some(cond) = &edge.condition {
            edges_by_from
                .entry(edge.from.as_str())
                .or_default()
                .push(cond);
        }
    }
    for (from, conds) in &edges_by_from {
        let has_expr = conds.iter().any(|c| matches!(c, EdgeCondition::Expr(_)));
        let has_label = conds.iter().any(|c| matches!(c, EdgeCondition::Label(_)));
        if has_expr && has_label {
            tracing::warn!(
                "validate_workflow: step '{}' has mixed Expr + Label edge conditions \
                 (Expr edges are evaluated by EdgeConditionJudge; Label edges require a custom judge)",
                from
            );
        }
    }

    Ok(())
}

fn validate_step_policy(
    step_id: &str,
    policy: Option<&StepExecutionPolicy>,
) -> Result<(), WorkflowValidationError> {
    let Some(policy) = policy else {
        return Ok(());
    };
    if let Some(timeout_ms) = policy.timeout_ms
        && (timeout_ms == 0 || timeout_ms > MAX_STEP_TIMEOUT_MS)
    {
        return Err(WorkflowValidationError::PolicyOutOfRange {
            step_id: step_id.to_string(),
            field: "timeout_ms",
            min: 1,
            max: MAX_STEP_TIMEOUT_MS,
        });
    }
    if let Some(max_attempts) = policy.max_attempts
        && (max_attempts == 0 || max_attempts > MAX_STEP_ATTEMPTS)
    {
        return Err(WorkflowValidationError::PolicyOutOfRange {
            step_id: step_id.to_string(),
            field: "max_attempts",
            min: 1,
            max: MAX_STEP_ATTEMPTS as u64,
        });
    }
    if let Some(retry_backoff_ms) = policy.retry_backoff_ms
        && retry_backoff_ms > MAX_STEP_RETRY_BACKOFF_MS
    {
        return Err(WorkflowValidationError::PolicyOverMax {
            step_id: step_id.to_string(),
            field: "retry_backoff_ms",
            max: MAX_STEP_RETRY_BACKOFF_MS,
        });
    }
    Ok(())
}

fn validate_edge_condition(edge: &Edge) -> Result<(), WorkflowValidationError> {
    let Some(condition) = &edge.condition else {
        return Ok(());
    };
    match condition {
        EdgeCondition::Label(label) => {
            if label.is_empty() {
                return Err(WorkflowValidationError::EmptyConditionLabel {
                    from: edge.from.to_string(),
                    to: edge.to.to_string(),
                });
            }
        }
        EdgeCondition::Expr(expr) => {
            // Validate all pointers recursively (handles All/Any/Not composites).
            for ptr in expr.all_pointers() {
                validate_condition_pointer(edge, ptr)?;
            }
            // Check for non-finite values in any numeric sub-expression.
            if expr.has_non_finite_value() {
                return Err(WorkflowValidationError::NonFiniteConditionValue {
                    from: edge.from.to_string(),
                    to: edge.to.to_string(),
                });
            }
        }
    }
    Ok(())
}

fn validate_condition_pointer(edge: &Edge, pointer: &str) -> Result<(), WorkflowValidationError> {
    if pointer.is_empty() || pointer.starts_with('/') {
        return Ok(());
    }
    Err(WorkflowValidationError::InvalidConditionPointer {
        from: edge.from.to_string(),
        to: edge.to.to_string(),
    })
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
        "description": "Submit a workflow plan. Contains steps (step list), edges (transitions between steps), and entry_step (entry step ID). Validation rules: 1) entry_step must equal a step's id; 2) edge from/to must reference existing step ids; 3) non-entry steps must have inbound edges (no orphans); 4) a step cannot have multiple unconditional outbound edges; 5) step/edge count limits 1000/5000; 6) condition pointer must be RFC 6901 JSON Pointer (starts with /); 7) numeric condition values cannot be NaN/Infinity. Validation failures return specific error messages — fix and retry.",
        "examples": [{
            "steps": [
                {"kind": "llm", "id": "analyze", "name": "Analyze", "prompt": "Analyze the input data"},
                {"kind": "llm", "id": "report", "name": "Report", "prompt": "Generate a report"}
            ],
            "edges": [
                {"from": "analyze", "to": "report"},
                {"from": "report", "to": "analyze", "condition": {"op": "lt", "pointer": "/score", "value": 0.8}}
            ],
            "entry_step": "analyze"
        }],
        "properties": {
            "steps": { "type": "array", "items": {
                "oneOf": [
                    {
                        "type": "object",
                        "description": "LLM step: runs a prompt through an AgentHarness",
                        "properties": {
                            "kind": { "type": "string", "const": "llm", "description": "Fixed value 'llm'" },
                            "id": { "type": "string", "description": "Unique step identifier for edge from/to references. Must not be empty or duplicate" },
                            "name": { "type": "string", "description": "Human-readable name" },
                            "prompt": { "type": "string", "description": "Instruction for the step agent (required)" },
                            "allowed_tools": { "type": "array", "items": { "type": "string" }, "description": "List of tool names allowed in this step. Empty array = no tools" },
                            "policy": step_policy_schema()
                        },
                        "required": ["kind", "id", "name", "prompt"]
                    },
                    {
                        "type": "object",
                        "description": "Executor step: calls a registered executor function for domain logic",
                        "properties": {
                            "kind": { "type": "string", "const": "executor", "description": "Fixed value 'executor'" },
                            "id": { "type": "string", "description": "Unique step identifier for edge from/to references. Must not be empty or duplicate" },
                            "name": { "type": "string", "description": "Human-readable name" },
                            "executor_name": { "type": "string", "description": "Registered executor name (required, must not be empty)" },
                            "config": { "type": "object", "description": "Configuration parameters passed to the executor (optional)" },
                            "policy": step_policy_schema()
                        },
                        "required": ["kind", "id", "name", "executor_name"]
                    }
                ]
            }, "description": "Step list. Each step has a unique id; edges and entry_step reference by id"},
            "edges": { "type": "array", "items": edge_schema(), "description": "Transitions between steps. from/to reference step ids. Optional condition for conditional branching" },
            "entry_step": { "type": "string", "description": "Entry step id, must match a step in steps" }
        },
        "required": ["steps", "edges", "entry_step"]
    })
}

fn edge_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "Transition between steps. No condition = sequential; with condition = conditional branch (judge evaluates condition to decide)",
        "properties": {
            "from": { "type": "string", "description": "Source step id (must exist in steps)" },
            "to": { "type": "string", "description": "Target step id (must exist in steps)" },
            "condition": condition_schema()
        },
        "required": ["from", "to"]
    })
}

fn condition_schema() -> serde_json::Value {
    serde_json::json!({
        "description": "Edge condition. Omitted = unconditional (sequential). String = custom judge label. Object = declarative condition expression (op + pointer + value)",
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
        "description": "Declarative condition expression. pointer is RFC 6901 JSON Pointer (starts with /), pointing to a JSON field in the previous step's structured output. e.g. /score points to {\"score\": 0.85}",
        "oneOf": [
            {
                "properties": {
                    "op": { "type": "string", "const": "exists", "description": "Take this edge when the pointer field exists" },
                    "pointer": { "type": "string", "description": "RFC 6901 JSON Pointer, e.g. /score" }
                },
                "required": ["op", "pointer"]
            },
            {
                "properties": {
                    "op": { "type": "string", "const": "missing", "description": "Take this edge when the pointer field does not exist" },
                    "pointer": { "type": "string", "description": "RFC 6901 JSON Pointer, e.g. /score" }
                },
                "required": ["op", "pointer"]
            },
            {
                "properties": {
                    "op": { "type": "string", "enum": ["eq", "ne"], "description": "eq = equals; ne = not equals" },
                    "pointer": { "type": "string", "description": "RFC 6901 JSON Pointer, e.g. /status" },
                    "value": {}
                },
                "required": ["op", "pointer", "value"]
            },
            {
                "properties": {
                    "op": { "type": "string", "enum": ["gt", "gte", "lt", "lte"], "description": "gt=greater than, gte=greater or equal, lt=less than, lte=less or equal" },
                    "pointer": { "type": "string", "description": "RFC 6901 JSON Pointer, e.g. /score" },
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
            },
            "cache": {
                "type": "object",
                "description": "Step-level result cache: reuse a cached StepResult when the cache key (resolved from context variables) hasn't changed. Skips execution on cache hit.",
                "properties": {
                    "key_pointer": {
                        "type": "string",
                        "description": "JSON pointer (RFC 6901) into WorkflowContext variables, e.g. /design_version"
                    },
                    "ttl_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Cache TTL in seconds. Omit for no expiry (cache invalidated only when key changes)."
                    }
                },
                "required": ["key_pointer"]
            }
        }
    })
}

impl Tool for PlanWorkflowTool {
    fn name(&self) -> &str {
        SUBMIT_WORKFLOW_TOOL_NAME
    }

    fn level(&self) -> ToolLevel {
        ToolLevel::BuiltIn
    }

    fn description(&self) -> &str {
        "Submit a workflow plan (steps + edges + entry_step) for execution. Validates the plan, persists it, and returns a task_id.\n\n\
         Use when: orchestrating multi-step tasks with conditional branching; defining a DAG of steps with structured transitions.\n\
         Don't use for: simple sequential tool calls (just call tools directly); dynamic runtime decisions (use sub-agents instead).\n\
         Returns: task_id on success. Returns specific validation errors on failure (entry_step not in steps, dangling edge references, missing inbound edges for non-entry steps, invalid JSON pointer in conditions).\n\n\
         Examples:\n\
         {\"entry_step\": \"fetch\", \"steps\": [{\"kind\": \"llm\", \"id\": \"fetch\", \"name\": \"Fetch\", \"prompt\": \"Fetch the data\"}, {\"kind\": \"llm\", \"id\": \"process\", \"name\": \"Process\", \"prompt\": \"Process the data\"}], \"edges\": [{\"from\": \"fetch\", \"to\": \"process\"}]}"
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
    ) -> BoxFuture<'a, std::result::Result<ToolResult, ToolFailure>> {
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
            Ok(ToolResult::full(
                vec![llm_harness_types::DataBlock::text(format!(
                    "Workflow plan accepted ({} steps). task_id={}",
                    workflow.steps.len(),
                    self.task_id.0
                ))],
                serde_json::json!({ "task_id": self.task_id.0, "step_count": workflow.steps.len() }),
                false,
            ))
        })
    }
}

llm_harness_types::assert_builtin_registered!(PlanWorkflowTool => SUBMIT_WORKFLOW_TOOL_NAME);

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
                        cache: None,
                        r#loop: None,
                    },
                ),
            ],
            edges: vec![],
        };
        let err = validate_workflow(&wf).unwrap_err();
        assert!(err.to_string().contains("policy.max_attempts"));
    }

    // ── PlanWorkflowTool 测试 ──────────────────────────────────────────────

    use crate::lifecycle::task_store::JsonlTaskStore;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    fn make_tool_ctx() -> ToolContext {
        ToolContext {
            run: Arc::new(llm_harness_types::RunContext::new(
                llm_harness_types::RunRequest::default(),
            )),
            env: Arc::new(llm_harness_loop::test_utils::NoOpEnv),
            abort: CancellationToken::new(),
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

#[test]
fn mixed_expr_label_edges_passes_with_warning() {
    // Mixed Expr + Label edges should not fail validation (only warns).
    let wf = Workflow {
        entry_step: "s1".into(),
        steps: vec![
            Step::llm("s1", "A", "do A", vec![]),
            Step::llm("s2", "B", "do B", vec![]),
            Step::llm("s3", "C", "do C", vec![]),
        ],
        edges: vec![
            Edge {
                from: "s1".into(),
                to: "s2".into(),
                condition: Some(EdgeCondition::Label("success".into())),
            },
            Edge {
                from: "s1".into(),
                to: "s3".into(),
                condition: Some(EdgeCondition::Expr(ConditionExpr::Exists {
                    pointer: "/x".into(),
                })),
            },
        ],
    };
    // Should pass (warning only, not error).
    assert!(validate_workflow(&wf).is_ok());
}
