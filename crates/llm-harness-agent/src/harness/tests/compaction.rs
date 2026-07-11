use super::*;

#[tokio::test]
async fn compact_override_with_invalid_first_kept_entry_returns_error() {
    use futures::future::BoxFuture;
    use llm_harness_types::{
        BeforeCompactCtx, BeforeCompactDecision, BeforeCompactHook, CompactionResult, EntryId,
    };

    struct BadOverrideHook;

    impl BeforeCompactHook for BadOverrideHook {
        fn before_compact<'a>(
            &'a self,
            _ctx: BeforeCompactCtx<'a>,
        ) -> BoxFuture<'a, BeforeCompactDecision> {
            Box::pin(async move {
                BeforeCompactDecision::Override(CompactionResult {
                    summary_message: AgentMessage::User(UserMessage {
                        content: vec![ContentBlock::Text {
                            text: "summary".into(),
                        }],
                        timestamp: chrono::Utc::now(),
                    }),
                    first_kept_entry: EntryId::new(),
                    tokens_before: 100,
                    tokens_after: 10,
                })
            })
        }
    }

    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("hello")]));
    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.hooks.before_compact = vec![Arc::new(BadOverrideHook)];
    let h = futures::executor::block_on(AgentHarness::new_in_memory(client, env, opts));

    h.prompt("hello").await.unwrap();

    let err = h.compact().await;
    assert!(
        matches!(
            err,
            Err(HarnessError::Compaction(
                CompactionError::InvalidFirstKeptEntry(_)
            ))
        ),
        "compact() must return InvalidFirstKeptEntry for a nonexistent entry: {err:?}"
    );
}
/// Supplies a fixed `CompactionResult` so tests don't need a real LLM for compaction.
struct OverrideCompactHook {
    first_kept_entry: std::sync::Mutex<Option<EntryId>>,
}

impl OverrideCompactHook {
    fn new() -> Self {
        Self {
            first_kept_entry: std::sync::Mutex::new(None),
        }
    }

    fn set_first_kept(&self, id: EntryId) {
        *self.first_kept_entry.lock().unwrap() = Some(id);
    }
}

impl BeforeCompactHook for OverrideCompactHook {
    fn before_compact<'a>(
        &'a self,
        ctx: BeforeCompactCtx<'a>,
    ) -> BoxFuture<'a, BeforeCompactDecision> {
        let first_kept = self.first_kept_entry.lock().unwrap().unwrap();
        let summary = AgentMessage::CompactionSummary(CompactionSummaryMessage {
            summary: format!("summary of {} messages", ctx.messages.len()),
            timestamp: chrono::Utc::now(),
        });
        let result = CompactionResult {
            summary_message: summary,
            first_kept_entry: first_kept,
            tokens_before: 1000,
            tokens_after: 100,
        };
        Box::pin(async move { BeforeCompactDecision::Override(result) })
    }
}

