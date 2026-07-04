use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::async_spawn::{AsyncCompletion, AsyncSpawnHandle, AsyncSpawnRegistry};
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
use crate::peer::{PeerCleanupGuard, PeerDirectory, PeerMessageHook, PeerMetadata, SharedPeerRx};

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
///
/// - `cwd`：sub-agent 工作目录。
/// - `peer_rx`：peer 模式下为 `Some(rx)`（与 `PeerMessageHook` 共享同一
///   `SharedPeerRx`），plugin 可据此注册 `WaitForPeerMessageTool`；非 peer 模式为 `None`。
/// - `agent_id`：peer 模式下为 `Some(id)`（取自 `PeerMetadata.agent_id`），
///   plugin 可据此在 `SendToPeerTool` 中标注发送者身份；非 peer 模式为 `None`。
pub type PluginFactory =
    Arc<dyn Fn(&Path, Option<SharedPeerRx>, Option<&str>) -> Box<dyn Plugin> + Send + Sync>;

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
    /// 可选：peer 通讯录。None 则无 peer 通信。
    peer_directory: Option<Arc<PeerDirectory>>,
    /// 可选：异步投递 registry。None 则无异步投递（spawn_async 会返回错误）。
    async_registry: Option<Arc<AsyncSpawnRegistry>>,
}

impl HarnessSubAgentSpawner {
    /// 创建新的 spawner。
    ///
    /// `sub_plugin_factory` 在每次 spawn 时使用 sub-agent 的 cwd 调用。
    /// 必须返回不含 `SpawnAgentTool` 的 Plugin，以防止递归派发（v0.3+）。
    /// 参数：
    /// - `peer_rx`：peer 模式下 `Some(rx)`（与 `PeerMessageHook` 共享），
    ///   非 peer 模式 `None`。plugin 可据此决定是否注册 `WaitForPeerMessageTool`。
    /// - `agent_id`：peer 模式下 `Some(id)`（取自 `PeerMetadata.agent_id`），
    ///   非 peer 模式 `None`。plugin 可据此在 `SendToPeerTool` 中标注发送者。
    pub fn new(
        default_model: impl Into<String>,
        default_client: Arc<dyn LlmClient>,
        session_base_dir: PathBuf,
        sub_plugin_factory: impl Fn(&Path, Option<SharedPeerRx>, Option<&str>) -> Box<dyn Plugin>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            default_model: default_model.into(),
            default_client,
            aliases: vec![],
            sub_plugin_factory: Arc::new(sub_plugin_factory),
            env_factory: Arc::new(NoOpEnvFactory),
            session_factory: Arc::new(NoOpSessionFactory),
            session_base_dir,
            peer_directory: None,
            async_registry: None,
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

    /// 启用 sub↔sub peer 通信。传入共享的 `PeerDirectory`。
    ///
    /// 启用后，`spawn_with_peer` / `spawn_async_with_peer` 会把 sub-agent
    /// 注册进通讯录，使其能收发 peer 消息。
    pub fn enable_peer_communication(mut self, directory: Arc<PeerDirectory>) -> Self {
        self.peer_directory = Some(directory);
        self
    }

    /// 启用 sub→主 异步投递。传入通过 `async_spawn_pair()` 创建的 registry。
    ///
    /// 启用后，`spawn_async` / `spawn_async_with_peer` 会把 sub-agent 的
    /// completion 投递回主 agent 的 `AsyncSpawnHook`。
    pub fn enable_async_delivery(mut self, registry: Arc<AsyncSpawnRegistry>) -> Self {
        self.async_registry = Some(registry);
        self
    }
}

impl HarnessSubAgentSpawner {
    /// 私有：构建 sub-agent harness + tokio task，返回 `SubAgentHandle` 和 session_id。
    ///
    /// `peer_meta` 决定是否往 `PeerDirectory` 注册：
    /// - `Some(meta)` 且 spawner 已 `enable_peer_communication`：建 peer channel +
    ///   `PeerMessageHook`（延迟注入 `FollowUpHandle`）+ `PeerCleanupGuard`（move 进 task）。
    /// - `None` 或未启用 peer：普通 spawn。
    ///
    /// 返回 `(handle, session_id)`：`spawn_async*` 用 session_id 作 agent_id
    /// （非 peer 模式，设计 §7.4）；peer 模式由调用方提供 agent_id。
    /// `preregister_abort`：true 时将 abort token 预注册到 `async_registry`
    /// （仅 `spawn_async*` 路径，它们会调 `track()`，track task 完成时移除 entry）。
    /// 同步路径（`spawn`/`spawn_with_peer`）传 false——不调 track，预注册的 entry
    /// 永不会被移除，会泄漏。
    fn spawn_inner(
        &self,
        opts: SpawnOptions,
        peer_meta: Option<PeerMetadata>,
        preregister_abort: bool,
    ) -> BoxFuture<'_, Result<(SubAgentHandle, String), AgentError>> {
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
        let peer_directory = self.peer_directory.clone();

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

