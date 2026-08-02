//! Built-in cost accumulator hook.
//!
//! Implements `AfterProviderResponseHook` to accumulate token usage and
//! estimated cost into a shared `Arc<Mutex<CostAggregate>>`. The harness
//! holds the same `Arc` so `AgentHarness::usage()` reads live state without
//! duplicating accumulation logic.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use llm_harness_types::{
    AfterProviderResponseCtx, AfterProviderResponseHook, CostAggregate, PricingProvider,
    calculate_token_usage_cost, canonical_billing_usage_profile,
};

/// Accumulates token usage and cost from each provider response into a shared
/// `Arc<Mutex<CostAggregate>>`.
///
/// # Behavior
///
/// - **With a `PricingProvider`**: per-call USD cost is computed and added to
///   `CostAggregate.total_cost` and the `by_model` breakdown (tokens, cost,
///   call count per `provider/model` key).
/// - **Without pricing**: only the session-wide token totals
///   (`total_input_tokens`, `total_output_tokens`, `total_cache_read_tokens`,
///   `total_cache_write_tokens`) accumulate. `total_cost` stays `0.0` and
///   `by_model` stays empty — per-model token breakdown is only meaningful
///   when pricing is configured, since `ModelCost` carries a USD `cost` field.
pub(crate) struct CostAccumulatorHook {
    cost: Arc<Mutex<CostAggregate>>,
    pricing: Option<Arc<dyn PricingProvider>>,
}

impl CostAccumulatorHook {
    /// Create a new accumulator writing into the shared cost state.
    pub(crate) fn new(
        cost: Arc<Mutex<CostAggregate>>,
        pricing: Option<Arc<dyn PricingProvider>>,
    ) -> Self {
        Self { cost, pricing }
    }

    /// Snapshot of the current accumulated cost.
    #[allow(dead_code)]
    pub(crate) fn current_cost(&self) -> CostAggregate {
        self.cost.lock().unwrap().clone()
    }
}

impl AfterProviderResponseHook for CostAccumulatorHook {
    fn after_response<'a>(&'a self, ctx: AfterProviderResponseCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let info = ctx.info;
            let Some(usage) = &info.usage else {
                return;
            };

            let canonical_profile = canonical_billing_usage_profile();
            let usage_is_valid = usage.validate_billing_profile().is_ok();
            let profile_is_valid = {
                let mut cost = self.cost.lock().unwrap();
                let profile_is_valid = match &cost.billing_usage_profile {
                    Some(profile) => profile == &canonical_profile,
                    None => {
                        cost.billing_usage_profile = Some(canonical_profile);
                        true
                    }
                };
                cost.total_input_tokens += u64::from(usage.input_tokens);
                cost.total_output_tokens += u64::from(usage.output_tokens);
                cost.total_cache_read_tokens += u64::from(usage.cache_read_tokens);
                cost.total_cache_write_tokens += u64::from(usage.cache_creation_tokens);
                cost.total_reasoning_tokens += u64::from(usage.reasoning_tokens);
                if !usage_is_valid || !profile_is_valid {
                    cost.billing_usage_violations += 1;
                }
                profile_is_valid
            };

            // Invalid usage never reaches the externally supplied pricing
            // source. The violation makes trusted accounting fail closed.
            if !usage_is_valid || !profile_is_valid {
                return;
            }

            // Resolve pricing only after the usage contract has been checked.
            let price = self.pricing.as_ref().and_then(|p| {
                info.model
                    .as_deref()
                    .and_then(|m| p.price_for(m, info.provider.as_deref().unwrap_or("")))
            });

