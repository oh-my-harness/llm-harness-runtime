//! 13 — Budget & Pricing: UsageLedger cost tracking, PricingProvider,
//! BudgetControlAdapter, and BudgetExceededHook.
//!
//! Demonstrates:
//! - `UsageLedger`: caller-owned cost accounting that survives harness runs
//! - `PricingProvider`: inject per-model token pricing to compute USD cost
//! - `CostAggregate` snapshot: total tokens, total cost, per-model breakdown
//! - `BudgetControlAdapter`: pure ShouldStopHook decider (construction demo)
//! - `BudgetExceededHook`: callback for continue/stop override decisions
//!
//! Architecture note:
//!   Cost tracking is split across three crates:
//!   - `llm-harness-types`: contracts (TokenPrice, PricingProvider,
//!     CostAggregate, BudgetExceededHook)
//!   - `llm-harness-runtime`: accumulation (UsageLedger, CostAccumulatorHook)
//!   - `llm-harness-strategy`: decisions (BudgetControlAdapter)
//!
//!   The builder injects `CostAccumulatorHook` internally and wires it to
//!   the shared `Arc<Mutex<CostAggregate>>` inside `UsageLedger`. The
//!   `BudgetControlAdapter` needs that same `Arc` to read accumulated cost.
//!   However, `UsageLedger::shared_state()` is `pub(crate)` — it cannot be
//!   called from outside the runtime crate. This means end-users cannot
//!   directly wire `BudgetControlAdapter` to the harness's internal cost
//!   state without runtime-crate access.
//!
//!   Part 1 (cost tracking) works fully from external code. Part 2
//!   demonstrates `BudgetControlAdapter` construction with a standalone
//!   cost state (mirroring the runtime's own unit tests) and documents
//!   the wiring path for in-crate consumers.
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 13_budget_pricing

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use llm_harness_live_tests::{
    EventCapture, LiveProvider, MULTI_TURN, make_harness_with, with_timeout,
};
use llm_harness_runtime::control::cost::{CostAggregate, PricingProvider, TokenPrice, UsageLedger};
use llm_harness_strategy::budget::BudgetControlAdapter;
use llm_harness_types::BudgetExceededHook;

// ── Fixed pricing provider ──────────────────────────────────────────────
//
// Returns a fixed token price for any model/provider combination. In
// production you would look up real pricing from a catalog keyed by model
// and provider name.

struct FixedPricing;

impl PricingProvider for FixedPricing {
    fn price_for(&self, _model: &str, _provider: &str) -> Option<TokenPrice> {
        // Pricing roughly approximating a mid-tier model:
        //   $3.00 / M input, $15.00 / M output,
        //   $0.30 / M cache read, $3.75 / M cache write.
        Some(TokenPrice {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cache_read_per_mtok: 0.3,
            cache_write_per_mtok: 3.75,
        })
    }
}

// ── BudgetExceededHook: logs and decides whether to continue ────────────
//
// When total_cost exceeds the budget limit, the BudgetControlAdapter
// consults this hook. Return `true` to continue (surveillance mode),
// `false` to stop the run.

struct LoggingBudgetHook {
    called: Mutex<bool>,
    /// `true` = continue past budget, `false` = stop the run.
    continue_anyway: bool,
}

impl LoggingBudgetHook {
    fn new(continue_anyway: bool) -> Self {
        Self {
            called: Mutex::new(false),
            continue_anyway,
        }
    }

    fn was_called(&self) -> bool {
        *self.called.lock().unwrap()
    }
}

impl BudgetExceededHook for LoggingBudgetHook {
    fn on_budget_exceeded<'a>(
        &'a self,
        cost: &'a CostAggregate,
        budget_limit: f64,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            *self.called.lock().unwrap() = true;
            println!(
                "  [BudgetExceededHook] budget ${:.4} exceeded! current cost: ${:.6} (in={}, out={})",
                budget_limit, cost.total_cost, cost.total_input_tokens, cost.total_output_tokens
            );
            self.continue_anyway
        })
    }
}

// ── Helper: run a single prompt turn and print a short response preview ─

async fn prompt_turn(harness: &llm_harness_agent::AgentHarness, label: &str, message: &str) {
    println!("--- {} ---", label);
    let capture = EventCapture::subscribe(harness);
    with_timeout(MULTI_TURN, harness.prompt(message))
        .await
        .expect("prompt failed");
    capture.settle().await;
    let text = capture.texts();
    let preview: String = text.chars().take(100).collect();
    println!("Response: {}...\n", preview);
}

