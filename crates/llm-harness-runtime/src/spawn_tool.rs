// crates/llm-harness-runtime/src/spawn_tool.rs
//! LLM-visible spawn_agent tool with wait parameter for sync/async semantics.

use std::sync::{Arc, LazyLock};

use futures::future::BoxFuture;
use llm_harness_agent::{ContextMode, ProviderSelection, SpawnOptions};
use llm_harness_types::{
    ContentBlock, Tool, ToolContext, ToolError, ToolExecutionMode, ToolResult,
};

use crate::async_spawn::AsyncCompletion;
use crate::peer::PeerMetadata;
use crate::spawner::HarnessSubAgentSpawner;

pub struct SpawnAgentTool {
    spawner: Arc<HarnessSubAgentSpawner>,
    directory: Option<Arc<crate::peer::PeerDirectory>>,
}

impl SpawnAgentTool {
    /// `registry` 参数保留向后兼容（sync path 现用 oneshot；async path 经 completion channel，由 `track(deliver_to_channel=true)` 控制）。
    pub fn new(
        spawner: Arc<HarnessSubAgentSpawner>,
        _registry: Arc<crate::async_spawn::AsyncSpawnRegistry>,
    ) -> Self {
        Self {
            spawner,
            directory: None,
        }
    }

    pub fn with_peer_directory(mut self, directory: Arc<crate::peer::PeerDirectory>) -> Self {
        self.directory = Some(directory);
        self
    }
}

fn spawn_agent_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "prompt": { "type": "string", "description": "给 sub-agent 的任务指令" },
            "wait": { "type": "boolean", "description": "是否同步等待结果（默认 false）", "default": false },
            "agent_id": { "type": "string", "description": "sub-agent 标识 ID（peer 模式下必填且用作 peer 寻址；非 peer 模式自动取 session_id）" },
            "context": { "type": "string", "description": "附加上下文" },
            "provider": { "type": "string", "description": "provider 别名" }
        },
        "required": ["prompt"]
    })
}

static SPAWN_AGENT_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(spawn_agent_schema);

fn format_completion(completion: &AsyncCompletion) -> String {
    match &completion.result {
        Ok(r) => format!(
            "[Sub-agent '{id}' completed]\n{output}\n\nTool calls: {n}\nSession: {sid}",
            id = completion.agent_id,
            output = r.output,
            n = r.tool_calls_count,
            sid = r.session_id,
        ),
        Err(e) => format!(
            "[Sub-agent '{id}' failed]\n{e}",
            id = completion.agent_id,
            e = e
        ),
    }
}

impl Tool for SpawnAgentTool {
    fn name(&self) -> &str {
        "spawn_agent"
    }

    fn description(&self) -> &str {
        "派发一个 sub-agent 处理子任务。默认异步（wait=false）：派发后立即返回，\
         sub-agent 完成后结果自动出现在后续对话中，可同时派发多个并行工作。\
         设 wait=true 则同步等待：结果作为 tool 结果返回。"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &SPAWN_AGENT_SCHEMA
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let prompt = args
                .get("prompt")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::Execution("missing prompt".into()))?
                .to_string();
            let wait = args.get("wait").and_then(|v| v.as_bool()).unwrap_or(false);
            let agent_id = args
                .get("agent_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            let context = args
                .get("context")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| ContextMode::Inject(s.to_string()))
                .unwrap_or(ContextMode::None);
            let provider = args
                .get("provider")
                .and_then(|v| v.as_str())
                .map(|s| ProviderSelection::Alias(s.to_string()))
                .unwrap_or(ProviderSelection::Inherit);

            let opts = SpawnOptions {
                prompt,
                provider,
                cwd: None,
                context,
            };