            if let Some(p) = price {
                let call_cost = calculate_token_usage_cost(usage, &p)
                    .expect("usage was validated before pricing lookup");
                let mut cost = self.cost.lock().unwrap();
                cost.total_cost += call_cost;

                if let Some(m) = info.model.as_deref() {
                    let pr = info.provider.as_deref().unwrap_or("");
                    let key = if pr.is_empty() {
                        m.to_string()
                    } else {
                        format!("{pr}/{m}")
                    };
                    let mc = cost.by_model.entry(key).or_default();
                    mc.input_tokens += u64::from(usage.input_tokens);
                    mc.output_tokens += u64::from(usage.output_tokens);
                    mc.cache_read_tokens += u64::from(usage.cache_read_tokens);
                    mc.cache_write_tokens += u64::from(usage.cache_creation_tokens);
                    mc.reasoning_tokens += u64::from(usage.reasoning_tokens);
                    mc.cost += call_cost;
                    mc.call_count += 1;
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::budget::{BudgetControlAdapter, BudgetExceededHook};
    use futures::future::BoxFuture;
    use llm_harness_types::{
        AfterProviderResponseCtx, AssistantMessage, CostAggregate, ProviderResponseInfo,
        RunContext, RunRequest, ShouldStopCtx, ShouldStopHook, StopReason, TokenPrice, TokenUsage,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    struct FixedPricing;
    impl PricingProvider for FixedPricing {
        fn price_for(&self, _model: &str, _provider: &str) -> Option<TokenPrice> {
            Some(TokenPrice {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cache_read_per_mtok: 0.3,
                cache_write_per_mtok: 3.75,
            })
        }
    }

    struct CachePricing;
    impl PricingProvider for CachePricing {
        fn price_for(&self, _model: &str, _provider: &str) -> Option<TokenPrice> {
            Some(TokenPrice {
                input_per_mtok: 1.0,
                output_per_mtok: 2.0,
                cache_read_per_mtok: 0.1,
                cache_write_per_mtok: 0.5,
            })
        }
    }

    struct StopOnExceeded;
    impl BudgetExceededHook for StopOnExceeded {
        fn on_budget_exceeded<'a>(
            &'a self,
            _cost: &'a CostAggregate,
            _budget_limit: f64,
        ) -> BoxFuture<'a, bool> {
            Box::pin(async { false })
        }
    }

    struct CountingPricing {
        calls: Arc<AtomicU64>,
    }

    impl PricingProvider for CountingPricing {
        fn price_for(&self, _model: &str, _provider: &str) -> Option<TokenPrice> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Some(TokenPrice {
                input_per_mtok: 1.0,
                output_per_mtok: 1.0,
                cache_read_per_mtok: 1.0,
                cache_write_per_mtok: 1.0,
            })
        }
    }

    fn make_info(usage: TokenUsage, model: &str, provider: &str) -> ProviderResponseInfo {
        ProviderResponseInfo {
            status_code: None,
            response_headers: vec![],
            usage: Some(usage),
            latency_ms: 0,
            model: Some(model.into()),
            provider: Some(provider.into()),
        }
    }

    /// Mirrors the production path where `StreamingState` does not populate
    /// `provider` (always `None`).
    fn make_info_no_provider(usage: TokenUsage, model: &str) -> ProviderResponseInfo {
        ProviderResponseInfo {
            status_code: None,
            response_headers: vec![],
            usage: Some(usage),
            latency_ms: 0,
            model: Some(model.into()),
            provider: None,
        }
    }

    async fn record_response(hook: &CostAccumulatorHook, info: &ProviderResponseInfo) {
        let run = RunContext::new(RunRequest::default());
        hook.after_response(AfterProviderResponseCtx {
            run: &run,
            turn_index: 0,
            info,
        })
        .await;
    }

    fn assistant() -> AssistantMessage {
        let mut message = llm_harness_loop::test_utils::test_assistant_message(vec![]);
        message.stop_reason = Some(StopReason::EndTurn);
        message
    }

