//! 验证：主 agent abort 后，已派发的异步 sub-agent 被级联取消。
//!
//! 行为预期（abort 级联实现后）：
//! - `MessageBus` 在 `track_with_oneshot()` 时存入 sub-agent 的 abort token。
//! - 主 agent `abort()` 经 `AbortCascadeHook::cancel_all_subagents()` 取消所有
//!   in-flight sub-agent 的 abort token。
//! - 因此：主 agent abort 后，sub-agent 的 LLM 不会被调用（或调用被中断），
//!   sub-agent join 返回 `Err(Aborted)`，in_flight 因 abort 归零。

use std::sync::Arc;

use llm_harness_agent::Plugin;
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
use llm_harness_runtime::builder::HarnessBuilder;
use llm_harness_runtime::spawn::delivery::async_spawn_pair;
use llm_harness_runtime::spawn::message_bus::message_bus_pair;
use llm_harness_runtime::spawn::spawner::{HarnessSubAgentSpawner, JsonlSessionFactory};
use llm_harness_runtime::spawn::tools::SpawnAgentTool;
use llm_harness_runtime_sandbox_os::{OsEnv, OsEnvFactory};
use llm_harness_types::{ExecutionEnv, HarnessPhase};
/// 一个长跑 tool：sleep 直到完成或被 abort。
/// 用于让 sub-agent 在 tool 执行阶段阻塞，使 abort 可在 sub-agent in-flight 时触发。
struct BlockingTool;

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
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
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
struct BlockingToolPlugin;
impl Plugin for BlockingToolPlugin {
    fn name(&self) -> &str {
        "blocking-tool"
    }
    fn register_tools(&self, tools: &mut Vec<Arc<dyn llm_harness_types::Tool>>) {
        tools.push(Arc::new(BlockingTool));
    }
}
/// 场景：主 agent spawn 异步 sub-agent（sub-agent 在 BlockingTool 中阻塞），
/// 主 agent abort → sub-agent 的 abort token 被 cancel → BlockingTool 返回 Aborted →
/// sub-agent 终止。断言：abort 后 sub-agent 在短时间内终止（in_flight 归零），
/// 远短于 30s sleep，证明是 cancel 而非自然完成。
#[tokio::test]
async fn abort_cancels_inflight_subagent() {
    let main_client = Arc::new(MockLlmClient::new(vec![MockResponse::tool_use(
        "call_1",
        "spawn_agent",
        r#"{"prompt":"compute","wait":false}"#,
    )]));

    // sub-agent：第一条响应调用 block_forever tool（会阻塞直到被 cancel）
    let sub_client = Arc::new(MockLlmClient::new(vec![MockResponse::tool_use(
        "sub_call_1",
        "block_forever",
        "{}",
    )]));

    let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new(std::env::current_dir().unwrap()));
    let session_dir = std::env::temp_dir().join("abort-cascade-inflight-test");
    let _ = std::fs::remove_dir_all(&session_dir);
    std::fs::create_dir_all(&session_dir).unwrap();

    let registry = message_bus_pair();
    let async_hook = async_spawn_pair(registry.clone());
    let spawner = Arc::new(
        HarnessSubAgentSpawner::new(
            "sub-model",
            sub_client.clone(),
            session_dir.join("sub-sessions"),
            registry.clone(),
            |_cwd, _bus, _agent_id| Box::new(BlockingToolPlugin),
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

    // 等待 sub-agent 启动并进入 BlockingTool（in_flight=1）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if registry.in_flight_count() == 1 {
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
        "sub-agent should be in-flight (blocked in tool)"
    );

    // abort 主 agent —— 应级联取消 sub-agent
    harness.abort();
    let _ = main_task.await;

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
}

/// 场景：abort 级联取消当前 in-flight sub-agent 后，再次 prompt + spawn 应正常工作。
/// 证明级联只杀"当前这批"，不影响 registry 后续使用。
#[tokio::test]
async fn abort_does_not_affect_future_spawns() {
    let main_client = Arc::new(MockLlmClient::new(vec![
        // 第一轮：spawn 慢 sub-agent
        MockResponse::tool_use(
            "call_1",
            "spawn_agent",
            r#"{"prompt":"compute","wait":false}"#,
        ),
        // 第二轮：spawn 快 sub-agent
        MockResponse::tool_use(
            "call_2",
            "spawn_agent",
            r#"{"prompt":"compute again","wait":false}"#,
        ),
    ]));

    // sub-agent client 共享：第一个 sub-agent 取走第一条响应（block_forever），
    // 第二个 sub-agent 取走第二条响应（text，直接返回不调 tool）。
    let sub_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use("sub_call_1", "block_forever", "{}"),
        MockResponse::text("second sub-agent done"),
    ]));

    let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new(std::env::current_dir().unwrap()));
    let session_dir = std::env::temp_dir().join("abort-cascade-future-test");
    let _ = std::fs::remove_dir_all(&session_dir);
    std::fs::create_dir_all(&session_dir).unwrap();

    let registry = message_bus_pair();
    let async_hook = async_spawn_pair(registry.clone());
    let spawner = Arc::new(
        HarnessSubAgentSpawner::new(
            "sub-model",
            sub_client.clone(),
            session_dir.join("sub-sessions"),
            registry.clone(),
            |_cwd, _bus, _agent_id| Box::new(BlockingToolPlugin),
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

    // 等待 sub-agent in-flight
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if registry.in_flight_count() == 1 {
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
        "first sub-agent should be in-flight"
    );

    // abort —— 取消第一个 sub-agent
    harness.abort();
    let _ = main_task.await;

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
    harness
        .prompt("please spawn another sub-agent")
        .await
        .unwrap();

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

    // 第二个 sub-agent 的 LLM 被调用了（call_count >= 2：第一个 block_forever + 第二个 text）
    let sub_calls = sub_client
        .call_count
        .load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        sub_calls >= 2,
        "second sub-agent should have run its LLM, but call_count={sub_calls}"
    );
}

/// 场景：主 agent spawn sub-agent 后进入 Idle（IdleWatcher 运行中），
/// 此时 abort → watcher 被 cancel + sub-agent 被 cancel。
/// 断言：abort 后主 agent 不被 aborted completion 重新唤醒
/// （main_client 的 call_count 不超过 1，不会因 follow_up 续命）。
#[tokio::test]
async fn idle_watcher_abort() {
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
    let session_dir = std::env::temp_dir().join("abort-cascade-idle-watcher-test");
    let _ = std::fs::remove_dir_all(&session_dir);
    std::fs::create_dir_all(&session_dir).unwrap();

    let registry = message_bus_pair();
    let async_hook = async_spawn_pair(registry.clone());
    let spawner = Arc::new(
        HarnessSubAgentSpawner::new(
            "sub-model",
            sub_client.clone(),
            session_dir.join("sub-sessions"),
            registry.clone(),
            |_cwd, _bus, _agent_id| Box::new(BlockingToolPlugin),
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

    // 等待主 agent 进 Idle + sub-agent in-flight（watcher 应已启动）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if registry.in_flight_count() == 1 && harness.state().phase == HarnessPhase::Idle {
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

    // 此时主 agent 在 Idle（spawn tool 返回后 LLM 第二 turn 无响应 → Idle）
    // watcher 正在监听 completion channel。abort 应 cancel watcher + sub-agent。
    harness.abort();
    let _ = main_task.await;

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
