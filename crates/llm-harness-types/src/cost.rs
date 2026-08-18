use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::TokenUsage;

/// Stable identity for the canonical token-accounting semantics.
pub const BILLING_USAGE_PROFILE_ID: &str = "llm-harness-billing-usage";
/// Version of the canonical token-accounting semantics.
pub const BILLING_USAGE_PROFILE_VERSION: &str = "1";
/// Canonical, digest-bound definition of each usage category.
pub const BILLING_USAGE_PROFILE_CONTRACT: &str = "input_tokens=gross_input;cache_read_tokens=input_subset;cache_creation_input_tokens=input_subset;cache_read_and_creation=disjoint;output_tokens=gross_output;reasoning_tokens=output_subset;uncached_input=input_tokens-cache_read_tokens-cache_creation_input_tokens";
/// SHA-256 digest of [`BILLING_USAGE_PROFILE_CONTRACT`].
pub const BILLING_USAGE_PROFILE_DIGEST: &str =
    "sha256:6afb4df926d32f3917ab28fb7ea30af87b20d7b81391d800c076f637c7eb73fb";
/// Stable identity for provider usage-report provenance semantics.
pub const USAGE_REPORT_PROFILE_ID: &str = "llm-harness-usage-report";
/// Version of the provider usage-report provenance semantics.
pub const USAGE_REPORT_PROFILE_VERSION: &str = "2";
/// Canonical, digest-bound definition of usage-report provenance.
pub const USAGE_REPORT_PROFILE_CONTRACT: &str = "provenance=provider_payload;states=reported_valid,missing,reported_invalid;zero_tokens_do_not_imply_missing;reported_valid_requires_billing_invariants;parsed_usage_normalization_failure=reported_invalid;terminal=first_message_stop";
/// SHA-256 digest of [`USAGE_REPORT_PROFILE_CONTRACT`].
pub const USAGE_REPORT_PROFILE_DIGEST: &str =
    "sha256:9d9e1d63b51fb44ef06383f1b7e92729404b7729357c25aef2d4d15bfdc288c5";

/// Compact runtime marker for the accounting semantics used by an aggregate.
///
/// The full ID, version, and digest are exposed by the accessors and are bound
/// separately into persisted trusted-pricing admissions.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BillingUsageProfile {
    /// Canonical gross-input, mutually exclusive cache-subset profile.
    V1,
}

impl BillingUsageProfile {
    /// Stable profile ID.
    pub fn id(&self) -> &str {
        BILLING_USAGE_PROFILE_ID
    }

    /// Stable profile version.
    pub fn version(&self) -> &str {
        BILLING_USAGE_PROFILE_VERSION
    }

    /// Digest of the canonical profile contract.
    pub fn digest(&self) -> &str {
        BILLING_USAGE_PROFILE_DIGEST
    }

    /// Returns whether this identity is the runtime's canonical profile.
    pub fn is_canonical(&self) -> bool {
        matches!(self, Self::V1)
    }
}

/// Returns the verifier-owned canonical token-accounting profile.
pub fn canonical_billing_usage_profile() -> BillingUsageProfile {
    BillingUsageProfile::V1
}

/// Compact runtime marker for provider usage-report provenance semantics.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum UsageReportProfile {
    /// Legacy provider-payload provenance contract.
    V1,
    /// First-terminal provider-payload provenance contract.
    V2,
}

impl UsageReportProfile {
    /// Stable profile ID.
    pub fn id(&self) -> &str {
        USAGE_REPORT_PROFILE_ID
    }

    /// Stable profile version.
    pub fn version(&self) -> &str {
        match self {
            Self::V1 => "1",
            Self::V2 => USAGE_REPORT_PROFILE_VERSION,
        }
    }

    /// Digest of the canonical provenance contract.
    pub fn digest(&self) -> &str {
        match self {
            Self::V1 => "sha256:f6a776c595f99114eb692faf4f72eb4eb0012a33fa3722244e65fad0d6f48073",
            Self::V2 => USAGE_REPORT_PROFILE_DIGEST,
        }
    }

