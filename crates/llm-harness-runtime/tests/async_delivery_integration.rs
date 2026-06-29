//! 端到端测试：主 agent spawn 异步 sub-agent，结果异步投递回主 agent 对话。
//!
//! 覆盖两条路径：
//! - Running 期间：sub-agent 在主 agent 仍在跑时完成，AsyncSpawnHook 在
//!   should_stop 排空 completion channel，注入 follow_up，主 agent 下一 turn
//!   看到结果。
//! - Idle 期间：主 agent spawn 后进入 Idle，IdleWatcher 监听 completion channel，
//!   sub-agent 完成时自动调 continue_run 唤醒主 agent。
//!
//! 断言策略：follow_up 消息注入到 LLM 的对话上下文（`captured_requests`），
//! 而非 session 持久化层——follow_up 是 loop 内 ephemeral 注入（`loop_fn.rs`
//! push 到 `ctx.messages`），不写 session。因此验证主 agent 的 LLM 请求
//! 包含 sub-agent 结果文本，证明投递链路完整。

use llm_harness_agent::Plugin;
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
use llm_harness_loop::{LlmMessage, RequestContent};
use llm_harness_runtime::async_spawn::{AsyncSpawnRegistry, IdleWatcher, async_spawn_pair};
use llm_harness_runtime::builder::HarnessBuilder;
use llm_harness_runtime::spawn_tool::SpawnAgentTool;
use llm_harness_runtime::spawner::{HarnessSubAgentSpawner, JsonlSessionFactory};
use llm_harness_runtime_sandbox_os::{OsEnv, OsEnvFactory};
use llm_harness_types::ExecutionEnv;
use std::sync::Arc;
struct NoopPlugin;
impl Plugin for NoopPlugin {
    fn name(&self) -> &str {
        "noop"
    }
}

/// 从 captured_requests 提取所有 user 消息的文本内容，拼接成单条字符串。
fn user_text_seen_by_llm(client: &MockLlmClient) -> String {
    let reqs = client.captured_requests.lock().unwrap();
    let mut buf = String::new();
    for req in reqs.iter() {
        for msg in req.messages() {
            if let LlmMessage::User(blocks) = msg {
                for block in blocks {
                    if let RequestContent::Text(t) = block {
                        buf.push_str(t);
                        buf.push('\n');
                    }
                }
            }
        }
    }
    buf
}

/// 场景 1：主 agent 在 Running 期间 spawn，sub-agent 完成时主 agent 还在跑，
/// should_stop 排空 completion 注入 follow_up，主 agent 下一 turn 看到结果。
#[tokio::test]
async fn async_delivery_running_period_sub_completes_during_loop() {
    // 主 agent 响应序列：
    // 1. 调 spawn_agent tool（wait=false）派发 sub-agent
    // 2. 看到 sub-agent 结果后回复 "got result"
    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use(
            "call_1",
            "spawn_agent",
            r#"{"prompt":"compute","wait":false}"#,
        ),
        MockResponse::text("got result"),
    ]));

    // sub-agent 响应：立即返回结果
    let sub_client = Arc::new(MockLlmClient::new(vec![MockResponse::text(
        "sub-agent output: 42",
    )]));

    let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new(std::env::current_dir().unwrap()));

    let session_dir = std::env::temp_dir().join("async-delivery-running-test");
    let _ = std::fs::remove_dir_all(&session_dir);
    std::fs::create_dir_all(&session_dir).unwrap();

    // sub-agent spawner：必须 enable_async_delivery，否则 spawn_async 返回错误。
    let (registry, async_hook) = async_spawn_pair();
    let spawner = Arc::new(
        HarnessSubAgentSpawner::new(
            "sub-model",
            sub_client,
            session_dir.join("sub-sessions"),
            |_cwd, _peer_rx, _agent_id| Box::new(NoopPlugin),
        )
        .session_factory(Arc::new(JsonlSessionFactory))
        .env_factory(Arc::new(OsEnvFactory))
        .enable_async_delivery(registry.clone()),
    );

    // 主 agent harness
    let harness = HarnessBuilder::new("main-model")
        .provider("main-model", main_client.clone())
        .tool(Arc::new(SpawnAgentTool::new(
            spawner.clone(),
            registry.clone(),
        )))
        .should_stop_hook(async_hook.clone())
        .build(env.clone())
        .await
        .unwrap();
    let harness = Arc::new(harness);

    // 配置 IdleWatcher（continue_run 回调捕获 Weak<AgentHarness> 打断引用环 F-1）
    let watcher = IdleWatcher::new(registry.clone(), {
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

    // 启动主 agent
    harness
        .prompt("please spawn a sub-agent to compute")
        .await
        .unwrap();

    // 等待 sub-agent 完成 + 结果投递。Running 场景下若 sub-agent 在主 loop
    // 期间完成，should_stop 排空注入；若在主 loop 结束后完成，IdleWatcher 唤醒。
    wait_for_in_flight_zero(&registry).await;
    // 确定性等待：follow_up 注入会触发主 agent 额外 LLM 调用（>=3）。
    // 轮询 call_count 而非固定 sleep，避免 CI 负载下的 flake。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let calls = main_client
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst);
        if calls >= 3 {
            break;
        }
        if std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // 验证：主 agent 的某次 LLM 请求包含 sub-agent 结果文本（follow_up 注入）。
    let seen = user_text_seen_by_llm(&main_client);
    assert!(
        seen.contains("sub-agent output: 42") || seen.contains("completed"),
        "expected sub-agent result in LLM context, got: {seen}"
    );

    // 主 agent 应被 follow_up 唤醒并产生额外 LLM 调用（>=3：spawn turn +
    // tool-result turn + follow_up turn）。
    let calls = main_client
        .call_count
        .load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        calls >= 3,
        "expected >=3 LLM calls after follow_up delivery, got {calls}"
    );
}

