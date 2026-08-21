//! 23 — Infrastructure Integration: EventStream + Knowledge + Memory + SessionRecall.
//!
//! Demonstrates:
//! - Part 1: EventStream (TimerStream) — a one-shot timer fires after 3s;
//!   the LLM calls `wait_for_external_event` and receives the timer event.
//! - Part 2: EventStream (WebhookStream) — an external payload is pushed
//!   after 2s via a `WebhookChannel`; the LLM calls `wait_for_external_event`
//!   and receives the webhook payload.
//! - Part 3: Knowledge — `LocalDocumentSource` with temp .md files is
//!   registered in a `KnowledgeRegistry`; `KnowledgePlugin` is installed;
//!   the LLM calls `knowledge_search` to find documents.
//! - Part 4: Memory + SessionRecall construction demo — shows the API wiring
//!   for `MemoryService`/`MemoryPlugin` and `SessionRecallService`/
//!   `HistoryRecallPlugin` without making LLM calls (the setup is too heavy
//!   for a single example turn).
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 23_infra_integration

use std::sync::Arc;

use chrono::Utc;
use llm_harness_knowledge::{
    AllowAllAuthorizer, EvidenceAuthority, EvidenceProviderId, KnowledgeAccessContext,
    KnowledgeAccessControl, KnowledgePlugin, KnowledgePluginConfig, KnowledgeRegistry,
    KnowledgeScope, PrincipalRef,
};
use llm_harness_knowledge_local::{
    DocumentParserRegistry, DocumentRootConfig, LocalDocumentSource, LocalDocumentSourceConfig,
    OsDocumentFileSystem,
};
use llm_harness_live_tests::{
    EventCapture, LiveProvider, MULTI_TURN, SINGLE_TURN, make_harness_with_env, with_timeout,
};
use llm_harness_strategy::{EventDetails, TimerStream, WebhookStream};
use llm_harness_types::{EventStream, RunRequest};
use llm_harness_workflow::lifecycle::event::WaitForExternalEventTool;
use llm_harness_workflow::lifecycle::task::TaskId;
use tokio::sync::Mutex;

// ── main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    println!("=== 23: Infrastructure Integration ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .or(provider.anthropic.as_ref())
        .expect("set OPENAI_API_KEY or ANTHROPIC_API_KEY to run this example");

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);

    part1_timer_stream(entry).await;
    println!();
    part2_webhook_stream(entry).await;
    println!();
    part3_knowledge_search(entry).await;
    println!();
    part4_memory_session_recall_construction();

    println!("\n--- Summary ---");
    println!("Part 1: TimerStream + WaitForExternalEventTool (LLM waits for timer)");
    println!("Part 2: WebhookStream + WaitForExternalEventTool (LLM receives webhook payload)");
    println!("Part 3: KnowledgePlugin + knowledge_search tool");
    println!("Part 4: Memory + SessionRecall construction demo (no LLM call)");
}

// ── Part 1: TimerStream ─────────────────────────────────────────────────────