    /// Returns whether this identity is runtime's canonical profile.
    pub fn is_canonical(&self) -> bool {
        matches!(self, Self::V2)
    }
}

/// Returns the verifier-owned canonical usage-report profile.
pub fn canonical_usage_report_profile() -> UsageReportProfile {
    UsageReportProfile::V2
}

/// Provenance and validity of one provider usage report.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UsageReportProvenance {
    /// The response completed without authoritative provider usage.
    #[default]
    Missing,
    /// The provider supplied usage and canonical invariants hold.
    ReportedValid,
    /// The provider supplied usage but an invariant failed.
    ReportedInvalid,
}

/// A provider usage payload violates the canonical accounting profile.
#[derive(Debug, Clone, Eq, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct BillingUsageProfileError {
    message: String,
}

impl BillingUsageProfileError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// A provider usage-report aggregate violates the provenance contract.
#[derive(Debug, Clone, Eq, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct UsageReportProfileError {
    message: String,
}

impl UsageReportProfileError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl TokenUsage {
    /// Gross input plus gross output, without double-counting subsets.
    pub fn total_tokens(&self) -> u32 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Prompt tokens billed at the normal, non-cache input rate.
    pub fn billable_uncached_input_tokens(&self) -> Result<u32, BillingUsageProfileError> {
        let cached = self
            .cache_read_tokens
            .checked_add(self.cache_creation_tokens)
            .ok_or_else(|| {
                BillingUsageProfileError::new("cache input token categories overflow")
            })?;
        self.input_tokens.checked_sub(cached).ok_or_else(|| {
            BillingUsageProfileError::new(format!(
                "gross input tokens ({}) are smaller than cache read plus cache creation tokens ({cached})",
                self.input_tokens
            ))
        })
    }

    /// Validates the canonical gross-plus-exclusive-subsets profile.
    pub fn validate_billing_profile(&self) -> Result<(), BillingUsageProfileError> {
        self.billable_uncached_input_tokens()?;
        if self.reasoning_tokens > self.output_tokens {
            return Err(BillingUsageProfileError::new(format!(
                "reasoning tokens ({}) exceed gross output tokens ({})",
                self.reasoning_tokens, self.output_tokens
            )));
        }
        Ok(())
    }

    /// Requires an authoritative, numerically valid provider report.
    pub fn validate_report(&self) -> Result<(), BillingUsageProfileError> {
        match self.provenance {
            UsageReportProvenance::ReportedValid => self.validate_billing_profile(),
            UsageReportProvenance::Missing => Err(BillingUsageProfileError::new(
                "provider usage report is missing",
            )),
            UsageReportProvenance::ReportedInvalid => Err(BillingUsageProfileError::new(
                "provider usage report is invalid",
            )),
        }
    }
}

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
    /// Total reasoning/thinking tokens.
    pub reasoning_tokens: u64,
    /// Estimated total cost in USD.
    pub cost: f64,
    /// Number of LLM calls.
    pub call_count: u64,
}

/// Completeness and validity evidence for completed provider responses.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageReportAggregate {
    /// Versioned provenance contract used to interpret these counters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<UsageReportProfile>,
    /// Completed provider responses observed at the accounting boundary.
    pub provider_calls: u64,
    /// Responses carrying an authoritative, valid provider usage payload.
    pub reported_valid: u64,
    /// Responses that completed without a provider usage payload.
    pub missing: u64,
    /// Responses carrying a provider usage payload that failed validation.
    pub reported_invalid: u64,
}

impl UsageReportAggregate {
    /// Records exactly one completed provider response.
    pub fn record(&mut self, provenance: UsageReportProvenance) {
        if self.profile.is_none() {
            self.profile = Some(canonical_usage_report_profile());
        }
        self.provider_calls += 1;
        match provenance {
            UsageReportProvenance::ReportedValid => self.reported_valid += 1,
            UsageReportProvenance::Missing => self.missing += 1,
            UsageReportProvenance::ReportedInvalid => self.reported_invalid += 1,
        }
    }

