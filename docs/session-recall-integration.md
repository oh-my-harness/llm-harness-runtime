# Session Recall integration

`llm-harness-runtime-session-recall` provides opt-in History Recall over durable
runtime Sessions. `SessionRepo` remains the only authority for conversation
bodies; the recall index is a disposable candidate projection.

## Install the projection explicitly

Keep the authoritative repository separate from its observer wrapper. The
projector and recall service reopen the authoritative repository for every
projection and exact read.

```rust
use std::sync::Arc;

use llm_harness_agent::{ObservedSessionRepo, SessionMutationObserver, SessionRepo};
use llm_harness_runtime_knowledge::KnowledgeAccessControl;
use llm_harness_runtime_session_recall::{
    HistoryRecallPlugin, HistoryRecallPluginConfig, InMemorySessionRecallIndex,
    SessionRecallIndex, SessionRecallKnowledgeSource, SessionRecallProjector,
    SessionRecallService,
};

// Supply the application's JsonlSessionRepo or other authoritative repo.
let authority: Arc<dyn SessionRepo> = authoritative_repo;
let index: Arc<dyn SessionRecallIndex> = Arc::new(InMemorySessionRecallIndex::new());
let projector = Arc::new(SessionRecallProjector::new(
    authority.clone(),
    index.clone(),
));
let observer: Arc<dyn SessionMutationObserver> = projector.clone();
let sessions: Arc<dyn SessionRepo> = Arc::new(ObservedSessionRepo::new(
    authority.clone(),
    observer,
));

let service = Arc::new(SessionRecallService::new(
    authority,
    index,
    knowledge_access_control,
));
let source = Arc::new(
    SessionRecallKnowledgeSource::new(service).with_uri_mapper(|target| {
        Some(format!("chat:{}:{}", target.session_id, target.entry_id))
    }),
);
let history_recall = HistoryRecallPlugin::new(
    source.clone(),
    HistoryRecallPluginConfig::default(),
)?;
```

Register `source` in the common Knowledge registry when model-driven
`knowledge_search` / `knowledge_read` access is wanted. Register
`history_recall` as a Harness plugin (or call `register_hooks` on the Harness
hooks) for automatic context recall.

The in-memory index must be rebuilt at process startup:

```rust
projector.rebuild(shutdown.child_token()).await?;
```

`reconcile` repairs missed observer notifications by comparing authoritative
Session revisions with the projection. A later persistent FTS backend can use
it at startup instead of rebuilding every process-local generation.

## Mark only eligible Sessions recallable

Recall remains disabled by default. A Session must be durable, active, enabled,
and carry a trusted scope supplied by the application:

```rust
use llm_harness_agent::{SessionDurability, SessionRecallPolicy, SessionRecallScope};
use llm_harness_agent::session::CreateSessionOptions;

let storage = sessions
    .create(CreateSessionOptions {
        durability: Some(SessionDurability::Durable),
        recall_policy: SessionRecallPolicy::Enabled,
        recall_scope: Some(SessionRecallScope::new("folumi")),
        ..Default::default()
    })
    .await?;
```

The scope's namespace, tenant, project, and attributes must exactly match the
trusted `SessionRecallAccessContext` supplied for the run. Recall partitioning
is deliberately independent of `KnowledgeAccessContext.scope`, so unrelated
Knowledge routing attributes do not make existing conversation history
disappear. Principal and delegated access decisions are still made from
`KnowledgeAccessContext` by the configured `KnowledgeAuthorizer`; credentials
are never stored in Session metadata.

## Opt in for each run

Installing the plugin is not enough to trigger automatic recall. Each run must
carry the trusted Knowledge access context, a trusted Session Recall context,
and `HistoryRecallRequest`. The current Session ID belongs to the recall
context and is mandatory for the automatic plugin so it can be excluded from
cross-Session results.

```rust
use llm_harness_agent::SessionRecallScope;
use llm_harness_runtime_session_recall::{
    HistoryRecallRequest, SessionRecallAccessContext,
};
use llm_harness_types::RunRequest;

let request = RunRequest::from_text(user_prompt)
    .with_extension(knowledge_access_context)
    .with_extension(
        SessionRecallAccessContext::new(SessionRecallScope::new("folumi"))
            .with_current_session_id(current_session_id),
    )
    .with_extension(HistoryRecallRequest::new());

harness.run(request).await?;
```

Without `HistoryRecallRequest`, the automatic plugin leaves provider context
unchanged. Model-driven `knowledge_search` / `knowledge_read` can still use the
Session source when the run has both access contexts; they do not require the
automatic opt-in marker. Missing `SessionRecallAccessContext` always fails
closed. Recalled bodies are inserted only into the transformed provider
context: they are not appended to the current Session, copied into Saved
Memory, or persisted by generic Knowledge tool projections.

The runtime exposes a product-neutral structured navigation target containing
the Session and entry IDs. Configure `with_uri_mapper` to turn it into the
application's route. If no mapper is configured, `uri` remains absent while
the structured target is still present in result metadata.

## Operational behavior

- Only user text and the following final assistant text on the active branch
  are indexed. Thinking, tool calls/results, binary blocks, and custom entries
  are excluded.
- Every hit is reopened through `SessionRepo` and rechecked for current scope,
  authorization, lifecycle, branch, entries, and revision before it is emitted.
- Default recall is bounded to three hits, 4 KiB per hit, and 8 KiB total, with
  a 750 ms timeout. Applications can tighten these limits.
- The default failure mode ignores unavailable recall and continues the run.
  `HistoryRecallFailureMode::FailRun` is available for strict deployments.
- Do not expose the unwrapped authority to product write paths. Mutations that
  bypass `ObservedSessionRepo` remain safe at read time but require `reconcile`
  or `rebuild` before newly added content becomes searchable.

The persistent local FTS backend, crash recovery, and performance benchmarks
are intentionally a separate follow-up. They do not change this integration or
authority boundary.
