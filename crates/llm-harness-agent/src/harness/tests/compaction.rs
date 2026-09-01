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

    let mut event_rx = h.subscribe();
    let stats = h.compact().await.unwrap();
    assert_eq!(
        stats.tokens_before, 1000,
        "wrong tokens_before from override"
    );

    // Override path must emit CompactionStart before CompactionEnd.
    let mut saw_start = false;
    let mut saw_end = false;
    let mut start_before_end = false;
    while let Ok(event) = event_rx.try_recv() {
        match event.as_ref() {
            AgentHarnessEvent::CompactionStart { .. } => {
                saw_start = true;
                if !saw_end {
                    start_before_end = true;
                }
            }
            AgentHarnessEvent::CompactionEnd { .. } => {
                saw_end = true;
            }
            _ => {}
        }
    }
    assert!(saw_start, "Override path must emit CompactionStart");
    assert!(saw_end, "Override path must emit CompactionEnd");
    assert!(
        start_before_end,
        "CompactionStart must precede CompactionEnd"
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
        // context_window large enough for the conversation to fit, but
        // threshold = context_window - reserve is low enough to trigger
        // compaction after two prompts.
        context_window: 100,
        max_tokens: 50,
    });
    opts.compaction_reserve_tokens = Some(95);
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
        // context_window large enough for the conversation to fit, but
        // threshold = 10 - 9 = 1, so 2 user tokens trigger compaction.
        context_window: 10,
        max_tokens: 50,
    });
    opts.compaction_reserve_tokens = Some(9);
    opts.compaction_keep_recent_tokens = Some(0);
    opts.compaction_max_failures = Some(1);
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
    // msg2's compaction returned NoValidBoundary (not counted as failure);
    // msg3 is the first SummaryFailed. With compaction_max_failures=1,
    // the first consecutive failure (count 0→1) trips the circuit
    // breaker → CircuitOpen(1). The error must propagate and abort the
    // run (context is too large to continue without compaction).
    let err = h.prompt("third").await.unwrap_err();
    assert!(
        matches!(
            err,
            HarnessError::Compaction(CompactionError::CircuitOpen(1))
        ),
        "expected CircuitOpen(1) after 1 consecutive failure, got: {err:?}"
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
        // threshold = 2 - 1 = 1, so 2 user tokens from 2 prompts trigger.
        context_window: 2,
        max_tokens: 50,
    });
    opts.compaction_reserve_tokens = Some(1);
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
/// `None` (all entries fit within `keep_recent_tokens`), and tokens are
/// still within the context window, auto-compaction silently lets the
/// run proceed — no `CompactionEnd` event is emitted.
///
/// This test deliberately builds context across 2 prompts so the session
/// path is non-empty when `try_auto_compact` runs on the 3rd prompt.
/// `keep_recent_tokens` is set high enough to retain all entries, so
/// the backward scan in `compute_cut_point` reaches `i == 0` and returns
/// `None` — the actual NoValidBoundary path. The context window is set
/// large enough that `estimated_tokens <= context_window`, so this is
/// a soft threshold exceedance, not a hard limit.
#[tokio::test]
async fn auto_compact_silent_when_no_valid_boundary_under_context_window() {
    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("response1"),
        MockResponse::text("response2"),
        MockResponse::text("response3"),
    ]));
    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    // context_window large enough that estimated_tokens (~4) < context_window.
    // reserve_tokens set so threshold < estimated_tokens.
    opts.model_info = Some(ModelInfo {
        context_window: 1_000_000,
        max_tokens: 50,
    });
    opts.compaction_reserve_tokens = Some(1_000_000);
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

    // Should NOT emit CompactionEnd — soft threshold exceedance with
    // no valid boundary is a silent skip (consistent with InsufficientTokens).
    let mut had_compaction_end = false;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(event.as_ref(), AgentHarnessEvent::CompactionEnd { .. }) {
            had_compaction_end = true;
        }
    }
    assert!(
        !had_compaction_end,
        "should not emit CompactionEnd for soft threshold exceedance with no valid boundary"
    );
}

/// When tokens exceed the context window (hard limit) and
/// `compute_cut_point` returns `None`, auto-compaction must return
/// `Err(NoValidBoundary)` so the caller knows continuing is futile.
#[tokio::test]
async fn auto_compact_errors_when_no_valid_boundary_over_context_window() {
    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("response1"),
        MockResponse::text("response2"),
        // response3 won't be consumed — prompt("third") should error.
    ]));
    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    // Tiny context window + zero reserve → threshold = 1.
    // MockLlmClient produces AssistantMessage with usage.output_tokens=0,
    // so only user messages contribute tokens. After 2 prompts:
    // path = [u1(1), a1(0), u2(1), a2(0)] → estimated_tokens = 2 > context_window(1).
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

    // Build up context. MockLlmClient sets usage.output_tokens=0, so
    // assistant entries contribute 0 tokens. Only user entries count.
    // prompt("first"): path was empty → skips. After: [u1(1), a1(0)].
    // prompt("second"): estimated_tokens=1 <= threshold=1 → skips. After: [u1, a1, u2, a2].
    h.prompt("first").await.unwrap();
    h.prompt("second").await.unwrap();

    // 3rd prompt: try_auto_compact sees estimated_tokens=2 > context_window(1),
    // but compute_cut_point returns None → Err(NoValidBoundary) propagates.
    let result = h.prompt("third").await;
    assert!(
        result.is_err(),
        "prompt should fail when context exceeds window and no valid cut boundary exists"
    );
    let err = result.unwrap_err();
    assert!(
        matches!(
            err,
            HarnessError::Compaction(CompactionError::NoValidBoundary)
        ),
        "expected NoValidBoundary error, got: {err:?}"
    );
}

