use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Per-model token pricing.
#[derive(Debug, Clone)]
pub struct TokenPrice {
    /// Input cost per million tokens (USD).
    pub input_per_mtok: f64,
    /// Output cost per million tokens (USD).
    pub output_per_mtok: f64,
    /// Cache read cost per million tokens (USD).
    pub cache_read_per_mtok: f64,
    /// Cache write cost per million tokens (USD).
    pub cache_write_per_mtok: f64,
}

/// Pricing provider (business layer injects).
pub trait PricingProvider: Send + Sync {
    /// Return pricing for a model/provider combination. `None` if unknown.
    fn price_for(&self, model: &str, provider: &str) -> Option<TokenPrice>;
}

/// Per-model cost breakdown.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCost {
    /// Total input tokens.
    pub input_tokens: u64,
    /// Total output tokens.
    pub output_tokens: u64,
    /// Total cache read tokens.
    pub cache_read_tokens: u64,
    /// Total cache write tokens.
    pub cache_write_tokens: u64,
    /// Estimated total cost in USD.
    pub cost: f64,
    /// Number of LLM calls.
    pub call_count: u64,
}

/// Aggregated cost across all models in a session/task.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostAggregate {
    /// Total input tokens across all models.
    pub total_input_tokens: u64,
    /// Total output tokens across all models.
    pub total_output_tokens: u64,
    /// Total cache read tokens.
    pub total_cache_read_tokens: u64,
    /// Total cache write tokens.
    pub total_cache_write_tokens: u64,
    /// Total estimated cost in USD.
    pub total_cost: f64,
    /// Per-model breakdown.
    pub by_model: HashMap<String, ModelCost>,
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_cost_default_has_zero_cache_fields() {
        let mc = ModelCost::default();
        assert_eq!(mc.cache_read_tokens, 0);
        assert_eq!(mc.cache_write_tokens, 0);
        assert_eq!(mc.input_tokens, 0);
        assert_eq!(mc.output_tokens, 0);
        assert_eq!(mc.call_count, 0);
        assert_eq!(mc.cost, 0.0);
    }

    #[test]
    fn model_cost_serializes_cache_fields() {
        let mc = ModelCost {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 200,
            cache_write_tokens: 100,
            cost: 0.01,
            call_count: 1,
        };
        let json = serde_json::to_string(&mc).expect("serialize");
        assert!(json.contains("\"cache_read_tokens\":200"), "got {json}");
        assert!(json.contains("\"cache_write_tokens\":100"), "got {json}");
    }
}
