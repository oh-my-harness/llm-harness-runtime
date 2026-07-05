//! Budget control integration tests.
//!
//! Verifies BudgetControlAdapter (pure ShouldStopHook decider), the shared
//! cost state contract, and the PricingProvider / BudgetExceededHook trait
//! contracts.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use llm_harness_runtime::control::budget::{BudgetControlAdapter, BudgetExceededHook};
use llm_harness_runtime::control::cost::{CostAggregate, ModelCost, PricingProvider, TokenPrice};
use llm_harness_types::{AssistantMessage, ShouldStopCtx, ShouldStopHook, StopReason};

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
        _cost: &'a CostAggregate,
        _budget_limit: f64,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            *self.called.lock().unwrap() = true;
            self.response
        })
    }
}

fn make_assistant() -> AssistantMessage {
    let mut m = llm_harness_loop::test_utils::test_assistant_message(vec![]);
    m.stop_reason = Some(StopReason::EndTurn);
    m
}

fn make_ctx<'a>(assistant: &'a AssistantMessage) -> ShouldStopCtx<'a> {
    ShouldStopCtx {
        last_assistant: assistant,
        stop_reason: StopReason::EndTurn,
        turn_index: 0,
    }
}

#[test]
fn budget_adapter_constructs_without_hook() {
    let cost = Arc::new(Mutex::new(CostAggregate::default()));
    let adapter = BudgetControlAdapter::new(cost, 5.0, None);
    // Just verify construction; no hook means surveillance mode.
    let _ = adapter;
}

#[test]
fn budget_adapter_constructs_with_hook() {
    let cost = Arc::new(Mutex::new(CostAggregate::default()));
    let hook = Arc::new(RecordingBudgetHook::new(false));
    let adapter =
        BudgetControlAdapter::new(cost, 5.0, Some(hook.clone() as Arc<dyn BudgetExceededHook>));
    // Hook not called until budget is exceeded (which requires a should_stop call).
    assert!(!hook.was_called());
    let _ = adapter;
}

#[tokio::test]
async fn budget_adapter_calls_hook_when_over_limit() {
    let cost = Arc::new(Mutex::new(CostAggregate::default()));
    let hook = Arc::new(RecordingBudgetHook::new(true));
    let adapter = BudgetControlAdapter::new(
        cost.clone(),
        5.0,
        Some(hook.clone() as Arc<dyn BudgetExceededHook>),
    );

    // Inflate cost above limit.
    cost.lock().unwrap().total_cost = 10.0;

    let assistant = make_assistant();
    let ctx = make_ctx(&assistant);
    let stop = adapter.should_stop(ctx).await;
    assert!(
        hook.was_called(),
        "exceeded hook must be called when over limit"
    );
    assert!(
        !stop,
        "hook returned true (continue) → should_stop must be false"
    );
}

#[test]
fn pricing_provider_returns_none_for_unknown_model() {
    let provider = UnknownPricingProvider;
    assert!(
        provider
            .price_for("unknown-model", "unknown-provider")
            .is_none()
    );
}

#[test]
fn fixed_pricing_provider_returns_price() {
    let provider = FixedPricingProvider {
        price: TokenPrice {
            input_per_mtok: 15.0,
            output_per_mtok: 60.0,
            cache_read_per_mtok: 1.5,
            cache_write_per_mtok: 30.0,
        },
    };
    assert!(provider.price_for("claude-sonnet", "anthropic").is_some());
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