            // 3. peer 注册（若 peer_meta + peer_directory 都有）。
            //    先于 plugin 创建：register 建 channel + 返回 rx，rx 的克隆传给
            //    plugin factory（供 WaitForPeerMessageTool 注册）和 PeerMessageHook。
            //    register 早于 set_follow_up_handle——此时别的 peer 已能 send，
            //    但 hook 的 follow_up_handle=None 时不取消息（Task 4 降级），
            //    消息留 channel，set_follow_up_handle 后下次 should_stop 再取。
            let peer_rx: Option<SharedPeerRx> = if let Some(meta) = peer_meta.as_ref() {
                if let Some(dir) = peer_directory.as_ref() {
                    let rx = dir
                        .register(meta.clone())
                        .map_err(|e| AgentError::Internal(format!("peer register failed: {e}")))?;
                    Some(rx)
                } else {
                    None
                }
            } else {
                None
            };

            // 4. 创建 sub-agent plugin（不含 SpawnAgentTool）。
            //    传 peer_rx 克隆：plugin 可据此注册 WaitForPeerMessageTool。
            //    传 agent_id：plugin 可据此在 SendToPeerTool 中标注发送者。
            //    agent_id 借自 peer_meta，在下方 build_with_session 前即用毕。
            let sub_plugin = (sub_plugin_factory)(
                &cwd,
                peer_rx.clone(),
                peer_meta.as_ref().map(|m| m.agent_id.as_str()),
            );

            // 5. 创建 session（通过 SessionFactory，不写死后端）。
            // 注意：session 在磁盘上创建后，如果下一步构建 harness 失败，
            // 该 session 会成为孤儿（存在于磁盘但无引用）。
            // 未来可通过内存 storage 先构建 harness、确认成功后再持久化来避免。
            let (storage, session_id) = session_factory.create(&session_base_dir, &model).await?;

            // 6. 建 PeerMessageHook（持同一 peer_rx）。
            let peer_hook: Option<Arc<PeerMessageHook>> =
                peer_rx.map(|rx| Arc::new(PeerMessageHook::new(rx)));

            // 构建 harness（装 peer_hook 若有）。
            let mut builder = HarnessBuilder::new(&model)
                .install(sub_plugin.as_ref())
                .provider(&model, client);
            if let Some(ref hook) = peer_hook {
                builder = builder.should_stop_hook(hook.clone());
            }
            let harness = builder
                .build_with_session(env, Session::new(storage))
                .map_err(|e: HarnessBuildError| {
                    AgentError::Internal(format!("failed to build sub-agent harness: {e}"))
                })?;

            // 7. peer: set_follow_up_handle + guard。
            //    guard 必须在 set_follow_up_handle 之后建，且 move 进执行 task。
            let peer_guard: Option<PeerCleanupGuard> =
                if let (Some(hook), Some(meta), Some(dir)) =
                    (peer_hook.as_ref(), peer_meta.as_ref(), peer_directory.as_ref())
                {
                    hook.set_follow_up_handle(harness.follow_up_handle());
                    Some(PeerCleanupGuard::new(dir.clone(), meta.agent_id.clone()))
                } else {
                    None
                };

