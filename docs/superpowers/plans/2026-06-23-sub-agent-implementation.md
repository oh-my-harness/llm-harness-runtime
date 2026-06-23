# Sub-Agent 系统实现方案

> **面向执行者：** 必须使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 按任务逐步实现。步骤使用 checkbox 语法 (`- [ ]`) 跟踪进度。

**目标：** 构建 sub-agent 派发系统，让 coding-agent 能派发 sub-agent 并行执行子任务，支持 LLM 主动触发（SpawnAgentTool）和用户显式触发（/sub-agent REPL 命令）两种模式。

**架构：** 新 `SubAgentSpawner` trait 定义在 `llm-harness-agent` 中，由 `llm-harness-runtime` 中的 `HarnessSubAgentSpawner` 实现。Sub-agent 运行在独立 `AgentHarness` 实例上（不共享 session，不可递归派发）。删除 `crates/llm-harness-runtime/src/subagent.rs` 中的旧占位 `SubAgentSpawner`。

**技术栈：** Rust（edition 2024）、tokio、futures、tokio-util（CancellationToken）、serde_json、schemars（用于 SpawnAgentTool 的 JSON Schema 生成）

## 全局约束

- 框架层定义机制（trait），App 层实现策略
- Sub-agent 不可递归派发（v0.3+）
- 所有已有测试必须继续通过
- 旧 `crates/llm-harness-runtime/src/subagent.rs` 必须删除并清理 re-export
- Coding agent 测试代码（`plugin.rs` 测试）必须适配新 `CodingAgentPlugin::new()` 签名
- 每次 commit 必须通过 `cargo fmt` + `cargo clippy --all-targets --all-features`

---

### 任务 1：为 `AgentHarness` 添加 `set_abort_token()`

**文件：**
- 修改：`crates/llm-harness-agent/src/harness.rs`

**接口：**
- 消费：（无）
- 产出：`AgentHarness` 上的 `pub fn set_abort_token(&self, token: CancellationToken)`

**背景：** Spec 的 `spawn()` 步骤 7 需要将 spawner 的 `CancellationToken` 传入 sub-agent harness，以便 `handle.abort()` 能真正取消 agent loop。目前 `AgentHarness` 只在 `run_loop()`（第 1297 行）内部创建 abort token。需要一种在调用 `prompt()` 之前注入外部 token 的方法。

- [ ] **步骤 1：添加 `set_abort_token` 方法**

在 `crates/llm-harness-agent/src/harness.rs` 的 `abort()` 方法（第 1020 行）之后添加：

```rust
/// 为下一次 `prompt()` 运行设置外部提供的 abort token。
///
/// 设置后，此 token 将替代 `run_loop()` 内部创建的 token。
/// 适用于 sub-agent 派发场景（父 agent 控制 abort 信号）。
/// 此 token 在下次 `prompt()` 调用时被消费；后续未调用
/// 此方法的运行将恢复自行创建 token。
pub fn set_abort_token(&self, token: CancellationToken) {
    self.inner.lock().unwrap().current_abort = Some(token);
}
```

- [ ] **步骤 2：调整 `run_loop()` 以支持预设 abort token**

在 `run_loop()`（第 1290 行）中，当前代码总是创建新 token：

```rust
let abort = CancellationToken::new();
inner.current_abort = Some(abort.clone());
```

改为仅在外部未设置时创建新 token：

```rust
let abort = inner
    .current_abort
    .take()
    .unwrap_or_else(CancellationToken::new);
inner.current_abort = Some(abort.clone());
```

- [ ] **步骤 3：运行已有测试验证无破坏**

运行：`cargo test -p llm-harness-agent --all-features`
预期：所有已有测试通过

- [ ] **步骤 4：提交**

```bash
git add crates/llm-harness-agent/src/harness.rs
git commit -m "feat(agent): add set_abort_token() for external abort control

Allows sub-agent spawner to inject its CancellationToken before calling
prompt(), so handle.abort() reaches the agent loop instead of being
orphaned."
```

---

### 任务 2：在 `llm-harness-agent` 中创建 `SubAgentSpawner` trait 及相关类型

**文件：**
- 新建：`crates/llm-harness-agent/src/sub_agent.rs`
- 修改：`crates/llm-harness-agent/src/lib.rs`
- 查看：`crates/llm-harness-agent/Cargo.toml`

**接口：**
- 消费：（无——全新模块）
- 产出：
  - `pub struct SpawnOptions { pub prompt, pub provider, pub cwd, pub context }`
  - `pub enum ProviderSelection { Inherit, Alias(String) }`
  - `pub enum ContextMode { None, Inject(String) }`
  - `pub trait SubAgentSpawner: Send + Sync { fn spawn(&self, opts: SpawnOptions) -> BoxFuture<'_, Result<SubAgentHandle, AgentError>>; }`
  - `pub struct SubAgentHandle { pub(crate) join, pub(crate) abort }`
  - `impl SubAgentHandle { pub fn abort_token(), pub fn abort(), pub async fn join() }`
  - `impl Drop for SubAgentHandle`
  - `#[derive(Clone)] pub struct SubAgentResult { pub output, pub tool_calls_count, pub session_id }`

