//! Budget control: pure decider hook + business override trait.
//!
//! `BudgetControlAdapter` implements only `ShouldStopHook`. It reads a shared
//! `Arc<Mutex<CostAggregate>>` (maintained by `CostAccumulatorHook`) and, when
//! `total_cost` exceeds `budget_limit`, consults the optional
//! `BudgetExceededHook` for a continue/stop decision. Cost accumulation itself
//! lives in `CostAccumulatorHook` (`cost_hook.rs`).

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use llm_harness_types::{CostAggregate, ShouldStopCtx, ShouldStopHook};

/// Business-layer hook for budget override decisions.
/// Called when the per-task budget limit is exceeded.
pub trait BudgetExceededHook: Send + Sync {
    /// Called when budget is exceeded. Return `true` to continue, `false` to
    /// transition the task to `Failed`.
    ///
    /// `cost` is the session-wide accumulated cost snapshot. The previous
    /// `task_cost` parameter was always equal to `session_cost` in non-TaskRunner
    /// scenarios and has been removed; callers needing task-scoped cost should
    /// track it separately.
    fn on_budget_exceeded<'a>(
        &'a self,
        cost: &'a CostAggregate,
        budget_limit: f64,
    ) -> BoxFuture<'a, bool>;
}

/// Pure budget-exceeded decider.
///
/// Implements only `ShouldStopHook`: reads a shared `Arc<Mutex<CostAggregate>>`
/// (maintained by `CostAccumulatorHook`) and, when `total_cost` exceeds
/// `budget_limit`, consults the optional `BudgetExceededHook` for a
/// continue/stop decision.
///
/// - `exceeded_hook = None` → surveillance mode: cost is tracked but the loop
///   never stops on budget.
/// - `exceeded_hook = Some(h)` → `h.on_budget_exceeded` decides: `true` =
///   continue, `false` = stop.
///
/// Cost accumulation is handled separately by `CostAccumulatorHook`; this
/// adapter holds no pricing or model state.
pub struct BudgetControlAdapter {
    cost: Arc<Mutex<CostAggregate>>,
    budget_limit: f64,
    exceeded_hook: Option<Arc<dyn BudgetExceededHook>>,
}

impl BudgetControlAdapter {
    /// Create a new pure-decider adapter.
    ///
    /// `cost` is the shared accumulated cost state (same `Arc` held by
    /// `CostAccumulatorHook` and `AgentHarness`).
    /// `budget_limit` is in USD (e.g., `5.0` for $5.00).
    /// `exceeded_hook = None` means cost-surveillance mode only (never stops).
    pub fn new(
        cost: Arc<Mutex<CostAggregate>>,
        budget_limit: f64,
        exceeded_hook: Option<Arc<dyn BudgetExceededHook>>,
    ) -> Self {
        Self {
            cost,
            budget_limit,
            exceeded_hook,
        }
    }
}

impl ShouldStopHook for BudgetControlAdapter {
    fn should_stop<'a>(&'a self, _ctx: ShouldStopCtx<'a>) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            // Snapshot cost under lock, release before awaiting the hook.
            let cost_snapshot = self.cost.lock().unwrap().clone();
            if cost_snapshot.total_cost <= self.budget_limit {
                return false;
            }
            match &self.exceeded_hook {
                None => false, // surveillance mode: track but don't stop
                Some(hook) => {
                    // on_budget_exceeded returns true = continue, false = stop.
                    // should_stop returns true = stop. Invert.
                    !hook
                        .on_budget_exceeded(&cost_snapshot, self.budget_limit)
                        .await
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::{AssistantMessage, StopReason};

    struct NoExceededHook;
    impl BudgetExceededHook for NoExceededHook {
        fn on_budget_exceeded<'a>(
            &'a self,
            _cost: &'a CostAggregate,
            _budget_limit: f64,
        ) -> BoxFuture<'a, bool> {
            Box::pin(async move { false })
        }
    }

    fn make_ctx<'a>(assistant: &'a AssistantMessage) -> ShouldStopCtx<'a> {
        ShouldStopCtx {
            last_assistant: assistant,
            stop_reason: StopReason::EndTurn,
            turn_index: 0,
        }
    }

    fn make_assistant() -> AssistantMessage {
        AssistantMessage {
            content: vec![],
            stop_reason: Some(StopReason::EndTurn),
            timestamp: chrono::Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        }
    }

    #[tokio::test]
    async fn does_not_stop_when_under_limit() {
        let cost = Arc::new(Mutex::new(CostAggregate::default()));
        let hook = Arc::new(NoExceededHook) as Arc<dyn BudgetExceededHook>;
        let adapter = BudgetControlAdapter::new(cost.clone(), 5.0, Some(hook));

        {
            let mut c = cost.lock().unwrap();
            c.total_cost = 1.0;
        }

        let assistant = make_assistant();
        let ctx = make_ctx(&assistant);
        let stop = adapter.should_stop(ctx).await;
        assert!(!stop, "under budget → must not stop");
    }

    #[tokio::test]
    async fn surveillance_mode_never_stops_without_hook() {
        let cost = Arc::new(Mutex::new(CostAggregate::default()));
        let adapter = BudgetControlAdapter::new(cost.clone(), 5.0, None);

        {
            let mut c = cost.lock().unwrap();
            c.total_cost = 10.0;
        }

        let assistant = make_assistant();
        let ctx = make_ctx(&assistant);
        let stop = adapter.should_stop(ctx).await;
        assert!(!stop, "surveillance mode (no hook) must not stop");
    }

    #[tokio::test]
    async fn stops_when_over_limit_and_hook_says_stop() {
        let cost = Arc::new(Mutex::new(CostAggregate::default()));
        let hook = Arc::new(NoExceededHook) as Arc<dyn BudgetExceededHook>;
        let adapter = BudgetControlAdapter::new(cost.clone(), 5.0, Some(hook));

        {
            let mut c = cost.lock().unwrap();
            c.total_cost = 10.0;
        }

        let assistant = make_assistant();
        let ctx = make_ctx(&assistant);
        let stop = adapter.should_stop(ctx).await;
        assert!(stop, "hook returned false (stop) → adapter must stop");
    }

    #[test]
    fn budget_control_adapter_is_should_stop_hook() {
        let cost = Arc::new(Mutex::new(CostAggregate::default()));
        let adapter = BudgetControlAdapter::new(cost, 5.0, None);
        let _: &dyn ShouldStopHook = &adapter;
    }
}
