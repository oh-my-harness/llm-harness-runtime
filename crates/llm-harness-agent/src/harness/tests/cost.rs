use super::*;
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse, NoOpEnv};

#[test]
fn reset_usage_zeroes_accumulated_cost() {
    let h = make_harness_for_cost(vec![]);
    // Inject cost into the shared Arc that the harness holds.
    {
        let cost = h.inner.lock().unwrap().cost.clone();
        let mut c = cost.lock().unwrap();
        c.total_input_tokens = 100;
        c.total_output_tokens = 50;
        c.total_cost = 2.5;
        c.by_model.insert(
            "anthropic/claude".into(),
            ModelCost {
                input_tokens: 100,
                output_tokens: 50,
                cost: 2.5,
                call_count: 1,
                ..Default::default()
            },
        );
    }
    // Precondition: usage reflects the injected values.
    let before = h.usage();
    assert_eq!(before.total_input_tokens, 100);
    assert_eq!(before.total_cost, 2.5);
    assert_eq!(before.by_model.len(), 1);

    h.reset_usage();

    let after = h.usage();
    assert_eq!(after.total_input_tokens, 0);
    assert_eq!(after.total_output_tokens, 0);
    assert_eq!(after.total_cache_read_tokens, 0);
    assert_eq!(after.total_cache_write_tokens, 0);
    assert_eq!(after.total_cost, 0.0);
    assert!(after.by_model.is_empty());
}

/// Minimal harness constructor (parallel to `tests::make_harness`).
fn make_harness_for_cost(responses: Vec<MockResponse>) -> AgentHarness {
    let client = Arc::new(MockLlmClient::new(responses));
    let env = Arc::new(NoOpEnv);
    let opts = AgentHarnessOptions::new("test-model");
    futures::executor::block_on(AgentHarness::new_in_memory(
        client as Arc<dyn LlmClient>,
        env,
        opts,
    ))
}