- [ ] **步骤 1：确认 `tokio-util` 依赖**

`crates/llm-harness-agent/Cargo.toml` 中已有 `tokio-util.workspace = true`，无需改动。

- [ ] **步骤 2：编写模块**

创建 `crates/llm-harness-agent/src/sub_agent.rs`：

```rust
use std::path::PathBuf;

use futures::future::BoxFuture;
use llm_harness_types::AgentError;
use tokio_util::sync::CancellationToken;

/// 启动 sub-agent 的完整配置。
pub struct SpawnOptions {
    /// 派发给 sub-agent 的初始 prompt。
    pub prompt: String,
    /// 使用哪个 provider；Inherit 表示继承父 agent。
    pub provider: ProviderSelection,
    /// 工作目录；None 继承父 agent 的 cwd。
    pub cwd: Option<PathBuf>,
    /// 向 sub-agent 传递父 agent 上下文的方式。
    pub context: ContextMode,
}

/// 选择 sub-agent 使用的 provider。
pub enum ProviderSelection {
    /// 使用与父 agent 相同的 client + model。
    Inherit,
    /// 使用在 HarnessSubAgentSpawner 上注册的命名别名。
    Alias(String),
}

/// 向 sub-agent 传递父 agent 上下文的方式。
pub enum ContextMode {
    /// Sub-agent 从零开始，无父 context。
    None,
    /// 将上下文文本作为前缀注入 sub-agent 的 prompt。
    Inject(String),
}

/// 派发 sub-agent 的核心 trait。
///
/// spawn() 只做初始化（建 session、建 harness、起 tokio task），
/// 实际执行在 handle.join() 处等待。
pub trait SubAgentSpawner: Send + Sync {
    fn spawn(
        &self,
        opts: SpawnOptions,
    ) -> BoxFuture<'_, Result<SubAgentHandle, AgentError>>;
}

/// sub-agent 运行中的句柄。
pub struct SubAgentHandle {
    pub(crate) join: tokio::task::JoinHandle<Result<SubAgentResult, AgentError>>,
    pub(crate) abort: CancellationToken,
}

impl SubAgentHandle {
    /// 返回 abort token 的克隆，用于外部取消（不 move self）。
    pub fn abort_token(&self) -> CancellationToken {
        self.abort.clone()
    }

    /// 取消 sub-agent（发 abort 信号，不阻塞）。
    pub fn abort(&self) {
        self.abort.cancel();
    }

    /// 等待 sub-agent 完成并拿到结果。
    pub async fn join(self) -> Result<SubAgentResult, AgentError> {
        self.join.await.unwrap_or(Err(AgentError::Aborted))
    }
}

impl Drop for SubAgentHandle {
    fn drop(&mut self) {
        // Drop 时取消，防止孤儿 tokio task 继续消耗 LLM token。
        self.abort.cancel();
    }
}

/// sub-agent 完成后的结果。
#[derive(Clone)]
pub struct SubAgentResult {
    /// 最后一个 turn 中 TextDelta 拼接的完整文本输出。
    pub output: String,
    /// 本次执行共调用了几次工具。
    pub tool_calls_count: u32,
    /// Sub-agent 的 session ID（供调试和审计）。
    pub session_id: String,
}
```

- [ ] **步骤 3：在 `lib.rs` 中添加模块声明和 re-export**

在 `crates/llm-harness-agent/src/lib.rs` 中：

第 17 行 (`pub mod harness;`) 之后添加：
```rust
pub mod sub_agent;
```

`pub use` 块中（第 23 行之后）添加：
```rust
pub use sub_agent::{
    ContextMode, ProviderSelection, SpawnOptions, SubAgentHandle, SubAgentResult,
    SubAgentSpawner,
};
```

`pub mod prelude` 块中（第 48 行之后），加入 `pub use crate::{...}` 列表：
```rust
    ContextMode, ProviderSelection, SpawnOptions, SubAgentHandle, SubAgentResult,
    SubAgentSpawner,
```

- [ ] **步骤 4：验证编译**

运行：`cargo build -p llm-harness-agent`
预期：编译无错误

- [ ] **步骤 5：提交**

```bash
git add crates/llm-harness-agent/src/sub_agent.rs crates/llm-harness-agent/src/lib.rs
git commit -m "feat(agent): add SubAgentSpawner trait with SpawnOptions, SubAgentHandle, SubAgentResult"
```

---

### 任务 3：从 `llm-harness-runtime` 删除旧 `SubAgentSpawner`

**文件：**
- 删除：`crates/llm-harness-runtime/src/subagent.rs`
- 修改：`crates/llm-harness-runtime/src/lib.rs`

**背景：** 旧 `SubAgentSpawner` trait（subagent.rs:48-55）和 `SubAgentSpawnOpts`/`SubAgentResult` 类型是 v0.1 占位实现，已被任务 2 中 `llm-harness-agent` 的新 trait 替代。必须清理所有引用。

- [ ] **步骤 1：删除旧文件**

运行：`rm crates/llm-harness-runtime/src/subagent.rs`

- [ ] **步骤 2：清理 lib.rs**

