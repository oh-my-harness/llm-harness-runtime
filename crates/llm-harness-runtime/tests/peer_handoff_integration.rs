//! 端到端测试：sub-agent 之间通过 PeerDirectory 直接通信（handoff 闭环）。
//!
//! A send_to_peer B 转交任务 → B 用 wait_for_peer_message 阻塞等消息（不进 Idle）
//! → B 处理 → B send_to_peer A 回传结果 → A 用 wait_for_peer_message 阻塞等结果
//! → A 输出最终结果。
//!
//! 关键设计：A 和 B 在等待对方消息时都调用 `wait_for_peer_message` tool，在 tool
//! 执行阶段 `recv().await` 阻塞，而非 EndTurn 后进 Idle。sub-agent 不挂 IdleWatcher，
//! 一旦进 Idle 便无法被 peer 消息唤醒；用 `wait_for_peer_message` 保持 active 是
//! 当前架构下实现 handoff 闭环的正确路径。

use std::sync::Arc;
use std::time::Duration;

use llm_harness_agent::{ContextMode, Plugin, ProviderSelection, SpawnOptions};
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
use llm_harness_runtime::async_spawn::async_spawn_pair;
use llm_harness_runtime::peer::{
    ListPeersTool, PeerDirectory, PeerMetadata, SendToPeerTool, SharedPeerRx,
    WaitForPeerMessageTool,
};
use llm_harness_runtime::spawner::{HarnessSubAgentSpawner, JsonlSessionFactory};
use llm_harness_types::Tool;

/// sub-agent plugin：注册 peer 通信工具。
///
/// `SendToPeerTool` + `ListPeersTool` 只需 `PeerDirectory`；
/// `WaitForPeerMessageTool` 需要与 `PeerMessageHook` 共享的 `SharedPeerRx`——
/// 由 spawner 的 `spawn_inner` 在 peer 模式下通过 plugin factory 第二参数传入。
/// `agent_id` 由 plugin factory 第三参数传入，用于 `SendToPeerTool` 标注发送者。
struct PeerHandoffPlugin {
    directory: Arc<PeerDirectory>,
    peer_rx: Option<SharedPeerRx>,
    agent_id: Option<String>,
}

impl Plugin for PeerHandoffPlugin {
    fn name(&self) -> &str {
        "peer-handoff"
    }

    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        tools.push(Arc::new(SendToPeerTool::new(
            self.directory.clone(),
            self.agent_id.clone(),
        )));
        tools.push(Arc::new(ListPeersTool::new(self.directory.clone())));
        if let Some(rx) = self.peer_rx.clone() {
            tools.push(Arc::new(WaitForPeerMessageTool::new(
                rx,
                Duration::from_secs(10),
            )));
        }
    }
}

