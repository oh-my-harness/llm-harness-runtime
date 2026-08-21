use super::*;

#[tokio::test]
async fn prompt_completes_and_returns_idle() {
    let h = make_harness(vec![MockResponse::text("Hello!")]);
    h.prompt("hi").await.unwrap();
    assert_eq!(h.state().phase, HarnessPhase::Idle);
}

#[tokio::test]
async fn prompt_persists_final_answer_identity() {
    let h = make_harness(vec![MockResponse::text("Hello!")]);
    h.prompt("hi").await.unwrap();

    let path = h.read_active_path().await.unwrap();
    let assistant = path
        .iter()
        .find_map(|entry| {
            if let SessionEntryPayload::Message(AgentMessage::Assistant(message)) = &entry.payload {
                Some(message)
            } else {
                None
            }
        })
        .expect("assistant message should be persisted");

    assert_eq!(assistant.kind, AssistantMessageKind::FinalAnswer);
    assert_eq!(assistant.text_content(), "Hello!");
    assert!(!assistant.message_id.is_empty());
    assert!(!assistant.turn_id.is_empty());
}

#[tokio::test]
async fn rejected_final_answer_is_not_persisted() {
    struct RejectFinalAnswer;

    impl FinalAnswerValidator for RejectFinalAnswer {
        fn validate<'a>(
            &'a self,
            _: FinalAnswerValidationCtx<'a>,
        ) -> BoxFuture<'a, Result<(), FinalAnswerValidationError>> {
            Box::pin(async {
                Err(FinalAnswerValidationError::new(
                    "test_rejection",
                    "candidate rejected",
                ))
            })
        }
    }

    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text(
        "untrusted answer",
    )]));
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.hooks = HarnessHooks {
        final_answer_validator: vec![Arc::new(RejectFinalAnswer)],
        ..HarnessHooks::none()
    };
    let h =
        AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, Arc::new(NoOpEnv), opts).await;

    h.prompt("question").await.unwrap();

    assert!(
        h.read_active_path().await.unwrap().is_empty(),
        "the rejected turn must not leave a user or final-answer session entry"
    );
    assert!(
        h.state()
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("test_rejection"))
    );
    assert!(
        h.state().streaming_message.is_none(),
        "a rejected provisional answer must not remain as the streaming snapshot"
    );
}

#[tokio::test]
async fn prompt_while_running_returns_not_idle() {
    let h = make_harness(vec![]);
    {
        let mut inner = h.inner.lock().unwrap();
        inner.state.phase = HarnessPhase::Turning;
    }
    let err = h.prompt("hi").await;
    assert!(matches!(err, Err(HarnessError::NotIdle(_))));
    h.inner.lock().unwrap().state.phase = HarnessPhase::Idle;
}

#[tokio::test]
async fn append_message_while_turning_returns_not_idle() {
    let h = make_harness(vec![]);
    {
        let mut inner = h.inner.lock().unwrap();
        inner.state.phase = HarnessPhase::Turning;
    }

    let msg = AgentMessage::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: "test".into(),
        }],
        timestamp: chrono::Utc::now(),
    });
    let err = h.append_message(msg).await;

    h.inner.lock().unwrap().state.phase = HarnessPhase::Idle;
    assert!(matches!(err, Err(HarnessError::NotIdle(_))));
}

#[tokio::test]
async fn append_custom_entry_while_turning_returns_not_idle() {
    let h = make_harness(vec![]);
    {
        let mut inner = h.inner.lock().unwrap();
        inner.state.phase = HarnessPhase::Turning;
    }

    let err = h
        .append_custom_entry("test_type".into(), serde_json::json!({}))
        .await;

    h.inner.lock().unwrap().state.phase = HarnessPhase::Idle;
    assert!(matches!(err, Err(HarnessError::NotIdle(_))));
}

#[tokio::test]
async fn session_has_messages_after_prompt() {
    let h = make_harness(vec![MockResponse::text("response")]);
    h.prompt("question").await.unwrap();
    let ctx = h.session.build_context().await.unwrap();
    // At least user message + assistant message.
    assert!(ctx.messages.len() >= 2);
}

