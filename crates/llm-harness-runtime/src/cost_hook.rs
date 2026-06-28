//! Built-in cost accumulator hook.
//!
//! Implements `AfterProviderResponseHook` to accumulate token usage and
//! estimated cost into a shared `Arc<Mutex<CostAggregate>>`. The harness
//! holds the same `Arc` so `AgentHarness::usage()` reads live state without
//! duplicating accumulation logic.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use llm_harness_types::{
    AfterProviderResponseHook, CostAggregate, PricingProvider, ProviderResponseInfo,
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
pub struct CostAccumulatorHook {
    cost: Arc<Mutex<CostAggregate>>,
    pricing: Option<Arc<dyn PricingProvider>>,
}

impl CostAccumulatorHook {
    /// Create a new accumulator writing into the shared cost state.
    pub fn new(cost: Arc<Mutex<CostAggregate>>, pricing: Option<Arc<dyn PricingProvider>>) -> Self {
        Self { cost, pricing }
    }

    /// Snapshot of the current accumulated cost.
    pub fn current_cost(&self) -> CostAggregate {
        self.cost.lock().unwrap().clone()
    }
}

impl AfterProviderResponseHook for CostAccumulatorHook {
    fn after_response<'a>(&'a self, info: &'a ProviderResponseInfo) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let Some(usage) = &info.usage else {
                return;
            };

            // Resolve pricing for this model (if configured). Provider is
            // optional: when the adapter does not expose provider metadata
            // (the common case — see `StreamingState`), pricing is looked up
            // by model alone with an empty provider string.
            let price = self.pricing.as_ref().and_then(|p| {
                info.model
                    .as_deref()
                    .and_then(|m| p.price_for(m, info.provider.as_deref().unwrap_or("")))
            });

            let mut cost = self.cost.lock().unwrap();
            cost.total_input_tokens += usage.input_tokens as u64;
            cost.total_output_tokens += usage.output_tokens as u64;
            cost.total_cache_read_tokens += usage.cache_read_tokens as u64;
            cost.total_cache_write_tokens += usage.cache_creation_tokens as u64;

            if let Some(p) = price {
                let call_cost = (usage.input_tokens as f64 / 1_000_000.0) * p.input_per_mtok
                    + (usage.output_tokens as f64 / 1_000_000.0) * p.output_per_mtok
                    + (usage.cache_read_tokens as f64 / 1_000_000.0) * p.cache_read_per_mtok
                    + (usage.cache_creation_tokens as f64 / 1_000_000.0)
                        * p.cache_write_per_mtok;
                cost.total_cost += call_cost;

                if let Some(m) = info.model.as_deref() {
                    let pr = info.provider.as_deref().unwrap_or("");
                    let key = if pr.is_empty() {
                        m.to_string()
                    } else {
                        format!("{pr}/{m}")
                    };
                    let mc = cost.by_model.entry(key).or_default();
                    mc.input_tokens += usage.input_tokens as u64;
                    mc.output_tokens += usage.output_tokens as u64;
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
    use llm_harness_types::{TokenPrice, TokenUsage};

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

    #[tokio::test]
    async fn accumulates_token_counts_without_pricing() {
        let cost = Arc::new(Mutex::new(CostAggregate::default()));
        let hook = CostAccumulatorHook::new(cost.clone(), None);

        let usage = TokenUsage {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 200,
            cache_creation_tokens: 100,
        };
        hook.after_response(&make_info(usage, "claude-sonnet", "anthropic"))
            .await;

        let c = hook.current_cost();
        assert_eq!(c.total_input_tokens, 1000);
        assert_eq!(c.total_output_tokens, 500);
        assert_eq!(c.total_cache_read_tokens, 200);
        assert_eq!(c.total_cache_write_tokens, 100);
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
        };
        hook.after_response(&make_info(usage, "claude-sonnet", "anthropic"))
            .await;

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
        hook.after_response(&make_info(u1, "claude-sonnet", "anthropic"))
            .await;
        hook.after_response(&make_info(u2, "claude-sonnet", "anthropic"))
            .await;

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
        hook.after_response(&info).await;

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
        };
        hook.after_response(&make_info_no_provider(usage, "claude-sonnet"))
            .await;

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
}
