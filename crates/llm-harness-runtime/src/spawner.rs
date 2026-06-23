use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;
use llm_harness_agent::{
    AgentHarnessEvent, Plugin, Session,
    session::SessionStorage,
    session::{JsonlSessionRepo, SessionRepo, types::CreateSessionOptions},
    sub_agent::{
        ContextMode, ProviderSelection, SpawnOptions, SubAgentHandle, SubAgentResult,
        SubAgentSpawner,
    },
};
use llm_harness_loop::LlmClient;
use llm_harness_types::{AgentError, AgentEvent, ExecutionEnv, HarnessError, HarnessPhase};
use tokio_util::sync::CancellationToken;

use crate::builder::{HarnessBuildError, HarnessBuilder};

// ── EnvFactory ────────────────────────────────────────────────────────────

/// `ExecutionEnv` 的抽象工厂。
///
/// 位于 `llm-harness-runtime` 中，使 `HarnessSubAgentSpawner` 无需
/// 直接依赖 `llm-harness-runtime-sandbox-os`。
pub trait EnvFactory: Send + Sync {
    fn create(&self, cwd: &Path) -> Result<Arc<dyn ExecutionEnv>, AgentError>;
}

// ── SessionFactory ────────────────────────────────────────────────────────

/// `SessionFactory::create` 的返回类型。
pub type SessionCreateResult = (Arc<dyn SessionStorage>, String);

/// Session 存储的抽象工厂。
///
/// 位于 `llm-harness-runtime` 中，使调用方可以选择 session 存储后端
///（`JsonlSessionRepo`、`InMemorySessionRepo` 等），`HarnessSubAgentSpawner`
/// 不写死后端。
pub trait SessionFactory: Send + Sync {
    /// 创建一个新 session，返回 storage 和 session ID。
    fn create(
        &self,
        base_dir: &Path,
        model: &str,
    ) -> BoxFuture<'_, Result<SessionCreateResult, AgentError>>;
}

// ── PluginFactory ──────────────────────────────────────────────────────────

/// Sub-agent plugin 的工厂函数类型。
pub type PluginFactory = Arc<dyn Fn(&Path) -> Box<dyn Plugin> + Send + Sync>;

// ── ProviderAlias ─────────────────────────────────────────────────────────

struct ProviderAlias {
    name: String,
    model: String,
    client: Arc<dyn LlmClient>,
}

// ── HarnessSubAgentSpawner ────────────────────────────────────────────────

/// `SubAgentSpawner` 的具体实现：为每个 sub-agent 创建独立的
/// `AgentHarness` 实例，支持可选的 provider 别名。
pub struct HarnessSubAgentSpawner {
    default_model: String,
    default_client: Arc<dyn LlmClient>,
    aliases: Vec<ProviderAlias>,
    sub_plugin_factory: PluginFactory,
    env_factory: Arc<dyn EnvFactory>,
    session_factory: Arc<dyn SessionFactory>,
    session_base_dir: PathBuf,
}

impl HarnessSubAgentSpawner {
    /// 创建新的 spawner。
    ///
    /// `sub_plugin_factory` 在每次 spawn 时使用 sub-agent 的 cwd 调用。
    /// 必须返回不含 `SpawnAgentTool` 的 Plugin，以防止递归派发（v0.3+）。
    pub fn new(
        default_model: impl Into<String>,
        default_client: Arc<dyn LlmClient>,
        session_base_dir: PathBuf,
        sub_plugin_factory: impl Fn(&Path) -> Box<dyn Plugin> + Send + Sync + 'static,
    ) -> Self {
        Self {
            default_model: default_model.into(),
            default_client,
            aliases: vec![],
            sub_plugin_factory: Arc::new(sub_plugin_factory),
            env_factory: Arc::new(NoOpEnvFactory),
            session_factory: Arc::new(NoOpSessionFactory),
            session_base_dir,
        }
    }