在 `crates/llm-harness-runtime/src/lib.rs` 中：

删除第 10 行：`//! - Sub-agent spawning (\`SubAgentSpawner\`)`

删除第 29 行：`pub mod subagent;`

删除第 54 行：`pub use subagent::{SubAgentResult, SubAgentSpawnOpts, SubAgentSpawner};`

- [ ] **步骤 3：验证编译**

运行：`cargo build -p llm-harness-runtime`
预期：编译无错误（检查无其他 crate 导入旧类型）

运行：`cargo build --workspace` 验证所有 crate 编译通过
预期：所有 crate 编译通过

- [ ] **步骤 4：提交**

```bash
git add crates/llm-harness-runtime/src/subagent.rs crates/llm-harness-runtime/src/lib.rs
git commit -m "refactor(runtime): delete old SubAgentSpawner placeholder, replaced by llm-harness-agent::sub_agent"
```

---

### 任务 4：在 `llm-harness-runtime` 中创建 `EnvFactory` trait 和 `HarnessSubAgentSpawner`

**文件：**
- 新建：`crates/llm-harness-runtime/src/spawner.rs`
- 修改：`crates/llm-harness-runtime/src/lib.rs`

**接口：**
- 消费：`SubAgentSpawner`、`SpawnOptions`、`SubAgentHandle`、`SubAgentResult`（来自 `llm-harness-agent::sub_agent`）
- 消费：`AgentHarness`、`Session`、`Plugin`（来自 `llm-harness-agent`）
- 消费：`ExecutionEnv`（来自 `llm-harness-types`）
- 消费：`LlmClient`（来自 `llm-harness-loop`）
- 消费：`HarnessBuilder`、`HarnessBuildError`（来自自身 crate）
- 产出：
  - `pub trait EnvFactory: Send + Sync { fn create(&self, cwd: &Path) -> Arc<dyn ExecutionEnv>; }`
  - `pub struct HarnessSubAgentSpawner` 含 `new()`、`alias()`、`env_factory()` builder 方法
  - `impl SubAgentSpawner for HarnessSubAgentSpawner`

- [ ] **步骤 1：编写 spawner 模块**

创建 `crates/llm-harness-runtime/src/spawner.rs`：