#[tokio::test]
async fn compaction_via_hook_override_inserts_summary() {
    let hook = Arc::new(OverrideCompactHook::new());

    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("reply")]));
    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.hooks = HarnessHooks {
        before_compact: vec![hook.clone()],
        ..HarnessHooks::none()
    };
    let h = AgentHarness::new_in_memory(client, env, opts).await;

    h.prompt("question").await.unwrap();

    // Use the last written entry as first_kept_entry so compaction keeps nothing.
    let ctx_before = h.build_context().await.unwrap();
    assert!(!ctx_before.messages.is_empty());

    // Append a marker entry and tell the hook to keep from there.
    let kept_id = h
        .append_custom_entry("marker".into(), serde_json::json!({}))
        .await
        .unwrap();
    hook.set_first_kept(kept_id);

    let stats = h.compact().await.unwrap();
    assert_eq!(
        stats.tokens_before, 1000,
        "wrong tokens_before from override"
    );

    // After compaction, the first message must be the summary.
    let ctx_after = h.build_context().await.unwrap();
    assert!(
        matches!(
            ctx_after.messages.first(),
            Some(AgentMessage::CompactionSummary(_))
        ),
        "expected summary as first message; got {:?}",
        ctx_after.messages.first().map(std::mem::discriminant)
    );
}
/// A hook returning `BeforeCompactDecision::Compact` forces the framework
/// to run compaction via `compute_cut_point` (bypassing the threshold check),
/// using the independent compaction client for the LLM summary.
#[tokio::test]
async fn compact_decision_forces_framework_compaction() {
    use futures::future::BoxFuture;
    use llm_harness_types::{BeforeCompactCtx, BeforeCompactDecision, BeforeCompactHook};

    struct CompactDecisionHook;
    impl BeforeCompactHook for CompactDecisionHook {
        fn before_compact<'a>(
            &'a self,
            _ctx: BeforeCompactCtx<'a>,
        ) -> BoxFuture<'a, BeforeCompactDecision> {
            Box::pin(async { BeforeCompactDecision::Compact })
        }
    }

    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("response1"),
        MockResponse::text("response2"),
    ]));
    let compaction_client = Arc::new(MockLlmClient::new(vec![MockResponse::text(
        "forced compact summary",
    )]));

    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.model_info = Some(ModelInfo {
        context_window: 1,
        max_tokens: 50,
    });
    opts.compaction_reserve_tokens = Some(0);
    opts.compaction_keep_recent_tokens = Some(0);
    opts.compaction_model = Some(CompactionModelConfig {
        client: compaction_client.clone() as Arc<dyn LlmClient>,
        model: "compaction-model".into(),
        model_info: ModelInfo {
            context_window: 200_000,
            max_tokens: 8192,
        },
    });
    opts.hooks = HarnessHooks {
        before_compact: vec![Arc::new(CompactDecisionHook)],
        ..HarnessHooks::none()
    };
    let h = AgentHarness::new_in_memory(main_client.clone() as Arc<dyn LlmClient>, env, opts).await;

    // Two prompts → 4 entries (user, assistant, user, assistant).
    h.prompt("hi").await.unwrap();
    h.prompt("yo").await.unwrap();

    // compact() triggers do_compact, which calls the hook → Compact decision
    // → compute_cut_point → compact() fn → apply_compaction_result.
    let stats = h.compact().await.unwrap();
    assert!(
        stats.tokens_before > 0,
        "tokens_before should be positive; got {}",
        stats.tokens_before
    );

    // Compaction client should have been used for the summary.
    assert_eq!(
        compaction_client
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "compaction client must receive exactly one summary request"
    );

    // Main client should only have the two prompt() calls.
    assert_eq!(
        main_client
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "main client must not receive compaction request"
    );

    // The summary must be in the session, and from_hook must be false.
    let path = h.session.read_active_path().await.unwrap();
    let has_summary = path.iter().any(|e| {
        if let SessionEntryPayload::Compaction(c) = &e.payload {
            matches!(
                &c.summary_message,
                AgentMessage::CompactionSummary(cs)
                    if cs.summary.contains("forced compact summary")
            ) && !c.from_hook
        } else {
            false
        }
    });
    assert!(
        has_summary,
        "compaction summary with from_hook=false not found in session"
    );
}
/// A hook returning `BeforeCompactDecision::Skip` must map to
/// `CompactionError::Skipped` (not `InsufficientTokens`), giving the
/// caller an accurate error message on manual `compact()`.
#[tokio::test]
async fn compact_skip_returns_skipped_error() {
    use futures::future::BoxFuture;
    use llm_harness_types::{BeforeCompactCtx, BeforeCompactDecision, BeforeCompactHook};

    struct SkipHook;
    impl BeforeCompactHook for SkipHook {
        fn before_compact<'a>(
            &'a self,
            _ctx: BeforeCompactCtx<'a>,
        ) -> BoxFuture<'a, BeforeCompactDecision> {
            Box::pin(async { BeforeCompactDecision::Skip })
        }
    }

    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("response1"),
        MockResponse::text("response2"),
    ]));
    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.model_info = Some(ModelInfo {
        context_window: 1,
        max_tokens: 50,
    });
    opts.compaction_reserve_tokens = Some(0);
    opts.compaction_keep_recent_tokens = Some(0);
    opts.hooks = HarnessHooks {
        before_compact: vec![Arc::new(SkipHook)],
        ..HarnessHooks::none()
    };
    let h = AgentHarness::new_in_memory(main_client.clone() as Arc<dyn LlmClient>, env, opts).await;

    h.prompt("hi").await.unwrap();
    h.prompt("yo").await.unwrap();

    let err = h.compact().await.unwrap_err();
    assert!(
        matches!(err, HarnessError::Compaction(CompactionError::Skipped)),
        "expected CompactionError::Skipped, got: {err:?}"
    );
}