/// 验证两个 sub-agent 之间的 peer 通信 handoff 闭环：
/// A 派发任务给 B，B 处理后回传结果，A 收到结果输出最终答案。
#[tokio::test]
async fn peer_handoff_a_to_b_and_back() {
    let session_dir = std::env::temp_dir().join("peer-handoff-test");
    let _ = std::fs::remove_dir_all(&session_dir);
    std::fs::create_dir_all(&session_dir).unwrap();

    let directory = Arc::new(PeerDirectory::new());

    // sub-agent A 响应序列：
    //   1. list_peers（看到 B）
    //   2. send_to_peer("B", "please do the task")
    //   3. wait_for_peer_message（阻塞等 B 的结果，不进 Idle）
    //   4. 文本 "final: task done by B"（EndTurn，A 完成）
    let client_a = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use("a1", "list_peers", r#"{}"#),
        MockResponse::tool_use(
            "a2",
            "send_to_peer",
            r#"{"target_id":"B","message":"please do the task"}"#,
        ),
        MockResponse::tool_use("a3", "wait_for_peer_message", r#"{}"#),
        MockResponse::text("final: task done by B"),
    ]));

    // sub-agent B 响应序列：
    //   1. wait_for_peer_message（阻塞等 A 的任务，不进 Idle）
    //   2. send_to_peer("A", "task done by B")（回传结果）
    //   3. 文本 "B done"（EndTurn，B 完成）
    let client_b = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use("b1", "wait_for_peer_message", r#"{}"#),
        MockResponse::tool_use(
            "b2",
            "send_to_peer",
            r#"{"target_id":"A","message":"task done by B"}"#,
        ),
        MockResponse::text("B done"),
    ]));

    // 主 agent client（占位——本测试直接用 spawner spawn A/B，不走主 agent LLM）。
    let main_client = Arc::new(MockLlmClient::new(vec![]));

    // sub-agent plugin factory：捕获 PeerDirectory，按 peer_rx 注册 Wait tool。
    let dir_for_plugin = directory.clone();
    let sub_plugin_factory = move |_cwd: &std::path::Path,
                                   peer_rx: Option<SharedPeerRx>,
                                   agent_id: Option<&str>|
          -> Box<dyn Plugin> {
        Box::new(PeerHandoffPlugin {
            directory: dir_for_plugin.clone(),
            peer_rx,
            agent_id: agent_id.map(String::from),
        })
    };

    // spawner：default_client 用 main_client 占位；A 用 default（client_a 通过 alias
    // "a-client" 提供），B 用 alias "b-client"。
    let (registry, _async_hook) = async_spawn_pair();
    let spawner = Arc::new(
        HarnessSubAgentSpawner::new(
            "sub-model",
            main_client,
            session_dir.join("sub-sessions"),
            sub_plugin_factory,
        )
        .alias("a-client", client_a, "sub-model")
        .alias("b-client", client_b, "sub-model")
        .enable_peer_communication(directory.clone())
        .session_factory(Arc::new(JsonlSessionFactory))
        .env_factory(Arc::new(llm_harness_runtime_sandbox_os::OsEnvFactory))
        .enable_async_delivery(registry.clone()),
    );

    // 先 spawn B：B 进 wait_for_peer_message 阻塞等 A 的任务。
    let opts_b = SpawnOptions {
        prompt: "you are B. wait for a task from A, then send the result back.".into(),
        provider: ProviderSelection::Alias("b-client".into()),
        cwd: None,
        context: ContextMode::None,
    };
    let meta_b = PeerMetadata {
        agent_id: "B".into(),
        capabilities: vec!["worker".into()],
        description: "sub-agent B".into(),
    };
    let (_handle_b, _sync_rx_b) = spawner
        .spawn_async_with_peer(opts_b, meta_b, true)
        .await
        .expect("spawn B should succeed");

    // 等 B 注册进通讯录（B 会立即进 wait_for_peer_message 阻塞）。
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        directory.list_peers().iter().any(|p| p.agent_id == "B"),
        "B should be registered before A starts"
    );

    // spawn A：A list_peers → send_to_peer → wait_for_peer_message → final。
    let opts_a = SpawnOptions {
        prompt: "you are A. list peers, send task to B, wait for result, then report.".into(),
        provider: ProviderSelection::Alias("a-client".into()),
        cwd: None,
        context: ContextMode::None,
    };
    let meta_a = PeerMetadata {
        agent_id: "A".into(),
        capabilities: vec!["orchestrator".into()],
        description: "sub-agent A".into(),
    };
    let _handle_a = spawner
        .spawn_async_with_peer(opts_a, meta_a, true)
        .await
        .expect("spawn A should succeed");

    // 等 A 和 B 都完成（各自最后一条 response 是文本 → EndTurn）。
    // completion 进 channel；取 A 和 B 的 completion。
    let mut guard = registry.take_completion_rx().expect("borrow rx");
    let mut a_completion = None;
    let mut b_completion = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while a_completion.is_none() || b_completion.is_none() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, guard.recv()).await {
            Ok(Some(c)) => {
                if c.agent_id == "A" && a_completion.is_none() {
                    a_completion = Some(c);
                } else if c.agent_id == "B" && b_completion.is_none() {
                    b_completion = Some(c);
                }
            }
            _ => break,
        }
    }
    drop(guard);

    let a_completion = a_completion.expect("A did not complete in time");
    let result = a_completion.result.expect("A should complete successfully");
    assert!(
        result.final_answer.contains("final: task done by B"),
        "A's final_answer should contain final result, got: {}",
        result.final_answer
    );

    let _ = b_completion.expect("B did not complete in time");

    // A/B 完成后应从通讯录 unregister。
    tokio::time::sleep(Duration::from_millis(200)).await;
    let peers = directory.list_peers();
    assert!(
        !peers.iter().any(|p| p.agent_id == "A"),
        "A should have unregistered after completion: {peers:?}"
    );
    assert!(
        !peers.iter().any(|p| p.agent_id == "B"),
        "B should have unregistered after completion: {peers:?}"
    );
}