```rust
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::FutureExt;
use llm_harness_agent::{
    AgentHarness, AgentHarnessEvent, Plugin, Session,
    session::{JsonlSessionRepo, SessionRepo, types::CreateSessionOptions},
    sub_agent::{
        ContextMode, ProviderSelection, SpawnOptions, SubAgentHandle, SubAgentResult,
        SubAgentSpawner,
    },
};
use llm_harness_loop::LlmClient;
use llm_harness_types::{AgentError, AgentEvent, ExecutionEnv, HarnessPhase};
use tokio_util::sync::CancellationToken;

use crate::builder::{HarnessBuildError, HarnessBuilder};

// ── EnvFactory ────────────────────────────────────────────────────────────

/// ExecutionEnv 的抽象工厂。
///
/// 位于 llm-harness-runtime 中，使 HarnessSubAgentSpawner 无需
/// 直接依赖 llm-harness-runtime-sandbox-os。
pub trait EnvFactory: Send + Sync {
    fn create(&self, cwd: &Path) -> Arc<dyn ExecutionEnv>;
}

// ── ProviderAlias ─────────────────────────────────────────────────────────

struct ProviderAlias {
    name: String,
    model: String,
    client: Arc<dyn LlmClient>,
}

// ── HarnessSubAgentSpawner ────────────────────────────────────────────────

/// SubAgentSpawner 的具体实现：为每个 sub-agent 创建独立的 AgentHarness
/// 实例，支持可选的 provider 别名。
pub struct HarnessSubAgentSpawner {
    default_model: String,
    default_client: Arc<dyn LlmClient>,
    aliases: Vec<ProviderAlias>,
    sub_plugin_factory: Arc<dyn Fn(&Path) -> Box<dyn Plugin> + Send + Sync>,
    env_factory: Arc<dyn EnvFactory>,
    session_base_dir: PathBuf,
}

impl HarnessSubAgentSpawner {
    /// 创建新的 spawner。
    ///
    /// `sub_plugin_factory` 在每次 spawn 时使用 sub-agent 的 cwd 调用。
    /// 必须返回不含 SpawnAgentTool 的 Plugin，以防止递归派发（v0.3+）。
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
            session_base_dir,
        }
    }

    /// 注册 provider 别名（如 "fast" → 更便宜的模型）。
    pub fn alias(
        mut self,
        name: &str,
        client: Arc<dyn LlmClient>,
        model: &str,
    ) -> Self {
        self.aliases.push(ProviderAlias {
            name: name.to_string(),
            model: model.to_string(),
            client,
        });
        self
    }

    /// 设置环境工厂（默认：NoOpEnvFactory —— 未设置真实 factory 时
    /// create() 会 panic）。
    pub fn env_factory(mut self, factory: Arc<dyn EnvFactory>) -> Self {
        self.env_factory = factory;
        self
    }
}

impl SubAgentSpawner for HarnessSubAgentSpawner {
    fn spawn(
        &self,
        opts: SpawnOptions,
    ) -> futures::future::BoxFuture<'_, Result<SubAgentHandle, AgentError>> {
        let default_model = self.default_model.clone();
        let default_client = self.default_client.clone();
        let aliases: Vec<(&str, &str, &Arc<dyn LlmClient>)> = self
            .aliases
            .iter()
            .map(|a| (a.name.as_str(), a.model.as_str(), &a.client))
            .collect();
        let sub_plugin_factory = self.sub_plugin_factory.clone();
        let env_factory = self.env_factory.clone();
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
            let env = env_factory.create(&cwd);

            // 3. 创建 sub-agent plugin（不含 SpawnAgentTool）。
            let sub_plugin = (sub_plugin_factory)(&cwd);

            // 4. 创建 session。
            let session_dir = session_base_dir.join("sub-agents");
            std::fs::create_dir_all(&session_dir).map_err(|e| {
                AgentError::Internal(format!("failed to create session dir: {e}"))
            })?;
            let repo = JsonlSessionRepo::new(&session_dir);
            let storage = repo
                .create(CreateSessionOptions {
                    name: None,
                    initial_model: Some(model.clone()),
                    initial_thinking_level: None,
                    initial_tools: vec![],
                })
                .await
                .map_err(|e| {
                    AgentError::Internal(format!("failed to create sub-agent session: {e}"))
                })?;
            let session_id = storage.id().to_string();

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

                let mut rx = harness.subscribe();

                // 事件收集 task。
                let event_task = tokio::spawn(async move {
                    let mut output = String::new();
                    let mut tool_count = 0u32;
                    loop {
                        match rx.recv().await {
                            Ok(ev) => match ev.as_ref() {
                                AgentHarnessEvent::Agent(AgentEvent::MessageUpdate {
                                    partial,
                                    ..
                                }) => {
                                    for block in &partial.content {
                                        if let llm_harness_types::ContentBlock::Text {
                                            text,
                                        } = block
                                        {
                                            output.push_str(text);
                                        }
                                    }
                                }
                                AgentHarnessEvent::ToolCallStart { .. } => {
                                    tool_count += 1;
                                }
                                AgentHarnessEvent::PhaseChange {
                                    to: HarnessPhase::Idle,
                                    ..
                                } => break,
                                _ => {}
                            },
                            Err(_) => break,
                        }
                    }
                    (output, tool_count)
                });

                if let Err(e) = harness.prompt(&prompt).await {
                    return Err(AgentError::Internal(format!(
                        "sub-agent prompt failed: {e}"
                    )));
                }

                let (output, tool_calls_count) = event_task
                    .await
                    .map_err(|_| AgentError::Internal("event collection task panicked".into()))?;

                Ok(SubAgentResult {
                    output,
                    tool_calls_count,
                    session_id: session_id_clone,
                })
            });

            Ok(SubAgentHandle { join, abort })
        }
        .boxed()
    }
}

// ── 默认（空操作）EnvFactory ────────────────────────────────────────────

struct NoOpEnvFactory;

impl EnvFactory for NoOpEnvFactory {
    fn create(&self, _cwd: &Path) -> Arc<dyn ExecutionEnv> {
        panic!("EnvFactory not set — call HarnessSubAgentSpawner::env_factory() before use")
    }
}
```

- [ ] **步骤 2：在 `lib.rs` 中注册模块和 re-export**

在 `crates/llm-harness-runtime/src/lib.rs` 中：

在删除 `pub mod subagent;` 的位置添加模块声明：
```rust
pub mod spawner;
```

在已有 `pub use` 块之后添加 re-export：
```rust
pub use spawner::{EnvFactory, HarnessSubAgentSpawner};
```

- [ ] **步骤 3：验证编译**

运行：`cargo build -p llm-harness-runtime`
预期：编译无错误

- [ ] **步骤 4：提交**

```bash
git add crates/llm-harness-runtime/src/spawner.rs crates/llm-harness-runtime/src/lib.rs
git commit -m "feat(runtime): add HarnessSubAgentSpawner with EnvFactory trait"
```

---

### 任务 5：在 `llm-harness-runtime-sandbox-os` 中创建 `OsEnvFactory`

**文件：**
- 新建：`crates/llm-harness-runtime-sandbox-os/src/env_factory.rs`
- 修改：`crates/llm-harness-runtime-sandbox-os/src/lib.rs`

**接口：**
- 消费：`EnvFactory`（来自 `llm-harness-runtime::spawner`）
- 消费：`ExecutionEnv`（来自 `llm-harness-types`）
- 消费：`OsEnv`（来自自身 crate）
- 产出：`pub struct OsEnvFactory;` 实现 `impl EnvFactory`

- [ ] **步骤 1：编写 OsEnvFactory**

创建 `crates/llm-harness-runtime-sandbox-os/src/env_factory.rs`：

```rust
use std::path::Path;
use std::sync::Arc;

use llm_harness_runtime::EnvFactory;
use llm_harness_types::ExecutionEnv;

use crate::OsEnv;

/// 基于真实 OS 的 EnvFactory 实现，使用 OsEnv。
pub struct OsEnvFactory;

impl EnvFactory for OsEnvFactory {
    fn create(&self, cwd: &Path) -> Arc<dyn ExecutionEnv> {
        Arc::new(OsEnv::new(cwd))
    }
}
```