#[tokio::test]
async fn do_compact_uses_independent_compaction_client() {
    // Main client: two short text responses for two prompt() calls.
    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("response1"),
        MockResponse::text("response2"),
    ]));
    // Compaction client: returns the summary text.
    let compaction_client = Arc::new(MockLlmClient::new(vec![MockResponse::text(
        "compacted summary",
    )]));

    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    // Tiny context window + zero reserve so compaction triggers easily.
    opts.model_info = Some(ModelInfo {
        context_window: 1,
        max_tokens: 50,
    });
    opts.compaction_reserve_tokens = Some(0);
    opts.compaction_keep_recent_tokens = Some(0);
    opts.compaction_model = Some(CompactionModelConfig {
        client: compaction_client as Arc<dyn LlmClient>,
        model: "compaction-model".into(),
        model_info: ModelInfo {
            context_window: 200_000,
            max_tokens: 8192,
        },
    });
    let h = AgentHarness::new_in_memory(main_client.clone() as Arc<dyn LlmClient>, env, opts).await;

    // Two prompts → 4 entries (user, assistant, user, assistant).
    // Each ~1 token (char-based estimate), total ~4 > threshold 3.
    h.prompt("hi").await.unwrap();
    h.prompt("yo").await.unwrap();

    // Trigger compaction. Should use compaction_client, not main_client.
    h.compact().await.unwrap();
    // Main client should only receive the two prompt() calls, NOT the
    // compaction summary request — that must route through compaction_client.
    assert_eq!(
        main_client
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "main client must not receive compaction request"
    );

    // The compaction summary from compaction_client should be in the session.
    let path = h.session.read_active_path().await.unwrap();
    let has_summary = path.iter().any(|e| {
        if let SessionEntryPayload::Compaction(c) = &e.payload {
            matches!(
                &c.summary_message,
                AgentMessage::CompactionSummary(cs) if cs.summary.contains("compacted summary")
            )
        } else {
            false
        }
    });
    assert!(
        has_summary,
        "compaction summary from independent client not found in session"
    );
}

#[tokio::test]
async fn compact_public_method_works() {
    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("response1"),
        MockResponse::text("response2"),
        MockResponse::text("compacted summary"),
    ]));
    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    // Tiny context window + zero reserve so compaction triggers easily.
    opts.model_info = Some(ModelInfo {
        context_window: 1,
        max_tokens: 50,
    });
    opts.compaction_reserve_tokens = Some(0);
    opts.compaction_keep_recent_tokens = Some(0);
    // Use main client for compaction but with a large enough context window
    // for the summary request to fit.
    opts.compaction_model = Some(CompactionModelConfig {
        client: main_client.clone() as Arc<dyn LlmClient>,
        model: "test-model".into(),
        model_info: ModelInfo {
            context_window: 200_000,
            max_tokens: 8192,
        },
    });
    let h = AgentHarness::new_in_memory(main_client.clone() as Arc<dyn LlmClient>, env, opts).await;

    h.prompt("hi").await.unwrap();
    h.prompt("yo").await.unwrap();

    // compact() is now a production method.
    let stats = h.compact().await.unwrap();
    assert!(stats.tokens_before > 0);

    let path = h.session.read_active_path().await.unwrap();
    let has_summary = path
        .iter()
        .any(|e| matches!(&e.payload, SessionEntryPayload::Compaction(_)));
    assert!(has_summary, "compaction summary not found in session");
}