/// When a `before_compact` hook returns `Compact` but no valid cut
/// boundary exists, and tokens are within the context window,
/// auto-compaction silently lets the run proceed — no `CompactionEnd`
/// event is emitted.
#[tokio::test]
async fn auto_compact_hook_compact_no_boundary_silent_under_context_window() {
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
    // context_window large enough that estimated_tokens < context_window.
    opts.model_info = Some(ModelInfo {
        context_window: 1_000_000,
        max_tokens: 50,
    });
    opts.compaction_reserve_tokens = Some(1_000_000);
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
    // keep_recent window) → NoValidBoundary → silent skip, run proceeds.
    let mut event_rx = h.subscribe();
    h.prompt("third").await.unwrap();

    // Should NOT emit CompactionEnd — soft threshold exceedance with
    // no valid boundary is a silent skip, even in the hook path.
    let mut had_compaction_end = false;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(event.as_ref(), AgentHarnessEvent::CompactionEnd { .. }) {
            had_compaction_end = true;
        }
    }
    assert!(
        !had_compaction_end,
        "hook Compact path with NoValidBoundary under context window must not emit CompactionEnd"
    );
}

/// When a `before_compact` hook returns `Compact` but no valid cut
/// boundary exists, and tokens exceed the context window, auto-compaction
/// must return `Err(NoValidBoundary)`.
#[tokio::test]
async fn auto_compact_hook_compact_no_boundary_errors_over_context_window() {
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
        // response3 won't be consumed — prompt("third") should error.
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

    // Build up context. Assistant entries have usage.output_tokens=0,
    // so only user entries contribute tokens.
    // prompt("first"): path empty → skips. After: [u1(1), a1(0)].
    // prompt("second"): hook Compact, compute_cut_point None,
    //   estimated_tokens=1 <= context_window=1 → silent skip, proceeds. After: [u1, a1, u2, a2].
    h.prompt("first").await.unwrap();
    h.prompt("second").await.unwrap();

    // 3rd prompt: hook returns Compact, compute_cut_point returns None,
    // estimated_tokens=2 > context_window=1 → Err(NoValidBoundary) propagates.
    let result = h.prompt("third").await;
    assert!(
        result.is_err(),
        "prompt should fail when context exceeds window and hook Compact has no valid boundary"
    );
    let err = result.unwrap_err();
    assert!(
        matches!(
            err,
            HarnessError::Compaction(CompactionError::NoValidBoundary)
        ),
        "expected NoValidBoundary error, got: {err:?}"
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

#[tokio::test]
async fn circuit_breaker_trips_after_max_failures() {
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

    // Use a compaction client that always fails.
    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("reply1"),
        MockResponse::text("reply2"),
        MockResponse::text("reply3"),
        MockResponse::text("reply4"),
    ]));
    // Compaction client that always returns an error.
    let failing_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::rate_limit_error(),
        MockResponse::rate_limit_error(),
        MockResponse::rate_limit_error(),
    ]));

    let env = Arc::new(NoOpEnv);
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.model_info = Some(ModelInfo {
        context_window: 100_000,
        max_tokens: 50,
    });
    opts.compaction_reserve_tokens = Some(0);
    opts.compaction_keep_recent_tokens = Some(0);
    opts.compaction_max_failures = Some(3);
    opts.auto_compact = true;
    opts.compaction_model = Some(CompactionModelConfig {
        client: failing_client as Arc<dyn LlmClient>,
        model: "failing-model".into(),
        model_info: ModelInfo {
            context_window: 200_000,
            max_tokens: 8192,
        },
    });
    opts.hooks = HarnessHooks {
        before_compact: vec![Arc::new(CompactDecisionHook)],
        ..HarnessHooks::none()
    };
    let h = AgentHarness::new_in_memory(main_client as Arc<dyn LlmClient>, env, opts).await;

    // msg1: empty path → no compaction attempt.
    // msg2: path has 2 entries → NoValidBoundary (not a failure, not counted).
    // msg3: path has 4 entries → SummaryFailed (count=1, swallowed).
    // msg4: path has 6 entries → SummaryFailed (count=2, swallowed).
    // msg5: path has 8 entries → SummaryFailed (count=3) → circuit trips.
    h.prompt("msg1").await.unwrap();
    h.prompt("msg2").await.unwrap();
    h.prompt("msg3").await.unwrap();
    h.prompt("msg4").await.unwrap();

    let err = h.prompt("msg5").await;
    assert!(
        matches!(
            &err,
            Err(HarnessError::Compaction(CompactionError::CircuitOpen(3)))
        ),
        "expected CircuitOpen after 3 failures, got: {err:?}"
    );
}

