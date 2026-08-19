//! 验证：主 agent abort 后，已派发的异步 sub-agent 被级联取消。
//!
//! 行为预期（abort 级联实现后）：
//! - `MessageBus` 在 `track_with_oneshot()` 时存入 sub-agent 的 abort token。
//! - 主 agent `abort()` 经 `AbortCascadeHook::cancel_all_subagents()` 取消所有
//!   in-flight sub-agent 的 abort token。
//! - 测试通过独立 entry signal 确认 sub-agent 已进入 BlockingTool，再触发
//!   主 agent abort；sub-agent join 返回 `Err(Aborted)`，in_flight 因 abort 归零。

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use llm_harness_agent::HarnessBuilder;
use llm_harness_agent::Plugin;
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
use llm_harness_runtime::spawn::delivery::async_spawn_pair;
use llm_harness_runtime::spawn::message_bus::message_bus_pair;
use llm_harness_runtime::spawn::spawner::{HarnessSubAgentSpawner, JsonlSessionFactory};
use llm_harness_runtime::spawn::tools::SpawnAgentTool;
use llm_harness_sandbox::{OsEnv, OsEnvFactory};
use llm_harness_types::{ExecutionEnv, HarnessError, HarnessPhase};
use tokio::sync::Notify;

const TOOL_ENTRY_TIMEOUT: Duration = Duration::from_secs(2);
const PROMPT_COMPLETION_TIMEOUT: Duration = Duration::from_secs(2);

/// 一个长跑 tool：sleep 直到完成或被 abort。
/// 用于让 sub-agent 在 tool 执行阶段阻塞，使 abort 可在 sub-agent in-flight 时触发。
struct BlockingTool {
    entered: Arc<Notify>,
}

impl llm_harness_types::Tool for BlockingTool {
    fn name(&self) -> &str {
        "block_forever"
    }

    fn description(&self) -> &str {
        "Blocks for a long time; used to test abort cancellation."
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            })
        })
    }

    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        ctx: &'a llm_harness_types::ToolContext,
    ) -> futures::future::BoxFuture<
        'a,
        Result<llm_harness_types::ToolResult, llm_harness_types::ToolFailure>,
    > {
        Box::pin(async move {
            self.entered.notify_one();
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(30)) => {
                    Ok(llm_harness_types::ToolResult::full(
                        vec![llm_harness_types::DataBlock::text("done")],
                        serde_json::Value::Null,
                        false,
                    ))
                }
                _ = ctx.abort.cancelled() => {
                    Err(llm_harness_types::ToolFailure::aborted())
                }
            }
        })
    }
}

/// sub-agent plugin：注册 BlockingTool。
struct BlockingToolPlugin {
    entered: Arc<Notify>,
}

impl Plugin for BlockingToolPlugin {
    fn name(&self) -> &str {
        "blocking-tool"
    }
    fn register_tools(&self, tools: &mut Vec<Arc<dyn llm_harness_types::Tool>>) {
        tools.push(Arc::new(BlockingTool {
            entered: self.entered.clone(),
        }));
    }
}

async fn wait_for_blocking_tool_entry(entered: &Notify) {
    tokio::time::timeout(TOOL_ENTRY_TIMEOUT, entered.notified())
        .await
        .expect("BlockingTool was not entered before the deadline");
}

async fn expect_prompt_task_success(
    task: tokio::task::JoinHandle<Result<(), HarnessError>>,
    context: &str,
) {
    match tokio::time::timeout(PROMPT_COMPLETION_TIMEOUT, task).await {
        Err(_) => panic!("{context} timed out"),
        Ok(Err(error)) => panic!("{context} task failed to join: {error}"),
        Ok(Ok(Err(error))) => panic!("{context} returned HarnessError: {error}"),
        Ok(Ok(Ok(()))) => {}
    }
}

async fn expect_prompt_success(
    prompt: impl Future<Output = Result<(), HarnessError>>,
    context: &str,
) {
    match tokio::time::timeout(PROMPT_COMPLETION_TIMEOUT, prompt).await {
        Err(_) => panic!("{context} timed out"),
        Ok(Err(error)) => panic!("{context} returned HarnessError: {error}"),
        Ok(Ok(())) => {}
    }
}

