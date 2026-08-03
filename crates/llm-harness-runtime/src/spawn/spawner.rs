use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::spawn::message_bus::{AgentEntry, AgentStatus, MessageBus};
use crate::spawn::spawn_types::{
    ContextMode, ProviderSelection, SpawnOptions, SubAgentHandle, SubAgentResult,
};
use futures::FutureExt;
use futures::future::BoxFuture;
use llm_harness_agent::{
    AgentHarnessEvent, Plugin, Session,
    session::SessionStorage,
    session::{JsonlSessionRepo, SessionRepo, types::CreateSessionOptions},
};
use llm_harness_loop::LlmClient;
use llm_harness_types::{AgentError, AgentEvent, ExecutionEnv, HarnessError, HarnessPhase};
use tokio_util::sync::CancellationToken;

use crate::builder::{HarnessBuildError, HarnessBuilder};

// ── EnvFactory ────────────────────────────────────────────────────────────

/// `ExecutionEnv` 的抽象工厂，使 `HarnessSubAgentSpawner` 无需
/// 直接依赖 `llm-harness-runtime-sandbox-os`。
pub trait EnvFactory: Send + Sync {
    fn create(&self, cwd: &Path) -> Result<Arc<dyn ExecutionEnv>, AgentError>;
}

// ── SessionFactory ────────────────────────────────────────────────────────

/// `SessionFactory::create` 的返回类型。
pub type SessionCreateResult = (Arc<dyn SessionStorage>, String);

/// Session 存储的抽象工厂，使调用方可以选择 session 存储后端
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
/// - `bus`：共享的 `MessageBus`，plugin 可据此注册通信工具。
/// - `agent_id`：sub-agent 的标识（取自 session_id）。
pub type PluginFactory = Arc<dyn Fn(&Path, Arc<MessageBus>, &str) -> Box<dyn Plugin> + Send + Sync>;

// ── ProviderAlias ─────────────────────────────────────────────────────────

struct ProviderAlias {
    name: String,
    model: String,
    client: Arc<dyn LlmClient>,
}

// ── HarnessSubAgentSpawner ────────────────────────────────────────────────

/// 为每个 sub-agent 创建独立的 `AgentHarness` 实例，支持可选的 provider 别名。
/// 提供 `spawn_async()`（异步）派发方式。
pub struct HarnessSubAgentSpawner {
    default_model: String,
    default_client: Arc<dyn LlmClient>,
    aliases: Vec<ProviderAlias>,
    sub_plugin_factory: PluginFactory,
    env_factory: Arc<dyn EnvFactory>,
    session_factory: Arc<dyn SessionFactory>,
    session_base_dir: PathBuf,
    /// 共享 MessageBus。`spawn_async` 把 sub-agent 注册到 bus 并 track，
    /// 完成后结果经 event channel 投递回主 agent。
    message_bus: Arc<MessageBus>,
    /// builder 定制闭包（与 WorkflowEngine 的共享闭包同源）。
    builder_customize: Option<Arc<dyn Fn(HarnessBuilder) -> HarnessBuilder + Send + Sync>>,
    /// Maximum concurrent in-flight sub-agents. `None` = unlimited.
    max_concurrent: Option<usize>,
}

/// Owns the provisional cancellation registration and in-flight slot while a
/// sub-agent is being constructed. Dropping the spawn future must release both;
/// async tool cancellation can drop it at any await point without returning an
/// error through the normal control flow.
struct PendingSpawnReservation {
    bus: Arc<MessageBus>,
    pending_id: String,
    slot_acquired: bool,
}

impl PendingSpawnReservation {
    fn new(bus: Arc<MessageBus>, pending_id: String) -> Self {
        Self {
            bus,
            pending_id,
            slot_acquired: false,
        }
    }

    fn mark_slot_acquired(&mut self) {
        self.slot_acquired = true;
    }

    fn commit(mut self) {
        self.bus.unregister_subagent_abort(&self.pending_id);
        self.slot_acquired = false;
    }
}