#[tokio::test]
async fn auto_compact_triggers_on_prompt() {
    // Main client: 2 prompt responses + 1 compaction summary + 1 response for third prompt.
    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("response1"),
        MockResponse::text("response2"),
        MockResponse::text("auto compact summary"),
        MockResponse::text("response3"),
    ]));
    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.model_info = Some(ModelInfo {
        context_window: 1,
        max_tokens: 50,
    });
    opts.compaction_reserve_tokens = Some(0);
    opts.compaction_keep_recent_tokens = Some(0);
    opts.auto_compact = true;
    opts.compaction_model = Some(CompactionModelConfig {
        client: main_client.clone() as Arc<dyn LlmClient>,
        model: "test-model".into(),
        model_info: ModelInfo {
            context_window: 200_000,
            max_tokens: 8192,
        },
    });
    let h = AgentHarness::new_in_memory(main_client.clone() as Arc<dyn LlmClient>, env, opts).await;

    // Two prompts build up context. Third prompt triggers auto-compaction
    // at run entry (Idle phase), then the run proceeds.
    h.prompt("first message").await.unwrap();
    h.prompt("second message").await.unwrap();
    h.prompt("third message").await.unwrap();

    // Verify compaction happened: session should have a CompactionEntry.
    let path = h.session.read_active_path().await.unwrap();
    let has_compaction = path
        .iter()
        .any(|e| matches!(&e.payload, SessionEntryPayload::Compaction(_)));
    assert!(
        has_compaction,
        "auto-compaction should have triggered on the third prompt"
    );
}

#[tokio::test]
async fn auto_compact_failure_propagates_error() {
    // Main client: 2 normal responses for first 2 prompts.
    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("response1"),
        MockResponse::text("response2"),
    ]));
    // Compaction client that returns a RateLimit error.
    let compaction_client = Arc::new(MockLlmClient::new(vec![MockResponse::rate_limit_error()]));
    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.model_info = Some(ModelInfo {
        context_window: 1,
        max_tokens: 50,
    });
    opts.compaction_reserve_tokens = Some(0);
    opts.compaction_keep_recent_tokens = Some(0);
    opts.auto_compact = true;
    opts.compaction_model = Some(CompactionModelConfig {
        client: compaction_client as Arc<dyn LlmClient>,
        model: "compaction-model".into(),
        model_info: ModelInfo {
            context_window: 200_000,
            max_tokens: 8192,
        },
    });
    let h = AgentHarness::new_in_memory(main_client.clone() as Arc<dyn LlmClient>, env, opts).await;

    h.prompt("first").await.unwrap();
    h.prompt("second").await.unwrap();

    // Third prompt triggers auto-compaction at run entry (Idle phase).
    // The compaction client returns RateLimit error → SummaryFailed.
    // The error must propagate and abort the run (context is too large
    // to continue without compaction).
    let err = h.prompt("third").await.unwrap_err();
    assert!(
        matches!(
            err,
            HarnessError::Compaction(CompactionError::SummaryFailed(_))
        ),
        "expected SummaryFailed from compaction rate limit, got: {err:?}"
    );

    // Main client should NOT have received a third request (run aborted).
    assert_eq!(
        main_client
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "main client must not receive a third request after compaction failure"
    );
}

