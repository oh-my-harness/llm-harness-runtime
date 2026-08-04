# Issue #104 Session Recall Projection Plan

Updated: 2026-08-03 +08:00
Issue: [#104 — Add a runtime-owned cross-session recall projection for SessionRepo](https://github.com/oh-my-harness/llm-harness-runtime/issues/104)

Implementation status: PR1–PR4 are implemented and pushed on
`codex/issue-104-session-recall-metadata`. The implementation includes recall
authority metadata, reference-only mutation observation, typed turn refs, an
atomic rebuildable lexical index, active-branch projection, per-hit current
authorization, a reference-only Knowledge projection policy, and an explicit
ephemeral History Recall context plugin.

The default backend is dependency-free and in-memory. An optional SQLite/FTS5
backend now provides transactional persistence, restart reconciliation, schema
versioning, durable tombstones, and atomic rebuilds without changing the
authority, authorization, reference, or plugin contracts delivered here.

## Objective

Add opt-in cross-session history recall without creating a second authority for
conversation bodies. Runtime Sessions remain authoritative; every index is a
derived, disposable projection that can be rebuilt from `SessionRepo` and
`SessionStorage`.

The feature must have near-zero runtime, dependency, storage, and privacy cost
when it is not enabled.

## Product Boundary

- Saved Memory and History Recall remain separate products and data flows.
- History Recall searches only explicitly recallable durable Sessions.
- Search produces stable Session/entry references and lightweight snippets.
- Exact content is read from current Session authority after authorization is
  checked again.
- Retrieved conversation bodies are ephemeral context contributions by
  default. They are not copied into Memory, Session entries, tool projections,
  tracing fields, or error messages.
- Applications may opt into a different persistence policy explicitly, but the
  runtime default is fail-closed and non-persistent.

## Non-Goals

- Replacing `SessionRepo` with a document database.
- Making every `SessionRepo` implementation provide search.
- Enabling recall for existing or new Sessions by default.
- Indexing model reasoning, tool arguments, tool results, binary blocks, or
  arbitrary custom entries in the first implementation.
- Shipping embeddings or a vector database in the core runtime.
- Treating an index hit or opaque reference as authorization.
- Solving Saved Memory promotion or mutation in this feature.

## Hard Invariants

1. `SessionRepo`/`SessionStorage` are the sole authority for Session metadata,
   branch topology, entries, and exact content.
2. An index may generate candidates, but cannot authorize a hit or satisfy an
   exact read.
3. Search and read both fail closed using the current access context.
4. A deleted, archived, temporary, recall-disabled, or newly unauthorized
   Session is not observable after the authoritative mutation succeeds, even
   if physical index cleanup is delayed.
5. The complete projection can be rebuilt from Session authority.
6. No index, background task, optional search dependency, or Session write
   interception exists unless recall is explicitly installed.
7. Recall context is bounded by result, byte, and token limits and observes the
   current run cancellation token.
8. Search/read persistence records references and sanitized accounting only;
   conversation snippets and bodies are ephemeral unless an explicit policy
   permits persistence.

## Proposed Architecture

```text
SessionRepo / SessionStorage (authority)
        |
        | opt-in wrapper observes successful mutations
        v
ObservedSessionRepo / ObservedSessionStorage
        |
        | stable reference-only change notifications
        v
SessionRecallProjector ---> SessionRecallIndex (derived, rebuildable)
        |                           |
        | exact authoritative read  | candidate search
        +-------------+-------------+
                      v
             SessionRecallService
                      |
          +-----------+-----------+
          |                       |
          v                       v
SessionRecallKnowledgeSource  HistoryRecallPlugin
                                  |
                                  v
                         ephemeral context layer
```

### Crate Boundaries

Keep search dependencies out of the core Session crates.

```text
llm-harness-agent
  Session authority and backward-compatible recall metadata only

llm-harness-runtime-session-recall
  typed recall contracts, opt-in repo/storage wrappers, projector,
  service, KnowledgeSource adapter, and context plugin

llm-harness-runtime-session-recall-local (follow-up implementation crate)
  local persistent full-text index for JsonlSessionRepo deployments
```

An embedding implementation may be added later as another optional crate. The
core recall contract must not expose lexical- or vector-specific types.

## Session Authority Changes

### Recall Metadata

Extend `SessionMetadata` and `CreateSessionOptions` with backward-compatible
types whose serde defaults disable recall:

```rust
pub enum SessionDurability {
    Temporary,
    Durable,
}

pub enum SessionLifecycle {
    Active,
    Archived,
}

pub enum SessionRecallPolicy {
    Disabled,
    Enabled,
}

pub struct SessionRecallScope {
    pub namespace: String,
    pub tenant: Option<String>,
    pub project: Option<String>,
    pub attributes: BTreeMap<String, String>,
}
```

`SessionMetadata` also needs a monotonic recall-visible revision or generation.
It advances when recallability, lifecycle, scope, branch visibility, or indexed
content changes.

Compatibility requirements:

- legacy metadata deserializes as recall-disabled;
- in-memory and JSONL repos use identical defaults;
- temporary Sessions cannot be made recallable accidentally;
- metadata updates are authoritative before projection work is scheduled.

### Mutation Observation

Do not add mandatory indexing methods to `SessionRepo`. Provide opt-in
`ObservedSessionRepo` and wrapped `SessionStorage` values that observe only
successful mutations:

- create and fork;
- append and delete entries;
- active cursor/visible branch changes;
- archive, recall-policy, and authorization-scope changes;
- Session deletion.

Notifications contain stable IDs and revisions, not conversation bodies. The
projector reopens the authoritative Session to extract permitted text.

Missed notifications are tolerated because startup reconciliation and full
rebuild compare authoritative revisions with projected revisions.

## Recall Model

### Typed Reference

Preserve Session semantics before adapting to the source-neutral knowledge
protocol:

```rust
pub struct SessionRecallRef {
    pub session_id: String,
    pub user_entry_id: EntryId,
    pub assistant_entry_id: Option<EntryId>,
    pub branch_leaf_id: EntryId,
    pub revision: u64,
}
```

The knowledge adapter encodes this as an opaque `KnowledgeRef` and includes
sanitized Session/turn provenance in hit metadata. Code outside the recall
crate does not parse `KnowledgeRef.item_id` directly.

### Search Unit

The first implementation indexes one completed conversational turn:

- one user message;
- the following final assistant message on the selected branch, when present.

Default exclusions:

- thinking/reasoning content;
- tool arguments and tool results;
- binary blocks;
- custom entries;
- compaction and branch summaries as independent hits;
- the Session currently answering the request.

The initial branch policy is active-branch-only. Branch switching increments
the authoritative recall revision and reprojections that Session. Supporting
all named branches is a future explicit policy, not an implicit expansion.

## Search and Read Flow

### Search

1. Require an authenticated `KnowledgeAccessContext`, a trusted
   `SessionRecallAccessContext`, and an enabled runtime History Recall policy.
2. Use the dedicated trusted recall scope as the index partition/filter before
   the query executes. It is intentionally independent of Knowledge routing
   scope and attributes.
3. Search for bounded candidates.
4. For every candidate, reopen current Session metadata and verify existence,
   durability, lifecycle, recall policy, scope, authorization version, branch,
   entry IDs, and revision.
5. Drop stale or unauthorized candidates before returning snippets.
6. Return stable references and bounded snippets only.

The common Knowledge registry currently authorizes search at source level, so
the Session source remains responsible for per-hit filtering. Contract tests
must prove that this does not become an authorization oracle.

### Exact Read

1. Decode the typed Session reference.
2. Authorize the current reference as an item.
3. Open the current Session through `SessionRepo`.
4. Verify metadata, branch membership, entries, and revision again.
5. Read and format current authoritative content subject to byte/token limits.
6. Return `Unauthorized`, `NotFound`, or `StaleReference` without exposing
   hidden Session existence through diagnostics.

## Projection and Invalidation

`SessionRecallIndex` is backend-neutral and stores only derived data:

```rust
pub trait SessionRecallIndex: Send + Sync {
    fn search(...);
    fn replace_session(...);
    fn remove_session(...);
    fn projected_revision(...);
    fn reset(...);
}
```

Required behavior:

- writes are idempotent by Session ID and authoritative recall revision;
- a newer revision cannot be replaced by an older projector task;
- deletion/privacy mutations install an immediate logical tombstone or deny
  generation before returning success;
- physical text/vector cleanup may be asynchronous, but stale material is
  never emitted;
- rebuild writes into a fresh generation and switches atomically after
  validation;
- cancellation leaves the previous valid generation intact.

The local backend should start with lexical full-text search. Embeddings are a
separate later optimization after authorization and invalidation tests pass.

## Context Integration

History Recall is opt-in through a plugin/configuration object. It must not use
`BeforeRunHook.additional_messages`, because initial messages are persisted to
the current Session.

Use `TransformContextHook`, or introduce a narrowly scoped ephemeral context
contribution hook if transform-hook idempotence becomes fragile. The hook:

- runs only when History Recall is enabled for the current request;
- searches using the current user prompt;
- exact-reads selected hits;
- deduplicates by Session/turn reference;
- excludes the current Session;
- adds provenance-bearing ephemeral blocks;
- caches selected references in run-local state for deterministic later turns;
- traces IDs, counts, durations, truncation, and token usage, never bodies.

```rust
pub struct SessionRecallBudget {
    pub max_hits: usize,
    pub max_bytes_per_hit: usize,
    pub max_total_bytes: usize,
    pub max_tokens_per_hit: usize,
    pub max_total_tokens: usize,
}
```

Search tool projection for this source must be ephemeral or reference-only.
The existing generic Knowledge search behavior persists snippets, so the
recall adapter/plugin must not inherit that behavior without a projection
policy change.

## Delivery Plan

### PR1 — Session Recall Metadata and Mutation Boundary

- Add durability, lifecycle, recall policy, scope, and recall revision types.
- Add backward-compatible JSONL serialization and metadata mutation APIs.
- Add opt-in repo/storage wrappers and reference-only mutation events.
- Cover old metadata, temporary exclusion, archive/policy changes, branch
  revisions, and no-wrapper zero-cost behavior.

Exit condition: Session authority can express and observe all lifecycle events
needed by recall without adding a search dependency.

### PR2 — Recall Contracts and Rebuildable Projection

- Add typed references, request/result/error types, projector, index trait, and
  in-memory test index.
- Define active-branch turn extraction and default content exclusions.
- Implement incremental projection, reconciliation, tombstones, and full
  rebuild with atomic generation cutover.
- Add deterministic ordering and stale-task rejection tests.

Exit condition: a derived projection can be built, updated, removed, and fully
reconstructed from Session authority.

### PR3 — Knowledge Adapter and Authorization

- Implement `SessionRecallKnowledgeSource`.
- Map typed references to `KnowledgeRef` without flattening provenance.
- Enforce query partitioning, per-hit current authorization, exact authoritative
  reads, stale-reference semantics, and cancellation.
- Add a source projection policy so snippets/bodies are not persisted.
- Run reusable Knowledge source contract tests plus Session-specific races.

Exit condition: authorized callers can search/read through runtime contracts
without leaking stale or cross-scope results.

### PR4 — Opt-In History Recall Context Plugin

- Add plugin/configuration and explicit per-request enablement.
- Inject exact-read results through an ephemeral context boundary.
- Enforce result, byte, and token budgets.
- Add current-Session exclusion, deduplication, tracing, and bounded latency.
- Validate Folumi integration without a product-owned conversation copy.

Exit condition: Folumi can enable History Recall while default runtime users
retain existing behavior and cost.

### Follow-Up — Local Persistent FTS Backend

- Implemented as an opt-in SQLite/FTS5 feature without adding it to default
  dependencies.
- Added WAL crash recovery, schema versioning, atomic rebuild, durable deletion
  tombstones, multilingual search, and restart reconciliation tests.
- Added a reproducible, dependency-free benchmark for bulk indexing latency,
  SQLite/WAL bytes, search p50/p95/p99, and reopen revision scanning. Deployment
  owners should record baselines with representative Session sets and hardware.
- Local Windows release-profile baseline (2026-08-03; 1,000 Sessions / 5,000
  turns / 200 searches): 386 ms bulk indexing, 5,525,504 SQLite/WAL bytes,
  18,429 us p50, 21,452 us p95, 24,362 us p99, and 3 ms to reopen plus scan
  1,000 projected revisions. These measurements are informational, not CI
  thresholds.

## Acceptance Test Matrix

### Authority and Reference

- stable turn reference round-trip;
- exact read returns current authoritative content;
- deleted entry/session returns sanitized stale/not-found behavior;
- old revision cannot read changed branch content;
- rebuild produces equivalent references and results.

### Privacy and Authorization

- user, tenant, project, namespace, and delegated-scope isolation;
- missing access context fails closed;
- temporary, archived, and recall-disabled Sessions never produce hits;
- authorization revoked between search and read is denied;
- deletion/privacy change racing with search emits no stale snippet;
- errors, tracing, and persisted projections contain no conversation body.

### Projection Lifecycle

- append, delete, fork, branch switch, archive, policy change, and Session
  deletion invalidate correctly;
- out-of-order projector tasks cannot restore stale content;
- cancellation preserves the previous valid generation;
- startup reconciliation repairs a missed mutation notification;
- full rebuild can start from an empty index.

### Context Integration

- feature disabled means no index, task, wrapper, search, or context change;
- current Session is excluded;
- recall context is ephemeral and absent from Session entries;
- byte and token limits are enforced before provider invocation;
- abort cancels search/read promptly;
- duplicate references are injected once;
- malformed or unavailable hits degrade safely according to configured strictness.

## Compatibility and Cost Gates

- No new search dependency in `llm-harness-agent` or the base runtime.
- No required method added to third-party `SessionRepo` implementations solely
  for search.
- Legacy Session metadata remains readable and recall-disabled.
- Existing Session write behavior is unchanged when no indexed wrapper is used.
- No background runtime or filesystem artifact is created until the plugin is
  explicitly installed.
- Workspace tests cover both enabled and disabled builds.

## Resolved Decisions for PR1

1. The first release indexes the active branch only.
2. `SessionRecallPolicy::Disabled` is the default for legacy and new Sessions.
3. Trusted namespace/tenant/project scope is stored in Session metadata;
   principals and credentials are never stored there.
4. Archive is a core Session lifecycle field.
5. Internal provenance is mandatory. User-visible reference presentation is
   configurable by the embedding application.
