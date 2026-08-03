# Billing Usage Contract

Status: cache accounting accepted; usage provenance under review

Tracking:

- [runtime issue #102](https://github.com/oh-my-harness/llm-harness-runtime/issues/102) (cache categories and formula, closed)
- [runtime issue #103](https://github.com/oh-my-harness/llm-harness-runtime/issues/103) (provider usage provenance)

## Scope

This contract defines token accounting from `llm-api-adapter` provider payloads,
through runtime cost accumulation, to trusted multi-agent pricing admission. It
does not attest that a configured client reached a particular remote backend or
that a caller-supplied price table matches a provider's market price.

## Canonical Profile

The canonical identity is:

- ID: `llm-harness-billing-usage`
- version: `1`
- digest: `sha256:6afb4df926d32f3917ab28fb7ea30af87b20d7b81391d800c076f637c7eb73fb`

The digest binds this exact contract:

```text
input_tokens=gross_input;cache_read_tokens=input_subset;cache_creation_input_tokens=input_subset;cache_read_and_creation=disjoint;output_tokens=gross_output;reasoning_tokens=output_subset;uncached_input=input_tokens-cache_read_tokens-cache_creation_input_tokens
```

`input_tokens` and `output_tokens` remain gross totals for token budgets and
observability. The billable categories are mutually exclusive:

```text
uncached_input = input_tokens - cache_read_tokens - cache_creation_input_tokens
cost = uncached_input * input_rate
     + cache_read_tokens * cache_read_rate
     + cache_creation_input_tokens * cache_write_rate
     + output_tokens * output_rate
```

All rates are USD per million tokens. Reasoning tokens are informational and
are not charged again because they are a subset of gross output.

The following invariants are mandatory:

- `cache_read_tokens + cache_creation_input_tokens <= input_tokens`
- cache read and cache creation categories do not overlap
- `reasoning_tokens <= output_tokens`

Overflow or a violated relationship is an accounting failure. Runtime records
the completed response and its unknown-cost status, does not add the untrusted
payload to billable totals, does not query or apply pricing, records a profile
violation, and budget control fails closed.

## Provider Mapping

`llm-api-adapter` revision `193117cbb8398aadeca74a925cfd699617f8c861`
normalizes provider-shaped usage as follows:

| Provider | Gross input | Cache read subset | Cache write subset |
| --- | --- | --- | --- |
| OpenAI | `prompt_tokens` | `prompt_tokens_details.cached_tokens` | `0` |
| DeepSeek | `prompt_tokens` | `prompt_cache_hit_tokens` | `0` |
| Anthropic | `input_tokens + cache_read_input_tokens + cache_creation_input_tokens` | `cache_read_input_tokens` | `cache_creation_input_tokens` |

For DeepSeek payloads that provide both hit and miss counters, the adapter also
requires `hit + miss == prompt_tokens`. Provider-shaped fixtures cover all three
mappings. The adapter and runtime compile-time tests require an exact match of
profile ID, version, digest, and contract text.

If a provider usage object was parsed but gross/subset normalization overflows,
the response still produces one `ReportedInvalid` completed-call report. Its
numeric totals are not trusted or added to runtime accounting.

## Usage Report Provenance

Token values do not prove that a provider supplied a usage payload. An explicit
provider payload containing all zeros is different from a response for which
usage is absent. This evidence uses an independent profile so that changes to
payload completeness do not silently change the accepted cache-billing
semantics.

The canonical identity is:

- ID: `llm-harness-usage-report`
- version: `2`
- digest: `sha256:9d9e1d63b51fb44ef06383f1b7e92729404b7729357c25aef2d4d15bfdc288c5`

The digest binds this exact contract:

```text
provenance=provider_payload;states=reported_valid,missing,reported_invalid;zero_tokens_do_not_imply_missing;reported_valid_requires_billing_invariants;parsed_usage_normalization_failure=reported_invalid;terminal=first_message_stop
```

The first `MessageStop` is the unique response terminal. Consumers stop polling
the transport immediately, ignore any later terminal, and account the first
terminal exactly once. A completed terminal cannot be delayed until SSE EOF.

Every completed provider response observed by runtime's after-response
accounting boundary has exactly one status:

- `ReportedValid`: the provider supplied usage and the billing invariants hold.
- `Missing`: the response completed without an authoritative usage payload.
- `ReportedInvalid`: the provider supplied usage, but the payload violates a
  canonical or provider-specific invariant.

This call count does not include transport attempts that fail before a
completed provider response exists. Those attempts already fail the model run
and are not represented as zero-cost completions. Ordinary, non-priced clients
may inspect or ignore missing provenance for compatibility. Runtime budget
control and trusted multi-agent pricing must fail closed on either `Missing` or
`ReportedInvalid`.

## Runtime Evidence

`CostAggregate.billing_usage_profile` identifies the interpretation used for
the counters. `billing_usage_violations` is zero only when every observed
provider payload satisfied the profile. `calculate_token_usage_cost` and
`calculate_aggregate_cost` are the shared formula implementations; upper layers
must not reproduce the formula.

`CostAggregate.usage_reports` separately records completed responses, valid
reports, missing reports, and invalid reports under the canonical provenance
profile. A response with explicit, valid zero usage increments both
`provider_calls` and `reported_valid`; it can therefore produce trusted zero
cost. A missing or invalid report increments `provider_calls` and
`unknown_cost_calls()`, does not create priced per-model evidence, and forces
budget control to stop even in surveillance mode.

Callers that need evidence after timeout or cancellation retain a clone of the
runtime `UsageLedger`. The built-in accumulator writes to that caller-owned
ledger before general after-response hooks and before session persistence or
output extraction. Dropping the execution future therefore cannot erase a
provider call whose first terminal was already observed.

Legacy serialized aggregates remain decodable because both new fields have
serde defaults. An aggregate without a canonical profile is not trusted
pricing evidence, even if its numeric fields happen to be self-consistent.

## Multi-Agent Admission Boundary

A trusted pricing admission must bind all of the following before provider or
session creation:

- participant role and ID
- configured provider identity and resolved model
- all four token rates
- price-table ID, version, and digest
- billing profile ID, version, and digest
- usage-report profile ID, version, and digest

The same immutable admission must be applied to execution. Post-call validation
requires the canonical runtime profile, zero profile violations, exact
provider/model call evidence, one valid usage report per completed response,
and cost recomputation through the shared runtime function. A missing or
different accounting or provenance profile fails before execution. Missing or
invalid provider usage fails after the response while preserving that a call
completed and that its cost is unknown; it must not be represented as trusted
zero cost.

The multi-agent provider identity is bound to configured client selection. It
is not remote provider attestation. A future paired-controls manifest must
persist the exact admission, including the billing profile, instead of
regenerating it from aggregate usage.

## Revision Policy

Any semantic change to gross/subset relationships, overlap rules, or billable
categories requires a new billing profile version and digest. Any semantic
change to provenance states, completeness, or validity requires a new usage
report profile version and digest. Adapter, runtime, trusted admission, and
experiment manifests must move together. Changing either contract while
retaining its prior version is invalid.