/// A 用 `list_peers` 能看到已注册的 B（peer 目录可见性验证）。
///
/// B 用 `wait_for_peer_message` 保持 active（不进 Idle），确保 A 查询时 B 仍在
/// 通讯录。A 完成后 abort B 让其退出。
#[tokio::test]
async fn list_peers_shows_registered_peer() {
    let session_dir = std::env::temp_dir().join("peer-list-peers-test");
    let _ = std::fs::remove_dir_all(&session_dir);
    std::fs::create_dir_all(&session_dir).unwrap();

    let directory = Arc::new(PeerDirectory::new());

    // A 调 list_peers 后输出 "ok"。
    let client_a = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use("a1", "list_peers", r#"{}"#),
        MockResponse::text("ok"),
    ]));
    // B 进 wait_for_peer_message 阻塞，保持注册状态直到被 abort。
    let client_b = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use("b1", "wait_for_peer_message", r#"{}"#),
        MockResponse::text("done"),
    ]));

    let main_client = Arc::new(MockLlmClient::new(vec![]));

    let dir_for_plugin = directory.clone();
    let sub_plugin_factory = move |_cwd: &std::path::Path,
                                   peer_rx: Option<SharedPeerRx>,
                                   agent_id: Option<&str>|
          -> Box<dyn Plugin> {
        Box::new(PeerHandoffPlugin {
            directory: dir_for_plugin.clone(),
            peer_rx,
            agent_id: agent_id.map(String::from),
        })
    };

    let (registry, _async_hook) = async_spawn_pair();
    let spawner = Arc::new(
        HarnessSubAgentSpawner::new(
            "sub-model",
            main_client,
            session_dir.join("sub-sessions"),
            sub_plugin_factory,
        )
        .alias("a-client", client_a, "sub-model")
        .alias("b-client", client_b, "sub-model")
        .enable_peer_communication(directory.clone())
        .session_factory(Arc::new(JsonlSessionFactory))
        .env_factory(Arc::new(llm_harness_runtime_sandbox_os::OsEnvFactory))
        .enable_async_delivery(registry.clone()),
    );

    // spawn B：B 进 wait_for_peer_message 阻塞，保持注册。
    let opts_b = SpawnOptions {
        prompt: "you are B. wait for a message.".into(),
        provider: ProviderSelection::Alias("b-client".into()),
        cwd: None,
        context: ContextMode::None,
    };
    let meta_b = PeerMetadata {
        agent_id: "B".into(),
        capabilities: vec!["worker".into()],
        description: "sub-agent B".into(),
    };
    let (handle_b, _sync_rx_b) = spawner
        .spawn_async_with_peer(opts_b, meta_b, true)
        .await
        .expect("spawn B should succeed");

    // 等 B 注册并进 wait_for_peer_message。
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        directory.list_peers().iter().any(|p| p.agent_id == "B"),
        "B should be registered while waiting for peer message"
    );

    // spawn A，A list_peers 应看到 B。
    let opts_a = SpawnOptions {
        prompt: "list peers.".into(),
        provider: ProviderSelection::Alias("a-client".into()),
        cwd: None,
        context: ContextMode::None,
    };
    let meta_a = PeerMetadata {
        agent_id: "A".into(),
        capabilities: vec!["orchestrator".into()],
        description: "sub-agent A".into(),
    };
    let _handle_a = spawner
        .spawn_async_with_peer(opts_a, meta_a, true)
        .await
        .expect("spawn A should succeed");

    // 等 A 完成。
    let mut guard = registry.take_completion_rx().expect("borrow rx");
    let mut a_completion = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while a_completion.is_none() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, guard.recv()).await {
            Ok(Some(c)) if c.agent_id == "A" => a_completion = Some(c),
            Ok(_) => continue,
            _ => break,
        }
    }
    drop(guard);

    let a_completion = a_completion.expect("A did not complete in time");
    let result = a_completion.result.expect("A should complete successfully");
    assert_eq!(result.tool_calls_count, 1, "A should call list_peers once");

    // A 完成后 abort B，让 B 退出（否则 B 会等到 wait_for_peer_message 超时）。
    handle_b.abort();
}