    /// 注册 provider 别名（如 "fast" → 更便宜的模型）。
    pub fn alias(mut self, name: &str, client: Arc<dyn LlmClient>, model: &str) -> Self {
        self.aliases.push(ProviderAlias {
            name: name.to_string(),
            model: model.to_string(),
            client,
        });
        self
    }

    /// 设置环境工厂（默认：NoOpEnvFactory —— 未设置真实 factory 时
    /// `create()` 会 panic）。
    pub fn env_factory(mut self, factory: Arc<dyn EnvFactory>) -> Self {
        self.env_factory = factory;
        self
    }

    /// 设置 session 工厂（默认：NoOpSessionFactory —— 未设置真实 factory 时
    /// `create()` 会 panic。生产环境应使用 `JsonlSessionFactory`）。
    pub fn session_factory(mut self, factory: Arc<dyn SessionFactory>) -> Self {
        self.session_factory = factory;
        self
    }
}

impl SubAgentSpawner for HarnessSubAgentSpawner {
    fn spawn(&self, opts: SpawnOptions) -> BoxFuture<'_, Result<SubAgentHandle, AgentError>> {
        let default_model = self.default_model.clone();
        let default_client = self.default_client.clone();
        let aliases: Vec<(&str, &str, &Arc<dyn LlmClient>)> = self
            .aliases
            .iter()
            .map(|a| (a.name.as_str(), a.model.as_str(), &a.client))
            .collect();
        let sub_plugin_factory = self.sub_plugin_factory.clone();
        let env_factory = self.env_factory.clone();
        let session_factory = self.session_factory.clone();
        let session_base_dir = self.session_base_dir.clone();

