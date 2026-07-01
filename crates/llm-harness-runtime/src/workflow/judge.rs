// crates/llm-harness-runtime/src/workflow/judge.rs
//! 步骤转换 judge —— 业务层实现，判断"这步做完该去哪"。

use futures::future::BoxFuture;
use serde_json::Value;

use crate::workflow::model::{
    ConditionExpr, Edge, EdgeCondition, Step, StepRecord, StepResult, Transition, Workflow,
    WorkflowContext,
};

/// 传递给 `StepTransitionJudge::decide` 的上下文。
pub struct StepCtx<'a> {
    /// 当前步定义。
    pub current_step: &'a Step,
    /// 本步 step agent 的执行结果。含 `structured: Option<Value>`，
    /// judge 直接读 `ctx.last_result.structured`，不用自己经 TaskStore 反查 checkpoint。
    pub last_result: &'a StepResult,
    /// 已完成的步骤历史（审计链）。
    pub step_history: &'a [StepRecord],
    /// 共享可变上下文（只读快照）。judge 可读 context 做路由决策。
    pub context: &'a WorkflowContext,
}

/// 业务层实现。如"膜厚合格→下一步,不合格→返工 step_1"。
///
/// 越界检测：judge 返回 `To(step_id)` 后，引擎在 `apply_transition` 校验
/// 该 step_id 是否在 workflow steps 里、是否从 current_step 可达（有 edge 或是 entry）。
/// 非法跳转 → Failed。业务层无需自行保证合法性。
pub trait StepTransitionJudge: Send + Sync {
    /// 判断这步做完后的跳转决策。
    fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition>;

    /// Whether this judge is the built-in no-op placeholder.
    ///
    /// WorkflowEngine uses this to preserve custom judge behavior while
    /// allowing declarative workflows to opt into the built-in edge judge by
    /// passing the default `NoopJudge`.
    fn is_noop(&self) -> bool {
        false
    }
}

/// 空 judge：永不跳转。自由 task（单步 workflow）用。
/// `run()` 检测"无后继 step"自动结束，无需 judge 参与。
pub struct NoopJudge;

impl StepTransitionJudge for NoopJudge {
    fn decide<'a>(&'a self, _ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
        // 自由 task 单步流程，run() 不会调到此处（无后继 step 自动结束）。
        // 若被调到，返回 Abort 防止无限循环。
        Box::pin(async {
            Transition::Abort {
                reason: "NoopJudge has no transitions".into(),
            }
        })
    }

    fn is_noop(&self) -> bool {
        true
    }
}

/// Built-in deterministic judge for declarative edge conditions.
///
/// It evaluates structured edge conditions against `StepResult.structured`.
/// String labels are intentionally treated as custom-judge labels and are not
/// guessed by this judge.
pub struct EdgeConditionJudge {
    edges: Vec<Edge>,
}

impl EdgeConditionJudge {
    pub fn new(edges: Vec<Edge>) -> Self {
        Self { edges }
    }

    pub fn from_workflow(workflow: &Workflow) -> Self {
        Self::new(workflow.edges.clone())
    }

    fn decide_sync(&self, ctx: &StepCtx<'_>) -> Transition {
        let outgoing: Vec<&Edge> = self
            .edges
            .iter()
            .filter(|edge| edge.from.as_str() == ctx.current_step.id().as_str())
            .collect();
        if outgoing.is_empty() {
            return Transition::Abort {
                reason: "workflow complete".into(),
            };
        }

        let mut fallback: Option<&Edge> = None;
        let mut legacy_labels = vec![];
        for edge in &outgoing {
            match &edge.condition {
                None => {
                    if fallback.is_some() {
                        return Transition::Fail {
                            reason: format!(
                                "multiple unconditional edges from step '{}'",
                                ctx.current_step.id()
                            ),
                        };
                    }
                    fallback = Some(edge);
                }
                Some(EdgeCondition::Label(label)) => legacy_labels.push(label.as_str()),
                Some(EdgeCondition::Expr(expr)) => {
                    if evaluate_condition(expr, ctx.last_result.structured.as_ref()) {
                        return Transition::To(edge.to.clone());
                    }
                }
            }
        }

        if let Some(edge) = fallback {
            return Transition::To(edge.to.clone());
        }
        if !legacy_labels.is_empty() {
            return Transition::Fail {
                reason: format!(
                    "edge condition label '{}' from step '{}' requires a custom judge",
                    legacy_labels[0],
                    ctx.current_step.id()
                ),
            };
        }
        Transition::Fail {
            reason: format!(
                "no edge condition matched for step '{}'",
                ctx.current_step.id()
            ),
        }
    }
}

impl StepTransitionJudge for EdgeConditionJudge {
    fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
        let transition = self.decide_sync(ctx);
        Box::pin(async move { transition })
    }
}