/// When `auto_compact` is disabled, repeated prompts must NOT trigger
/// any compaction, even if tokens exceed the threshold.
#[tokio::test]
async fn auto_compact_disabled_does_not_compact() {
    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("response1"),
        MockResponse::text("response2"),
        MockResponse::text("response3"),
    ]));
    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.model_info = Some(ModelInfo {
        context_window: 1,
        max_tokens: 50,
    });
    opts.compaction_reserve_tokens = Some(0);
    opts.compaction_keep_recent_tokens = Some(0);
    opts.auto_compact = false; // explicitly disabled
    let h = AgentHarness::new_in_memory(main_client.clone() as Arc<dyn LlmClient>, env, opts).await;

    h.prompt("first").await.unwrap();
    h.prompt("second").await.unwrap();
    h.prompt("third").await.unwrap();

    let path = h.session.read_active_path().await.unwrap();
    let has_compaction = path
        .iter()
        .any(|e| matches!(&e.payload, SessionEntryPayload::Compaction(_)));
    assert!(
        !has_compaction,
        "auto_compact=false must not trigger compaction"
    );
}

/// When `auto_compact` is enabled and the compaction LLM call blocks,
/// calling `abort()` must cancel it (the temporary abort token is set
/// in `current_abort` before compaction starts).
#[tokio::test]
async fn auto_compact_abort_cancels_compaction() {
    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("response1"),
        MockResponse::text("response2"),
    ]));
    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.model_info = Some(ModelInfo {
        context_window: 1,
        max_tokens: 50,
    });
    opts.compaction_reserve_tokens = Some(0);
    opts.compaction_keep_recent_tokens = Some(0);
    opts.auto_compact = true;
    // BlockingClient never returns — compaction will hang unless aborted.
    opts.compaction_model = Some(CompactionModelConfig {
        client: Arc::new(BlockingClient) as Arc<dyn LlmClient>,
        model: "blocking-model".into(),
        model_info: ModelInfo {
            context_window: 200_000,
            max_tokens: 8192,
        },
    });
    let h =
        Arc::new(AgentHarness::new_in_memory(main_client as Arc<dyn LlmClient>, env, opts).await);

    h.prompt("first").await.unwrap();
    h.prompt("second").await.unwrap();

    // Third prompt triggers auto-compaction. The compaction LLM call
    // blocks forever. We abort from another task.
    let h_clone = h.clone();
    let prompt_task = tokio::spawn(async move { h_clone.prompt("third").await });

    // Give compaction time to start (enter Compacting phase + LLM call).
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    h.abort();

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), prompt_task).await;
    assert!(
        result.is_ok(),
        "prompt task should finish after abort, not hang"
    );
    let prompt_result = result.unwrap().unwrap();
    assert!(
        prompt_result.is_err(),
        "aborted auto-compaction should produce an error, got: {prompt_result:?}"
    );
}

/// When a `before_compact` hook returns `Compact`, auto-compaction must
/// trigger even if tokens are under the threshold (the pre-check is
/// skipped when a hook is registered).
#[tokio::test]
async fn auto_compact_with_compact_hook_under_threshold() {
    use futures::future::BoxFuture;
    use llm_harness_types::{BeforeCompactCtx, BeforeCompactDecision, BeforeCompactHook};

    struct CompactDecisionHook;
    impl BeforeCompactHook for CompactDecisionHook {
        fn before_compact<'a>(
            &'a self,
            _ctx: BeforeCompactCtx<'a>,
        ) -> BoxFuture<'a, BeforeCompactDecision> {
            Box::pin(async { BeforeCompactDecision::Compact })
        }
    }

    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("response1"),
        MockResponse::text("response2"),
        MockResponse::text("forced compact summary"),
        MockResponse::text("response3"),
    ]));
    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    // Large context window → tokens will be well under threshold.
    opts.model_info = Some(ModelInfo {
        context_window: 1_000_000,
        max_tokens: 50,
    });
    opts.compaction_reserve_tokens = Some(100_000);
    opts.compaction_keep_recent_tokens = Some(0);
    opts.auto_compact = true;
    opts.compaction_model = Some(CompactionModelConfig {
        client: main_client.clone() as Arc<dyn LlmClient>,
        model: "test-model".into(),
        model_info: ModelInfo {
            context_window: 200_000,
            max_tokens: 8192,
        },
    });
    opts.hooks = HarnessHooks {
        before_compact: vec![Arc::new(CompactDecisionHook)],
        ..HarnessHooks::none()
    };
    let h = AgentHarness::new_in_memory(main_client.clone() as Arc<dyn LlmClient>, env, opts).await;

    h.prompt("first").await.unwrap();
    h.prompt("second").await.unwrap();
    // Third prompt: tokens are under threshold, but the hook returns
    // Compact → compaction should trigger anyway.
    h.prompt("third").await.unwrap();

    let path = h.session.read_active_path().await.unwrap();
    let has_compaction = path
        .iter()
        .any(|e| matches!(&e.payload, SessionEntryPayload::Compaction(_)));
    assert!(
        has_compaction,
        "hook returning Compact should trigger compaction even under threshold"
    );
}