- [ ] **步骤 2：注册模块**

在 `crates/llm-harness-runtime-sandbox-os/src/lib.rs` 中：

第 1 行 (`pub mod env;`) 之后添加：
```rust
pub mod env_factory;
```

第 5 行 (`pub use sandbox::OsEnvSandbox;`) 之后添加：
```rust
pub use env_factory::OsEnvFactory;
```

- [ ] **步骤 3：验证编译**

运行：`cargo build -p llm-harness-runtime-sandbox-os`
预期：编译无错误

- [ ] **步骤 4：提交**

```bash
git add crates/llm-harness-runtime-sandbox-os/src/env_factory.rs crates/llm-harness-runtime-sandbox-os/src/lib.rs
git commit -m "feat(sandbox-os): add OsEnvFactory implementing EnvFactory trait"
```

---

### 任务 6：在 coding-agent 中创建 `SpawnAgentTool`

**文件：**
- 新建：`examples/coding-agent/src/tools/spawn_agent.rs`
- 修改：`examples/coding-agent/src/tools/mod.rs`

**接口：**
- 消费：`SubAgentSpawner`、`SpawnOptions`、`SubAgentHandle`、`SubAgentResult`、`ProviderSelection`、`ContextMode`（来自 `llm-harness-agent::sub_agent`）
- 消费：`Tool`、`ToolContext`、`ToolResult`、`ToolError`、`ContentBlock`、`AgentError`（来自 `llm-harness-types`）
- 产出：`pub struct SpawnAgentTool` 实现 `Tool`

- [ ] **步骤 1：确认依赖**

coding-agent 已依赖 `serde_json`、`tokio`、`tokio-util`、`futures` 及所有框架 crate。`schemars` 来自 `llm-harness-types` 的 re-export。无需修改 Cargo.toml。

- [ ] **步骤 2：编写 SpawnAgentTool**

创建 `examples/coding-agent/src/tools/spawn_agent.rs`：