async fn part1_timer_stream(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 1: TimerStream + WaitForExternalEventTool ---\n");

    // Create a one-shot timer that fires 3 seconds from now.
    let fire_at = Utc::now() + chrono::Duration::seconds(3);
    let timer = TimerStream::once(fire_at, "demo-timer");

    // Wrap in the type-erased `Box<dyn EventStream>` + `Arc<Mutex<…>>` shape
    // that `WaitForExternalEventTool` expects.
    let stream: Box<dyn EventStream> = Box::new(timer);
    let stream_arc = Arc::new(Mutex::new(stream));

    // WaitForExternalEventTool persists the wait descriptor for crash
    // recovery via TaskStore. Here we pass None (no recovery) for simplicity.
    let (harness, _env, _tmp) = make_harness_with_env(entry, |b| {
        let tool = WaitForExternalEventTool::new(
            stream_arc,
            EventDetails::new("timer", "timer.once").to_value(),
            None,
            TaskId("demo-timer-task".into()),
        )
        .with_timeout(std::time::Duration::from_secs(30));

        b.tool(Arc::new(tool))
    })
    .await;

    let cap = EventCapture::subscribe(&harness);

    let prompt_text = "Use the wait_for_external_event tool to wait for an external event. \
                       The tool will block until a timer fires (about 3 seconds). \
                       After the event arrives, tell me what event you received.";
    println!("LLM prompt: {}", prompt_text);

    with_timeout(MULTI_TURN, harness.run(RunRequest::from_text(prompt_text)))
        .await
        .expect("run failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();
    println!("\nTools called: {:?}", tools_called);
    println!(
        "Final text:   {}",
        cap.texts().chars().take(200).collect::<String>()
    );

    let wait_called = tools_called.iter().any(|t| t == "wait_for_external_event");
    println!("\nObservation:");
    println!("  wait_for_external_event called: {}", wait_called);
    if !wait_called {
        println!("  (If the tool was not called, this may be due to the empty-parameters");
        println!("   schema bug in some streaming providers — the tool's parameters_schema");
        println!("   returns {{\"properties\": {{}}}} which some models skip.)");
    }
}

// ── Part 2: WebhookStream ────────────────────────────────────────────────────

async fn part2_webhook_stream(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 2: WebhookStream + WaitForExternalEventTool ---\n");

    // WebhookStream::new returns (channel, boxed stream).
    let (channel, stream) = WebhookStream::new(16);
    let stream_arc = Arc::new(Mutex::new(stream));

    // Spawn a task that pushes a webhook payload after 2 seconds.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let payload = serde_json::json!({
            "event": "deploy_complete",
            "status": "success",
            "service": "api-gateway",
            "version": "v2.1.0"
        });
        println!("  [spawn] pushing webhook payload: {}", payload);
        let _ = channel.push(payload).await;
    });

    let (harness, _env, _tmp) = make_harness_with_env(entry, |b| {
        let tool = WaitForExternalEventTool::new(
            stream_arc,
            serde_json::json!({"source": "webhook", "channel": "deploy"}),
            None,
            TaskId("demo-webhook-task".into()),
        )
        .with_timeout(std::time::Duration::from_secs(30));

        b.tool(Arc::new(tool))
    })
    .await;

    let cap = EventCapture::subscribe(&harness);

    let prompt_text = "Use the wait_for_external_event tool to wait for an external event. \
                       A webhook with deployment status will arrive in about 2 seconds. \
                       After receiving it, report the deployment status and version.";
    println!("LLM prompt: {}", prompt_text);

    with_timeout(MULTI_TURN, harness.run(RunRequest::from_text(prompt_text)))
        .await
        .expect("run failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();
    println!("\nTools called: {:?}", tools_called);
    println!(
        "Final text:   {}",
        cap.texts().chars().take(200).collect::<String>()
    );

    let wait_called = tools_called.iter().any(|t| t == "wait_for_external_event");
    println!("\nObservation:");
    println!("  wait_for_external_event called: {}", wait_called);
    if wait_called {
        let text = cap.texts();
        let mentions_deploy =
            text.to_lowercase().contains("deploy") || text.to_lowercase().contains("success");
        println!("  response mentions deployment/status: {}", mentions_deploy);
    }
}

// ── Part 3: Knowledge (knowledge_search) ─────────────────────────────────────