        async move {
            // 1. 解析 provider。
            let (client, model) = match &opts.provider {
                ProviderSelection::Inherit => (default_client.clone(), default_model),
                ProviderSelection::Alias(name) => {
                    let found = aliases
                        .iter()
                        .find(|(n, _, _)| *n == name.as_str())
                        .ok_or_else(|| {
                            AgentError::Internal(format!("unknown provider alias: {name}"))
                        })?;
                    (found.2.clone(), found.1.to_string())
                }
            };

            // 2. 构建环境。
            let default_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let cwd = opts.cwd.unwrap_or(default_cwd);
            let env = env_factory.create(&cwd)?;

            // 3. 创建 sub-agent plugin（不含 SpawnAgentTool）。
            let sub_plugin = (sub_plugin_factory)(&cwd);

            // 4. 创建 session（通过 SessionFactory，不写死后端）。
            // 注意：session 在磁盘上创建后，如果下一步构建 harness 失败，
            // 该 session 会成为孤儿（存在于磁盘但无引用）。
            // 未来可通过内存 storage 先构建 harness、确认成功后再持久化来避免。
            let (storage, session_id) = session_factory.create(&session_base_dir, &model).await?;

            // 5. 构建 harness。
            let harness = HarnessBuilder::new(&model)
                .install(sub_plugin.as_ref())
                .provider(&model, client)
                .build_with_session(env, Session::new(storage))
                .map_err(|e: HarnessBuildError| {
                    AgentError::Internal(format!("failed to build sub-agent harness: {e}"))
                })?;

            // 6. 构建 prompt（可选的 context 注入）。
            let prompt = match &opts.context {
                ContextMode::None => opts.prompt,
                ContextMode::Inject(ctx) => {
                    format!("Context:\n{ctx}\n\n---\n\n{}", opts.prompt)
                }
            };

            // 7. 启动 tokio task。
            let abort = CancellationToken::new();
            let abort_clone = abort.clone();
            let session_id_clone = session_id.clone();
            let join = tokio::spawn(async move {
                // 将 abort token 注入 harness，使 handle.abort()
                // 能到达 agent loop。
                harness.set_abort_token(abort_clone);

                // Bridge: broadcast → unbounded mpsc。
                // event_task 从 mpsc 读，不会因 broadcast buffer 满而 Lagged。
                // 但从 harness 到 bridge 这一跳仍可能 Lagged（buffer = 256）：
                //   - Closed:  harness drop，正常退出。
                //   - Lagged:  消息丢失。若丢失的恰好是 PhaseChange::Idle 哨兵，
                //              event_task 会永久等待。这里直接退出让 mpsc_tx drop，
                //              event_task 通过 mpsc_rx.recv() == None 自然收尾。
                //              代价是 output / tool_count 可能缺失尾部事件，
                //              但好过挂死。
                let mut broadcast_rx = harness.subscribe();
                let (mpsc_tx, mut mpsc_rx) =
                    tokio::sync::mpsc::unbounded_channel::<Arc<AgentHarnessEvent>>();
                let bridge = tokio::spawn(async move {
                    // recv() 返回 Err（Closed 或 Lagged）都直接退出循环：
                    //   - Closed:  harness drop，正常结束。
                    //   - Lagged:  消息丢失（可能含 Idle 哨兵），退出让 mpsc_tx
                    //              drop，消费方通过 recv() == None 收尾。
                    while let Ok(ev) = broadcast_rx.recv().await {
                        if mpsc_tx.send(ev).is_err() {
                            break;
                        }
                    }
                });

                // 事件收集 task（从 mpsc_rx 读取，不受 broadcast Lagged 影响）。
                let event_task = tokio::spawn(async move {
                    let mut output = String::new();
                    let mut tool_count = 0u32;
                    while let Some(ev) = mpsc_rx.recv().await {
                        match ev.as_ref() {
                            AgentHarnessEvent::Agent(AgentEvent::TextDelta { text, .. }) => {
                                output.push_str(text);
                            }
                            AgentHarnessEvent::ToolCallStart { .. } => {
                                tool_count += 1;
                            }
                            AgentHarnessEvent::PhaseChange {
                                to: HarnessPhase::Idle,
                                ..
                            } => break,
                            _ => {}
                        }
                    }
                    (output, tool_count)
                });

                if let Err(e) = harness.prompt(&prompt).await {
                    event_task.abort();
                    bridge.abort();
                    return Err(match e {
                        HarnessError::Agent(agent_err) => agent_err,
                        other => AgentError::Internal(format!("sub-agent prompt failed: {other}")),
                    });
                }

                // prompt 返回后不再有新事件，主动关闭 bridge 让 mpsc_tx drop。
                // event_task 的正常退出是看到 PhaseChange::Idle；若 Idle
                // 因 broadcast Lagged 丢失，这就是 event_task 的唯一退出路径。
                drop(bridge);

                let (output, tool_calls_count) = event_task
                    .await
                    .map_err(|_| AgentError::Internal("event collection task panicked".into()))?;

                Ok(SubAgentResult {
                    output,
                    tool_calls_count,
                    session_id: session_id_clone,
                })
            });

            Ok(SubAgentHandle::new(join, abort))
        }
        .boxed()
    }
}

// ── 默认（空操作）Factory ────────────────────────────────────────────────

struct NoOpEnvFactory;

impl EnvFactory for NoOpEnvFactory {
    fn create(&self, _cwd: &Path) -> Result<Arc<dyn ExecutionEnv>, AgentError> {
        Err(AgentError::Internal(
            "EnvFactory not set — call HarnessSubAgentSpawner::env_factory() before use".into(),
        ))
    }
}

struct NoOpSessionFactory;

impl SessionFactory for NoOpSessionFactory {
    fn create(
        &self,
        _base_dir: &Path,
        _model: &str,
    ) -> BoxFuture<'_, Result<SessionCreateResult, AgentError>> {
        Box::pin(async {
            Err(AgentError::Internal(
                "SessionFactory not set — call HarnessSubAgentSpawner::session_factory() before use"
                    .into(),
            ))
        })
    }
}

// ── JSONL SessionFactory（生产环境默认）──────────────────────────────────

/// 基于 JSONL 文件的 `SessionFactory` 实现。
///
/// 每次 `create()` 在 `<base_dir>/sub-agents/` 下创建新的 session 目录。
pub struct JsonlSessionFactory;