fn evaluate_condition(expr: &ConditionExpr, structured: Option<&Value>) -> bool {
    match expr {
        ConditionExpr::Exists { pointer } => read_structured_pointer(structured, pointer).is_some(),
        ConditionExpr::Missing { pointer } => {
            read_structured_pointer(structured, pointer).is_none()
        }
        ConditionExpr::Eq { pointer, value } => {
            read_structured_pointer(structured, pointer).is_some_and(|found| found == value)
        }
        ConditionExpr::Ne { pointer, value } => {
            read_structured_pointer(structured, pointer).is_some_and(|found| found != value)
        }
        ConditionExpr::Gt { pointer, value } => {
            compare_number(structured, pointer, *value, f64::gt)
        }
        ConditionExpr::Gte { pointer, value } => {
            compare_number(structured, pointer, *value, f64::ge)
        }
        ConditionExpr::Lt { pointer, value } => {
            compare_number(structured, pointer, *value, f64::lt)
        }
        ConditionExpr::Lte { pointer, value } => {
            compare_number(structured, pointer, *value, f64::le)
        }
    }
}

fn read_structured_pointer<'a>(structured: Option<&'a Value>, pointer: &str) -> Option<&'a Value> {
    structured.and_then(|value| value.pointer(pointer))
}

fn compare_number(
    structured: Option<&Value>,
    pointer: &str,
    expected: f64,
    compare: impl FnOnce(&f64, &f64) -> bool,
) -> bool {
    read_structured_pointer(structured, pointer)
        .and_then(Value::as_f64)
        .is_some_and(|found| compare(&found, &expected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::CostAggregate;

    fn make_step() -> Step {
        Step::llm("s1", "Step 1", "do step 1", vec![])
    }

    fn make_result() -> StepResult {
        StepResult {
            output: "done".into(),
            structured: None,
            tool_calls_count: 0,
            session_id: "sess1".into(),
            cost: CostAggregate::default(),
            started_at: None,
            ended_at: None,
        }
    }

    fn make_structured_result(structured: serde_json::Value) -> StepResult {
        StepResult {
            structured: Some(structured),
            ..make_result()
        }
    }

    fn edge(to: &str, condition: Option<EdgeCondition>) -> Edge {
        Edge {
            from: "s1".into(),
            to: to.into(),
            condition,
        }
    }

    #[tokio::test]
    async fn noop_judge_returns_abort() {
        let judge = NoopJudge;
        let step = make_step();
        let result = make_result();
        let ctx = StepCtx {
            current_step: &step,
            last_result: &result,
            step_history: &[],
            context: Box::leak(Box::new(WorkflowContext::new())),
        };
        let t = judge.decide(&ctx).await;
        assert!(matches!(t, Transition::Abort { .. }));
    }

    #[tokio::test]
    async fn edge_condition_judge_takes_first_matching_condition() {
        let judge = EdgeConditionJudge::new(vec![
            edge(
                "bad",
                Some(EdgeCondition::Expr(ConditionExpr::Eq {
                    pointer: "/status".into(),
                    value: serde_json::json!("bad"),
                })),
            ),
            edge(
                "ok",
                Some(EdgeCondition::Expr(ConditionExpr::Eq {
                    pointer: "/status".into(),
                    value: serde_json::json!("ok"),
                })),
            ),
        ]);
        let step = make_step();
        let result = make_structured_result(serde_json::json!({"status": "ok"}));
        let ctx = StepCtx {
            current_step: &step,
            last_result: &result,
            step_history: &[],
            context: Box::leak(Box::new(WorkflowContext::new())),
        };

        let transition = judge.decide(&ctx).await;

        assert!(matches!(transition, Transition::To(next) if next == "ok"));
    }

    #[tokio::test]
    async fn edge_condition_judge_falls_back_to_unconditional_edge() {
        let judge = EdgeConditionJudge::new(vec![
            edge(
                "matched",
                Some(EdgeCondition::Expr(ConditionExpr::Gt {
                    pointer: "/score".into(),
                    value: 0.9,
                })),
            ),
            edge("fallback", None),
        ]);
        let step = make_step();
        let result = make_structured_result(serde_json::json!({"score": 0.5}));
        let ctx = StepCtx {
            current_step: &step,
            last_result: &result,
            step_history: &[],
            context: Box::leak(Box::new(WorkflowContext::new())),
        };

        let transition = judge.decide(&ctx).await;

        assert!(matches!(transition, Transition::To(next) if next == "fallback"));
    }

    #[tokio::test]
    async fn edge_condition_judge_fails_when_no_condition_matches() {
        let judge = EdgeConditionJudge::new(vec![edge(
            "next",
            Some(EdgeCondition::Expr(ConditionExpr::Exists {
                pointer: "/missing".into(),
            })),
        )]);
        let step = make_step();
        let result = make_structured_result(serde_json::json!({"status": "ok"}));
        let ctx = StepCtx {
            current_step: &step,
            last_result: &result,
            step_history: &[],
            context: Box::leak(Box::new(WorkflowContext::new())),
        };

        let transition = judge.decide(&ctx).await;

        assert!(
            matches!(transition, Transition::Fail { reason } if reason.contains("no edge condition matched"))
        );
    }

    #[tokio::test]
    async fn edge_condition_judge_fails_legacy_label_without_custom_judge() {
        let judge = EdgeConditionJudge::new(vec![edge(
            "next",
            Some(EdgeCondition::Label("business_label".into())),
        )]);
        let step = make_step();
        let result = make_result();
        let ctx = StepCtx {
            current_step: &step,
            last_result: &result,
            step_history: &[],
            context: Box::leak(Box::new(WorkflowContext::new())),
        };

        let transition = judge.decide(&ctx).await;

        assert!(
            matches!(transition, Transition::Fail { reason } if reason.contains("requires a custom judge"))
        );
    }
}
