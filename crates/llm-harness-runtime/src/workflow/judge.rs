// crates/llm-harness-runtime/src/workflow/judge.rs
//! 步骤转换 judge —— 业务层实现，判断"这步做完该去哪"。

use futures::future::BoxFuture;
use serde_json::Value;

use std::collections::HashMap;
use std::sync::Arc;

use crate::workflow::model::{
    ConditionExpr, Edge, EdgeCondition, Step, StepId, StepRecord, StepResult, Transition, Workflow,
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
pub(crate) struct NoopJudge;

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
#[derive(Clone)]
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

    /// Return a clone of the internal edges vec.
    pub fn clone_edges(&self) -> Vec<Edge> {
        self.edges.clone()
    }

    pub fn decide_sync(&self, ctx: &StepCtx<'_>) -> Transition {
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
        let mut label_edges: Vec<(&str, &Edge)> = vec![];
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
                Some(EdgeCondition::Label(label)) => label_edges.push((label.as_str(), edge)),
                Some(EdgeCondition::Expr(expr)) => {
                    if evaluate_condition(expr, ctx.last_result.structured.as_ref()) {
                        return Transition::To(edge.to.clone());
                    }
                }
            }
        }

        // If we have label edges, try to match the route_key from
        // structured output against them, with fallback semantics.
        if !label_edges.is_empty() {
            // Extract route_key from structured output (set by executor).
            let route_key = ctx
                .last_result
                .structured
                .as_ref()
                .and_then(|s| s.get("route_key"))
                .and_then(|v| v.as_str())
                .unwrap_or("success");

            // 1. Direct match: route_key matches a label.
            if let Some((_, edge)) = label_edges.iter().find(|(label, _)| *label == route_key) {
                return Transition::To(edge.to.clone());
            }

            // 2. Fallback: success/pass/done → "success" label.
            if matches!(route_key, "success" | "pass" | "done")
                && let Some((_, edge)) = label_edges.iter().find(|(label, _)| *label == "success")
            {
                return Transition::To(edge.to.clone());
            }

            // 3. Fallback: failure → "failure" label.
            if route_key == "failure"
                && let Some((_, edge)) = label_edges.iter().find(|(label, _)| *label == "failure")
            {
                return Transition::To(edge.to.clone());
            }

            // 4. No label matched: if there's an unconditional fallback, use it.
            if let Some(edge) = fallback {
                return Transition::To(edge.to.clone());
            }

            return Transition::Fail {
                reason: format!(
                    "no edge label matched route_key '{}' from step '{}' (labels: {})",
                    route_key,
                    ctx.current_step.id(),
                    label_edges
                        .iter()
                        .map(|(l, _)| *l)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
        }

        if let Some(edge) = fallback {
            return Transition::To(edge.to.clone());
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

type JudgeHandler = Arc<dyn Fn(&StepCtx) -> Transition + Send + Sync>;

/// Composite judge: dispatches to per-step handlers instead of one big decide.
///
/// Register a handler for each step that needs custom routing logic.
/// Steps without a registered handler fall through to the fallback
/// (user-set handler, EdgeConditionJudge, or Abort).
pub struct CompositeJudge {
    handlers: HashMap<StepId, JudgeHandler>,
    fallback: Option<JudgeHandler>,
    edge_fallback: Option<EdgeConditionJudge>,
}

impl Default for CompositeJudge {
    fn default() -> Self {
        Self::new()
    }
}

impl CompositeJudge {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            fallback: None,
            edge_fallback: None,
        }
    }

    /// Register a routing handler for a specific step.
    pub fn on<F>(&mut self, step: &str, f: F) -> &mut Self
    where
        F: Fn(&StepCtx) -> Transition + Send + Sync + 'static,
    {
        self.handlers.insert(step.to_string(), Arc::new(f));
        self
    }

    /// Set a fallback handler for steps without a registered handler.
    pub fn fallback_fn<F>(mut self, f: F) -> Self
    where
        F: Fn(&StepCtx) -> Transition + Send + Sync + 'static,
    {
        self.fallback = Some(Arc::new(f));
        self
    }

    /// Set `EdgeConditionJudge` as the fallback for unregistered steps.
    pub fn set_edge_fallback(&mut self, judge: EdgeConditionJudge) {
        self.edge_fallback = Some(judge);
    }
}

impl StepTransitionJudge for CompositeJudge {
    fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition> {
        let step_id = ctx.current_step.id().as_str();
        let transition = if let Some(handler) = self.handlers.get(step_id) {
            handler(ctx)
        } else if let Some(fb) = &self.fallback {
            fb(ctx)
        } else if let Some(edge) = &self.edge_fallback {
            edge.decide_sync(ctx)
        } else {
            Transition::Abort {
                reason: format!("no handler registered for step '{}'", step_id),
            }
        };
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
        ConditionExpr::All { exprs } => exprs.iter().all(|e| evaluate_condition(e, structured)),
        ConditionExpr::Any { exprs } => exprs.iter().any(|e| evaluate_condition(e, structured)),
        ConditionExpr::Not { expr } => !evaluate_condition(expr, structured),
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
    use crate::workflow::model::StructuredStatus;
    use llm_harness_types::CostAggregate;

    fn make_step_with_id(id: &str) -> Step {
        Step::llm(id, id, "do", vec![])
    }

    #[tokio::test]
    async fn composite_judge_dispatches_to_registered_handler() {
        let mut judge = CompositeJudge::new();
        judge.on("s1", |_| Transition::To("next".into()));
        judge.on("s2", |_| Transition::Retry);

        let step = make_step_with_id("s1");
        let result = make_result();
        let ctx = StepCtx {
            current_step: &step,
            last_result: &result,
            step_history: &[],
            context: Box::leak(Box::new(WorkflowContext::new())),
        };
        let t = judge.decide(&ctx).await;
        assert!(matches!(t, Transition::To(id) if id == "next"));

        let step2 = make_step_with_id("s2");
        let ctx2 = StepCtx {
            current_step: &step2,
            last_result: &result,
            step_history: &[],
            context: Box::leak(Box::new(WorkflowContext::new())),
        };
        let t2 = judge.decide(&ctx2).await;
        assert!(matches!(t2, Transition::Retry));
    }

    #[tokio::test]
    async fn composite_judge_aborts_for_unregistered_step_without_fallback() {
        let mut judge = CompositeJudge::new();
        judge.on("s1", |_| Transition::To("next".into()));

        let step = make_step_with_id("unknown");
        let result = make_result();
        let ctx = StepCtx {
            current_step: &step,
            last_result: &result,
            step_history: &[],
            context: Box::leak(Box::new(WorkflowContext::new())),
        };
        let t = judge.decide(&ctx).await;
        assert!(matches!(t, Transition::Abort { reason } if reason.contains("no handler")));
    }

    #[tokio::test]
    async fn composite_judge_uses_fallback_fn_for_unregistered_step() {
        let mut judge = CompositeJudge::new();
        judge.on("s1", |_| Transition::To("next".into()));
        let judge = judge.fallback_fn(|_| Transition::Fail {
            reason: "fb".into(),
        });

        let step = make_step_with_id("unknown");
        let result = make_result();
        let ctx = StepCtx {
            current_step: &step,
            last_result: &result,
            step_history: &[],
            context: Box::leak(Box::new(WorkflowContext::new())),
        };
        let t = judge.decide(&ctx).await;
        assert!(matches!(t, Transition::Fail { reason } if reason == "fb"));
    }

    #[tokio::test]
    async fn composite_judge_uses_edge_fallback() {
        let mut judge = CompositeJudge::new();
        judge.on("custom", |_| Transition::To("handled".into()));
        judge.set_edge_fallback(EdgeConditionJudge::new(vec![edge("next", None)]));

        // Unregistered step "s1" → edge fallback → unconditional edge to "next"
        let step = make_step_with_id("s1");
        let result = make_result();
        let ctx = StepCtx {
            current_step: &step,
            last_result: &result,
            step_history: &[],
            context: Box::leak(Box::new(WorkflowContext::new())),
        };
        let t = judge.decide(&ctx).await;
        assert!(matches!(t, Transition::To(id) if id == "next"));
    }

    fn make_step() -> Step {
        Step::llm("s1", "Step 1", "do step 1", vec![])
    }

    fn make_result() -> StepResult {
        StepResult {
            output: "done".into(),
            structured: None,
            structured_status: StructuredStatus::NotRequired,
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
    async fn edge_condition_judge_fails_legacy_label_without_matching_route_key() {
        // Label edges are now matched against structured.route_key (with
        // fallback to "success"). If no label matches the route_key and no
        // unconditional fallback edge exists, the judge fails.
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

        // make_result() has no route_key → defaults to "success".
        // "business_label" != "success" → no match → Fail.
        assert!(
            matches!(transition, Transition::Fail { reason } if reason.contains("no edge label matched"))
        );
    }

    #[tokio::test]
    async fn edge_condition_all_requires_all_true() {
        let judge = EdgeConditionJudge::new(vec![edge(
            "next",
            Some(EdgeCondition::Expr(ConditionExpr::All {
                exprs: vec![
                    ConditionExpr::Gte {
                        pointer: "/score".into(),
                        value: 0.8,
                    },
                    ConditionExpr::Eq {
                        pointer: "/status".into(),
                        value: serde_json::json!("ok"),
                    },
                ],
            })),
        )]);
        let step = make_step();

        // Both true → match
        let result = make_structured_result(serde_json::json!({"score": 0.9, "status": "ok"}));
        let ctx = StepCtx {
            current_step: &step,
            last_result: &result,
            step_history: &[],
            context: Box::leak(Box::new(WorkflowContext::new())),
        };
        let t = judge.decide(&ctx).await;
        assert!(matches!(t, Transition::To(id) if id == "next"));

        // One false → no match
        let result = make_structured_result(serde_json::json!({"score": 0.9, "status": "bad"}));
        let ctx = StepCtx {
            current_step: &step,
            last_result: &result,
            step_history: &[],
            context: Box::leak(Box::new(WorkflowContext::new())),
        };
        let t = judge.decide(&ctx).await;
        assert!(matches!(t, Transition::Fail { .. }));
    }

    #[tokio::test]
    async fn edge_condition_any_matches_first_true() {
        let judge = EdgeConditionJudge::new(vec![edge(
            "next",
            Some(EdgeCondition::Expr(ConditionExpr::Any {
                exprs: vec![
                    ConditionExpr::Eq {
                        pointer: "/status".into(),
                        value: serde_json::json!("bad"),
                    },
                    ConditionExpr::Gte {
                        pointer: "/score".into(),
                        value: 0.8,
                    },
                ],
            })),
        )]);
        let step = make_step();
        let result = make_structured_result(serde_json::json!({"score": 0.9, "status": "ok"}));
        let ctx = StepCtx {
            current_step: &step,
            last_result: &result,
            step_history: &[],
            context: Box::leak(Box::new(WorkflowContext::new())),
        };
        let t = judge.decide(&ctx).await;
        assert!(matches!(t, Transition::To(id) if id == "next"));
    }

    #[tokio::test]
    async fn edge_condition_not_inverts() {
        let judge = EdgeConditionJudge::new(vec![edge(
            "next",
            Some(EdgeCondition::Expr(ConditionExpr::Not {
                expr: Box::new(ConditionExpr::Exists {
                    pointer: "/error".into(),
                }),
            })),
        )]);
        let step = make_step();

        // No error → Not(Exists) = true → match
        let result = make_structured_result(serde_json::json!({"score": 0.9}));
        let ctx = StepCtx {
            current_step: &step,
            last_result: &result,
            step_history: &[],
            context: Box::leak(Box::new(WorkflowContext::new())),
        };
        let t = judge.decide(&ctx).await;
        assert!(matches!(t, Transition::To(id) if id == "next"));

        // Error present → Not(Exists) = false → no match
        let result = make_structured_result(serde_json::json!({"error": "fail"}));
        let ctx = StepCtx {
            current_step: &step,
            last_result: &result,
            step_history: &[],
            context: Box::leak(Box::new(WorkflowContext::new())),
        };
        let t = judge.decide(&ctx).await;
        assert!(matches!(t, Transition::Fail { .. }));
    }

    #[tokio::test]
    async fn edge_condition_nested_composite() {
        // (score >= 0.8 AND status == "ok") OR error exists
        let judge = EdgeConditionJudge::new(vec![edge(
            "next",
            Some(EdgeCondition::Expr(ConditionExpr::Any {
                exprs: vec![
                    ConditionExpr::All {
                        exprs: vec![
                            ConditionExpr::Gte {
                                pointer: "/score".into(),
                                value: 0.8,
                            },
                            ConditionExpr::Eq {
                                pointer: "/status".into(),
                                value: serde_json::json!("ok"),
                            },
                        ],
                    },
                    ConditionExpr::Exists {
                        pointer: "/error".into(),
                    },
                ],
            })),
        )]);
        let step = make_step();

        // error exists → match (via Any second branch)
        let result = make_structured_result(serde_json::json!({"score": 0.1, "error": "fail"}));
        let ctx = StepCtx {
            current_step: &step,
            last_result: &result,
            step_history: &[],
            context: Box::leak(Box::new(WorkflowContext::new())),
        };
        let t = judge.decide(&ctx).await;
        assert!(matches!(t, Transition::To(id) if id == "next"));
    }
}