```rust
use std::path::PathBuf;
use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_agent::sub_agent::{
    ContextMode, ProviderSelection, SpawnOptions, SubAgentSpawner,
};
use llm_harness_types::*;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// 让 LLM 派发 sub-agent 处理子任务的工具。
///
/// 同一 turn 中出现多个 spawn_agent 工具调用时，
/// sub-agent 可并行执行（框架已支持并发工具执行）。
pub struct SpawnAgentTool {
    spawner: Arc<dyn SubAgentSpawner>,
    default_cwd: PathBuf,
}

impl SpawnAgentTool {
    pub fn new(spawner: Arc<dyn SubAgentSpawner>, cwd: PathBuf) -> Self {
        Self {
            spawner,
            default_cwd: cwd,
        }
    }
}

impl Tool for SpawnAgentTool {
    fn name(&self) -> &str {
        "spawn_agent"
    }

    fn description(&self) -> &str {
        "派发一个 sub-agent 来并行处理子任务。\
         Sub-agent 有独立的 session 和工具集（read、write、\
         edit、bash、grep）。完成后返回 sub-agent 的输出。\
         用于可并发执行的独立子任务。"
    }

    fn parameters_schema(&self) -> &Value {
        // 静态 schema —— 只计算一次。
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "给 sub-agent 的任务描述"
                    },
                    "provider": {
                        "type": "string",
                        "description": "Provider 别名（如 \"fast\"）；不填则继承父 agent"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "工作目录，不填则继承父 agent"
                    },
                    "context": {
                        "type": "string",
                        "description": "注入到 sub-agent prompt 前缀的上下文文本"
                    }
                },
                "required": ["task"]
            })
        })
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            // 1. 解析参数。
            let task = args["task"]
                .as_str()
                .ok_or_else(|| ToolError::InvalidArguments("task required".into()))?;

            let provider = match args.get("provider").and_then(|v| v.as_str()) {
                Some(name) if !name.is_empty() => ProviderSelection::Alias(name.to_string()),
                _ => ProviderSelection::Inherit,
            };

            let cwd = args
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(PathBuf::from);

            let context = args
                .get("context")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| ContextMode::Inject(s.to_string()))
                .unwrap_or(ContextMode::None);

            // 2. 构建 SpawnOptions 并派发。
            let opts = SpawnOptions {
                prompt: task.to_string(),
                provider,
                cwd: cwd.or_else(|| Some(self.default_cwd.clone())),
                context,
            };

            let handle = self
                .spawner
                .spawn(opts)
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;

            // 3. 等待完成或父 agent 取消。
            let sub_abort = handle.abort_token();
            let result = tokio::select! {
                r = handle.join() => r,
                _ = ctx.abort.cancelled() => {
                    sub_abort.cancel();
                    return Err(ToolError::Aborted);
                }
            };

            match result {
                Ok(sub_result) => {
                    let content = format!(
                        "Sub-agent completed ({} tool calls):\n\n{}",
                        sub_result.tool_calls_count,
                        sub_result.output,
                    );
                    Ok(ToolResult {
                        content: vec![ContentBlock::Text { text: content }],
                        details: serde_json::json!({
                            "session_id": sub_result.session_id,
                            "tool_calls_count": sub_result.tool_calls_count,
                        }),
                        terminate: false,
                    })
                }
                Err(e) => Err(ToolError::Execution(e.to_string())),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use llm_harness_agent::sub_agent::{SubAgentHandle, SubAgentResult};

    struct MockSpawner {
        result: SubAgentResult,
    }

    impl SubAgentSpawner for MockSpawner {
        fn spawn(
            &self,
            _opts: SpawnOptions,
        ) -> BoxFuture<'_, Result<SubAgentHandle, AgentError>> {
            let result = self.result.clone();
            Box::pin(async move {
                let join = tokio::spawn(async move { Ok(result) });
                Ok(SubAgentHandle {
                    join,
                    abort: CancellationToken::new(),
                })
            })
        }
    }

    /// 创建测试用 ToolContext 的辅助函数。
    fn make_tool_ctx() -> (ToolContext, tokio::sync::mpsc::Receiver<ToolResult>) {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let ctx = ToolContext {
            env: std::sync::Arc::new(llm_harness_loop::test_utils::NoOpEnv),
            abort: CancellationToken::new(),
            tool_use_id: "test-spawn-1".into(),
            turn_index: 0,
            assistant_message: std::sync::Arc::new(AssistantMessage {
                content: vec![],
                stop_reason: None,
                timestamp: chrono::Utc::now(),
                provider: None,
                api: None,
                model: None,
                usage: None,
                error_message: None,
            }),
            update_tx: tx,
        };
        (ctx, rx)
    }

    #[tokio::test]
    async fn spawn_agent_returns_formatted_result() {
        let spawner = Arc::new(MockSpawner {
            result: SubAgentResult {
                output: "任务完成：发现 3 个问题".into(),
                tool_calls_count: 5,
                session_id: "sess-123".into(),
            },
        });

        let tool = SpawnAgentTool::new(spawner, PathBuf::from("/tmp"));
        let (ctx, _rx) = make_tool_ctx();

        let result = tool
            .execute(
                serde_json::json!({"task": "分析安全性"}),
                &ctx,
            )
            .await
            .unwrap();

        // 检查 content。
        let text = match &result.content[0] {
            ContentBlock::Text { text } => text,
            _ => panic!("expected text block"),
        };
        assert!(text.contains("5 tool calls"));
        assert!(text.contains("发现 3 个问题"));

        // 检查 details。
        assert_eq!(
            result.details["session_id"].as_str().unwrap(),
            "sess-123"
        );
        assert_eq!(result.details["tool_calls_count"].as_i64().unwrap(), 5);
        assert!(!result.terminate);
    }

    #[tokio::test]
    async fn spawn_agent_aborts_on_parent_cancel() {
        let spawner = Arc::new(MockSpawner {
            result: SubAgentResult {
                output: "done".into(),
                tool_calls_count: 0,
                session_id: "sess-456".into(),
            },
        });

        let tool = SpawnAgentTool::new(spawner, PathBuf::from("/tmp"));
        let (mut ctx, _rx) = make_tool_ctx();
        ctx.abort.cancel(); // 父 agent 已取消

        let err = tool
            .execute(serde_json::json!({"task": "做点什么"}), &ctx)
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::Aborted));
    }
}
```

- [ ] **步骤 3：注册新工具模块**

在 `examples/coding-agent/src/tools/mod.rs` 第 4 行 (`pub mod grep;`) 之后添加：

```rust
pub mod spawn_agent;
```

- [ ] **步骤 4：验证编译**

运行：`cargo build -p coding-agent`
预期：编译无错误

- [ ] **步骤 5：运行新测试**

运行：`cargo test -p coding-agent -- spawn_agent`
预期：2 个测试通过

- [ ] **步骤 6：提交**

```bash
git add examples/coding-agent/src/tools/spawn_agent.rs examples/coding-agent/src/tools/mod.rs
git commit -m "feat(coding-agent): add SpawnAgentTool for LLM-triggered sub-agent dispatch"
```

---

### 任务 7：更新 `CodingAgentPlugin` 和 Plugin 测试

**文件：**
- 修改：`examples/coding-agent/src/plugin.rs`
- 检查：`examples/coding-agent/src/lib.rs`（如存在）

**接口：**
- 消费：`SubAgentSpawner`（来自 `llm-harness-agent::sub_agent`）
- 产出：`CodingAgentPlugin::new(cwd, spawner: Option<Arc<dyn SubAgentSpawner>>)`

- [ ] **步骤 1：更新 `CodingAgentPlugin::new()` 签名**

在 `examples/coding-agent/src/plugin.rs` 中修改构造函数：

