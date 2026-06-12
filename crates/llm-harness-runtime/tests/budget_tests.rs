//! Budget control integration tests.
//!
//! Verifies BudgetControlAdapter construction, CostAggregate state, and
//! the PricingProvider / BudgetExceededHook trait contracts.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use llm_harness_runtime::budget::{BudgetControlAdapter, BudgetExceededHook};
use llm_harness_runtime::cost::{CostAggregate, ModelCost, PricingProvider, TokenPrice};

struct FixedPricingProvider {
    price: TokenPrice,
}

impl PricingProvider for FixedPricingProvider {
    fn price_for(&self, _model: &str, _provider: &str) -> Option<TokenPrice> {
        Some(self.price.clone())
    }
}

struct UnknownPricingProvider;

impl PricingProvider for UnknownPricingProvider {
    fn price_for(&self, _model: &str, _provider: &str) -> Option<TokenPrice> {
        None
    }
}

struct RecordingBudgetHook {
    called: Mutex<bool>,
    response: bool,
}

impl RecordingBudgetHook {
    fn new(response: bool) -> Self {
        Self {
            called: Mutex::new(false),
            response,
        }
    }

    fn was_called(&self) -> bool {
        *self.called.lock().unwrap()
    }
}

impl BudgetExceededHook for RecordingBudgetHook {
    fn on_budget_exceeded<'a>(
        &'a self,
        _session_cost: &'a CostAggregate,
        _task_cost: &'a CostAggregate,
        _budget_limit: f64,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            *self.called.lock().unwrap() = true;
            self.response
        })
    }
}

#[test]
fn budget_adapter_constructs_without_hook() {
    let pricing = Arc::new(FixedPricingProvider {
        price: TokenPrice {
            input_per_mtok: 15.0,
            output_per_mtok: 60.0,
            cache_read_per_mtok: 1.5,
            cache_write_per_mtok: 30.0,
        },
    });

    let adapter = BudgetControlAdapter::new(pricing, 5.0, None);
    let cost = adapter.current_cost();
    assert_eq!(cost.total_input_tokens, 0);
    assert_eq!(cost.total_output_tokens, 0);
    assert_eq!(cost.total_cost, 0.0);
}

#[test]
fn budget_adapter_constructs_with_hook() {
    let pricing = Arc::new(FixedPricingProvider {
        price: TokenPrice {
            input_per_mtok: 15.0,
            output_per_mtok: 60.0,
            cache_read_per_mtok: 1.5,
            cache_write_per_mtok: 30.0,
        },
    });
    let hook = Arc::new(RecordingBudgetHook::new(false));

    let adapter = BudgetControlAdapter::new(pricing, 5.0, Some(hook.clone()));
    // Hook not called until budget is exceeded.
    assert!(!hook.was_called());
    let _ = adapter;
}

#[test]
fn budget_adapter_with_model_accepts_model_info() {
    let pricing = Arc::new(FixedPricingProvider {
        price: TokenPrice {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cache_read_per_mtok: 0.3,
            cache_write_per_mtok: 3.75,
        },
    });
    let adapter =
        BudgetControlAdapter::new(pricing, 100.0, None).with_model("claude-sonnet", "anthropic");
    assert_eq!(adapter.current_cost().total_cost, 0.0);
}

#[test]
fn pricing_provider_returns_none_for_unknown_model() {
    let provider = UnknownPricingProvider;
    assert!(provider.price_for("unknown-model", "unknown-provider").is_none());
}

#[test]
fn cost_aggregate_default_is_zero() {
    let cost = CostAggregate::default();
    assert_eq!(cost.total_input_tokens, 0);
    assert_eq!(cost.total_output_tokens, 0);
    assert_eq!(cost.total_cost, 0.0);
    assert!(cost.by_model.is_empty());
}

#[test]
fn model_cost_accumulates() {
    let mut mc = ModelCost::default();
    mc.input_tokens += 1_000;
    mc.output_tokens += 500;
    mc.cost += 0.05;
    mc.call_count += 1;

    assert_eq!(mc.input_tokens, 1_000);
    assert_eq!(mc.output_tokens, 500);
    assert_eq!(mc.cost, 0.05);
    assert_eq!(mc.call_count, 1);
}
