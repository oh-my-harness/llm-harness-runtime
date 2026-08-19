# Session Recall integration

`llm-harness-session-recall` provides opt-in History Recall over durable
runtime Sessions. `SessionRepo` remains the only authority for conversation
bodies; the recall index is a disposable candidate projection.

## Install the projection explicitly

Keep the authoritative repository separate from its observer wrapper. The
projector and recall service reopen the authoritative repository for every
projection and exact read.

```rust
use std::sync::Arc;

use llm_harness_agent::{ObservedSessionRepo, SessionMutationObserver, SessionRepo};
use llm_harness_knowledge::KnowledgeAccessControl;
use llm_harness_session_recall::{
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

The dependency-free in-memory index must be rebuilt at process startup:

```rust
projector.rebuild(shutdown.child_token()).await?;
```

`reconcile` repairs missed observer notifications by comparing authoritative
Session revisions with the projection.

For a durable local projection, enable the portable bundled SQLite backend:

```toml
[dependencies]
llm-harness-session-recall = { version = "0.3", features = ["sqlite-bundled"] }
```

Then open the derived FTS5 database and reconcile it at startup:

```rust
use llm_harness_session_recall::SqliteSessionRecallIndex;

let index: Arc<dyn SessionRecallIndex> = Arc::new(
    SqliteSessionRecallIndex::open(state_dir.join("session-recall.sqlite3"))?
);
let projector = Arc::new(SessionRecallProjector::new(
    authority.clone(),
    index.clone(),
));
projector.reconcile(shutdown.child_token()).await?;
```

Unchanged Session revisions are not reopened during that reconciliation. The
database uses transactions, WAL recovery, durable deletion tombstones, and an
atomically replaced rebuild generation. It remains disposable derived state:
after stopping writers, it may be deleted and regenerated from `SessionRepo`.
Use the `sqlite` feature instead when the target supplies a compatible SQLite
build with FTS5. With neither feature enabled, `rusqlite` and SQLite are absent
from the dependency graph and the existing in-memory behavior is unchanged.

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
use llm_harness_session_recall::{
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

Applications using the automatic plugin may explicitly observe the bounded
references that passed exact read, for example to render source-jump links:

```rust
use llm_harness_session_recall::HistoryRecallObservation;

let observation = HistoryRecallObservation::new();
let request = RunRequest::from_text(user_prompt)
    // Add the access, recall, and HistoryRecallRequest extensions shown above.
    .with_extension(observation.clone());

harness.run(request).await?;
let snapshot = observation.snapshot();
for accepted in snapshot.accepted_references {
    open_history_target(accepted.navigation_target, accepted.uri);
}
```

Create a fresh observation handle for each run. This extension is optional;
without it the plugin allocates no public observation state. Snapshots contain
only the run ID, opaque Knowledge reference, structured Session/entry target,
and optional application URI—never recalled conversation text. They are not
appended to the Session; an application must deliberately project them into a
UI trace if desired.

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

Persistent-index performance benchmarks and deployment-specific retention
policies remain application concerns. They do not change this integration or
authority boundary.

Run the reproducible local baseline with:

```text
cargo bench -p llm-harness-session-recall --bench sqlite_recall --features sqlite-bundled
```

The benchmark reports bulk indexing time, SQLite/WAL bytes, search p50/p95/p99,
and reopen plus projected-revision scan time. Override its dataset with
`SESSION_RECALL_BENCH_SESSIONS`, `SESSION_RECALL_BENCH_DOCS_PER_SESSION`, and
`SESSION_RECALL_BENCH_SEARCHES`; record results on the target hardware rather
than enforcing machine-specific latency thresholds in correctness tests.