/// When tokens exceed the threshold but `compute_cut_point` returns
/// `None` (all entries fit within `keep_recent_tokens`), auto-compaction
/// must NOT emit `CompactionEnd` events.
///
/// This test deliberately builds context across 2 prompts so the session
/// path is non-empty when `try_auto_compact` runs on the 3rd prompt.
/// `keep_recent_tokens` is set high enough to retain all entries, so
/// the backward scan in `compute_cut_point` reaches `i == 0` and returns
/// `None` — the actual NoValidBoundary path.
#[tokio::test]
async fn auto_compact_no_spurious_events_when_no_valid_boundary() {
    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("response1"),
        MockResponse::text("response2"),
        MockResponse::text("response3"),
    ]));
    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    // Tiny context window + zero reserve → threshold = 1.
    // Two prompts produce ~4 tokens of entries → estimated_tokens > threshold.
    opts.model_info = Some(ModelInfo {
        context_window: 1,
        max_tokens: 50,
    });
    opts.compaction_reserve_tokens = Some(0);
    // keep_recent_tokens large enough to retain all entries → backward
    // scan reaches i == 0 → compute_cut_point returns None.
    opts.compaction_keep_recent_tokens = Some(100_000);
    opts.auto_compact = true;
    opts.compaction_model = Some(CompactionModelConfig {
        client: main_client.clone() as Arc<dyn LlmClient>,
        model: "test-model".into(),
        model_info: ModelInfo {
            context_window: 200_000,
            max_tokens: 8192,
        },
    });
    let h = AgentHarness::new_in_memory(main_client.clone() as Arc<dyn LlmClient>, env, opts).await;

    // Build up context so the session path is non-empty.
    h.prompt("first").await.unwrap();
    h.prompt("second").await.unwrap();

    // Collect events for the 3rd prompt — try_auto_compact sees a
    // non-empty path, tokens exceed threshold, but compute_cut_point
    // returns None (all entries within keep_recent window).
    let mut event_rx = h.subscribe();
    h.prompt("third").await.unwrap();

    // Drain events — should see no CompactionEnd (benign skip, silent).
    let mut had_compaction_end = false;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(event.as_ref(), AgentHarnessEvent::CompactionEnd { .. }) {
            had_compaction_end = true;
        }
    }
    assert!(
        !had_compaction_end,
        "should not emit CompactionEnd when compute_cut_point returns None"
    );
}