impl SessionFactory for JsonlSessionFactory {
    fn create(
        &self,
        base_dir: &Path,
        model: &str,
    ) -> BoxFuture<'_, Result<SessionCreateResult, AgentError>> {
        let session_dir = base_dir.join("sub-agents");
        let model = model.to_string();
        Box::pin(async move {
            std::fs::create_dir_all(&session_dir)
                .map_err(|e| AgentError::Internal(format!("failed to create session dir: {e}")))?;
            let repo = JsonlSessionRepo::new(&session_dir);
            let storage = repo
                .create(CreateSessionOptions {
                    name: None,
                    initial_model: Some(model),
                    initial_thinking_level: None,
                    initial_tools: vec![],
                })
                .await
                .map_err(|e| {
                    AgentError::Internal(format!("failed to create sub-agent session: {e}"))
                })?;
            let session_id = storage
                .metadata()
                .await
                .map_err(|e| {
                    AgentError::Internal(format!("failed to read sub-agent session metadata: {e}"))
                })?
                .id;
            Ok((storage, session_id))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use llm_harness_agent::session::repo::InMemorySessionRepo;
    use llm_harness_agent::session::types::CreateSessionOptions;
    use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};
    use llm_harness_loop::{LlmMessage, RequestContent};
    use llm_harness_types::ExecutionEnv;

    // ── CaptureModelSessionFactory ────────────────────────────────────────────

    /// Session factory that captures the model name passed to create().
    struct CaptureModelSessionFactory {
        captured_model: Mutex<Option<String>>,
    }

    impl CaptureModelSessionFactory {
        fn new() -> Self {
            Self {
                captured_model: Mutex::new(None),
            }
        }

        fn model(&self) -> Option<String> {
            self.captured_model.lock().unwrap().clone()
        }
    }

    impl SessionFactory for CaptureModelSessionFactory {
        fn create(
            &self,
            _base_dir: &Path,
            model: &str,
        ) -> BoxFuture<'_, Result<SessionCreateResult, AgentError>> {
            *self.captured_model.lock().unwrap() = Some(model.to_string());
            let model = model.to_string();
            Box::pin(async move {
                let repo = InMemorySessionRepo::new();
                let storage = repo
                    .create(CreateSessionOptions {
                        name: None,
                        initial_model: Some(model),
                        initial_thinking_level: None,
                        initial_tools: vec![],
                    })
                    .await
                    .map_err(|e| {
                        AgentError::Internal(format!("test: failed to create session: {e}"))
                    })?;
                let id = storage
                    .metadata()
                    .await
                    .map_err(|e| {
                        AgentError::Internal(format!("test: failed to read metadata: {e}"))
                    })?
                    .id;
                Ok((storage, id))
            })
        }
    }

    // ── NoopPlugin ─────────────────────────────────────────────────────────────

    struct NoopPlugin;

    impl llm_harness_agent::Plugin for NoopPlugin {
        fn name(&self) -> &str {
            "noop-test-plugin"
        }
    }

    // ── NoopEnvFactory ──────────────────────────────────────────────────────────

    struct NoopEnvFactory;

    impl EnvFactory for NoopEnvFactory {
        fn create(&self, _cwd: &Path) -> Result<Arc<dyn ExecutionEnv>, AgentError> {
            Ok(Arc::new(llm_harness_loop::test_utils::NoOpEnv))
        }
    }

    // ── Helpers ────────────────────────────────────────────────────────────────

    fn make_spawner(
        client: Arc<dyn LlmClient>,
        model: &str,
    ) -> (
        HarnessSubAgentSpawner,
        Arc<CaptureModelSessionFactory>,
        Arc<dyn LlmClient>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let factory = Arc::new(CaptureModelSessionFactory::new());
        let spawner = HarnessSubAgentSpawner::new(
            model.to_string(),
            client.clone(),
            dir.path().to_path_buf(),
            |_| Box::new(NoopPlugin),
        )
        .env_factory(Arc::new(NoopEnvFactory))
        .session_factory(factory.clone());
        (spawner, factory, client)
    }