    /// Merges independently accumulated response evidence without inventing a
    /// profile when non-empty inputs disagree.
    pub fn merge(&mut self, incoming: &Self) {
        if incoming.provider_calls != 0 {
            if self.provider_calls == 0 {
                self.profile = incoming.profile;
            } else if self.profile != incoming.profile {
                self.profile = None;
            }
        }
        self.provider_calls += incoming.provider_calls;
        self.reported_valid += incoming.reported_valid;
        self.missing += incoming.missing;
        self.reported_invalid += incoming.reported_invalid;
    }

    /// Calls whose cost cannot be trusted because usage is absent or invalid.
    pub fn unknown_cost_calls(&self) -> u64 {
        self.missing.saturating_add(self.reported_invalid)
    }

    /// Validates only the profile and mutually exclusive status counters.
    pub fn validate_accounting(&self) -> Result<(), UsageReportProfileError> {
        let classified = self
            .reported_valid
            .checked_add(self.missing)
            .and_then(|count| count.checked_add(self.reported_invalid))
            .ok_or_else(|| UsageReportProfileError::new("usage report counters overflow"))?;
        if classified != self.provider_calls {
            return Err(UsageReportProfileError::new(format!(
                "provider calls ({}) do not equal classified usage reports ({classified})",
                self.provider_calls
            )));
        }
        if self.provider_calls == 0 {
            if self.profile.is_some() {
                return Err(UsageReportProfileError::new(
                    "usage report profile is present without a provider call",
                ));
            }
            return Ok(());
        }
        if self.profile.as_ref() != Some(&canonical_usage_report_profile()) {
            return Err(UsageReportProfileError::new(
                "usage reports are not bound to the canonical provenance profile",
            ));
        }
        Ok(())
    }

    /// Requires at least one response and one valid report for every response.
    pub fn require_all_reported_valid(&self) -> Result<(), UsageReportProfileError> {
        self.validate_accounting()?;
        if self.provider_calls == 0 {
            return Err(UsageReportProfileError::new(
                "no completed provider call was recorded",
            ));
        }
        if self.missing != 0 || self.reported_invalid != 0 {
            return Err(UsageReportProfileError::new(format!(
                "usage reports are incomplete: {} missing, {} invalid, {} provider calls",
                self.missing, self.reported_invalid, self.provider_calls
            )));
        }
        if self.reported_valid != self.provider_calls {
            return Err(UsageReportProfileError::new(
                "not every provider call has one valid usage report",
            ));
        }
        Ok(())
    }
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
    /// Total reasoning/thinking tokens.
    pub total_reasoning_tokens: u64,
    /// Total estimated cost in USD.
    pub total_cost: f64,
    /// Accounting profile used to interpret the token counters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing_usage_profile: Option<BillingUsageProfile>,
    /// Number of provider payloads that violated the accounting profile.
    #[serde(default)]
    pub billing_usage_violations: u64,
    /// Provider usage-report completeness and validity evidence.
    #[serde(default)]
    pub usage_reports: UsageReportAggregate,
    /// Per-model breakdown.
    pub by_model: HashMap<String, ModelCost>,
}

impl CostAggregate {
    /// Prompt tokens billed at the normal, non-cache input rate.
    pub fn billable_uncached_input_tokens(&self) -> Result<u64, BillingUsageProfileError> {
        billable_uncached_input_tokens(
            self.total_input_tokens,
            self.total_cache_read_tokens,
            self.total_cache_write_tokens,
        )
    }

    /// Validates this aggregate against the canonical accounting profile.
    pub fn validate_billing_profile(&self) -> Result<(), BillingUsageProfileError> {
        if self.billing_usage_violations != 0 {
            return Err(BillingUsageProfileError::new(format!(
                "usage contains {} billing profile violation(s)",
                self.billing_usage_violations
            )));
        }
        if self.billing_usage_profile.as_ref() != Some(&canonical_billing_usage_profile()) {
            return Err(BillingUsageProfileError::new(
                "usage is not bound to the canonical billing profile",
            ));
        }
        self.billable_uncached_input_tokens()?;
        if self.total_reasoning_tokens > self.total_output_tokens {
            return Err(BillingUsageProfileError::new(format!(
                "reasoning tokens ({}) exceed gross output tokens ({})",
                self.total_reasoning_tokens, self.total_output_tokens
            )));
        }
        Ok(())
    }
}