#[tokio::test]
async fn subscribe_receives_events() {
    let h = make_harness(vec![MockResponse::text("hi")]);
    let mut rx = h.subscribe();
    h.prompt("hello").await.unwrap();
    assert!(rx.try_recv().is_ok());
}

#[tokio::test]
async fn wait_for_idle_returns_immediately_when_idle() {
    let h = make_harness(vec![]);
    tokio::time::timeout(std::time::Duration::from_millis(100), h.wait_for_idle())
        .await
        .expect("wait_for_idle timed out when already idle");
}

#[tokio::test]
async fn abort_cancels_token() {
    let h = make_harness(vec![]);
    let token = CancellationToken::new();
    h.inner.lock().unwrap().current_abort = Some(token.clone());
    h.abort();
    assert!(token.is_cancelled());
}
#[tokio::test]
async fn abort_invokes_on_abort_hook() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingHook(Arc<AtomicUsize>);
    impl OnAbortHook for CountingHook {
        fn on_abort(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let counter = Arc::new(AtomicUsize::new(0));
    let client = Arc::new(MockLlmClient::new(vec![]));
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.hooks = HarnessHooks {
        on_abort: vec![Arc::new(CountingHook(counter.clone()))],
        ..HarnessHooks::none()
    };
    let h =
        AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, Arc::new(NoOpEnv), opts).await;
    h.abort();
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "abort should call on_abort hook once"
    );
}
#[tokio::test]
async fn abort_invokes_on_abort_hook_twice_after_reabort() {
    // 回归测试：on_abort hooks 是生命周期持久的（Vec<Arc<dyn OnAbortHook>>
    // 遍历不 consume），re-prompt 后再次 abort() 仍触发。
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingHook(Arc<AtomicUsize>);
    impl OnAbortHook for CountingHook {
        fn on_abort(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let counter = Arc::new(AtomicUsize::new(0));
    let client = Arc::new(MockLlmClient::new(vec![]));
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.hooks = HarnessHooks {
        on_abort: vec![Arc::new(CountingHook(counter.clone()))],
        ..HarnessHooks::none()
    };
    let h =
        AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, Arc::new(NoOpEnv), opts).await;
    // 第一次 abort
    let token = CancellationToken::new();
    h.inner.lock().unwrap().current_abort = Some(token.clone());
    h.abort();
    assert_eq!(counter.load(Ordering::SeqCst), 1, "first abort");
    // re-prompt 后再次 abort
    let token2 = CancellationToken::new();
    h.inner.lock().unwrap().current_abort = Some(token2.clone());
    h.abort();
    assert_eq!(counter.load(Ordering::SeqCst), 2, "second abort");
}
#[tokio::test]
async fn after_run_hook_called_after_run_loop() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingHook(Arc<AtomicUsize>);
    impl AfterRunHook for CountingHook {
        fn after_run(&self) -> futures::future::BoxFuture<'_, ()> {
            let c = self.0.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
            })
        }
    }
    let counter = Arc::new(AtomicUsize::new(0));
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("hello")]));
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.hooks = HarnessHooks {
        after_run: vec![Arc::new(CountingHook(counter.clone()))],
        ..HarnessHooks::none()
    };
    let h =
        AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, Arc::new(NoOpEnv), opts).await;
    h.prompt("hi").await.unwrap();
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "after_run hook should be called once after run_loop"
    );
}
#[tokio::test]
async fn abort_emits_aborted_event() {
    // When abort() cancels the run token mid-flight, the agent loop yields
    // Error(Aborted) and drive_loop must emit AgentHarnessEvent::Aborted
    // (terminal sentinel) so consumers can distinguish abort from a crash.
    use tokio::sync::Notify;
    let ready = Arc::new(Notify::new());
    struct BlockingTool {
        schema: serde_json::Value,
        ready: Arc<Notify>,
    }
    impl llm_harness_types::Tool for BlockingTool {
        fn name(&self) -> &str {
            "blocking"
        }
        fn description(&self) -> &str {
            "blocks until aborted"
        }
        fn parameters_schema(&self) -> &serde_json::Value {
            &self.schema
        }
        fn execute<'a>(
            &'a self,
            _args: serde_json::Value,
            ctx: &'a llm_harness_types::ToolContext,
        ) -> futures::future::BoxFuture<
            'a,
            Result<llm_harness_types::ToolResult, llm_harness_types::ToolFailure>,
        > {
            let ready = self.ready.clone();
            Box::pin(async move {
                // Signal that the tool is now blocking on the abort token,
                // then wait for cancellation.
                ready.notify_one();
                ctx.abort.cancelled().await;
                Err(llm_harness_types::ToolFailure::aborted())
            })
        }
    }
    let h = make_harness(vec![MockResponse::tool_use("call_1", "blocking", "{}")]);
    let tool: Arc<dyn llm_harness_types::Tool> = Arc::new(BlockingTool {
        schema: serde_json::json!({"type":"object","properties":{}}),
        ready: ready.clone(),
    });
    h.set_tools(vec![tool]).await.unwrap();
    let mut rx = h.subscribe();
    let h_arc = Arc::new(h);
    let prompt_task = tokio::spawn({
        let h = h_arc.clone();
        async move { h.prompt("call the tool").await }
    });
    // Wait until the tool is blocking on the abort token (deterministic),
    // then abort.
    ready.notified().await;
    h_arc.abort();
    // Collect events until a terminal sentinel arrives.
    let mut saw_aborted = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if matches!(ev.as_ref(), AgentHarnessEvent::Aborted) {
            saw_aborted = true;
            break;
        }
    }
    let _ = prompt_task.await;
    assert!(
        saw_aborted,
        "abort() should emit AgentHarnessEvent::Aborted sentinel"
    );
    // Abort is a signal, not a failure: error_message must not be set.
    assert!(
        h_arc.state().error_message.is_none(),
        "abort must not leave error_message set (it is a signal, not an error)"
    );
}