            // 8. 构建 prompt（可选的 context 注入）。
            let prompt = match &opts.context {
                ContextMode::None => opts.prompt,
                ContextMode::Inject(ctx) => {
                    format!("Context:\n{ctx}\n\n---\n\n{}", opts.prompt)
                }
            };
            // 9. 创建 abort token。仅在调用方会 track() 时预注册到 registry
            // （spawn_async* 路径）。同步路径（spawn/spawn_with_peer）不调 track，
            // 预注册的 entry 永不会被 track task 移除，导致泄漏。
            // 预注册消除竞态：cancel_all 可能在 spawn 后、track insert 前触发，
            // 漏掉该 sub-agent。提前注册后 track() 的 insert 为幂等覆盖（同 token clone）。
            // key：peer 模式用 meta.agent_id，非 peer 用 session_id。
            let abort_token = CancellationToken::new();
            if preregister_abort && let Some(ref registry) = self.async_registry {
                let key = peer_meta
                    .as_ref()
                    .map(|m| m.agent_id.as_str())
                    .unwrap_or(&session_id);
                registry.register_subagent_abort(key, abort_token.clone());
            }
            // 10. 启动 tokio task。abort_token 注入 harness，使 handle.abort()
            // 能到达 agent loop。
            let abort_clone = abort_token.clone();
            let session_id_clone = session_id.clone();
            let join = tokio::spawn(async move {
                // 将 abort token 注入 harness，使 handle.abort()
                // 能到达 agent loop。
                harness.set_abort_token(abort_clone);
                // peer_guard move 进 task：task 结束（success/error/panic）才 drop→unregister。
                let _guard = peer_guard;

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
                    // 与 workflow engine 的 run_llm_step 一致：
                    //   - 优先取最后一个 FinalAnswer message 的文本（覆盖赋值）
                    //   - 若全程无 FinalAnswer，回退到 TextDelta 拼接（退化路径下保留可见文本）
                    //   - Progress 消息被忽略
                    let mut final_answer: Option<String> = None;
                    let mut text_delta_output = String::new();
                    let mut tool_count = 0u32;
                    while let Some(ev) = mpsc_rx.recv().await {
                        match ev.as_ref() {
                            AgentHarnessEvent::Agent(ev) => match ev {
                                AgentEvent::TextDelta { text, .. } => {
                                    text_delta_output.push_str(text);
                                }
                                _ => {
                                    if let Some((_, _, text)) = ev.as_final_answer() {
                                        final_answer = Some(text);
                                    }
                                }
                            },
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
                    (final_answer, text_delta_output, tool_count)
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

                let (final_answer, text_delta_output, tool_calls_count) = event_task
                    .await
                    .map_err(|_| AgentError::Internal("event collection task panicked".into()))?;

                let cost = harness.usage();
                Ok(SubAgentResult {
                    final_answer: final_answer.unwrap_or(text_delta_output),
                    tool_calls_count,
                    session_id: session_id_clone,
                    cost,
                })
            });

            Ok((SubAgentHandle::new(join, abort_token), session_id))
        }
        .boxed()
    }

    /// 同步、有 peer：`spawn_inner(opts, Some(meta), false)`，返回 `SubAgentHandle`。
    /// 调用方自己 `join()`。不预注册 abort token（不调 track，无需 cancel_all 覆盖）。
    pub fn spawn_with_peer(
        &self,
        opts: SpawnOptions,
        meta: PeerMetadata,
    ) -> BoxFuture<'_, Result<SubAgentHandle, AgentError>> {
        Box::pin(async move {
            let (handle, _session_id) = self.spawn_inner(opts, Some(meta), false).await?;
            Ok(handle)
        })
    }