/// 场景 2：主 agent spawn 后进 Idle（等用户输入或 sub-agent），
/// IdleWatcher 监听 completion channel，sub-agent 完成时自动 continue_run 唤醒。
#[tokio::test]
async fn async_delivery_idle_period_watcher_wakes_main() {
    // 主 agent：调 spawn 后立即停（无第二条 response——除非被唤醒）
    let main_client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use("call_1", "spawn_agent", r#"{"prompt":"work","wait":false}"#),
        // 被 watcher 唤醒后看到结果，回复
        MockResponse::text("processed async result"),
    ]));

    // sub-agent：立即完成（模拟耗时任务）
    let sub_client = Arc::new(MockLlmClient::new(vec![MockResponse::text(
        "async work done",
    )]));

    let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new(std::env::current_dir().unwrap()));
    let session_dir = std::env::temp_dir().join("async-delivery-idle-test");
    let _ = std::fs::remove_dir_all(&session_dir);
    std::fs::create_dir_all(&session_dir).unwrap();

    let (registry, async_hook) = async_spawn_pair();
    let spawner = Arc::new(
        HarnessSubAgentSpawner::new(
            "sub-model",
            sub_client,
            session_dir.join("sub-sessions"),
            |_cwd, _peer_rx, _agent_id| Box::new(NoopPlugin),
        )
        .session_factory(Arc::new(JsonlSessionFactory))
        .env_factory(Arc::new(OsEnvFactory))
        .enable_async_delivery(registry.clone()),
    );

    let harness = HarnessBuilder::new("main-model")
        .provider("main-model", main_client.clone())
        .tool(Arc::new(SpawnAgentTool::new(
            spawner.clone(),
            registry.clone(),
        )))
        .should_stop_hook(async_hook.clone())
        .build(env.clone())
        .await
        .unwrap();
    let harness = Arc::new(harness);
    let watcher = IdleWatcher::new(registry.clone(), {
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

    harness.prompt("spawn a sub-agent").await.unwrap();

    // prompt 返回后主 agent 进 Idle。sub-agent 可能已完成或仍在跑。
    // IdleWatcher 应自动唤醒处理。等待 sub-agent 完成 + watcher 投递。
    wait_for_in_flight_zero(&registry).await;
    // 确定性等待：watcher 唤醒后 follow_up 注入触发主 agent 额外 LLM 调用（>=3）。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let calls = main_client
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst);
        if calls >= 3 {
            break;
        }
        if std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let seen = user_text_seen_by_llm(&main_client);
    assert!(
        seen.contains("async work done") || seen.contains("completed"),
        "expected sub-agent result after idle wake: {seen}"
    );

    let calls = main_client
        .call_count
        .load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        calls >= 3,
        "expected >=3 LLM calls after idle wake, got {calls}"
    );
}

/// 轮询等待 registry 的 in_flight 归零（sub-agent 全部完成）。
async fn wait_for_in_flight_zero(registry: &AsyncSpawnRegistry) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if registry.in_flight_count() == 0 {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "timed out waiting for sub-agent to finish; in_flight={}",
                registry.in_flight_count()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