```rust
use llm_harness_agent::sub_agent::SubAgentSpawner;
use crate::tools::spawn_agent::SpawnAgentTool;

// 添加 spawner 参数：
pub fn new(cwd: &std::path::Path, spawner: Option<Arc<dyn SubAgentSpawner>>) -> Self {
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(read::ReadTool::new()),
        Arc::new(bash::BashTool::new()),
        Arc::new(edit::EditTool::new()),
        Arc::new(write::WriteTool::new()),
        Arc::new(grep::GrepTool::new()),
    ];

    // 当 spawner 可用时，条件性地添加 SpawnAgentTool。
    // Sub-agent 收到 None，防止递归派发。
    if let Some(spawner) = spawner {
        tools.push(Arc::new(SpawnAgentTool::new(spawner, cwd.to_path_buf())));
    }

    Self {
        context_files: prompt::find_context_files(cwd),
        tools,
        cwd: cwd.to_path_buf(),
    }
}
```

- [ ] **步骤 2：更新 Plugin 测试以适配新签名**

在 `tests` 模块中，将所有调用从：
```rust
CodingAgentPlugin::new(std::path::Path::new("."))
```
改为：
```rust
CodingAgentPlugin::new(std::path::Path::new("."), None)
```

受影响的测试函数：
- `plugin_name_is_coding_agent`
- `plugin_registers_five_tools` —— spawner 为 `None` 时仍然是 5 个工具。另添加 spawner=`Some` 的测试。
- `plugin_registers_before_run_hook`
- `plugin_registers_before_tool_call_hook`
- `system_prompt_is_not_empty`

同时添加 spawner=Some 的新测试：

```rust
#[test]
fn plugin_registers_six_tools_when_spawner_present() {
    use llm_harness_agent::sub_agent::*;
    use futures::FutureExt;

    struct DummySpawner;
    impl SubAgentSpawner for DummySpawner {
        fn spawn(&self, _opts: SpawnOptions)
            -> BoxFuture<'_, Result<SubAgentHandle, AgentError>>
        {
            Box::pin(async { Err(AgentError::Internal("dummy".into())) })
        }
    }

    let spawner = Arc::new(DummySpawner);
    let plugin = CodingAgentPlugin::new(
        std::path::Path::new("."),
        Some(spawner),
    );
    let mut tools: Vec<Arc<dyn Tool>> = vec![];
    plugin.register_tools(&mut tools);
    assert_eq!(tools.len(), 6);
    assert!(tools.iter().any(|t| t.name() == "spawn_agent"));
}
```

- [ ] **步骤 3：检查 `examples/coding-agent/src/lib.rs`**

运行：`ls examples/coding-agent/src/lib.rs`
如存在，阅读并检查是否需要更新 re-export。

- [ ] **步骤 4：验证编译和测试**

运行：`cargo build -p coding-agent`
预期：编译无错误

运行：`cargo test -p coding-agent`
预期：所有测试通过

- [ ] **步骤 5：提交**

```bash
git add examples/coding-agent/src/plugin.rs
git commit -m "feat(coding-agent): add spawner parameter to CodingAgentPlugin, conditionally register SpawnAgentTool"
```

---

### 任务 8：更新 `main.rs` 组装 spawner 和 `/sub-agent` REPL 命令

**文件：**
- 修改：`examples/coding-agent/src/main.rs`
- 可能涉及：`examples/coding-agent/src/provider.rs`（简要检查）

**接口：**
- 消费：`HarnessSubAgentSpawner`、`EnvFactory`（来自 `llm-harness-runtime`）
- 消费：`OsEnvFactory`（来自 `llm-harness-runtime-sandbox-os`）
- 消费：`SubAgentSpawner`、`SpawnOptions`、`ProviderSelection`、`ContextMode`（来自 `llm-harness-agent::sub_agent`）
- 产出：修改后的 `run_repl()` 带 spawner 参数、`/sub-agent` 命令解析

- [ ] **步骤 1：更新 `run_repl()` 签名和函数体**

在 `examples/coding-agent/src/main.rs` 中：

函数签名从：
```rust
async fn run_repl(harness: &Arc<AgentHarness>) -> Result<(), i32> {
```
改为：
```rust
use llm_harness_agent::sub_agent::{
    ContextMode, ProviderSelection, SpawnOptions, SubAgentSpawner,
};

async fn run_repl(
    harness: &Arc<AgentHarness>,
    spawner: Option<Arc<dyn SubAgentSpawner>>,
) -> Result<(), i32> {
```

在 REPL 输入处理循环中，`run_one_shot()` 之前（`let _ = editor.add_history_entry(&line);` 之后）添加：

```rust
        // /sub-agent "<prompt>" [--provider <alias>] [--cwd <path>]
        if let Some(rest) = line.strip_prefix("/sub-agent") {
            let rest = rest.trim();
            if rest.is_empty() {
                println!("用法: /sub-agent \"<prompt>\" [--provider <alias>]");
                continue;
            }
            match parse_sub_agent(rest, spawner.as_deref()) {
                Ok((opts, s)) => {
                    println!("正在派发 sub-agent...");
                    let handle = match s.spawn(opts).await {
                        Ok(h) => h,
                        Err(e) => {
                            eprintln!("派发 sub-agent 失败: {e}");
                            continue;
                        }
                    };
                    match handle.join().await {
                        Ok(result) => {
                            println!("\nSub-agent 完成 ({} 次工具调用):\n",
                                result.tool_calls_count);
                            println!("{}", result.output);
                            println!();
                        }
                        Err(e) => eprintln!("Sub-agent 错误: {e}"),
                    }
                }
                Err(msg) => eprintln!("{msg}"),
            }
            continue;
        }
```