#[tokio::test]
async fn continue_run_returns_not_idle_when_turning() {
    let h = make_harness(vec![]);
    {
        let mut inner = h.inner.lock().unwrap();
        inner.state.phase = HarnessPhase::Turning;
    }
    let err = h.continue_run().await.unwrap_err();
    assert!(matches!(err, HarnessError::NotIdle(_)));
}

#[tokio::test]
async fn continue_run_drives_loop_with_empty_initial() {
    let h = make_harness(vec![
        MockResponse::text("first"),
        MockResponse::text("handled follow-up"),
    ]);
    h.prompt("initial").await.unwrap(); // 消费 "first"，phase 回到 Idle
    // Idle 阶段调用 follow_up_message 返回 Err(Closed)——follow_up_rx 已被
    // reset_channels drop。continue_run 本身会驱动 LLM 调用。
    let result = h.follow_up_message(AgentMessage::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: "please respond".into(),
        }],
        timestamp: chrono::Utc::now(),
    }));
    assert_eq!(result, Err(ChannelError::Closed));
    h.continue_run().await.unwrap(); // 应消费 "handled follow-up"
    let ctx = h.session.build_context().await.unwrap();
    let assistant_count = ctx
        .messages
        .iter()
        .filter(|m| matches!(m, AgentMessage::Assistant(_)))
        .count();
    assert_eq!(assistant_count, 2);
}

#[tokio::test]
async fn continue_run_calls_before_run_hook() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingHook(Arc<AtomicUsize>);
    impl BeforeRunHook for CountingHook {
        fn before_run<'a>(
            &'a self,
            _ctx: llm_harness_types::BeforeRunCtx<'a>,
        ) -> futures::future::BoxFuture<
            'a,
            Result<llm_harness_types::BeforeRunResult, llm_harness_types::AgentError>,
        > {
            let c = self.0.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(llm_harness_types::BeforeRunResult {
                    additional_messages: vec![],
                    system_prompt: None,
                })
            })
        }
    }
    let counter = Arc::new(AtomicUsize::new(0));
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("first"),
        MockResponse::text("second"),
    ]));
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.hooks = HarnessHooks {
        before_run: vec![Arc::new(CountingHook(counter.clone()))],
        ..HarnessHooks::none()
    };
    let h =
        AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, Arc::new(NoOpEnv), opts).await;
    h.prompt("hello").await.unwrap(); // before_run called once
    h.continue_run().await.unwrap(); // before_run called again
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "before_run should be called for both prompt() and continue_run()"
    );
}