/// 场景：主 agent spawn 异步 sub-agent 后，主、子 agent 分别阻塞在独立的
/// BlockingTool 中。此时 abort 同时取消 active main run 和 in-flight sub-agent。
/// 断言：两条执行路径都在远短于 30s sleep 的时间内终止。
#[tokio::test]
async fn abort_cancels_inflight_subagent() {
    let main_tool_entered = Arc::new(Notify::new());
    let sub_tool_entered = Arc::new(Notify::new());
    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use(
            "call_1",
            "spawn_agent",
            r#"{"prompt":"compute","wait":false}"#,
        ),
        MockResponse::tool_use("call_2", "block_forever", "{}"),
    ]));

    // sub-agent：第一条响应调用 block_forever tool（会阻塞直到被 cancel）
    let sub_client = Arc::new(MockLlmClient::new(vec![MockResponse::tool_use(
        "sub_call_1",
        "block_forever",
        "{}",
    )]));

    let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new(std::env::current_dir().unwrap()));
    let session_dir = tempfile::tempdir().expect("create isolated session directory");

    let registry = message_bus_pair();
    let async_hook = async_spawn_pair(registry.clone());
    let plugin_tool_entered = sub_tool_entered.clone();
    let spawner = Arc::new(
        HarnessSubAgentSpawner::new(
            "sub-model",
            sub_client.clone(),
            session_dir.path().join("sub-sessions"),
            registry.clone(),
            move |_cwd, _bus, _agent_id| {
                Box::new(BlockingToolPlugin {
                    entered: plugin_tool_entered.clone(),
                })
            },
        )
        .session_factory(Arc::new(JsonlSessionFactory))
        .env_factory(Arc::new(OsEnvFactory)),
    );

    let harness = HarnessBuilder::new("main-model")
        .provider("main-model", main_client.clone())
        .tool(Arc::new(SpawnAgentTool::new(spawner.clone())))
        .tool(Arc::new(BlockingTool {
            entered: main_tool_entered.clone(),
        }))
        .after_turn_hook(async_hook.clone())
        .build(env.clone())
        .await
        .unwrap();
    let harness = Arc::new(harness);

    let watcher = llm_harness_runtime::spawn::delivery::IdleWatcher::new(registry.clone(), {
        let h = Arc::downgrade(&harness);
        Arc::new(move || {
            let h = h.clone();
            Box::pin(async move {
                if let Some(h) = h.upgrade() {
                    let _ = h.continue_run().await;
                }
            })
        })
    });
    harness.set_idle_watcher(Arc::new(watcher));
    harness.set_async_spawn_hook(async_hook.clone());
    harness.set_abort_cascade_hook(registry.clone());

    let h_clone = harness.clone();
    let main_task = tokio::spawn(async move { h_clone.prompt("please spawn a sub-agent").await });

    // 两个独立 signal 证明主 run 与 sub-agent 都已进入各自的 tool 阶段。
    wait_for_blocking_tool_entry(&sub_tool_entered).await;
    wait_for_blocking_tool_entry(&main_tool_entered).await;
    assert_eq!(
        harness.state().phase,
        HarnessPhase::Turning,
        "main Harness should still have an active run"
    );
    assert_eq!(
        registry.in_flight_count(),
        1,
        "sub-agent should be in-flight (blocked in tool)"
    );

    // abort 同时取消 active main run 与 in-flight sub-agent。
    harness.abort();
    expect_prompt_task_success(main_task, "active main prompt after abort cascade").await;

    // sub-agent 被 cancel 后 BlockingTool 返回 Aborted，sub-agent 终止，in_flight 归零。
    // 这必须在远短于 30s（BlockingTool 的 sleep）的时间内发生，证明是 cancel 而非完成。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if registry.in_flight_count() == 0 {
            break;
        }
        if std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        registry.in_flight_count(),
        0,
        "sub-agent should have been cancelled (in_flight=0) well before the 30s sleep"
    );
    assert_eq!(
        harness.state().phase,
        HarnessPhase::Idle,
        "main Harness should return to Idle after abort"
    );
    assert_eq!(
        main_client
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "main client should issue spawn_agent and main block_forever exactly once"
    );
    assert_eq!(
        sub_client
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "sub-agent client should issue block_forever exactly once"
    );
}