    /// Spawn sub-agent 后台执行（task 异步跑），经 `registry.track` 返回
    /// `(AsyncSpawnHandle, oneshot::Receiver)`。方法名 "async" 指 sub-agent
    /// 不阻塞当前 tool execute，**不**指结果投递方式——`SpawnAgentTool` 的
    /// wait=true / wait=false 都走此方法，由 `deliver_to_channel` 区分：
    ///
    /// - `deliver_to_channel=false`（wait=true / sync）：结果经 oneshot 直接
    ///   回 tool，**不**投递 completion channel（避免双投递）。in_flight 计数
    ///   此场景无实际作用（主 agent 在 tool execute 阻塞，不进 Idle），但保留
    ///   以统一 task 生命周期管理。
    /// - `deliver_to_channel=true`（wait=false / async）：tool 丢弃 oneshot，
    ///   结果必须经 completion channel 投递给 hook/watcher。
    ///
    /// 需先 `enable_async_delivery`，否则返回错误。
    /// agent_id 取 session_id（非 peer 模式，设计 §7.4）。
    pub fn spawn_async(
        &self,
        opts: SpawnOptions,
        deliver_to_channel: bool,
    ) -> BoxFuture<
        '_,
        Result<
            (
                AsyncSpawnHandle,
                tokio::sync::oneshot::Receiver<AsyncCompletion>,
            ),
            AgentError,
        >,
    > {
        let registry = self.async_registry.clone();
        Box::pin(async move {
            let registry = registry.ok_or_else(|| {
                AgentError::Internal("async delivery not enabled on spawner".into())
            })?;
            let (handle, session_id) = self.spawn_inner(opts, None, true).await?;
            Ok(registry.track(handle, session_id, deliver_to_channel))
        })
    }
    /// Spawn sub-agent 后台执行（task 异步跑）+ peer 注册，经 `registry.track`
    /// 返回 `(AsyncSpawnHandle, oneshot::Receiver)`。与 `spawn_async` 同构，
    /// `deliver_to_channel` 语义见该方法文档。agent_id 取自 `PeerMetadata.agent_id`。
    ///
    /// 需先 `enable_async_delivery`，否则返回错误。
    pub fn spawn_async_with_peer(
        &self,
        opts: SpawnOptions,
        meta: PeerMetadata,
        deliver_to_channel: bool,
    ) -> BoxFuture<
        '_,
        Result<
            (
                AsyncSpawnHandle,
                tokio::sync::oneshot::Receiver<AsyncCompletion>,
            ),
            AgentError,
        >,
    > {
        let registry = self.async_registry.clone();
        Box::pin(async move {
            let registry = registry.ok_or_else(|| {
                AgentError::Internal("async delivery not enabled on spawner".into())
            })?;
            let agent_id = meta.agent_id.clone();
            let (handle, _session_id) = self.spawn_inner(opts, Some(meta), true).await?;
            Ok(registry.track(handle, agent_id, deliver_to_channel))
        })
    }
}

impl SubAgentSpawner for HarnessSubAgentSpawner {
    fn spawn(&self, opts: SpawnOptions) -> BoxFuture<'_, Result<SubAgentHandle, AgentError>> {
        Box::pin(async move {
            let (handle, _session_id) = self.spawn_inner(opts, None, false).await?;
            Ok(handle)
        })
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
    use crate::async_spawn::async_spawn_pair;
    use crate::peer::{PeerDirectory, PeerMetadata};
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

    // ── EchoPlugin (for multi-turn final_answer test) ──────────────────────────

    struct EchoPlugin;

    impl llm_harness_agent::Plugin for EchoPlugin {
        fn name(&self) -> &str {
            "echo-test-plugin"
        }

        fn register_tools(&self, tools: &mut Vec<Arc<dyn llm_harness_types::Tool>>) {
            tools.push(Arc::new(EchoTool));
        }
    }

    struct EchoTool;

