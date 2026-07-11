use super::*;

#[tokio::test]
async fn next_turn_injected_on_next_prompt() {
    let h = make_harness(vec![
        MockResponse::text("first"),
        MockResponse::text("second"),
    ]);
    h.prompt("hello").await.unwrap();
    h.next_turn("injected context");
    h.prompt("next question").await.unwrap();
    let ctx = h.session.build_context().await.unwrap();
    // Should have messages from both runs.
    assert!(ctx.messages.len() >= 4);
}

#[tokio::test]
async fn set_active_tools_filters_to_subset() {
    let h = make_harness(vec![]);
    let active: HashSet<String> = vec!["grep".to_string()].into_iter().collect();
    h.set_active_tools(Some(active.clone())).await.unwrap();
    assert_eq!(h.state().active_tools, Some(active));
}

#[tokio::test]
async fn clear_all_queues_empties_next_turn() {
    let h = make_harness(vec![]);
    h.next_turn("msg");
    h.clear_all_queues().unwrap();
    assert!(!h.has_queued_messages());
}

#[tokio::test]
async fn clear_queues_rejected_when_not_idle() {
    let h = make_harness(vec![]);
    // 强制进入非 Idle 阶段
    h.inner.lock().unwrap().state.phase = HarnessPhase::Turning;
    assert!(matches!(
        h.clear_steering_queue(),
        Err(HarnessError::NotIdle(HarnessPhase::Turning))
    ));
    assert!(matches!(
        h.clear_follow_up_queue(),
        Err(HarnessError::NotIdle(HarnessPhase::Turning))
    ));
    assert!(matches!(
        h.clear_all_queues(),
        Err(HarnessError::NotIdle(HarnessPhase::Turning))
    ));
    // 恢复 Idle 供后续清理
    h.inner.lock().unwrap().state.phase = HarnessPhase::Idle;
}

/// A `ShouldStopHook` that coordinates with the test on its **first** call:
/// signals `entered`, waits for `inject` before returning `true`. On
/// subsequent calls it returns `true` immediately. This eliminates timing
/// races — the follow_up is injected while should_stop is held, exactly
/// how real hooks (`AsyncSpawnHook`, `PeerMessageHook`) behave.
struct CoordinatingShouldStop {
    entered: Arc<tokio::sync::Notify>,
    inject: Arc<tokio::sync::Notify>,
    call_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl ShouldStopHook for CoordinatingShouldStop {
    fn should_stop<'a>(&'a self, _ctx: ShouldStopCtx<'a>) -> futures::future::BoxFuture<'a, bool> {
        let entered = self.entered.clone();
        let inject = self.inject.clone();
        let count = self.call_count.clone();
        Box::pin(async move {
            if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                entered.notify_one();
                inject.notified().await;
            }
            true
        })
    }
}

#[tokio::test]
async fn follow_up_handle_injects_message_into_loop() {
    // The loop calls should_stop after the first assistant turn. Our
    // coordinating hook signals entry, then waits for the test to inject
    // a follow_up via FollowUpHandle before returning true. The loop then
    // checks follow_up_rx, finds the message, and continues 'main —
    // consuming the second mock response.
    let entered = Arc::new(tokio::sync::Notify::new());
    let inject = Arc::new(tokio::sync::Notify::new());
    let hook = Arc::new(CoordinatingShouldStop {
        entered: entered.clone(),
        inject: inject.clone(),
        call_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });

    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("first"),
        MockResponse::text("saw follow-up"),
    ]));
    let env = Arc::new(NoOpEnv);
    let opts = AgentHarnessOptions {
        hooks: HarnessHooks {
            should_stop: vec![hook],
            ..HarnessHooks::none()
        },
        ..AgentHarnessOptions::new("test-model")
    };
    let h = Arc::new(AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, env, opts).await);

    let handle = h.follow_up_handle();
    let prompt_task = tokio::spawn({
        let h = h.clone();
        async move { h.prompt("say first").await }
    });

    // Wait until the loop enters should_stop, then inject via the handle.
    entered.notified().await;
    handle.follow_up_message(AgentMessage::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: "please say 'saw follow-up'".into(),
        }],
        timestamp: chrono::Utc::now(),
    }));
    inject.notify_one();

    prompt_task.await.unwrap().unwrap();

    // The follow_up must have triggered a second turn consuming the second
    // mock response — so the session now holds two assistant messages.
    let ctx = h.session.build_context().await.unwrap();
    let assistant_count = ctx
        .messages
        .iter()
        .filter(|m| matches!(m, AgentMessage::Assistant(_)))
        .count();
    assert_eq!(
        assistant_count, 2,
        "follow_up should trigger a second turn consuming the second mock response"
    );
}