/// 场景：主轮次正常结束、异步 sub-agent 仍阻塞在 tool 中时执行 abort，随后再次
/// prompt + spawn。证明级联只取消当前 in-flight sub-agent，不污染后续 spawn。
#[tokio::test]
async fn abort_does_not_affect_future_spawns() {
    let tool_entered = Arc::new(Notify::new());
    let main_client = Arc::new(MockLlmClient::new(vec![
        // 第一轮：spawn 慢 sub-agent，然后显式结束本轮，避免测试调度
        // 决定第二个 spawn 是由本轮 continuation 还是下一次 prompt 消费。
        MockResponse::tool_use(
            "call_1",
            "spawn_agent",
            r#"{"prompt":"compute","wait":false}"#,
        ),
        MockResponse::text("first spawn dispatched"),
        // 第二轮：spawn 快 sub-agent
        MockResponse::tool_use(
            "call_2",
            "spawn_agent",
            r#"{"prompt":"compute again","wait":false}"#,
        ),
        MockResponse::text("second spawn dispatched"),
    ]));

    // sub-agent client 共享：第一个 sub-agent 取走第一条响应（block_forever），
    // 第二个 sub-agent 取走第二条响应（text，直接返回不调 tool）。
    let sub_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use("sub_call_1", "block_forever", "{}"),
        MockResponse::text("second sub-agent done"),
    ]));

    let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new(std::env::current_dir().unwrap()));
    let session_dir = tempfile::tempdir().expect("create isolated session directory");

    let registry = message_bus_pair();
    let async_hook = async_spawn_pair(registry.clone());
    let plugin_tool_entered = tool_entered.clone();
    let spawner = Arc::new(
        HarnessSubAgentSpawner::new(
            "sub-model",
            sub_client.clone(),
            session_dir.path().join("sub-sessions"),
            registry.clone(),
            move |_cwd, _bus, _agent_id| {
                Box::new(BlockingToolPlugin {
                    entered: plugin_tool_entered.clone(),
                })
            },
        )
        .session_factory(Arc::new(JsonlSessionFactory))
        .env_factory(Arc::new(OsEnvFactory)),
    );

    let harness = HarnessBuilder::new("main-model")
        .provider("main-model", main_client.clone())
        .tool(Arc::new(SpawnAgentTool::new(spawner.clone())))
        .after_turn_hook(async_hook.clone())
        .build(env.clone())
        .await
        .unwrap();
    let harness = Arc::new(harness);

    let watcher = llm_harness_runtime::spawn::delivery::IdleWatcher::new(registry.clone(), {
        let h = Arc::downgrade(&harness);
        Arc::new(move || {
            let h = h.clone();
            Box::pin(async move {
                if let Some(h) = h.upgrade() {
                    let _ = h.continue_run().await;
                }
            })
        })
    });
    harness.set_idle_watcher(Arc::new(watcher));
    harness.set_async_spawn_hook(async_hook.clone());
    harness.set_abort_cascade_hook(registry.clone());

    // 第一轮：spawn 慢 sub-agent
    let h_clone = harness.clone();
    let main_task = tokio::spawn(async move { h_clone.prompt("please spawn a sub-agent").await });

    // 必须等第一个 sub-agent 消费首个 mock 响应并进入 BlockingTool。
    wait_for_blocking_tool_entry(&tool_entered).await;
    assert_eq!(
        registry.in_flight_count(),
        1,
        "first sub-agent should be in-flight"
    );
    expect_prompt_task_success(main_task, "first main prompt").await;
    assert_eq!(
        harness.state().phase,
        HarnessPhase::Idle,
        "first main round should finish before abort"
    );
    assert_eq!(
        main_client
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "first main round should consume spawn plus terminal text"
    );
    assert_eq!(
        registry.in_flight_count(),
        1,
        "first sub-agent should still be running after the main round ends"
    );

    // 主轮次已经结束，但第一个 sub-agent 仍在 tool 中；abort 只取消它。
    harness.abort();

    // 等待第一个 sub-agent 终止
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if registry.in_flight_count() == 0 {
            break;
        }
        if std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        registry.in_flight_count(),
        0,
        "first sub-agent should be cancelled"
    );

    // 第二轮：再次 prompt，spawn 第二个 sub-agent（快，直接返回文本）
    expect_prompt_success(
        harness.prompt("please spawn another sub-agent"),
        "second main prompt",
    )
    .await;
    assert_eq!(
        main_client
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst),
        4,
        "second main round should consume the second spawn plus terminal text"
    );

    // 第二个 sub-agent 应正常完成（in_flight 归零因完成而非取消）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if registry.in_flight_count() == 0 {
            break;
        }
        if std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        registry.in_flight_count(),
        0,
        "second sub-agent should have completed normally"
    );

    // 恰好两次：第一个 block_forever + 第二个 text。重复调用也应失败。
    let sub_calls = sub_client
        .call_count
        .load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        sub_calls, 2,
        "sub-agents should consume exactly two responses"
    );
}