            // 分支：有 directory 且 spawner 启用 peer → spawn_async_with_peer
            let (handle, sync_rx) = if let Some(_dir) = &self.directory {
                let meta = PeerMetadata {
                    agent_id: agent_id
                        .clone()
                        .unwrap_or_else(|| format!("agent-{}", uuid::Uuid::new_v4())),
                    capabilities: vec![],
                    description: String::new(),
                };
                self.spawner
                    .spawn_async_with_peer(opts, meta, !wait)
                    .await
                    .map_err(|e| ToolError::Execution(e.to_string()))?
            } else {
                self.spawner
                    .spawn_async(opts, !wait)
                    .await
                    .map_err(|e| ToolError::Execution(e.to_string()))?
            };
            if !wait {
                // 异步：立即返回。sync_rx drop → track task 的 oneshot send 变 no-op。
                Ok(ToolResult {
                    content: vec![ContentBlock::Text {
                        text: format!(
                            "Spawned agent '{}'. Results will arrive when done.",
                            handle.agent_id()
                        ),
                    }],
                    details: serde_json::json!({"agent_id": handle.agent_id()}),
                    terminate: false,
                })
            } else {
                // 同步：直接 await oneshot（按 agent_id 一对一路由，修复 F-3/F-4）。
                // 不经 completion channel，无需借 rx，不受 hook 占用影响。
                // F-14（abort 打断）：sub-agent 的 abort token 通过 handle.abort()
                // 取消后，track task 的 join 返回 Aborted，oneshot 收到 Err completion。
                // 调用方需外层包 tokio::time::timeout + handle.abort()（见 spec §7.3）。
                let completion = match sync_rx.await {
                    Ok(c) => c,
                    Err(_) => {
                        return Ok(ToolResult {
                            content: vec![ContentBlock::Text {
                                text: "sub-agent channel closed".into(),
                            }],
                            details: serde_json::json!({"error": "channel_closed"}),
                            terminate: false,
                        });
                    }
                };
                let text = format_completion(&completion);
                let is_error = completion.result.is_err();
                Ok(ToolResult {
                    content: vec![ContentBlock::Text { text }],
                    details: serde_json::json!({
                        "agent_id": completion.agent_id,
                        "is_error": is_error
                    }),
                    terminate: false,
                })
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::async_spawn::async_spawn_pair;
    use crate::spawner::JsonlSessionFactory;
    use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
    use llm_harness_types::ExecutionEnv;

    fn make_tool() -> (SpawnAgentTool, Arc<crate::async_spawn::AsyncSpawnRegistry>) {
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
        let spawner = HarnessSubAgentSpawner::new(
            "test-model",
            client,
            std::env::temp_dir().join("spawn-tool-test"),
            |_cwd, _peer_rx, _agent_id| Box::new(NoopPlugin),
        )
        .session_factory(Arc::new(JsonlSessionFactory))
        .env_factory(Arc::new(NoopEnvFactory));
        let (registry, _hook) = async_spawn_pair();
        let spawner = Arc::new(spawner.enable_async_delivery(registry.clone()));
        let tool = SpawnAgentTool::new(spawner, registry.clone());
        (tool, registry)
    }

    struct NoopPlugin;
    impl llm_harness_agent::Plugin for NoopPlugin {
        fn name(&self) -> &str {
            "noop"
        }
    }

    struct NoopEnvFactory;
    impl crate::spawner::EnvFactory for NoopEnvFactory {
        fn create(
            &self,
            _cwd: &std::path::Path,
        ) -> Result<Arc<dyn ExecutionEnv>, llm_harness_types::AgentError> {
            Ok(Arc::new(llm_harness_loop::test_utils::NoOpEnv))
        }
    }

    #[test]
    fn spawn_agent_tool_name_and_schema() {
        let (tool, _) = make_tool();
        assert_eq!(tool.name(), "spawn_agent");
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
        assert!(schema.get("required").is_some());
    }

    #[tokio::test]
    async fn spawn_agent_async_returns_immediately() {
        let (tool, registry) = make_tool();
        let ctx = make_tool_context();
        let args = serde_json::json!({"prompt": "say ok", "wait": false});
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(!result.terminate);
        match &result.content[0] {
            ContentBlock::Text { text } => assert!(text.contains("Spawned agent")),
            _ => panic!("expected text"),
        }
        // 等 sub-agent 完成后 in_flight 归零
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(registry.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn spawn_agent_sync_returns_result_as_tool_result() {
        let (tool, _registry) = make_tool();
        let ctx = make_tool_context();
        let args = serde_json::json!({"prompt": "say ok", "wait": true});
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), tool.execute(args, &ctx))
                .await
                .expect("sync spawn should not hang")
                .unwrap();
        match &result.content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("completed") || text.contains("ok"));
            }
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn spawn_agent_missing_prompt_returns_error() {
        let (tool, _) = make_tool();
        let ctx = make_tool_context();
        let args = serde_json::json!({});
        let err = tool.execute(args, &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    /// format_completion 的 Err 分支：失败结果格式化含 agent_id 和错误信息。
    #[test]
    fn format_completion_failure_includes_agent_id_and_error() {
        use crate::async_spawn::AsyncCompletion;
        use llm_harness_types::AgentError;
        let completion = AsyncCompletion {
            agent_id: "failed-agent".into(),
            result: Err(AgentError::Internal("boom".into())),
        };
        let text = format_completion(&completion);
        assert!(text.contains("failed-agent"), "text: {text}");
        assert!(text.contains("failed"), "text: {text}");
        assert!(text.contains("boom"), "text: {text}");
    }

    // ── peer-mode tests ──────────────────────────────────────────────────

    fn make_peer_tool() -> (
        SpawnAgentTool,
        Arc<crate::async_spawn::AsyncSpawnRegistry>,
        Arc<crate::peer::PeerDirectory>,
    ) {
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
        let spawner = HarnessSubAgentSpawner::new(
            "test-model",
            client,
            std::env::temp_dir().join("spawn-tool-peer-test"),
            |_cwd, _peer_rx, _agent_id| Box::new(NoopPlugin),
        )
        .session_factory(Arc::new(JsonlSessionFactory))
        .env_factory(Arc::new(NoopEnvFactory));
        let (registry, _hook) = async_spawn_pair();
        let directory = Arc::new(crate::peer::PeerDirectory::new());
        let spawner = Arc::new(
            spawner
                .enable_async_delivery(registry.clone())
                .enable_peer_communication(directory.clone()),
        );
        let tool =
            SpawnAgentTool::new(spawner, registry.clone()).with_peer_directory(directory.clone());
        (tool, registry, directory)
    }

    /// peer 模式 + async：execute 走 spawn_async_with_peer 分支，
    /// 返回的 agent_id 取自参数（而非 session_id），peer 在 directory 注册。
    #[tokio::test]
    async fn spawn_agent_async_with_peer_uses_provided_agent_id() {
        let (tool, registry, directory) = make_peer_tool();
        let ctx = make_tool_context();
        let args = serde_json::json!({
            "prompt": "say ok",
            "wait": false,
            "agent_id": "peer-A",
        });
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(!result.terminate);
        // 返回文本含提供的 agent_id（非 session_id）
        match &result.content[0] {
            ContentBlock::Text { text } => assert!(text.contains("peer-A")),
            _ => panic!("expected text"),
        }
        assert_eq!(result.details["agent_id"], "peer-A");
        // peer 在 sub-agent 运行期间应注册进 directory
        assert!(
            directory
                .list_peers()
                .iter()
                .any(|p| p.agent_id == "peer-A"),
            "peer should be registered while sub-agent runs"
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(registry.in_flight_count(), 0);
        // 完成后 unregister
        assert!(
            !directory
                .list_peers()
                .iter()
                .any(|p| p.agent_id == "peer-A"),
            "peer should be unregistered after sub-agent completes"
        );
    }

    /// peer 模式 + sync：execute 走 spawn_async_with_peer 后借 completion_rx
    /// 阻塞 recv，结果作为 ToolResult 返回。
    #[tokio::test]
    async fn spawn_agent_sync_with_peer_returns_result() {
        let (tool, _registry, _directory) = make_peer_tool();
        let ctx = make_tool_context();
        let args = serde_json::json!({
            "prompt": "say ok",
            "wait": true,
            "agent_id": "peer-sync",
        });
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), tool.execute(args, &ctx))
                .await
                .expect("sync peer spawn should not hang")
                .unwrap();
        match &result.content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("completed") || text.contains("ok"));
            }
            _ => panic!("expected text"),
        }
        assert_eq!(result.details["agent_id"], "peer-sync");
    }

    fn make_tool_context() -> ToolContext {
        let env: Arc<dyn llm_harness_types::ExecutionEnv> = Arc::new(
            llm_harness_runtime_sandbox_os::OsEnv::new(std::env::current_dir().unwrap()),
        );
        let (update_tx, _update_rx) = tokio::sync::mpsc::channel(8);
        ToolContext {
            env,
            abort: tokio_util::sync::CancellationToken::new(),
            tool_use_id: "test".into(),
            turn_index: 0,
            assistant_message: Arc::new(llm_harness_loop::test_utils::test_assistant_message(
                vec![],
            )),
            update_tx,
        }
    }
}
