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
async fn abort_invokes_cascade_hook() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingHook(Arc<AtomicUsize>);
    impl AbortCascadeHook for CountingHook {
        fn cancel_all_subagents(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let h = make_harness(vec![]);
    let counter = Arc::new(AtomicUsize::new(0));
    h.set_abort_cascade_hook(Arc::new(CountingHook(counter.clone())));
    h.abort();
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "abort should call cascade hook once"
    );
}
#[tokio::test]
async fn abort_invokes_cascade_hook_twice_after_reabort() {
    // 回归测试：abort_cascade_hook 是生命周期持久配置（plan: "abort 不终止
    // agent 生命周期，可再被 prompt"）。take() 会消耗 hook 导致第二次 abort
    // 静默跳过级联；必须用 clone() 保留。
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingHook(Arc<AtomicUsize>);
    impl AbortCascadeHook for CountingHook {
        fn cancel_all_subagents(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let h = make_harness(vec![]);
    let counter = Arc::new(AtomicUsize::new(0));
    h.set_abort_cascade_hook(Arc::new(CountingHook(counter.clone())));
    // 模拟一次运行中的 abort。
    let token = CancellationToken::new();
    h.inner.lock().unwrap().current_abort = Some(token.clone());
    h.abort();
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "first abort should call cascade hook"
    );
    // re-prompt 后再次 abort（plan 支持的场景），hook 应仍被调用。
    let token2 = CancellationToken::new();
    h.inner.lock().unwrap().current_abort = Some(token2.clone());
    h.abort();
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "second abort should STILL call cascade hook (hook is lifetime-persistent)"
    );
}
#[tokio::test]
async fn abort_cancels_idle_watcher() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingWatcher(Arc<AtomicUsize>);
    impl IdleWatcherHook for CountingWatcher {
        fn on_idle(&self) {}
        fn cancel(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let h = make_harness(vec![]);
    let counter = Arc::new(AtomicUsize::new(0));
    h.set_idle_watcher(Arc::new(CountingWatcher(counter.clone())));
    h.abort();
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "abort should cancel idle watcher"
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
            Result<llm_harness_types::ToolResult, llm_harness_types::ToolError>,
        > {
            let ready = self.ready.clone();
            Box::pin(async move {
                // Signal that the tool is now blocking on the abort token,
                // then wait for cancellation.
                ready.notify_one();
                ctx.abort.cancelled().await;
                Err(llm_harness_types::ToolError::Aborted)
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
    // 注入 follow_up
    h.follow_up_message(AgentMessage::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: "please respond".into(),
        }],
        timestamp: chrono::Utc::now(),
    }));
    h.continue_run().await.unwrap(); // 应消费 "handled follow-up"
    let ctx = h.session.build_context().await.unwrap();
    let assistant_count = ctx
        .messages
        .iter()
        .filter(|m| matches!(m, AgentMessage::Assistant(_)))
        .count();
    assert_eq!(assistant_count, 2);
}