/// 场景：主 agent spawn sub-agent 后进入 Idle（IdleWatcher 运行中），
/// 此时 abort → watcher 被 cancel + sub-agent 被 cancel。
/// 断言：abort 后主 agent 不被 aborted completion 重新唤醒
/// （main_client 的 call_count 保持 2，不会因 follow_up 续命）。
#[tokio::test]
async fn idle_watcher_abort() {
    let tool_entered = Arc::new(Notify::new());
    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use(
            "call_1",
            "spawn_agent",
            r#"{"prompt":"compute","wait":false}"#,
        ),
        // 第二次 LLM 调用（spawn tool 返回后）拿到默认 "(no more mock responses)"
        // 文本 → EndTurn → Idle。若 watcher 错误唤醒，会有第三次调用（call_count=3）。
    ]));

    let sub_client = Arc::new(MockLlmClient::new(vec![MockResponse::tool_use(
        "sub_call_1",
        "block_forever",
        "{}",
    )]));

    let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new(std::env::current_dir().unwrap()));
    let session_dir = tempfile::tempdir().expect("create isolated session directory");

    let registry = message_bus_pair();
    let async_hook = async_spawn_pair(registry.clone());
    let plugin_tool_entered = tool_entered.clone();
    let spawner = Arc::new(
        HarnessSubAgentSpawner::new(
            "sub-model",
            sub_client.clone(),
            session_dir.path().join("sub-sessions"),
            registry.clone(),
            move |_cwd, _bus, _agent_id| {
                Box::new(BlockingToolPlugin {
                    entered: plugin_tool_entered.clone(),
                })
            },
        )
        .session_factory(Arc::new(JsonlSessionFactory))
        .env_factory(Arc::new(OsEnvFactory)),
    );

    let harness = HarnessBuilder::new("main-model")
        .provider("main-model", main_client.clone())
        .tool(Arc::new(SpawnAgentTool::new(spawner.clone())))
        .after_turn_hook(async_hook.clone())
        .build(env.clone())
        .await
        .unwrap();
    let harness = Arc::new(harness);

    let watcher = llm_harness_runtime::spawn::delivery::IdleWatcher::new(registry.clone(), {
        let h = Arc::downgrade(&harness);
        Arc::new(move || {
            let h = h.clone();
            Box::pin(async move {
                if let Some(h) = h.upgrade() {
                    let _ = h.continue_run().await;
                }
            })
        })
    });
    harness.set_idle_watcher(Arc::new(watcher));
    harness.set_async_spawn_hook(async_hook.clone());
    harness.set_abort_cascade_hook(registry.clone());

    let h_clone = harness.clone();
    let main_task = tokio::spawn(async move { h_clone.prompt("please spawn a sub-agent").await });

    // 先证明 sub-agent 已进入 tool 阶段，再等待主 agent 进入 Idle。
    wait_for_blocking_tool_entry(&tool_entered).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if harness.state().phase == HarnessPhase::Idle {
            break;
        }
        if std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        registry.in_flight_count(),
        1,
        "sub-agent should be in-flight"
    );
    assert_eq!(
        harness.state().phase,
        HarnessPhase::Idle,
        "main agent should be idle before testing watcher cancellation"
    );
    expect_prompt_task_success(main_task, "idle-watcher main prompt").await;

    // 此时主 agent 在 Idle（spawn tool 返回后 LLM 第二 turn 无响应 → Idle）
    // watcher 正在监听 completion channel。abort 应 cancel watcher + sub-agent。
    harness.abort();

    // 等待 sub-agent 终止
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if registry.in_flight_count() == 0 {
            break;
        }
        if std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        registry.in_flight_count(),
        0,
        "sub-agent should be cancelled"
    );

    // 关键断言：主 agent 没有被 aborted completion 重新唤醒。
    // main_client 只有 1 条预设响应（spawn tool_use）。spawn tool 返回后
    // 主 agent 第二次调 LLM 拿到默认 "(no more mock responses)" 文本 → Idle。
    // 正常流程 call_count=2。若 watcher 错误唤醒（aborted completion 触发
    // continue_run），会有第三次 LLM 调用（call_count=3）。
    // 给一秒钟观察是否有延迟的 continue_run。
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let main_calls = main_client
        .call_count
        .load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        main_calls, 2,
        "main agent should not be re-woken after abort (call_count={main_calls})"
    );
}