/// Integration test: setting `compaction_prompt` and `compaction_query` on
/// `AgentHarnessOptions` must propagate through the full compaction pathway —
/// the compaction LLM receives a request whose rendered user content contains
/// both the query text and the conversation text, proving the custom prompt
/// template (not the hardcoded constants) was used end-to-end.
#[tokio::test]
async fn custom_prompt_renders_query_and_conversation() {
    use crate::compaction::CompactionPromptSpec;
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
        "summary with preserved content",
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
    opts.compaction_prompt = Some(
        CompactionPromptSpec::new(
            "You are a summarizer.",
            "Query: {query}\nConversation:\n{conversation}",
        )
        .unwrap(),
    );
    opts.compaction_query = Some("test query".into());
    opts.hooks = HarnessHooks {
        before_compact: vec![Arc::new(CompactDecisionHook)],
        ..HarnessHooks::none()
    };
    let h = AgentHarness::new_in_memory(main_client as Arc<dyn LlmClient>, env, opts).await;

    // Two prompts → 4 entries (user, assistant, user, assistant).
    // A single prompt yields only 2 entries, for which `compute_cut_point`
    // returns `None` (the only valid boundary is the first entry, which
    // equals `prev_first_kept` → no compressible range). Two prompts give
    // a valid cut boundary between the two turns.
    h.prompt("hello").await.unwrap();
    h.prompt("world").await.unwrap();

    // Manually compact.
    let stats = h.compact().await.unwrap();
    assert!(stats.tokens_before > 0);

    // Verify the compaction client received the custom prompt with query and conversation.
    let captured = compaction_client.captured_requests.lock();
    assert!(!captured.is_empty(), "compaction LLM must have been called");
    let req = &captured[0];
    let user_content = format!("{:?}", req.messages().get(1).expect("has user message"));
    assert!(
        user_content.contains("test query"),
        "custom prompt must contain query"
    );
    assert!(
        user_content.contains("hello"),
        "custom prompt must contain conversation text"
    );
}

/// End-to-end test: a Keep-marked entry in the compressed region must have
/// its content structurally preserved in the compaction summary — appended
/// verbatim after the LLM-generated summary, independent of LLM compliance
/// with `<no-compact>` instructions.
#[tokio::test]
async fn keep_entry_content_preserved_in_summary_end_to_end() {
    use crate::session::CompactionPolicy;
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
    // The summary LLM returns a minimal summary — it does NOT include the
    // Keep entry's content. The structural preservation must add it.
    let compaction_client = Arc::new(MockLlmClient::new(vec![MockResponse::text(
        "LLM summary text",
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
    let h = AgentHarness::new_in_memory(main_client as Arc<dyn LlmClient>, env, opts).await;

    // First prompt to establish a turn.
    h.prompt("hello").await.unwrap();

    // Insert a Keep-marked user message into the session.
    h.session
        .append_with_policy(
            SessionEntryPayload::Message(AgentMessage::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: "CRITICAL: deploy to prod after compaction".into(),
                }],
                timestamp: chrono::Utc::now(),
            })),
            Some(CompactionPolicy::Keep),
        )
        .await
        .unwrap();

    // Second prompt to create a valid cut boundary.
    h.prompt("world").await.unwrap();

    // Manually compact.
    h.compact().await.unwrap();

    // The compaction summary in the session must contain both the LLM
    // summary AND the verbatim Keep entry content.
    let path = h.session.read_active_path().await.unwrap();
    let summary = path
        .iter()
        .find_map(|e| match &e.payload {
            SessionEntryPayload::Compaction(c) => match &c.summary_message {
                AgentMessage::CompactionSummary(cs) => Some(cs.summary.as_str()),
                _ => None,
            },
            _ => None,
        })
        .expect("compaction summary must exist");

    assert!(
        summary.contains("LLM summary text"),
        "summary must contain the LLM-generated text"
    );
    assert!(
        summary.contains("CRITICAL: deploy to prod after compaction"),
        "Keep entry content must be structurally preserved in summary"
    );
    assert!(
        summary.contains("Preserved Content"),
        "summary must have the preserved content section header"
    );

    // The LLM request must contain the <no-compact> tag wrapping the Keep
    // entry's content in the {conversation} placeholder.
    let captured = compaction_client.captured_requests.lock();
    let req = &captured[0];
    let user_content = format!("{:?}", req.messages().get(1).expect("has user message"));
    assert!(
        user_content.contains("<no-compact"),
        "LLM request must contain <no-compact> tag wrapping Keep entry"
    );
    assert!(
        user_content.contains("CRITICAL: deploy to prod after compaction"),
        "LLM request must contain Keep entry content in conversation"
    );
}