#[tokio::main]
async fn main() {
    println!("=== 13: Budget & Pricing ===\n");

    let provider = LiveProvider::from_env();
    let entry = match provider.openai.as_ref() {
        Some(e) => e,
        None => {
            eprintln!("skipping: set OPENAI_API_KEY to run this example");
            return;
        }
    };

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);

    // ══════════════════════════════════════════════════════════════════════
    // Part 1: Cost tracking with UsageLedger + PricingProvider
    // ══════════════════════════════════════════════════════════════════════
    //
    // UsageLedger is caller-owned: clone it, hand one copy to the builder,
    // and retain one to snapshot cost after the run. The builder injects
    // an internal CostAccumulatorHook that writes each completed provider
    // response into the shared state inside the ledger.

    println!("── Part 1: Cost tracking with UsageLedger ──\n");

    let ledger = UsageLedger::default();
    let pricing: Arc<dyn PricingProvider> = Arc::new(FixedPricing);

    let (harness, _tmp) = make_harness_with(entry, |b| {
        b.usage_ledger(ledger.clone()).pricing(pricing.clone())
    })
    .await;

    // Run a 3-turn conversation so cost accumulates across calls.
    prompt_turn(
        &harness,
        "Turn 1",
        "In one sentence, explain what a token is in the context of LLMs.",
    )
    .await;

    prompt_turn(
        &harness,
        "Turn 2",
        "In one sentence, what is the difference between input and output tokens?",
    )
    .await;

    prompt_turn(
        &harness,
        "Turn 3",
        "In one sentence, why does caching reduce cost?",
    )
    .await;

    // Snapshot the accumulated cost from the retained ledger clone.
    let cost = ledger.snapshot();

    println!("── Cost snapshot (UsageLedger::snapshot) ──\n");
    println!("  total_input_tokens:       {}", cost.total_input_tokens);
    println!("  total_output_tokens:      {}", cost.total_output_tokens);
    println!(
        "  total_cache_read_tokens:  {}",
        cost.total_cache_read_tokens
    );
    println!(
        "  total_cache_write_tokens: {}",
        cost.total_cache_write_tokens
    );
    println!(
        "  total_reasoning_tokens:   {}",
        cost.total_reasoning_tokens
    );
    println!("  total_cost (USD):         ${:.6}", cost.total_cost);

    // Per-model breakdown: by_model maps model ID → ModelCost.
    if !cost.by_model.is_empty() {
        println!("\n  Per-model breakdown:");
        for (model, mc) in &cost.by_model {
            println!(
                "    {}: in={}, out={}, calls={}, cost=${:.6}",
                model, mc.input_tokens, mc.output_tokens, mc.call_count, mc.cost
            );
        }
    }

    // The harness also exposes usage() directly — same data, different path.
    let harness_usage = harness.usage();
    println!("\n  harness.usage() cross-check:");
    println!(
        "    total_cost matches ledger: {} (${:.6} == ${:.6})",
        (harness_usage.total_cost - cost.total_cost).abs() < 1e-9,
        harness_usage.total_cost,
        cost.total_cost
    );

    // ══════════════════════════════════════════════════════════════════════
    // Part 2: BudgetControlAdapter construction
    // ══════════════════════════════════════════════════════════════════════
    //
    // BudgetControlAdapter is a pure ShouldStopHook: it reads a shared
    // Arc<Mutex<CostAggregate>> and, when total_cost exceeds budget_limit,
    // consults the optional BudgetExceededHook.
    //
    // Wiring limitation:
    //   The adapter needs the SAME Arc<Mutex<CostAggregate>> that the
    //   builder injects into CostAccumulatorHook. That Arc lives inside
    //   UsageLedger, but UsageLedger::shared_state() is pub(crate) —
    //   callable only from within the llm-harness-runtime crate.
    //   External consumers cannot extract it to wire BudgetControlAdapter.
    //
    //   The runtime's own unit tests (budget_tests.rs) construct a
    //   standalone Arc<Mutex<CostAggregate::default()>> and pass it to
    //   both the adapter and (directly) the accumulator. From outside
    //   the crate, the pattern is:
    //
    //     1. Build the harness with usage_ledger + pricing (as in Part 1).
    //     2. Read cost via ledger.snapshot() or harness.usage() after
    //        each turn for surveillance (no enforcement).
    //     3. For enforcement, use the builder's .should_stop_hook() with
    //        a custom hook that reads from ledger.snapshot() internally.
    //
    //   Below we demonstrate BudgetControlAdapter construction with a
    //   standalone cost state, matching the unit-test pattern, to show
    //   the API surface and the BudgetExceededHook callback.

    println!("\n── Part 2: BudgetControlAdapter construction ──\n");

    // Standalone shared cost state (as in budget_tests.rs).
    let budget_cost: Arc<Mutex<CostAggregate>> = Arc::new(Mutex::new(CostAggregate::default()));

    // Simulate accumulated cost above a tiny budget.
    {
        let mut c = budget_cost.lock().unwrap();
        c.total_input_tokens = 5_000;
        c.total_output_tokens = 2_000;
        c.total_cost = 0.045; // $0.045 — over the $0.01 budget below.
    }

    let budget_limit = 0.01; // $0.01 — intentionally low to trigger the hook.
    let exceeded_hook_concrete = Arc::new(LoggingBudgetHook::new(false));
    let exceeded_hook: Arc<dyn BudgetExceededHook> = exceeded_hook_concrete.clone();

    // Surveillance mode (no hook): tracks cost but never stops.
    let surveillance = BudgetControlAdapter::new(budget_cost.clone(), 1.0, None);
    println!(
        "  Surveillance adapter (limit=${:.2}, no hook): constructed OK",
        1.0
    );
    let _ = surveillance; // suppress unused warning

    // Enforcement mode (with hook): stops when budget exceeded.
    let enforcing = BudgetControlAdapter::new(
        budget_cost.clone(),
        budget_limit,
        Some(exceeded_hook.clone()),
    );
    println!(
        "  Enforcement adapter (limit=${:.4}, with hook): constructed OK",
        budget_limit
    );

    // Drive the adapter manually: construct a ShouldStopCtx and call
    // should_stop. This mirrors budget_adapter_calls_hook_when_over_limit.
    println!("\n  Calling should_stop on the enforcement adapter...");

    use llm_harness_loop::test_utils::test_assistant_message;
    use llm_harness_types::{RunContext, RunRequest, ShouldStopCtx, ShouldStopHook, StopReason};

    let assistant = {
        let mut m = test_assistant_message(vec![]);
        m.stop_reason = Some(StopReason::EndTurn);
        m
    };

    let run = RunContext::new(RunRequest::default());
    let ctx = ShouldStopCtx {
        run: &run,
        last_assistant: &assistant,
        stop_reason: StopReason::EndTurn,
        turn_index: 0,
    };

    let stop = enforcing.should_stop(ctx).await;

    println!(
        "  should_stop returned: {} (true = stop, false = continue)",
        stop
    );
    println!(
        "  BudgetExceededHook was called: {}",
        exceeded_hook_concrete.was_called()
    );

    // ══════════════════════════════════════════════════════════════════════
    // Part 3: BudgetExceededHook callback demonstration
    // ══════════════════════════════════════════════════════════════════════
    //
    // The hook above already fired during the should_stop call. Here we
    // show the two hook responses: continue (true) vs stop (false).

    println!("\n── Part 3: BudgetExceededHook callback ──\n");

    // Hook that says "continue" — surveillance with logging.
    let continue_hook = Arc::new(LoggingBudgetHook::new(true));
    let cost_below: Arc<Mutex<CostAggregate>> = Arc::new(Mutex::new(CostAggregate::default()));
    {
        let mut c = cost_below.lock().unwrap();
        c.total_cost = 0.50; // over $0.10 limit
    }
    let adapter_continue = BudgetControlAdapter::new(
        cost_below.clone(),
        0.10,
        Some(continue_hook.clone() as Arc<dyn BudgetExceededHook>),
    );

    let assistant2 = {
        let mut m = test_assistant_message(vec![]);
        m.stop_reason = Some(StopReason::EndTurn);
        m
    };
    let run2 = RunContext::new(RunRequest::default());
    let ctx2 = ShouldStopCtx {
        run: &run2,
        last_assistant: &assistant2,
        stop_reason: StopReason::EndTurn,
        turn_index: 0,
    };

    let stop2 = adapter_continue.should_stop(ctx2).await;
    println!(
        "  continue-hook: was_called={}, should_stop={} (continue → false)",
        continue_hook.was_called(),
        stop2
    );

    // Hook that says "stop" — enforce budget ceiling.
    let stop_hook = Arc::new(LoggingBudgetHook::new(false));
    let adapter_stop = BudgetControlAdapter::new(
        cost_below.clone(),
        0.10,
        Some(stop_hook.clone() as Arc<dyn BudgetExceededHook>),
    );

    let assistant3 = {
        let mut m = test_assistant_message(vec![]);
        m.stop_reason = Some(StopReason::EndTurn);
        m
    };
    let run3 = RunContext::new(RunRequest::default());
    let ctx3 = ShouldStopCtx {
        run: &run3,
        last_assistant: &assistant3,
        stop_reason: StopReason::EndTurn,
        turn_index: 0,
    };

    let stop3 = adapter_stop.should_stop(ctx3).await;
    println!(
        "  stop-hook:     was_called={}, should_stop={} (stop → true)",
        stop_hook.was_called(),
        stop3
    );

    // ══════════════════════════════════════════════════════════════════════
    // Summary
    // ══════════════════════════════════════════════════════════════════════

    println!("\n── Summary ──\n");
    println!("Part 1: UsageLedger + PricingProvider → cost tracked across 3 turns ✅");
    println!(
        "  Accumulated: {} in / {} out tokens, ${:.6} estimated cost",
        cost.total_input_tokens, cost.total_output_tokens, cost.total_cost
    );
    println!("Part 2: BudgetControlAdapter constructed with standalone cost state ✅");
    println!("  (wiring to harness internal state requires runtime-crate access —");
    println!("   UsageLedger::shared_state() is pub(crate))");
    println!("Part 3: BudgetExceededHook callback demonstrated (continue + stop) ✅");
}
