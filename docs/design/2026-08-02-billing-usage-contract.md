# Billing Usage Contract

Status: review candidate

Tracking: [runtime issue #102](https://github.com/oh-my-harness/llm-harness-runtime/issues/102)

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
the gross counters for observability, does not query or apply pricing, records a
profile violation, and budget control fails closed.

## Provider Mapping

`llm-api-adapter` revision `b4366a8aac3884103ced4c3e9052c86e93965614`
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

## Runtime Evidence

`CostAggregate.billing_usage_profile` identifies the interpretation used for
the counters. `billing_usage_violations` is zero only when every observed
provider payload satisfied the profile. `calculate_token_usage_cost` and
`calculate_aggregate_cost` are the shared formula implementations; upper layers
must not reproduce the formula.

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

The same immutable admission must be applied to execution. Post-call validation
requires the canonical runtime profile, zero profile violations, exact
provider/model call evidence, and cost recomputation through the shared runtime
function. A missing or different accounting profile fails before execution; an
invalid provider usage relationship fails after the response while preserving
gross usage evidence.

The multi-agent provider identity is bound to configured client selection. It
is not remote provider attestation. A future paired-controls manifest must
persist the exact admission, including the billing profile, instead of
regenerating it from aggregate usage.

## Revision Policy

Any semantic change to gross/subset relationships, overlap rules, or billable
categories requires a new profile version and digest. Adapter, runtime, trusted
admission, and experiment manifests must move together. Changing only the
formula while retaining profile version `1` is invalid.