    impl llm_harness_types::Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "echoes back nothing"
        }

        fn parameters_schema(&self) -> &serde_json::Value {
            static SCHEMA: std::sync::LazyLock<serde_json::Value> =
                std::sync::LazyLock::new(|| serde_json::json!({}));
            &SCHEMA
        }

        fn execute<'a>(
            &'a self,
            _args: serde_json::Value,
            _ctx: &'a llm_harness_types::ToolContext,
        ) -> BoxFuture<
            'a,
            std::result::Result<llm_harness_types::ToolResult, llm_harness_types::ToolError>,
        > {
            Box::pin(async {
                Ok(llm_harness_types::ToolResult {
                    content: vec![],
                    details: serde_json::Value::Null,
                    terminate: false,
                })
            })
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
            |_, _, _| Box::new(NoopPlugin),
        )
        .env_factory(Arc::new(NoopEnvFactory))
        .session_factory(factory.clone());
        (spawner, factory, client)
    }

    /// Like `make_spawner` but uses `EchoPlugin` (registers an `echo` tool)
    /// instead of `NoopPlugin`.
    fn make_spawner_with_echo(client: Arc<dyn LlmClient>, model: &str) -> HarnessSubAgentSpawner {
        let dir = tempfile::tempdir().unwrap();
        HarnessSubAgentSpawner::new(
            model.to_string(),
            client,
            dir.path().to_path_buf(),
            |_, _, _| Box::new(EchoPlugin),
        )
        .env_factory(Arc::new(NoopEnvFactory))
        .session_factory(Arc::new(CaptureModelSessionFactory::new()))
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
            result.final_answer.contains("response from sub-agent"),
            "final_answer should contain mock response: {}",
            result.final_answer
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

    /// 多轮场景：第一轮 tool_use（产生 Progress 消息 + 工具调用），
    /// 第二轮 text（产生 FinalAnswer 消息）。
    /// 验证 final_answer 只取 FinalAnswer 消息的文本，不混入 Progress 文本。
    #[tokio::test]
    async fn final_answer_extracts_last_final_answer_ignoring_progress() {
        let responses = vec![
            MockResponse::tool_use("call_1", "echo", "{}"),
            MockResponse::text("the real answer"),
        ];
        let client = Arc::new(MockLlmClient::new(responses));
        let spawner = make_spawner_with_echo(client, "mock-model");

        let handle = spawner
            .spawn(SpawnOptions {
                prompt: "do the thing".into(),
                provider: ProviderSelection::Inherit,
                cwd: None,
                context: ContextMode::None,
            })
            .await
            .expect("spawn should succeed");

        let result = handle.join().await.expect("sub-agent should complete");
        assert_eq!(result.final_answer, "the real answer");
        assert_eq!(result.tool_calls_count, 1);
    }

    /// 无 FinalAnswer 退化路径：stream 直接报错（RateLimit 不可 retry，
    /// spawner 默认 retry=None → max_retries=0），loop emit Error 退出，
    /// 全程无 FinalAnswer 消息也无 TextDelta。
    /// 验证 final_answer 兜底为空字符串而非 panic。
    #[tokio::test]
    async fn final_answer_falls_back_to_empty_when_no_final_answer() {
        let client = Arc::new(MockLlmClient::new(vec![
            MockResponse::rate_limit_error(),
        ]));
        let (spawner, _factory, _) = make_spawner(client, "mock-model");

        let handle = spawner
            .spawn(SpawnOptions {
                prompt: "do the thing".into(),
                provider: ProviderSelection::Inherit,
                cwd: None,
                context: ContextMode::None,
            })
            .await
            .expect("spawn should succeed");

        let result = handle.join().await.expect("sub-agent should complete");
        // 无 FinalAnswer 且无 TextDelta → 兜底为空字符串。
        assert_eq!(result.final_answer, "");
    }
    // ── Peer / Async spawn tests (Task 9) ───────────────────────────────────────

    /// 建一个可工作的 spawner：真实 SessionFactory + EnvFactory + NoopPlugin。
    /// MockLlmClient 给一条 "ok" 文本响应，sub-agent 一轮即停。
    fn make_peer_test_spawner() -> HarnessSubAgentSpawner {
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
        let dir = tempfile::tempdir().unwrap();
        HarnessSubAgentSpawner::new("test-model", client, dir.path().to_path_buf(), |_, _, _| {
            Box::new(NoopPlugin)
        })
        .env_factory(Arc::new(NoopEnvFactory))
        .session_factory(Arc::new(CaptureModelSessionFactory::new()))
    }

    #[tokio::test]
    async fn spawn_async_returns_handle_and_tracks_in_flight() {
        let spawner = make_peer_test_spawner();
        let (registry, _hook) = async_spawn_pair();
        let spawner = spawner.enable_async_delivery(registry.clone());

        let opts = SpawnOptions {
            prompt: "say hi".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        };
        let (handle, _sync_rx) = spawner
            .spawn_async(opts, true)
            .await
            .expect("spawn_async should succeed");
        assert!(!handle.agent_id().is_empty());
        // 等 track task 完成（sub-agent 跑完 "say hi"）
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(registry.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn spawn_with_peer_registers_in_directory() {
        let spawner = make_peer_test_spawner();
        let dir = Arc::new(PeerDirectory::new());
        let spawner = spawner.enable_peer_communication(dir.clone());

        let opts = SpawnOptions {
            prompt: "hi".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        };
        let meta = PeerMetadata {
            agent_id: "test-peer".into(),
            capabilities: vec!["test".into()],
            description: "test".into(),
        };
        let handle = spawner
            .spawn_with_peer(opts, meta)
            .await
            .expect("spawn_with_peer should succeed");
        // peer 注册期间在通讯录里
        assert!(
            dir.list_peers().iter().any(|p| p.agent_id == "test-peer"),
            "peer should be registered while sub-agent is running"
        );
        // join 等 sub-agent 完成
        let _result = handle.join().await.expect("sub-agent should complete");
        // 完成后 unregister
        assert!(
            !dir.list_peers().iter().any(|p| p.agent_id == "test-peer"),
            "peer should be unregistered after sub-agent completes"
        );
    }

    #[tokio::test]
    async fn spawn_async_with_peer_tracks_and_registers() {
        let spawner = make_peer_test_spawner();
        let dir = Arc::new(PeerDirectory::new());
        let (registry, _hook) = async_spawn_pair();
        let spawner = spawner
            .enable_peer_communication(dir.clone())
            .enable_async_delivery(registry.clone());

        let opts = SpawnOptions {
            prompt: "hi".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        };
        let meta = PeerMetadata {
            agent_id: "async-peer".into(),
            capabilities: vec![],
            description: "".into(),
        };
        let (handle, _sync_rx) = spawner
            .spawn_async_with_peer(opts, meta, true)
            .await
            .expect("spawn_async_with_peer should succeed");
        assert_eq!(handle.agent_id(), "async-peer");
        assert!(
            dir.list_peers().iter().any(|p| p.agent_id == "async-peer"),
            "peer should be registered while sub-agent is running"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !dir.list_peers().iter().any(|p| p.agent_id == "async-peer"),
            "peer should be unregistered after sub-agent completes"
        );
        assert_eq!(registry.in_flight_count(), 0);
    }
    /// peer 模式下 plugin factory 收到 `Some(rx)`，非 peer 模式收到 `None`。
    /// 验证 `SharedPeerRx` 被正确传递给 plugin，使其能注册 `WaitForPeerMessageTool`。
    #[tokio::test]
    async fn plugin_factory_receives_peer_rx_in_peer_mode() {
        use std::sync::Mutex as StdMutex;

        let dir = tempfile::tempdir().unwrap();
        // 捕获 plugin factory 收到的 `peer_rx` 是 `Some` 还是 `None`。
        let captured: Arc<StdMutex<Vec<bool>>> = Arc::new(StdMutex::new(vec![]));
        let captured_clone = captured.clone();
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
        let spawner = HarnessSubAgentSpawner::new(
            "test-model",
            client,
            dir.path().to_path_buf(),
            move |_cwd, peer_rx, _agent_id| {
                captured_clone.lock().unwrap().push(peer_rx.is_some());
                Box::new(NoopPlugin)
            },
        )
        .env_factory(Arc::new(NoopEnvFactory))
        .session_factory(Arc::new(CaptureModelSessionFactory::new()));

        // 非 peer 模式：factory 收到 None
        let opts = SpawnOptions {
            prompt: "hi".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        };
        let _ = spawner.spawn(opts).await.unwrap();
        assert_eq!(
            captured.lock().unwrap().as_slice(),
            &[false],
            "non-peer spawn should pass None to plugin factory"
        );

        // peer 模式：factory 收到 Some(rx)
        let peer_dir = Arc::new(PeerDirectory::new());
        let spawner = spawner.enable_peer_communication(peer_dir.clone());
        let opts = SpawnOptions {
            prompt: "hi".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        };
        let meta = PeerMetadata {
            agent_id: "rx-peer".into(),
            capabilities: vec![],
            description: "".into(),
        };
        let _ = spawner.spawn_with_peer(opts, meta).await.unwrap();
        assert_eq!(
            captured.lock().unwrap().as_slice(),
            &[false, true],
            "peer-mode spawn should pass Some(rx) to plugin factory"
        );
    }
}
