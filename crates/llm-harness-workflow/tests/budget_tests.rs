//! Budget control integration tests.
//!
//! Verifies BudgetControlAdapter (pure ShouldStopHook decider), the shared
//! cost state contract, and the PricingProvider / BudgetExceededHook trait
//! contracts.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use llm_harness_strategy::budget::BudgetControlAdapter;
use llm_harness_types::BudgetExceededHook;
use llm_harness_types::{AssistantMessage, ShouldStopCtx, ShouldStopHook, StopReason};
use llm_harness_types::{
    CostAggregate, ModelCost, PricingProvider, TokenPrice, TokenUsage, UsageReportProvenance,
    calculate_token_usage_cost, canonical_billing_usage_profile,
};

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

fn make_assistant() -> AssistantMessage {
    let mut m = llm_harness_loop::test_utils::test_assistant_message(vec![]);
    m.stop_reason = Some(StopReason::EndTurn);
    m
}

fn test_run() -> &'static llm_harness_types::RunContext {
    static RUN: std::sync::OnceLock<llm_harness_types::RunContext> = std::sync::OnceLock::new();
    RUN.get_or_init(|| llm_harness_types::RunContext::new(llm_harness_types::RunRequest::default()))
}

fn make_ctx<'a>(assistant: &'a AssistantMessage) -> ShouldStopCtx<'a> {
    ShouldStopCtx {
        run: test_run(),
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

#[tokio::test]
async fn cache_categories_are_billed_once_at_exact_budget_boundaries() {
    // Accumulated state reflects a single valid priced response with cache
    // categories billed under the canonical accounting profile (CachePricing:
    // input 1.0, output 2.0, cache_read 0.1, cache_write 0.5 per Mtok).
    let usage = TokenUsage {
        input_tokens: 1_000,
        output_tokens: 500,
        cache_read_tokens: 400,
        cache_creation_tokens: 0,
        reasoning_tokens: 100,
        provenance: UsageReportProvenance::ReportedValid,
    };
    let price = TokenPrice {
        input_per_mtok: 1.0,
        output_per_mtok: 2.0,
        cache_read_per_mtok: 0.1,
        cache_write_per_mtok: 0.5,
    };
    let billed = calculate_token_usage_cost(&usage, &price).unwrap();
    assert!(
        (billed - 0.00164).abs() < 1e-12,
        "expected 0.00164, got {billed}"
    );

    let mut aggregate = CostAggregate {
        total_input_tokens: u64::from(usage.input_tokens),
        total_output_tokens: u64::from(usage.output_tokens),
        total_cache_read_tokens: u64::from(usage.cache_read_tokens),
        total_reasoning_tokens: u64::from(usage.reasoning_tokens),
        total_cost: billed,
        billing_usage_profile: Some(canonical_billing_usage_profile()),
        ..Default::default()
    };
    // One valid provider report → no accounting or provenance violations.
    aggregate
        .usage_reports
        .record(UsageReportProvenance::ReportedValid);
    assert_eq!(aggregate.billing_usage_violations, 0);
    assert!(aggregate.usage_reports.require_all_reported_valid().is_ok());

    let cost = Arc::new(Mutex::new(aggregate));
    let message = make_assistant();

    // Exactly at the limit → not exceeded → continue.
    let exact = BudgetControlAdapter::new(cost.clone(), 0.00164, Some(Arc::new(StopOnExceeded)));
    assert!(!exact.should_stop(make_ctx(&message)).await);
    // Just below the limit → exceeded → stop.
    let below = BudgetControlAdapter::new(cost, 0.00163, Some(Arc::new(StopOnExceeded)));
    assert!(below.should_stop(make_ctx(&message)).await);
}

#[tokio::test]
async fn invalid_cache_relationship_fails_closed_before_pricing_lookup() {
    // An invalid cache relationship (cache_read + cache_creation > gross
    // input) is rejected upstream in the accumulator; the shared aggregate
    // records a billing violation. BudgetControlAdapter must stop even at an
    let mut aggregate = CostAggregate {
        billing_usage_violations: 1,
        ..Default::default()
    };
    aggregate
        .usage_reports
        .record(UsageReportProvenance::ReportedInvalid);
    assert!(aggregate.validate_billing_profile().is_err());

    let cost = Arc::new(Mutex::new(aggregate));
    let message = make_assistant();
    let budget = BudgetControlAdapter::new(cost, f64::MAX, None);
    assert!(budget.should_stop(make_ctx(&message)).await);
}

#[tokio::test]
async fn missing_usage_records_unknown_cost_and_stops_budget() {
    // A response completing without a usage payload is recorded upstream as a
    // missing/unknown-cost call. The aggregate's usage reports are then not
    // all valid, so BudgetControlAdapter stops even at an unbounded budget —
    // unknown cost is never silently billed through.
    let mut aggregate = CostAggregate::default();
    aggregate
        .usage_reports
        .record(UsageReportProvenance::Missing);
    assert_eq!(aggregate.usage_reports.unknown_cost_calls(), 1);

    let cost = Arc::new(Mutex::new(aggregate));
    let message = make_assistant();
    let budget = BudgetControlAdapter::new(cost, f64::MAX, None);
    assert!(budget.should_stop(make_ctx(&message)).await);
}

#[tokio::test]
async fn reported_zero_usage_is_valid_priced_evidence() {
    // An explicit zero-token, ReportedValid payload is authoritative: it keeps
    // the aggregate billing-clean (all reports valid, cost 0.0) and per-model
    // observability intact, so a zero budget does NOT stop the loop.
    let mut aggregate = CostAggregate::default();
    aggregate
        .usage_reports
        .record(UsageReportProvenance::ReportedValid);
    aggregate.by_model.insert(
        "anthropic/claude-sonnet".to_string(),
        ModelCost {
            call_count: 1,
            ..ModelCost::default()
        },
    );
    assert!(aggregate.usage_reports.require_all_reported_valid().is_ok());
    assert_eq!(aggregate.total_cost, 0.0);

    let cost = Arc::new(Mutex::new(aggregate));
    let message = make_assistant();
    let budget = BudgetControlAdapter::new(cost, 0.0, None);
    assert!(!budget.should_stop(make_ctx(&message)).await);
}

#[tokio::test]
async fn provider_reported_invalid_usage_records_unknown_cost() {
    // A ReportedInvalid payload upstream records an unknown-cost call plus a
    // billing violation; BudgetControlAdapter fails closed at an unbounded
    // budget rather than trusting the price of an invalid report.
    let mut aggregate = CostAggregate {
        billing_usage_violations: 1,
        ..Default::default()
    };
    aggregate
        .usage_reports
        .record(UsageReportProvenance::ReportedInvalid);
    assert_eq!(aggregate.usage_reports.unknown_cost_calls(), 1);

    let cost = Arc::new(Mutex::new(aggregate));
    let message = make_assistant();
    let budget = BudgetControlAdapter::new(cost, f64::MAX, None);
    assert!(budget.should_stop(make_ctx(&message)).await);
}