async fn part3_knowledge_search(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 3: KnowledgePlugin + knowledge_search ---\n");

    // Create a temp directory with a few .md documents.
    let doc_dir = std::env::temp_dir().join("infra-integration-docs");
    std::fs::create_dir_all(&doc_dir).unwrap();
    std::fs::write(
        doc_dir.join("rust_tips.md"),
        "# Rust Tips\n\nAlways use cargo fmt before committing code.\n\
         Clippy catches common mistakes. Run it in CI.\n",
    )
    .unwrap();
    std::fs::write(
        doc_dir.join("deploy_guide.md"),
        "# Deployment Guide\n\nDeploy with `cargo build --release`.\n\
         Use Docker for production deployments.\n\
         Blue-green deployment reduces downtime.\n",
    )
    .unwrap();
    std::fs::write(
        doc_dir.join("api_notes.md"),
        "# API Notes\n\nREST endpoints return JSON.\n\
         Use pagination for large result sets.\n\
         Rate limiting protects against abuse.\n",
    )
    .unwrap();

    // Build the access control with AllowAllAuthorizer (single-user demo).
    let access_control = Arc::new(KnowledgeAccessControl::new(Arc::new(AllowAllAuthorizer)));

    // Create the LocalDocumentSource.
    let config = LocalDocumentSourceConfig {
        source_id: "demo-docs".to_string(),
        name: "Demo Documents".to_string(),
        description: "A small set of .md files for the knowledge demo.".to_string(),
        domains: vec!["engineering".to_string()],
        roots: vec![DocumentRootConfig {
            id: "root".to_string(),
            path: doc_dir.clone(),
        }],
        source_secret: vec![42u8; 32],
        max_document_bytes: 1024 * 1024,
    };
    let local_source = LocalDocumentSource::create(
        config,
        Arc::new(OsDocumentFileSystem),
        DocumentParserRegistry::default(),
        access_control.clone(),
    )
    .await
    .expect("failed to create LocalDocumentSource");

    // Build the KnowledgeRegistry.
    let registry = Arc::new(
        KnowledgeRegistry::builder(access_control)
            .source(Arc::new(local_source))
            .build()
            .expect("failed to build KnowledgeRegistry"),
    );

    // Create the EvidenceAuthority.
    let provider_id = EvidenceProviderId("knowledge.registry".to_string());
    let authority = Arc::new(
        EvidenceAuthority::new(vec![7u8; 32], [provider_id.clone()])
            .expect("failed to create EvidenceAuthority"),
    );

    // Build the KnowledgePlugin.
    let plugin = KnowledgePlugin::new(
        registry,
        authority,
        provider_id,
        KnowledgePluginConfig::default(),
    )
    .expect("failed to build KnowledgePlugin");

    println!(
        "  KnowledgePlugin constructed with 3 .md documents in {}",
        doc_dir.display()
    );

    // Build the harness with the plugin installed.
    let (harness, _env, _tmp) = make_harness_with_env(entry, |b| b.install(&plugin)).await;

    let cap = EventCapture::subscribe(&harness);

    let prompt_text = "Use the knowledge_search tool to search for information about \
                       'deployment'. Then briefly summarize what you found.";
    println!("LLM prompt: {}", prompt_text);

    // The knowledge tools require a KnowledgeAccessContext run extension
    // for authorization. Without it, the tool returns "unauthorized".
    let access_ctx = KnowledgeAccessContext::new(
        KnowledgeScope::new("demo"),
        PrincipalRef::new("example-user", "user"),
    );
    let request = RunRequest::from_text(prompt_text).with_extension(access_ctx);

    with_timeout(SINGLE_TURN, harness.run(request))
        .await
        .expect("run failed");
    cap.settle().await;

    let tools_called = cap.tool_calls();
    println!("\nTools called: {:?}", tools_called);
    println!(
        "Final text:   {}",
        cap.texts().chars().take(300).collect::<String>()
    );

    let search_called = tools_called.iter().any(|t| t == "knowledge_search");
    println!("\nObservation:");
    println!("  knowledge_search called: {}", search_called);
    if search_called {
        let text = cap.texts().to_lowercase();
        let mentions_deploy = text.contains("deploy") || text.contains("docker");
        println!(
            "  response references deployment content: {}",
            mentions_deploy
        );
    }
}

// ── Part 4: Memory + SessionRecall construction demo ─────────────────────────