    fn stop_context(assistant: &AssistantMessage) -> ShouldStopCtx<'_> {
        static RUN: std::sync::OnceLock<RunContext> = std::sync::OnceLock::new();
        ShouldStopCtx {
            run: RUN.get_or_init(|| RunContext::new(RunRequest::default())),
            last_assistant: assistant,
            stop_reason: StopReason::EndTurn,
            turn_index: 0,
        }
    }

    #[tokio::test]
    async fn cache_categories_are_billed_once_at_exact_budget_boundaries() {
        let cost = Arc::new(Mutex::new(CostAggregate::default()));
        let hook = CostAccumulatorHook::new(cost.clone(), Some(Arc::new(CachePricing)));
        let usage = TokenUsage {
            input_tokens: 1_000,
            output_tokens: 500,
            cache_read_tokens: 400,
            cache_creation_tokens: 0,
            reasoning_tokens: 100,
        };

        record_response(&hook, &make_info(usage, "cached-model", "openai")).await;

        let observed = hook.current_cost();
        assert!((observed.total_cost - 0.00164).abs() < 1e-12);
        assert_eq!(
            observed.billing_usage_profile,
            Some(canonical_billing_usage_profile())
        );
        assert_eq!(observed.billing_usage_violations, 0);
        let message = assistant();
        let exact =
            BudgetControlAdapter::new(cost.clone(), 0.00164, Some(Arc::new(StopOnExceeded)));
        assert!(!exact.should_stop(stop_context(&message)).await);
        let below = BudgetControlAdapter::new(cost, 0.00163, Some(Arc::new(StopOnExceeded)));
        assert!(below.should_stop(stop_context(&message)).await);
    }

    #[tokio::test]
    async fn cache_read_and_write_categories_are_billed_once() {
        let cost = Arc::new(Mutex::new(CostAggregate::default()));
        let hook = CostAccumulatorHook::new(cost, Some(Arc::new(CachePricing)));
        let usage = TokenUsage {
            input_tokens: 1_000,
            output_tokens: 500,
            cache_read_tokens: 400,
            cache_creation_tokens: 100,
            reasoning_tokens: 100,
        };

        record_response(&hook, &make_info(usage, "cached-model", "anthropic")).await;

        assert!((hook.current_cost().total_cost - 0.00159).abs() < 1e-12);
    }

    #[tokio::test]
    async fn invalid_cache_relationship_fails_closed_before_pricing_lookup() {
        let calls = Arc::new(AtomicU64::new(0));
        let cost = Arc::new(Mutex::new(CostAggregate::default()));
        let hook = CostAccumulatorHook::new(
            cost.clone(),
            Some(Arc::new(CountingPricing {
                calls: calls.clone(),
            })),
        );
        let usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 80,
            cache_creation_tokens: 30,
            reasoning_tokens: 5,
        };

        record_response(&hook, &make_info(usage, "cached-model", "openai")).await;

        let observed = hook.current_cost();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(observed.total_input_tokens, 100);
        assert_eq!(observed.total_cache_read_tokens, 80);
        assert_eq!(observed.total_cache_write_tokens, 30);
        assert_eq!(observed.total_cost, 0.0);
        assert_eq!(observed.billing_usage_violations, 1);
        assert!(observed.by_model.is_empty());
        assert!(observed.validate_billing_profile().is_err());

        let message = assistant();
        let budget = BudgetControlAdapter::new(cost, f64::MAX, None);
        assert!(budget.should_stop(stop_context(&message)).await);
    }

    #[tokio::test]
    async fn accumulates_token_counts_without_pricing() {
        let cost = Arc::new(Mutex::new(CostAggregate::default()));
        let hook = CostAccumulatorHook::new(cost.clone(), None);

        let usage = TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 200,
            cache_creation_tokens: 100,
            reasoning_tokens: 300,
        };
        record_response(&hook, &make_info(usage, "claude-sonnet", "anthropic")).await;

        let c = hook.current_cost();
        assert_eq!(c.total_input_tokens, 1000);
        assert_eq!(c.total_output_tokens, 500);
        assert_eq!(c.total_cache_read_tokens, 200);
        assert_eq!(c.total_cache_write_tokens, 100);
        assert_eq!(c.total_reasoning_tokens, 300);
        assert_eq!(c.total_cost, 0.0, "no pricing → no cost");
        assert!(c.by_model.is_empty());
    }

    #[tokio::test]
    async fn accumulates_cost_with_pricing() {
        let cost = Arc::new(Mutex::new(CostAggregate::default()));
        let hook = CostAccumulatorHook::new(cost.clone(), Some(Arc::new(FixedPricing)));

        let usage = TokenUsage {
            input_tokens: 1_000_000, // 1M tokens → $3.0 at 3.0/M
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            reasoning_tokens: 0,
        };
        record_response(&hook, &make_info(usage, "claude-sonnet", "anthropic")).await;

        let c = hook.current_cost();
        assert!(
            (c.total_cost - 3.0).abs() < 1e-9,
            "expected 3.0, got {}",
            c.total_cost
        );
        let mc = c
            .by_model
            .get("anthropic/claude-sonnet")
            .expect("by_model entry");
        assert_eq!(mc.input_tokens, 1_000_000);
        assert_eq!(mc.call_count, 1);
        assert!((mc.cost - 3.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn accumulates_across_multiple_calls() {
        let cost = Arc::new(Mutex::new(CostAggregate::default()));
        let hook = CostAccumulatorHook::new(cost.clone(), Some(Arc::new(FixedPricing)));

        let u1 = TokenUsage {
            input_tokens: 500_000,
            ..Default::default()
        };
        let u2 = TokenUsage {
            input_tokens: 500_000,
            ..Default::default()
        };
        record_response(&hook, &make_info(u1, "claude-sonnet", "anthropic")).await;
        record_response(&hook, &make_info(u2, "claude-sonnet", "anthropic")).await;

        let c = hook.current_cost();
        assert_eq!(c.total_input_tokens, 1_000_000);
        assert!((c.total_cost - 3.0).abs() < 1e-9);
        let mc = c.by_model.get("anthropic/claude-sonnet").unwrap();
        assert_eq!(mc.call_count, 2);
    }

    #[tokio::test]
    async fn no_usage_is_noop() {
        let cost = Arc::new(Mutex::new(CostAggregate::default()));
        let hook = CostAccumulatorHook::new(cost.clone(), Some(Arc::new(FixedPricing)));

        let info = ProviderResponseInfo {
            status_code: None,
            response_headers: vec![],
            usage: None,
            latency_ms: 0,
            model: Some("claude-sonnet".into()),
            provider: Some("anthropic".into()),
        };
        record_response(&hook, &info).await;

        let c = hook.current_cost();
        assert_eq!(c.total_input_tokens, 0);
        assert_eq!(c.total_cost, 0.0);
    }
    /// Regression: production path has `provider: None` (StreamingState does
    /// not expose provider metadata). Pricing must still be resolved by model
    /// alone, and `by_model` key falls back to the bare model name.
    #[tokio::test]
    async fn pricing_works_when_provider_is_none() {
        let cost = Arc::new(Mutex::new(CostAggregate::default()));
        let hook = CostAccumulatorHook::new(cost.clone(), Some(Arc::new(FixedPricing)));

        let usage = TokenUsage {
            input_tokens: 1_000_000, // → $3.0 at 3.0/M
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            reasoning_tokens: 0,
        };
        record_response(&hook, &make_info_no_provider(usage, "claude-sonnet")).await;

        let c = hook.current_cost();
        assert!(
            (c.total_cost - 3.0).abs() < 1e-9,
            "provider=None must not block pricing; got {}",
            c.total_cost
        );
        // Key is the bare model name when provider is absent.
        let mc = c.by_model.get("claude-sonnet").expect("by_model entry");
        assert_eq!(mc.input_tokens, 1_000_000);
        assert_eq!(mc.call_count, 1);
        assert!((mc.cost - 3.0).abs() < 1e-9);
    }
    /// Regression: per-model `ModelCost` must record cache tokens, not just
    /// input/output. Otherwise `mc.cost` (which includes cache fees) cannot be
    /// reconciled against the token dimensions.
    #[tokio::test]
    async fn per_model_accumulates_cache_tokens() {
        let cost = Arc::new(Mutex::new(CostAggregate::default()));
        let hook = CostAccumulatorHook::new(cost.clone(), Some(Arc::new(FixedPricing)));

        let usage = TokenUsage {
            input_tokens: 400,
            output_tokens: 50,
            cache_read_tokens: 200,
            cache_creation_tokens: 100,
            reasoning_tokens: 50,
        };
        record_response(&hook, &make_info(usage, "claude-sonnet", "anthropic")).await;

        let c = hook.current_cost();
        let mc = c
            .by_model
            .get("anthropic/claude-sonnet")
            .expect("by_model entry");
        assert_eq!(mc.input_tokens, 400);
        assert_eq!(mc.output_tokens, 50);
        assert_eq!(
            mc.cache_read_tokens, 200,
            "cache_read must land in by_model"
        );
        assert_eq!(
            mc.cache_write_tokens, 100,
            "cache_write must land in by_model"
        );
        assert_eq!(mc.reasoning_tokens, 50);
        assert_eq!(mc.call_count, 1);
    }
}
