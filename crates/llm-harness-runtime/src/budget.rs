use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use llm_harness_types::{
    AfterProviderResponseHook, ProviderResponseInfo, ShouldStopCtx, ShouldStopHook,
};

use crate::cost::{CostAggregate, PricingProvider};

/// Business-layer hook for budget override decisions.
/// Called when the per-task budget limit is exceeded.
pub trait BudgetExceededHook: Send + Sync {
    /// Called when budget is exceeded. Return `true` to continue, `false` to
    /// transition the task to `Failed`.
    fn on_budget_exceeded<'a>(
        &'a self,
        session_cost: &'a CostAggregate,
        task_cost: &'a CostAggregate,
        budget_limit: f64,
    ) -> BoxFuture<'a, bool>;
}

/// Adapts `BudgetExceededHook` into core hooks:
///   - `AfterProviderResponseHook`: accumulates cost, detects budget exceeded
///   - `ShouldStopHook`: returns `true` when the `budget_exceeded` flag is set
///
/// NOTE: `PrepareNextTurnHook` cannot halt the loop — a `NextTurnDirective` with
/// all-`None` fields means "continue normally". `ShouldStopHook` is the correct
/// core extension point for stopping the agent loop.
pub struct BudgetControlAdapter {
    pricing: Arc<dyn PricingProvider>,
    budget_limit: f64,
    exceeded_hook: Option<Arc<dyn BudgetExceededHook>>,
    /// Model ID to use for pricing lookup (set at construction via `with_model`).
    model: Option<String>,
    /// Provider name to use for pricing lookup (set at construction via `with_model`).
    provider: Option<String>,
    session_cost: Mutex<CostAggregate>,
    /// Set by `AfterProviderResponseHook`; read by `ShouldStopHook`.
    budget_exceeded: Mutex<bool>,
    /// `true` = "continue despite over-budget", `false` = "stop now".
    /// Defaults to `true` (open loop) until the `exceeded_hook` says otherwise.
    continue_decision: Mutex<bool>,
}

impl BudgetControlAdapter {
    /// Create a new adapter.
    ///
    /// `budget_limit` is in USD (e.g., `5.0` for $5.00).
    /// `exceeded_hook = None` means cost-surveillance mode only (never stops the loop).
    pub fn new(
        pricing: Arc<dyn PricingProvider>,
        budget_limit: f64,
        exceeded_hook: Option<Arc<dyn BudgetExceededHook>>,
    ) -> Self {
        Self {
            pricing,
            budget_limit,
            exceeded_hook,
            model: None,
            provider: None,
            session_cost: Mutex::new(CostAggregate::default()),
            budget_exceeded: Mutex::new(false),
            continue_decision: Mutex::new(true),
        }
    }

    /// Set model and provider for per-call USD cost calculation.
    pub fn with_model(mut self, model: impl Into<String>, provider: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self.provider = Some(provider.into());
        self
    }

    /// Current session cost estimate.
    pub fn current_cost(&self) -> CostAggregate {
        self.session_cost.lock().unwrap().clone()
    }
}

impl AfterProviderResponseHook for BudgetControlAdapter {
    fn after_response<'a>(&'a self, info: &'a ProviderResponseInfo) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(ref usage) = info.usage {
                // Look up pricing if model/provider are configured.
                let price = self
                    .model
                    .as_deref()
                    .zip(self.provider.as_deref())
                    .and_then(|(m, p)| self.pricing.price_for(m, p));

                let mut cost = self.session_cost.lock().unwrap();
                cost.total_input_tokens += usage.input_tokens as u64;
                cost.total_output_tokens += usage.output_tokens as u64;
                cost.total_cache_read_tokens += usage.cache_read_tokens as u64;
                cost.total_cache_write_tokens += usage.cache_creation_tokens as u64;

                if let Some(ref p) = price {
                    let call_cost =
                        (usage.input_tokens as f64 / 1_000_000.0) * p.input_per_mtok
                            + (usage.output_tokens as f64 / 1_000_000.0) * p.output_per_mtok
                            + (usage.cache_read_tokens as f64 / 1_000_000.0)
                                * p.cache_read_per_mtok
                            + (usage.cache_creation_tokens as f64 / 1_000_000.0)
                                * p.cache_write_per_mtok;
                    cost.total_cost += call_cost;

                    if let (Some(m), Some(pr)) =
                        (self.model.as_deref(), self.provider.as_deref())
                    {
                        let key = format!("{pr}/{m}");
                        let mc = cost.by_model.entry(key).or_default();
                        mc.input_tokens += usage.input_tokens as u64;
                        mc.output_tokens += usage.output_tokens as u64;
                        mc.cost += call_cost;
                        mc.call_count += 1;
                    }
                }
            }

            // Snapshot cost, drop lock, then check budget (avoid holding lock across .await).
            let cost_snapshot = self.session_cost.lock().unwrap().clone();
            if cost_snapshot.total_cost > self.budget_limit {
                *self.budget_exceeded.lock().unwrap() = true;
                if let Some(ref hook) = self.exceeded_hook {
                    let should_continue = hook
                        .on_budget_exceeded(&cost_snapshot, &cost_snapshot, self.budget_limit)
                        .await;
                    *self.continue_decision.lock().unwrap() = should_continue;
                }
            }
        })
    }
}

impl ShouldStopHook for BudgetControlAdapter {
    fn should_stop<'a>(&'a self, _ctx: ShouldStopCtx<'a>) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            // Stop only when budget is exceeded AND the hook decided to stop.
            // Without an exceeded_hook the adapter is in surveillance-only mode.
            *self.budget_exceeded.lock().unwrap() && !*self.continue_decision.lock().unwrap()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct NoPricing;
    impl PricingProvider for NoPricing {
        fn price_for(&self, _model: &str, _provider: &str) -> Option<crate::cost::TokenPrice> {
            None
        }
    }

    #[test]
    fn budget_control_adapter_is_after_provider_response_hook() {
        let adapter = BudgetControlAdapter::new(Arc::new(NoPricing), 5.0, None);
        let _: &dyn AfterProviderResponseHook = &adapter;
    }

    #[test]
    fn budget_control_adapter_is_should_stop_hook() {
        let adapter = BudgetControlAdapter::new(Arc::new(NoPricing), 5.0, None);
        let _: &dyn ShouldStopHook = &adapter;
    }
}