fn part4_memory_session_recall_construction() {
    println!("--- Part 4: Memory + SessionRecall construction demo ---\n");

    // This part demonstrates the API wiring without making LLM calls.
    // The setup for Memory and SessionRecall is heavy (requires a
    // MemoryStore implementation, SessionRepo, projector, etc.) so we
    // show the construction code and explain what each piece does.

    // ── Memory ──────────────────────────────────────────────────────────────
    //
    // MemoryService joins a read-only KnowledgeSource with a write store.
    // The wiring is:
    //
    //   let access_control = Arc::new(KnowledgeAccessControl::new(
    //       Arc::new(AllowAllAuthorizer),
    //   ));
    //
    //   // 1. Read source: reuse LocalDocumentSource (or any KnowledgeSource
    //   //    with Search + Read + Revisioned capabilities).
    //   let read_source: Arc<dyn KnowledgeSource> = /* ... */;
    //
    //   // 2. Write store: implement MemoryStore trait.
    //   //    descriptor().read_source_id must match read_source.descriptor().id.
    //   let write_store: Arc<dyn MemoryStore> = /* ... */;
    //
    //   // 3. Write policy: SecureMemoryWritePolicy validates content size,
    //   //    TTL, and scans for injection.
    //   let write_policy = Arc::new(
    //       SecureMemoryWritePolicy::new(vec![8u8; 32], SecureMemoryWritePolicyConfig::default())
    //           .unwrap(),
    //   );
    //
    //   // 4. Mutation gate: mandatory trusted boundary. There is NO
    //   //    permissive default — the application must implement
    //   //    MemoryMutationGate explicitly.
    //   let mutation_gate: Arc<dyn MemoryMutationGate> = /* ... */;
    //
    //   let service = Arc::new(
    //       MemoryService::new(
    //           access_control,
    //           read_source,
    //           write_store,
    //           write_policy,
    //           mutation_gate,
    //       )
    //       .unwrap(),
    //   );
    //
    //   let plugin = MemoryPlugin::new(service);
    //   // builder.install(&plugin)
    //   // → registers memory_write + memory_forget tools.
    //
    // The MemoryPlugin registers two tools:
    //   - memory_write: saves a fact to the memory store
    //   - memory_forget: deletes a memory entry

    println!("  Memory:");
    println!("    MemoryService::new(access_control, read_source, write_store,");
    println!("                       write_policy, mutation_gate)");
    println!("    → MemoryPlugin::new(service)");
    println!("    → registers: memory_write, memory_forget");
    println!("    (requires custom MemoryStore + MemoryMutationGate implementations)");

    // ── SessionRecall ───────────────────────────────────────────────────────
    //
    // SessionRecall indexes past sessions so the LLM can search conversation
    // history. The wiring is:
    //
    //   use llm_harness_session_recall::*;
    //
    //   // 1. Create an index (InMemorySessionRecallIndex for tests,
    //   //    SqliteSessionRecallIndex for persistence).
    //   let index = Arc::new(InMemorySessionRecallIndex::new());
    //
    //   // 2. Create a projector that indexes sessions into the index.
    //   //    repo is an Arc<dyn SessionRepo> (e.g. JsonlSessionRepo).
    //   let projector = SessionRecallProjector::new(repo.clone(), index.clone());
    //
    //   // 3. Create the service that searches the index.
    //   let service = Arc::new(SessionRecallService::new(
    //       repo.clone(),
    //       index.clone(),
    //       access_control.clone(),
    //   ));
    //
    //   // 4. Wrap the service as a KnowledgeSource for federated search.
    //   let recall_source = Arc::new(SessionRecallKnowledgeSource::new(service));
    //
    //   // 5. Register in KnowledgeRegistry alongside other sources.
    //   let registry = KnowledgeRegistry::builder(access_control)
    //       .source(local_source)
    //       .source(recall_source.clone())
    //       .build()?;
    //
    //   // 6. Install HistoryRecallPlugin for automatic context injection.
    //   let recall_plugin = HistoryRecallPlugin::new(
    //       recall_source,
    //       HistoryRecallPluginConfig::default(),
    //   )?;
    //   // builder.install(&recall_plugin)
    //
    // HistoryRecallPlugin does not register tools — it hooks into
    // TransformContextHook to inject relevant past-session snippets
    // into the LLM context automatically.

    println!("\n  SessionRecall:");
    println!("    InMemorySessionRecallIndex::new()");
    println!("    → SessionRecallProjector::new(repo, index)");
    println!("    → SessionRecallService::new(repo, index, access_control)");
    println!("    → SessionRecallKnowledgeSource::new(service)");
    println!("    → register in KnowledgeRegistry");
    println!("    → HistoryRecallPlugin::new(source, config)");
    println!("    → hooks TransformContextHook (no tools, auto-injects history)");

    println!("\n  (No LLM call in this part — construction demo only.)");
}