- [ ] **步骤 2：添加 `parse_sub_agent()` 辅助函数**

在 `main.rs` 末尾（`create_session` 之前）添加：

```rust
/// 解析 `/sub-agent "<prompt>" [--provider <alias>] [--cwd <path>]`。
fn parse_sub_agent(
    input: &str,
    spawner: Option<&dyn SubAgentSpawner>,
) -> Result<(SpawnOptions, &dyn SubAgentSpawner), String> {
    let spawner = spawner.ok_or("Sub-agent 派发未配置。")?;

    // 提取引号内的 prompt。
    let prompt_start = input.find('"').ok_or(
        "需要引号包裹的 prompt。用法: /sub-agent \"<prompt>\" [--provider <alias>]"
    )?;
    let rest_after_open = &input[prompt_start + 1..];
    let prompt_end = rest_after_open.find('"').ok_or("Prompt 的引号未闭合。")?;
    let prompt = rest_after_open[..prompt_end].to_string();

    // 解析可选标志。
    let flags = rest_after_open[prompt_end + 1..].trim();
    let mut provider = ProviderSelection::Inherit;
    let mut cwd = None;
    let mut parts = flags.split_whitespace().peekable();
    while let Some(flag) = parts.next() {
        match flag {
            "--provider" => {
                let alias = parts.next().ok_or("--provider 需要一个值")?;
                provider = ProviderSelection::Alias(alias.to_string());
            }
            "--cwd" => {
                let path = parts.next().ok_or("--cwd 需要一个值")?;
                cwd = Some(std::path::PathBuf::from(path));
            }
            _ => return Err(format!("未知标志: {flag}")),
        }
    }

    Ok((
        SpawnOptions {
            prompt,
            provider,
            cwd,
            context: ContextMode::None,
        },
        spawner,
    ))
}
```

- [ ] **步骤 3：更新 `run()` 函数以组装 spawner 并传入 run_repl**

在 `run()` 函数中，`let resolved = provider::resolve_provider().ok_or(1)?;`（第 90 行）之后、`let plugin = CodingAgentPlugin::new(&cwd);`（第 94 行）之前插入：

```rust
    // ── 构建 sub-agent spawner ────────────────────────────────────────────

    use llm_harness_runtime::HarnessSubAgentSpawner;
    use llm_harness_runtime_sandbox_os::OsEnvFactory;

    let spawner: Arc<dyn SubAgentSpawner> = Arc::new(
        HarnessSubAgentSpawner::new(
            resolved.model.clone(),
            resolved.client.clone(),
            cwd.join(".coding-agent/sessions"),
            // Sub-agent plugin factory：不含 SpawnAgentTool（防止递归派发）。
            |cwd| Box::new(CodingAgentPlugin::new(cwd, None)),
        )
        .env_factory(Arc::new(OsEnvFactory)),
    );
```

然后将：
```rust
let plugin = CodingAgentPlugin::new(&cwd);
```
改为：
```rust
let plugin = CodingAgentPlugin::new(&cwd, Some(Arc::clone(&spawner) as Arc<dyn SubAgentSpawner>));
```

并将：
```rust
run_repl(&harness).await
```
改为：
```rust
run_repl(&harness, Some(spawner)).await
```

- [ ] **步骤 4：在 main.rs 顶部添加所需 import**

添加：
```rust
use llm_harness_agent::sub_agent::{ContextMode, ProviderSelection, SpawnOptions, SubAgentSpawner};
use llm_harness_runtime::HarnessSubAgentSpawner;
use llm_harness_runtime_sandbox_os::OsEnvFactory;
```

- [ ] **步骤 5：验证编译**

运行：`cargo build -p coding-agent`
预期：编译无错误

- [ ] **步骤 6：提交**

```bash
git add examples/coding-agent/src/main.rs
git commit -m "feat(coding-agent): wire spawner assembly, /sub-agent REPL command"
```

---

### 任务 9：全工作区验证

- [ ] **步骤 1：运行所有测试**

运行：`cargo test --all-targets --all-features`
预期：所有测试通过

- [ ] **步骤 2：运行 clippy**

运行：`cargo clippy --all-targets --all-features -- -D warnings`
预期：无警告

- [ ] **步骤 3：运行格式检查**

运行：`cargo fmt --all -- --check`
预期：无格式问题

- [ ] **步骤 4：修复问题并提交**

修复所有 clippy 警告或测试失败后：
```bash
git add -A
git commit -m "chore: fix clippy warnings and test issues from sub-agent implementation"
```

---

## 验证

所有任务完成后：

1. **单元测试验证**：`cargo test --all-targets --all-features` —— 全部通过
2. **Lint 验证**：`cargo clippy --all-targets --all-features -- -D warnings` —— 无警告
3. **构建验证**：`cargo build --workspace` —— 所有 crate 编译通过
4. **手动冒烟测试**：运行 `coding-agent -i`，输入 `/sub-agent "echo hello"`，确认输出正确