/// Calculates one provider call using mutually exclusive billing categories.
pub fn calculate_token_usage_cost(
    usage: &TokenUsage,
    price: &TokenPrice,
) -> Result<f64, BillingUsageProfileError> {
    usage.validate_billing_profile()?;
    calculate_cost(
        u64::from(usage.input_tokens),
        u64::from(usage.output_tokens),
        u64::from(usage.cache_read_tokens),
        u64::from(usage.cache_creation_tokens),
        price,
    )
}

/// Recalculates aggregate cost under the canonical accounting profile.
pub fn calculate_aggregate_cost(
    usage: &CostAggregate,
    price: &TokenPrice,
) -> Result<f64, BillingUsageProfileError> {
    usage.validate_billing_profile()?;
    calculate_cost(
        usage.total_input_tokens,
        usage.total_output_tokens,
        usage.total_cache_read_tokens,
        usage.total_cache_write_tokens,
        price,
    )
}

fn calculate_cost(
    gross_input: u64,
    gross_output: u64,
    cache_read: u64,
    cache_write: u64,
    price: &TokenPrice,
) -> Result<f64, BillingUsageProfileError> {
    let uncached_input = billable_uncached_input_tokens(gross_input, cache_read, cache_write)?;
    Ok((uncached_input as f64 / 1_000_000.0) * price.input_per_mtok
        + (gross_output as f64 / 1_000_000.0) * price.output_per_mtok
        + (cache_read as f64 / 1_000_000.0) * price.cache_read_per_mtok
        + (cache_write as f64 / 1_000_000.0) * price.cache_write_per_mtok)
}

