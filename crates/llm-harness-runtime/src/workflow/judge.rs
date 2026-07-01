// crates/llm-harness-runtime/src/workflow/judge.rs
//! 步骤转换 judge —— 业务层实现，判断"这步做完该去哪"。

use futures::future::BoxFuture;

use crate::workflow::model::{Step, StepRecord, StepResult, Transition};

/// 传递给 `StepTransitionJudge::decide` 的上下文。
pub struct StepCtx<'a> {
    /// 当前步定义。
    pub current_step: &'a Step,
    /// 本步 step agent 的执行结果。含 `structured: Option<Value>`，
    /// judge 直接读 `ctx.last_result.structured`，不用自己经 TaskStore 反查 checkpoint。
    pub last_result: &'a StepResult,
    /// 已完成的步骤历史（审计链）。
    pub step_history: &'a [StepRecord],
}

/// 业务层实现。如"膜厚合格→下一步,不合格→返工 step_1"。
///
/// 越界检测：judge 返回 `To(step_id)` 后，引擎在 `apply_transition` 校验
/// 该 step_id 是否在 workflow steps 里、是否从 current_step 可达（有 edge 或是 entry）。
/// 非法跳转 → Failed。业务层无需自行保证合法性。
pub trait StepTransitionJudge: Send + Sync {
    /// 判断这步做完后的跳转决策。
    fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition>;
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

    #[tokio::test]
    async fn noop_judge_returns_abort() {
        let judge = NoopJudge;
        let step = make_step();
        let result = make_result();
        let ctx = StepCtx {
            current_step: &step,
            last_result: &result,
            step_history: &[],
        };
        let t = judge.decide(&ctx).await;
        assert!(matches!(t, Transition::Abort { .. }));
    }
}
