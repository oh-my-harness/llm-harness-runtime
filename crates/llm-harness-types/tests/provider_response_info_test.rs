//! Verifies ProviderResponseInfo carries model/provider for cost pricing.

use llm_harness_types::ProviderResponseInfo;

#[test]
fn provider_response_info_has_model_and_provider() {
    let info = ProviderResponseInfo {
        status_code: None,
        response_headers: vec![],
        usage: None,
        latency_ms: 0,
        model: Some("claude-sonnet-4".into()),
        provider: Some("anthropic".into()),
    };
    assert_eq!(info.model.as_deref(), Some("claude-sonnet-4"));
    assert_eq!(info.provider.as_deref(), Some("anthropic"));
}