fn billable_uncached_input_tokens(
    gross_input: u64,
    cache_read: u64,
    cache_write: u64,
) -> Result<u64, BillingUsageProfileError> {
    let cached = cache_read
        .checked_add(cache_write)
        .ok_or_else(|| BillingUsageProfileError::new("cache input token categories overflow"))?;
    gross_input.checked_sub(cached).ok_or_else(|| {
        BillingUsageProfileError::new(format!(
            "gross input tokens ({gross_input}) are smaller than cache read plus cache creation tokens ({cached})"
        ))
    })
}
/// Business-layer hook for budget override decisions.
/// Called when the per-task budget limit is exceeded.
pub trait BudgetExceededHook: Send + Sync {
    /// Called when budget is exceeded. Return `true` to continue, `false` to
    /// transition the task to `Failed`.
    ///
    /// `cost` is the session-wide accumulated cost snapshot. The previous
    /// `task_cost` parameter was always equal to `session_cost` in pre-WorkflowEngine
    /// scenarios and has been removed; callers needing task-scoped cost should
    /// track it separately.
    fn on_budget_exceeded<'a>(
        &'a self,
        cost: &'a CostAggregate,
        budget_limit: f64,
    ) -> BoxFuture<'a, bool>;
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
        assert_eq!(mc.reasoning_tokens, 0);
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
            reasoning_tokens: 50,
            cost: 0.01,
            call_count: 1,
        };
        let json = serde_json::to_string(&mc).expect("serialize");
        assert!(json.contains("\"cache_read_tokens\":200"), "got {json}");
        assert!(json.contains("\"cache_write_tokens\":100"), "got {json}");
        assert!(json.contains("\"reasoning_tokens\":50"), "got {json}");
    }

    #[test]
    fn usage_report_aggregate_distinguishes_valid_missing_and_invalid_calls() {
        let mut reports = UsageReportAggregate::default();
        reports.record(UsageReportProvenance::ReportedValid);
        assert_eq!(reports.provider_calls, 1);
        assert_eq!(reports.reported_valid, 1);
        assert_eq!(reports.unknown_cost_calls(), 0);
        reports
            .require_all_reported_valid()
            .expect("one valid report is complete evidence");

        reports.record(UsageReportProvenance::Missing);
        reports.record(UsageReportProvenance::ReportedInvalid);
        assert_eq!(reports.provider_calls, 3);
        assert_eq!(reports.reported_valid, 1);
        assert_eq!(reports.missing, 1);
        assert_eq!(reports.reported_invalid, 1);
        assert_eq!(reports.unknown_cost_calls(), 2);
        assert!(reports.require_all_reported_valid().is_err());
    }

    #[test]
    fn usage_report_aggregate_rejects_missing_profile_and_counter_mismatch() {
        let missing_profile = UsageReportAggregate {
            provider_calls: 1,
            reported_valid: 1,
            ..Default::default()
        };
        assert!(missing_profile.validate_accounting().is_err());

        let counter_mismatch = UsageReportAggregate {
            profile: Some(canonical_usage_report_profile()),
            provider_calls: 2,
            reported_valid: 1,
            ..Default::default()
        };
        assert!(counter_mismatch.validate_accounting().is_err());
    }

    #[test]
    fn usage_report_aggregate_merge_preserves_exact_call_classification() {
        let mut valid = UsageReportAggregate::default();
        valid.record(UsageReportProvenance::ReportedValid);
        let mut missing = UsageReportAggregate::default();
        missing.record(UsageReportProvenance::Missing);

        valid.merge(&missing);

        assert_eq!(valid.provider_calls, 2);
        assert_eq!(valid.reported_valid, 1);
        assert_eq!(valid.missing, 1);
        assert_eq!(valid.unknown_cost_calls(), 1);
        valid
            .validate_accounting()
            .expect("matching canonical profiles merge without losing evidence");
        assert!(valid.require_all_reported_valid().is_err());
    }

    #[test]
    fn profile_digests_match_their_contract_text() {
        use sha2::{Digest, Sha256};
        let billing = format!(
            "sha256:{:x}",
            Sha256::digest(BILLING_USAGE_PROFILE_CONTRACT.as_bytes())
        );
        let provenance = format!(
            "sha256:{:x}",
            Sha256::digest(USAGE_REPORT_PROFILE_CONTRACT.as_bytes())
        );

        assert_eq!(billing, BILLING_USAGE_PROFILE_DIGEST);
        assert_eq!(provenance, USAGE_REPORT_PROFILE_DIGEST);
    }

    #[test]
    fn token_usage_total() {
        let u = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 20,
            cache_creation_tokens: 10,
            reasoning_tokens: 20,
            provenance: UsageReportProvenance::ReportedValid,
        };
        assert_eq!(u.total_tokens(), 150);
        assert_eq!(u.billable_uncached_input_tokens().unwrap(), 70);
        u.validate_billing_profile().unwrap();
    }

    #[test]
    fn token_usage_rejects_invalid_billing_subsets() {
        let cache_error = TokenUsage {
            input_tokens: 100,
            cache_read_tokens: 80,
            cache_creation_tokens: 30,
            ..Default::default()
        }
        .validate_billing_profile()
        .unwrap_err();
        assert!(cache_error.to_string().contains("smaller than cache read"));

        let reasoning_error = TokenUsage {
            output_tokens: 20,
            reasoning_tokens: 21,
            ..Default::default()
        }
        .validate_billing_profile()
        .unwrap_err();
        assert!(reasoning_error.to_string().contains("exceed gross output"));
    }

    #[test]
    fn token_usage_report_provenance_is_independent_of_zero_values() {
        let missing = TokenUsage::default();
        assert!(missing.validate_report().is_err());

        let reported_zero = TokenUsage {
            provenance: UsageReportProvenance::ReportedValid,
            ..Default::default()
        };
        reported_zero
            .validate_report()
            .expect("an explicit provider zero payload is valid evidence");

        let reported_invalid = TokenUsage {
            provenance: UsageReportProvenance::ReportedInvalid,
            ..Default::default()
        };
        assert!(reported_invalid.validate_report().is_err());
    }
}