impl Drop for PendingSpawnReservation {
    fn drop(&mut self) {
        self.bus.unregister_subagent_abort(&self.pending_id);
        if self.slot_acquired {
            self.bus.release_slot();
        }
    }
}

impl HarnessSubAgentSpawner {
    /// 创建新的 spawner。
    ///
    /// - `default_model`：默认模型名。
    /// - `default_client`：默认 LLM client。
    /// - `session_base_dir`：sub-agent session 文件的根目录。
    /// - `bus`：共享 `MessageBus`，`spawn_async` 据此注册 sub-agent 并 track。
    /// - `sub_plugin_factory`：每次 spawn 时用 sub-agent 的 cwd 调用，返回
    ///   不含 `SpawnAgentTool` 的 Plugin（防止递归派发）。闭包参数：
    ///   - `&Path`：sub-agent 工作目录。
    ///   - `Arc<MessageBus>`：共享消息总线，plugin 可据此注册通信工具。
    ///   - `&str`：sub-agent 的标识（取自 session_id）。
    pub fn new(
        default_model: impl Into<String>,
        default_client: Arc<dyn LlmClient>,
        session_base_dir: PathBuf,
        bus: Arc<MessageBus>,
        sub_plugin_factory: impl Fn(&Path, Arc<MessageBus>, &str) -> Box<dyn Plugin>
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
            message_bus: bus,
            builder_customize: None,
            max_concurrent: None,
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
    /// 返回已注册的 provider 别名列表。
    pub fn alias_names(&self) -> Vec<String> {
        self.aliases.iter().map(|a| a.name.clone()).collect()
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

    /// 设置 builder 定制闭包。可覆盖任意 builder 设置。
    pub fn builder_customize(
        mut self,
        customize: Option<Arc<dyn Fn(HarnessBuilder) -> HarnessBuilder + Send + Sync>>,
    ) -> Self {
        self.builder_customize = customize;
        self
    }

    /// Set the maximum number of concurrent in-flight sub-agents.
    /// `None` means unlimited (use with caution). When the limit is
    /// reached, `spawn_async` returns `AgentError::ResourceLimitExceeded`.
    pub fn max_concurrent(mut self, max: Option<usize>) -> Self {
        self.max_concurrent = max;
        self
    }
}

impl HarnessSubAgentSpawner {
    /// 私有：构建 sub-agent harness + tokio task，返回 `SubAgentHandle` 和 session_id。
    ///
    /// harness 被包裹在 `Arc` 中：一个 clone 注册到 `MessageBus` 的 `AgentEntry`，
    /// 另一个 clone move 进 tokio::spawn task。所有 harness 方法取 `&self`，
    /// 因此 `Arc<AgentHarness>` 直接可用于 `subscribe()` / `steer()` / `prompt()`。
    #[cfg(test)]
    fn spawn_inner(
        &self,
        opts: SpawnOptions,
    ) -> BoxFuture<'_, Result<(SubAgentHandle, String), AgentError>> {
        self.spawn_inner_with_abort(opts, CancellationToken::new())
    }

    fn spawn_inner_with_abort(
        &self,
        opts: SpawnOptions,
        abort_token: CancellationToken,
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
        let bus = self.message_bus.clone();
        let builder_customize = self.builder_customize.clone();

        async move {
            if abort_token.is_cancelled() {
                return Err(AgentError::Aborted);
            }

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

            // 3. 创建 session（通过 SessionFactory，不写死后端）。
            //    先创建 session 以获得 session_id，再传给 plugin factory。
            let (storage, session_id) = session_factory.create(&session_base_dir, &model).await?;
            if abort_token.is_cancelled() {
                return Err(AgentError::Aborted);
            }

            // 4. 创建 sub-agent plugin（不含 SpawnAgentTool）。
            //    传 bus 克隆：plugin 可据此注册通信工具。
            //    传 &session_id：plugin 可据此标识 agent。
            let sub_plugin = (sub_plugin_factory)(&cwd, bus.clone(), &session_id);

            // 5. 构建 harness，继承主 agent 的 LLM 行为配置。
            let mut builder = HarnessBuilder::new(&model)
                .install(sub_plugin.as_ref())
                .provider(&model, client);
            if let Some(customize) = &builder_customize {
                builder = customize(builder);
            }
            let harness = builder
                .build_with_session(env, Session::new(storage))
                .map_err(|e: HarnessBuildError| {
                    AgentError::Internal(format!("failed to build sub-agent harness: {e}"))
                })?;

            // 6. 包裹在 Arc 中：一个 clone 给 AgentEntry，一个 move 进 task。
            let harness = Arc::new(harness);

            // 7. 构建 prompt（可选的 context 注入）。
            let prompt = match &opts.context {
                ContextMode::None => opts.prompt,
                ContextMode::Inject(ctx) => {
                    format!("Context:\n{ctx}\n\n---\n\n{}", opts.prompt)
                }
            };

            // 8. 注册 abort token。spawn_async 在进入异步构建前还会用临时 ID
            //    预注册同一个 token，以覆盖 session_id 尚未生成的窗口。
            bus.register_subagent_abort(&session_id, abort_token.clone());
            // 注册到 agent registry，使 bus 可 steer / subscribe。
            bus.register(
                &session_id,
                AgentEntry {
                    status: AgentStatus::Running,
                    harness: Some(harness.clone()),
                    is_main: false,
                },
            );

            // 9. 启动 tokio task。abort_token 注入 harness，使 handle.abort()
            // 能到达 agent loop。
            let abort_clone = abort_token.clone();
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

    /// Spawn sub-agent 后台执行（task 异步跑），经 `bus.track_with_oneshot`
    /// 返回 agent_id。结果经 event channel 投递给主 agent 的 hook/watcher。
    ///
    /// agent_id 取 session_id。
    pub fn spawn_async(&self, opts: SpawnOptions) -> BoxFuture<'_, Result<String, AgentError>> {
        let bus = self.message_bus.clone();
        let max_concurrent = self.max_concurrent;
        Box::pin(async move {
            // Register cancellation before exposing the in-flight slot. Otherwise
            // abort() can observe in_flight=1 while spawn_inner is still creating
            // the session and miss the not-yet-registered token.
            let pending_id = format!("pending-spawn-{}", uuid::Uuid::new_v4());
            let abort_token = CancellationToken::new();
            bus.register_subagent_abort(&pending_id, abort_token.clone());
            let mut reservation = PendingSpawnReservation::new(bus.clone(), pending_id);

            // Atomically acquire an in-flight slot before spawning to close
            // the TOCTOU window: concurrent spawn_async calls (parallel tool
            // execution) would each pass the old check-then-act test.
            // Always acquire so that track_with_oneshot's fetch_sub is balanced.
            match max_concurrent {
                Some(max) => {
                    if !bus.try_acquire_slot(max) {
                        return Err(AgentError::ResourceLimitExceeded(format!(
                            "concurrent sub-agent limit reached ({max} in-flight)"
                        )));
                    }
                }
                None => bus.acquire_slot(),
            }
            reservation.mark_slot_acquired();
            let (handle, session_id) =
                match self.spawn_inner_with_abort(opts, abort_token.clone()).await {
                    Ok(v) => v,
                    Err(e) => return Err(e),
                };
            // spawn_inner has registered the same token under session_id, so
            // removing the temporary key does not open a cancellation gap.
            reservation.commit();
            let async_handle = bus.track_with_oneshot(handle, session_id);
            Ok(async_handle.agent_id().to_string())
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
/// 每次 `create()` 在传入的 `base_dir` 下创建新的 session 目录。
/// 调用方负责拼好完整路径前缀（如 `<task_id>/<step_id>-<attempt>/`）。
pub struct JsonlSessionFactory;

impl SessionFactory for JsonlSessionFactory {
    fn create(
        &self,
        base_dir: &Path,
        model: &str,
    ) -> BoxFuture<'_, Result<SessionCreateResult, AgentError>> {
        let session_dir = base_dir.to_path_buf();
        let model = model.to_string();
        Box::pin(async move {
            tokio::fs::create_dir_all(&session_dir)
                .await
                .map_err(|e| AgentError::Internal(format!("failed to create session dir: {e}")))?;
            let repo = JsonlSessionRepo::new(&session_dir);
            let storage = repo
                .create(CreateSessionOptions {
                    name: None,
                    initial_model: Some(model),
                    initial_thinking_level: None,
                    initial_tools: vec![],
                    ..Default::default()
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
    use crate::spawn::message_bus::message_bus_pair;
    use parking_lot::Mutex;

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
            self.captured_model.lock().clone()
        }
    }

    impl SessionFactory for CaptureModelSessionFactory {
        fn create(
            &self,
            _base_dir: &Path,
            model: &str,
        ) -> BoxFuture<'_, Result<SessionCreateResult, AgentError>> {
            *self.captured_model.lock() = Some(model.to_string());
            let model = model.to_string();
            Box::pin(async move {
                let repo = InMemorySessionRepo::new();
                let storage = repo
                    .create(CreateSessionOptions {
                        name: None,
                        initial_model: Some(model),
                        initial_thinking_level: None,
                        initial_tools: vec![],
                        ..Default::default()
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

    /// Session factory that suspends inside `create()` until its caller drops
    /// the future. Tests use it to exercise cancellation at a real await point.
    struct BlockingSessionFactory {
        entered: Arc<tokio::sync::Notify>,
    }

    impl BlockingSessionFactory {
        fn new() -> Self {
            Self {
                entered: Arc::new(tokio::sync::Notify::new()),
            }
        }

        async fn wait_until_entered(&self) {
            self.entered.notified().await;
        }
    }

    impl SessionFactory for BlockingSessionFactory {
        fn create(
            &self,
            _base_dir: &Path,
            _model: &str,
        ) -> BoxFuture<'_, Result<SessionCreateResult, AgentError>> {
            let entered = self.entered.clone();
            Box::pin(async move {
                entered.notify_one();
                std::future::pending::<Result<SessionCreateResult, AgentError>>().await
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
            std::result::Result<llm_harness_types::ToolResult, llm_harness_types::ToolFailure>,
        > {
            Box::pin(async {
                Ok(llm_harness_types::ToolResult::full(
                    vec![],
                    serde_json::Value::Null,
                    false,
                ))
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
        bus: Arc<MessageBus>,
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
            bus,
            |_, _, _| Box::new(NoopPlugin),
        )
        .env_factory(Arc::new(NoopEnvFactory))
        .session_factory(factory.clone());
        (spawner, factory, client)
    }

    fn make_spawner_with_echo(
        client: Arc<dyn LlmClient>,
        model: &str,
        bus: Arc<MessageBus>,
    ) -> HarnessSubAgentSpawner {
        let dir = tempfile::tempdir().unwrap();
        HarnessSubAgentSpawner::new(
            model.to_string(),
            client,
            dir.path().to_path_buf(),
            bus,
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
        let bus = message_bus_pair();
        let (spawner, factory, _client) = make_spawner(default_client, "default-model", bus);
        let spawner = spawner.alias("fast", fast_client, "fast-model");

        let (_handle, _session_id) = spawner
            .spawn_inner(SpawnOptions {
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
        let bus = message_bus_pair();
        let (spawner, _factory, _client) = make_spawner(client, "default-model", bus);

        let err = spawner
            .spawn_inner(SpawnOptions {
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
        let bus = message_bus_pair();
        let (spawner, factory, _client) = make_spawner(client, "default-model", bus);

        let (_handle, _session_id) = spawner
            .spawn_inner(SpawnOptions {
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
        let bus = message_bus_pair();
        let (spawner, _factory, _) = make_spawner(client, "default-model", bus);

        let (handle, _session_id) = spawner
            .spawn_inner(SpawnOptions {
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
        let bus = message_bus_pair();
        let spawner = make_spawner_with_echo(client, "mock-model", bus);

        let (handle, _session_id) = spawner
            .spawn_inner(SpawnOptions {
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
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::rate_limit_error()]));
        let bus = message_bus_pair();
        let (spawner, _factory, _) = make_spawner(client, "mock-model", bus);

        let (handle, _session_id) = spawner
            .spawn_inner(SpawnOptions {
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

    // ── Async spawn tests ───────────────────────────────────────────────────

    /// 建一个可工作的 spawner：真实 SessionFactory + EnvFactory + NoopPlugin。
    /// MockLlmClient 给一条 "ok" 文本响应，sub-agent 一轮即停。
    fn make_async_test_spawner(bus: Arc<MessageBus>) -> HarnessSubAgentSpawner {
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
        let dir = tempfile::tempdir().unwrap();
        HarnessSubAgentSpawner::new(
            "test-model",
            client,
            dir.path().to_path_buf(),
            bus,
            |_, _, _| Box::new(NoopPlugin),
        )
        .env_factory(Arc::new(NoopEnvFactory))
        .session_factory(Arc::new(CaptureModelSessionFactory::new()))
    }

    #[tokio::test]
    async fn spawn_async_returns_agent_id_and_tracks_in_flight() {
        let bus = message_bus_pair();
        let spawner = make_async_test_spawner(bus.clone());

        let opts = SpawnOptions {
            prompt: "say hi".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        };
        let agent_id = spawner
            .spawn_async(opts)
            .await
            .expect("spawn_async should succeed");
        assert!(!agent_id.is_empty());
        // 等 track task 完成（sub-agent 跑完 "say hi"）
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(bus.in_flight_count(), 0);
    }

    /// max_concurrent enforces a hard limit: when the limit is 0, spawn_async
    /// returns ResourceLimitExceeded immediately.
    #[tokio::test]
    async fn max_concurrent_zero_rejects_spawn() {
        let bus = message_bus_pair();
        let spawner = make_async_test_spawner(bus.clone()).max_concurrent(Some(0));
        let opts = SpawnOptions {
            prompt: "say hi".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        };
        let err = spawner
            .spawn_async(opts)
            .await
            .expect_err("should reject with limit 0");
        assert!(matches!(err, AgentError::ResourceLimitExceeded(_)));
        assert_eq!(bus.in_flight_count(), 0);
    }

    /// max_concurrent with a failing spawn_inner: in_flight must be released
    /// on the error path so the counter doesn't drift.
    #[tokio::test]
    async fn max_concurrent_releases_slot_on_spawn_failure() {
        let bus = message_bus_pair();
        // NoOpSessionFactory → spawn_inner fails at session creation.
        let client = Arc::new(MockLlmClient::new(vec![]));
        let dir = tempfile::tempdir().unwrap();
        let spawner = HarnessSubAgentSpawner::new(
            "test-model",
            client,
            dir.path().to_path_buf(),
            bus.clone(),
            |_, _, _| Box::new(NoopPlugin),
        )
        .env_factory(Arc::new(NoopEnvFactory))
        .max_concurrent(Some(3));

        let opts = SpawnOptions {
            prompt: "say hi".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        };
        let _err = spawner
            .spawn_async(opts)
            .await
            .expect_err("should fail (NoOpSessionFactory)");
        // Slot must have been released.
        assert_eq!(bus.in_flight_count(), 0);

        // After a failure, a subsequent spawn should still be allowed (slot was released).
        let spawner2 = make_async_test_spawner(bus.clone()).max_concurrent(Some(3));
        let opts2 = SpawnOptions {
            prompt: "say hi".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        };
        let _id = spawner2
            .spawn_async(opts2)
            .await
            .expect("should succeed after failure released the slot");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(bus.in_flight_count(), 0);
    }

    #[test]
    fn pending_spawn_reservation_is_cancelled_and_releases_slot_on_drop() {
        let bus = message_bus_pair();
        let token = CancellationToken::new();
        let pending_id = "pending-test".to_string();
        bus.register_subagent_abort(&pending_id, token.clone());
        bus.acquire_slot();

        let mut reservation = PendingSpawnReservation::new(bus.clone(), pending_id);
        reservation.mark_slot_acquired();
        bus.cancel_all();

        assert!(token.is_cancelled(), "abort must reach a pending spawn");
        drop(reservation);
        assert_eq!(
            bus.in_flight_count(),
            0,
            "dropped future must free its slot"
        );
    }

    #[tokio::test]
    async fn dropping_spawn_future_during_session_create_cleans_pending_state() {
        let bus = message_bus_pair();
        let factory = Arc::new(BlockingSessionFactory::new());
        let client = Arc::new(MockLlmClient::new(vec![]));
        let dir = tempfile::tempdir().unwrap();
        let spawner = HarnessSubAgentSpawner::new(
            "test-model",
            client,
            dir.path().to_path_buf(),
            bus.clone(),
            |_, _, _| Box::new(NoopPlugin),
        )
        .env_factory(Arc::new(NoopEnvFactory))
        .session_factory(factory.clone())
        .max_concurrent(Some(1));
        let opts = SpawnOptions {
            prompt: "say hi".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        };

        let spawn_task = tokio::spawn(async move { spawner.spawn_async(opts).await });
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            factory.wait_until_entered(),
        )
        .await
        .expect("spawn must reach SessionFactory::create");

        assert_eq!(bus.in_flight_count(), 1);
        assert_eq!(bus.registered_abort_count(), 1);
        bus.cancel_all();

        spawn_task.abort();
        let join_error = spawn_task
            .await
            .expect_err("dropping the in-flight spawn must cancel its task");
        assert!(join_error.is_cancelled());
        assert_eq!(bus.in_flight_count(), 0);
        assert_eq!(bus.registered_abort_count(), 0);
    }

    /// max_concurrent with successful spawn: in_flight must return to 0
    /// after the sub-agent completes (verifies no double-increment).
    #[tokio::test]
    async fn max_concurrent_balanced_after_successful_spawn() {
        let bus = message_bus_pair();
        let spawner = make_async_test_spawner(bus.clone()).max_concurrent(Some(5));
        let opts = SpawnOptions {
            prompt: "say hi".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        };
        let _id = spawner.spawn_async(opts).await.expect("should succeed");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            bus.in_flight_count(),
            0,
            "in_flight must be 0 after completion"
        );
    }

    // ── Arc<AgentHarness> shared across tokio tasks ────────────────────────────

    /// `AgentHarness` is `Send + Sync`: wrapping it in `Arc` and sharing one
    /// clone across a `tokio::spawn` boundary while another clone is held
    /// externally (the shape `MessageBus::AgentEntry` will need) compiles and
    /// runs. All harness methods take `&self`, so `Arc<AgentHarness>` works
    /// directly for `subscribe()` / `steer()` without `Arc::try_unwrap`.
    #[tokio::test]
    async fn arc_harness_can_be_shared_across_tasks() {
        let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
        let env: Arc<dyn ExecutionEnv> = Arc::new(llm_harness_loop::test_utils::NoOpEnv);
        let harness = Arc::new(
            HarnessBuilder::new("test-model")
                .provider("test-model", client)
                .build(env)
                .await
                .expect("build harness"),
        );

        // Clone for task — Arc<AgentHarness> moves into tokio::spawn.
        let harness_for_task = harness.clone();
        let handle = tokio::spawn(async move {
            let _rx = harness_for_task.subscribe();
            harness_for_task.steer("from-task");
        });

        // Original Arc still usable — AgentEntry would hold this clone.
        let _rx = harness.subscribe();
        harness.steer("from-main");

        handle.await.expect("spawned task should complete");
    }
}