    // ── Tests ──────────────────────────────────────────────────────────────────

    /// 已知 alias → 正确 (client, model)。
    #[tokio::test]
    async fn known_alias_uses_correct_client_and_model() {
        let default_client = Arc::new(MockLlmClient::new(vec![]));
        let fast_client = Arc::new(MockLlmClient::new(vec![]));
        let (spawner, factory, _client) = make_spawner(default_client, "default-model");
        let spawner = spawner.alias("fast", fast_client, "fast-model");

        let _handle = spawner
            .spawn(SpawnOptions {
                prompt: "hello".into(),
                provider: ProviderSelection::Alias("fast".into()),
                cwd: None,
                context: ContextMode::None,
            })
            .await
            .expect("spawn with known alias should succeed");

        assert_eq!(factory.model(), Some("fast-model".into()));
    }

    /// 未知 alias → AgentError。
    #[tokio::test]
    async fn unknown_alias_returns_error() {
        let client = Arc::new(MockLlmClient::new(vec![]));
        let (spawner, _factory, _client) = make_spawner(client, "default-model");

        let err = spawner
            .spawn(SpawnOptions {
                prompt: "hello".into(),
                provider: ProviderSelection::Alias("does-not-exist".into()),
                cwd: None,
                context: ContextMode::None,
            })
            .await
            .expect_err("spawn with unknown alias should fail");

        assert!(
            err.to_string().contains("unknown provider alias"),
            "error should mention unknown alias: {err}"
        );
    }

    /// ProviderSelection::Inherit → 使用 default。
    #[tokio::test]
    async fn inherit_uses_default_model() {
        let client = Arc::new(MockLlmClient::new(vec![]));
        let (spawner, factory, _client) = make_spawner(client, "default-model");

        let _handle = spawner
            .spawn(SpawnOptions {
                prompt: "hello".into(),
                provider: ProviderSelection::Inherit,
                cwd: None,
                context: ContextMode::None,
            })
            .await
            .expect("spawn with Inherit should succeed");

        assert_eq!(factory.model(), Some("default-model".into()));
    }

    /// ContextMode::Inject(text) → prompt 前缀正确。
    #[tokio::test]
    async fn context_injection_prefixes_prompt() {
        let mock = MockResponse::text("response from sub-agent");
        let client = Arc::new(MockLlmClient::new(vec![mock]));
        let client_clone = client.clone();
        let (spawner, _factory, _) = make_spawner(client, "default-model");

        let handle = spawner
            .spawn(SpawnOptions {
                prompt: "my task".into(),
                provider: ProviderSelection::Inherit,
                cwd: None,
                context: ContextMode::Inject("relevant context".into()),
            })
            .await
            .expect("spawn with Inject should succeed");

        // join() runs the spawned task to completion.
        let result = handle.join().await.expect("sub-agent should complete");
        assert!(
            result.output.contains("response from sub-agent"),
            "output should contain mock response: {}",
            result.output
        );
        assert_eq!(result.tool_calls_count, 0);

        // Verify the prompt format via captured requests.
        let captured = client_clone.captured_requests.lock().unwrap();
        let first_user_msg = captured
            .first()
            .expect("should have at least one request")
            .messages()
            .iter()
            .find(|m| matches!(m, LlmMessage::User(_)))
            .expect("should have a user message");
        let user_text = match first_user_msg {
            LlmMessage::User(content) => {
                let mut text = String::new();
                for item in content {
                    if let RequestContent::Text(t) = item {
                        text.push_str(t);
                    }
                }
                text
            }
            _ => unreachable!(),
        };
        assert!(
            user_text.contains("Context:\nrelevant context\n\n---\n\n"),
            "user message should contain context prefix, got: {user_text:?}"
        );
    }
}