/// When a `before_compact` hook returns `Compact` but no valid cut
/// boundary exists, auto-compaction must NOT emit `CompactionEnd` events.
/// This is the hook-path equivalent of `auto_compact_no_spurious_events_when_no_valid_boundary`.
#[tokio::test]
async fn auto_compact_hook_compact_no_boundary_no_spurious_events() {
    use futures::future::BoxFuture;
    use llm_harness_types::{BeforeCompactCtx, BeforeCompactDecision, BeforeCompactHook};

    struct CompactDecisionHook;
    impl BeforeCompactHook for CompactDecisionHook {
        fn before_compact<'a>(
            &'a self,
            _ctx: BeforeCompactCtx<'a>,
        ) -> BoxFuture<'a, BeforeCompactDecision> {
            Box::pin(async { BeforeCompactDecision::Compact })
        }
    }

    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("response1"),
        MockResponse::text("response2"),
        MockResponse::text("response3"),
    ]));
    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.model_info = Some(ModelInfo {
        context_window: 1,
        max_tokens: 50,
    });
    opts.compaction_reserve_tokens = Some(0);
    // keep_recent_tokens large enough to retain all entries → backward
    // scan reaches i == 0 → compute_cut_point returns None.
    opts.compaction_keep_recent_tokens = Some(100_000);
    opts.auto_compact = true;
    opts.compaction_model = Some(CompactionModelConfig {
        client: main_client.clone() as Arc<dyn LlmClient>,
        model: "test-model".into(),
        model_info: ModelInfo {
            context_window: 200_000,
            max_tokens: 8192,
        },
    });
    opts.hooks = HarnessHooks {
        before_compact: vec![Arc::new(CompactDecisionHook)],
        ..HarnessHooks::none()
    };
    let h = AgentHarness::new_in_memory(main_client.clone() as Arc<dyn LlmClient>, env, opts).await;

    // Build up context so the session path is non-empty.
    h.prompt("first").await.unwrap();
    h.prompt("second").await.unwrap();

    // 3rd prompt: try_auto_compact sees non-empty path, hook returns
    // Compact, but compute_cut_point returns None (all entries within
    // keep_recent window) → NoValidBoundary → benign skip, no CompactionEnd.
    let mut event_rx = h.subscribe();
    h.prompt("third").await.unwrap();

    // Drain events — should see no CompactionEnd (benign skip, silent).
    let mut had_compaction_end = false;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(event.as_ref(), AgentHarnessEvent::CompactionEnd { .. }) {
            had_compaction_end = true;
        }
    }
    assert!(
        !had_compaction_end,
        "hook Compact path with NoValidBoundary must not emit CompactionEnd"
    );
}

/// When a `before_compact` hook returns `Proceed` and tokens are under
/// the threshold, auto-compaction must NOT emit `CompactionEnd` events.
/// The `PhaseChange { Idle → Compacting → Idle }` churn is the
/// inherent cost of registering a hook (the framework must enter the
/// Compacting phase to call the hook), but the `CompactionEnd` event
/// is suppressed for this benign skip.
#[tokio::test]
async fn auto_compact_hook_proceed_under_threshold_no_compaction_end() {
    use futures::future::BoxFuture;
    use llm_harness_types::{BeforeCompactCtx, BeforeCompactDecision, BeforeCompactHook};

    struct ProceedHook;
    impl BeforeCompactHook for ProceedHook {
        fn before_compact<'a>(
            &'a self,
            _ctx: BeforeCompactCtx<'a>,
        ) -> BoxFuture<'a, BeforeCompactDecision> {
            Box::pin(async { BeforeCompactDecision::Proceed })
        }
    }

    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("response1"),
        MockResponse::text("response2"),
        MockResponse::text("response3"),
    ]));
    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    // Large context window → tokens always under threshold.
    opts.model_info = Some(ModelInfo {
        context_window: 1_000_000,
        max_tokens: 50,
    });
    opts.auto_compact = true;
    opts.hooks = HarnessHooks {
        before_compact: vec![Arc::new(ProceedHook)],
        ..HarnessHooks::none()
    };
    let h = AgentHarness::new_in_memory(main_client.clone() as Arc<dyn LlmClient>, env, opts).await;

    h.prompt("first").await.unwrap();

    let mut event_rx = h.subscribe();
    h.prompt("second").await.unwrap();

    // Drain events — should see no CompactionEnd (Proceed + under
    // threshold → InsufficientTokens → benign skip, silent).
    let mut had_compaction_end = false;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(event.as_ref(), AgentHarnessEvent::CompactionEnd { .. }) {
            had_compaction_end = true;
        }
    }
    assert!(
        !had_compaction_end,
        "hook Proceed + under-threshold must not emit CompactionEnd"
    );
}
