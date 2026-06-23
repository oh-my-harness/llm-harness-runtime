# Sub-Agent 系统设计

> 日期：2026-06-22
> 范围：`llm-harness-agent` / `llm-harness-runtime` / `llm-harness-runtime-sandbox-os` / `examples/coding-agent`
> 目标：让 coding-agent 能够派发 sub-agent 处理子任务，支持 LLM 主动触发和用户显式触发两种模式
> 约束：框架层定义机制（trait），App 层实现策略；sub-agent 不可递归派发（v0.3+ 再扩展）

---

## 一、架构总览

### 组件分布

```
llm-harness-types           (无需改动)

llm-harness-agent
  src/sub_agent.rs          (新增，统一命名为下划线风格)
    - SubAgentSpawner trait
    - SpawnOptions / ProviderSelection / ContextMode
    - SubAgentHandle / SubAgentResult

llm-harness-runtime
  src/spawner.rs            (新增)
    - HarnessSubAgentSpawner: SubAgentSpawner 的具体实现
    - ProviderAlias（别名 → client + model 的映射）
    - EnvFactory trait（抽象 OsEnv 创建，解耦 sandbox-os 依赖）

llm-harness-runtime-sandbox-os
  src/env_factory.rs        (新增)
    - OsEnvFactory: EnvFactory

examples/coding-agent
  src/tools/spawn_agent.rs  (新增)
    - SpawnAgentTool
  src/plugin.rs             (修改)
    - CodingAgentPlugin::new() 加 Option<Arc<dyn SubAgentSpawner>>
  src/main.rs               (修改)
    - 组装 HarnessSubAgentSpawner，注入 plugin
    - run_repl() 加 spawner 参数，解析 /sub-agent 语法
```

### 数据流

```
用户 / LLM
  │
  ├─ LLM 主动: spawn_agent 工具调用（JSON 参数）
  │
  └─ 用户显式: REPL 输入 /sub-agent "<prompt>" [--provider <alias>]
                     ↓
             直接构造 SpawnOptions，跳过主 LLM
  ↓
SpawnAgentTool::execute() / run_repl() 的 /sub-agent 分支
  │ spawner.spawn(opts)
  ↓
HarnessSubAgentSpawner
  │ 解析 alias → (client, model)
  │ 创建子 session（JsonlSessionRepo）
  │ HarnessBuilder 构建子 AgentHarness（sub-agent plugin，无 SpawnAgentTool）
  │ tokio::spawn 运行子 agent loop
  ↓
SubAgentHandle
  │ handle.join() 等待完成，同时收集 TextDelta 事件
  ↓
SubAgentResult → ToolResult / 终端渲染
```

### 并行执行

LLM 在一个 turn 里发出多个 `spawn_agent` tool_use block，框架已有并发执行多个工具调用的逻辑，sub-agent 并行是这个行为的自然延伸，无需额外 API。

---

## 二、`SubAgentSpawner` trait 及相关类型

位置：`crates/llm-harness-agent/src/sub_agent.rs`

```rust
/// 启动 sub-agent 时的完整配置。
pub struct SpawnOptions {
    /// 派发给 sub-agent 的初始 prompt。
    pub prompt: String,
    /// 使用哪个 provider；None 表示继承父 agent 的 client + model。
    pub provider: ProviderSelection,
    /// 工作目录；None 表示继承父 agent 的 cwd。
    pub cwd: Option<std::path::PathBuf>,
    /// 传给 sub-agent 的上下文模式。
    pub context: ContextMode,
}

/// 选择 sub-agent 使用的 provider。
pub enum ProviderSelection {
    /// 继承父 agent 的 client + model（默认）。
    Inherit,
    /// 通过注册的别名选取完整的 (client, model)。
    /// 别名在 HarnessSubAgentSpawner 构造时注册，例如 "fast"、"cheap"。
    Alias(String),
}

/// 向 sub-agent 传递父 agent 上下文的方式。
pub enum ContextMode {
    /// sub-agent 从零开始，无父 context。
    None,
    /// 调用方提供一段文本，拼入 sub-agent prompt 的前缀：
    ///   "Context:\n{text}\n\n---\n\n{prompt}"
    Inject(String),
}

/// 派发 sub-agent 的核心 trait。
pub trait SubAgentSpawner: Send + Sync {
    /// 启动一个 sub-agent，返回可等待的 handle。
    ///
    /// spawn() 只做初始化（建 session、建 harness、起 tokio task），
    /// 实际执行在 handle.join() 处等待。
    fn spawn(
        &self,
        opts: SpawnOptions,
    ) -> futures::future::BoxFuture<'_, Result<SubAgentHandle, AgentError>>;
}

/// sub-agent 运行中的句柄。
pub struct SubAgentHandle {
    pub(crate) join: tokio::task::JoinHandle<Result<SubAgentResult, AgentError>>,
    pub(crate) abort: tokio_util::sync::CancellationToken,
}

impl SubAgentHandle {
    /// 返回 abort token 的克隆，用于外部取消（不 move self）。
    pub fn abort_token(&self) -> tokio_util::sync::CancellationToken {
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
        // 确保 drop 时已取消，防止孤儿 task 继续运行
        self.abort.cancel();
    }
}
```

/// sub-agent 完成后的结果。
#[derive(Clone)]
pub struct SubAgentResult {
    /// 最后一个 turn 所有 TextDelta 拼成的完整文本输出。
    pub output: String,
    /// 本次执行共调用了几次工具。
    pub tool_calls_count: u32,
    /// sub-agent 的 session id（供调试和审计）。
    pub session_id: String,
}

---

## 三、`SpawnAgentTool`

位置：`examples/coding-agent/src/tools/spawn_agent.rs`

### LLM 看到的 JSON Schema

```rust
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct SpawnAgentParams {
    /// 派发给 sub-agent 的具体任务描述。
    task: String,
    /// 使用哪个 provider 别名（如 "fast"、"cheap"）；
    /// 不填则继承父 agent 的 provider。
    provider: Option<String>,
    /// 工作目录（绝对路径）；不填则继承父 agent 的 cwd。
    cwd: Option<String>,
    /// 注入 sub-agent prompt 前缀的上下文文本；
    /// 用于把当前任务背景传给 sub-agent。
    context: Option<String>,
}
```

### 工具实现要点

```rust
pub struct SpawnAgentTool {
    spawner: Arc<dyn SubAgentSpawner>,
    default_cwd: PathBuf,
}

// execute() 关键逻辑：
// 1. 反序列化 SpawnAgentParams
// 2. 构造 SpawnOptions
// 3. spawner.spawn(opts).await → SubAgentHandle
// 4. sub_abort = handle.abort_token()
// 5. tokio::select!:
//      handle.join() 完成 → 格式化 ToolResult
//      ctx.abort.cancelled() → sub_abort.cancel(); Err(AgentError::Aborted)
```

ToolResult 格式：
```
Sub-agent completed (3 tool calls):

<sub-agent 的文本输出>
```

**ToolResult 字段约定**：
- `content`：上述格式的完整输出字符串
- `details`：JSON 对象，包含 `{"session_id": "<sub-agent-session-id>", "tool_calls_count": 3}`
- `terminate`：false（sub-agent 的输出不终止主 agent）

### Plugin 注册

`CodingAgentPlugin::new()` 签名改为：

```rust
pub fn new(cwd: &Path, spawner: Option<Arc<dyn SubAgentSpawner>>) -> Self
```

当 `spawner` 为 `Some` 时，工具列表末尾追加 `SpawnAgentTool`；为 `None` 时工具列表保持原样（用于 sub-agent 自身，避免递归派发）。

---

## 四、`HarnessSubAgentSpawner`

位置：`crates/llm-harness-runtime/src/spawner.rs`

### 结构体

```rust
pub struct HarnessSubAgentSpawner {
    default_model: String,
    default_client: Arc<dyn LlmClient>,
    aliases: Vec<ProviderAlias>,
    sub_plugin_factory: Arc<dyn Fn(&Path) -> Box<dyn Plugin> + Send + Sync>,
    env_factory: Arc<dyn EnvFactory>,
    session_base_dir: PathBuf,
}

struct ProviderAlias {
    name: String,
    model: String,
    client: Arc<dyn LlmClient>,
}
```

**plugin factory 而非 `Arc<dyn Plugin>` 的原因：**
主 agent plugin 持有 `Arc<dyn SubAgentSpawner>`，spawner 若反持主 plugin 的 Arc 会形成引用环。
Factory 在 spawn 时按需创建 sub-agent plugin，后者不含 `SpawnAgentTool`，无反向引用。

### Builder API

```rust
impl HarnessSubAgentSpawner {
    pub fn new(
        default_model: impl Into<String>,
        default_client: Arc<dyn LlmClient>,
        session_base_dir: PathBuf,
        sub_plugin_factory: impl Fn(&Path) -> Box<dyn Plugin> + Send + Sync + 'static,
    ) -> Self;

    pub fn alias(
        mut self,
        name: &str,
        client: Arc<dyn LlmClient>,
        model: &str,
    ) -> Self;

    pub fn env_factory(mut self, factory: Arc<dyn EnvFactory>) -> Self;
}
```

### `spawn()` 流程

```
1. resolve(&opts.provider)
       → ProviderSelection::Inherit   → (default_client, default_model)
       → ProviderSelection::Alias(n) → 线性查找 aliases，找不到返回 AgentError

2. cwd = opts.cwd.unwrap_or(default_cwd)
   env = env_factory.create(&cwd)

3. sub_plugin = (sub_plugin_factory)(&cwd)

4. session_dir = session_base_dir.join("sub-agents")
   storage = JsonlSessionRepo::create(CreateSessionOptions {
       initial_model: Some(model.clone()), ..Default::default()
   }).await
       .map_err(|e| AgentError::Internal(e.to_string()))?
   session_id = storage.id().to_string()

5. harness = HarnessBuilder::new(&model)
       .install(sub_plugin.as_ref())
       .provider(&model, client)
       .build_with_session(env, Session::new(storage))
       .map_err(|e| AgentError::Internal(e.to_string()))?

6. prompt = match opts.context {
       ContextMode::None         => opts.prompt,
       ContextMode::Inject(ctx)  => format!("Context:\n{ctx}\n\n---\n\n{}", opts.prompt),
   }

7. abort = CancellationToken::new()
   abort_clone = abort.clone()
   join = tokio::spawn(async move {
       // 将 abort token 传给 harness，用于响应外部取消信号
       harness.set_abort_token(abort_clone);

       let mut rx = harness.subscribe();

       // 事件收集 task（TextDelta → output, ToolCallStart → count）
       let event_task = tokio::spawn(async move {
           let mut output = String::new();
           let mut tool_count = 0u32;
           loop {
               match rx.recv().await {
                   Ok(ev) => match ev.as_ref() {
                       TextDelta { text, .. }    => output.push_str(text),
                       ToolCallStart { .. }       => tool_count += 1,
                       PhaseChange { to: Idle, .. } => break,
                       _ => {}
                   },
                   Err(Closed) => break,
                   Err(Lagged(_)) => continue,
               }
           }
           (output, tool_count)
       });

       harness.prompt(&prompt).await?;

       let (output, tool_calls_count) = event_task.await
           .map_err(|_| AgentError::Internal(...))?;

       Ok(SubAgentResult { output, tool_calls_count, session_id })
   })

8. return Ok(SubAgentHandle { join, abort })
```

**abort 信号流**：外部调用 `handle.abort()` → `abort.cancel()` → harness 内 agent loop 的 `current_abort.cancelled()` 分支被触发 → harness 停止执行并返回错误 → spawned task 结束 → `handle.join()` 收到 `AgentError::Aborted`。

### `EnvFactory` 抽象

`HarnessSubAgentSpawner` 在 `llm-harness-runtime`，不直接依赖 `llm-harness-runtime-sandbox-os`。

```rust
// llm-harness-runtime/src/spawner.rs
pub trait EnvFactory: Send + Sync {
    fn create(&self, cwd: &Path) -> Arc<dyn ExecutionEnv>;
}

// llm-harness-runtime-sandbox-os/src/env_factory.rs
pub struct OsEnvFactory;
impl EnvFactory for OsEnvFactory {
    fn create(&self, cwd: &Path) -> Arc<dyn ExecutionEnv> {
        Arc::new(OsEnv::new(cwd))
    }
}
```

---

## 五、用户触发、`main.rs` 组装、测试策略

### REPL `/sub-agent` 命令

用户在 REPL 里用 `/sub-agent` 绕过主 LLM 直接派发任务：

```
> /sub-agent "分析 src/auth.rs 的安全问题" --provider fast
```

语法：
```
/sub-agent "<prompt>" [--provider <alias>] [--cwd <path>]
```

`run_repl()` 在 `harness.prompt()` 之前检查输入是否以 `/sub-agent` 开头。
若是，直接构造 `SpawnOptions` 并调用 `spawner.spawn()`，渲染输出后继续等待下一条输入。
若 spawner 为 `None`，提示"sub-agent 不可用"。

`run_repl()` 签名：
```rust
async fn run_repl(
    harness: &Arc<AgentHarness>,
    spawner: Option<Arc<dyn SubAgentSpawner>>,
) -> Result<(), i32>
```

### `main.rs` 组装顺序

```rust
// 1. 解析 provider（现有逻辑）
let resolved = provider::resolve_provider().ok_or(1)?;

// 2. 创建 spawner（provider 解析后，plugin 创建前）
let spawner: Arc<dyn SubAgentSpawner> = Arc::new(
    HarnessSubAgentSpawner::new(
        resolved.model.clone(),
        resolved.client.clone(),
        cwd.join(".coding-agent/sessions"),
        |cwd| Box::new(CodingAgentPlugin::new(cwd, None)),
    )
    .env_factory(Arc::new(OsEnvFactory)),
    // 可选别名（从环境变量读取 client 后注册）：
    // .alias("fast", siliconflow_client, "Qwen/Qwen2.5-Coder-14B-Instruct")
);

// 3. 创建主 plugin（注入 spawner）
let plugin = CodingAgentPlugin::new(&cwd, Some(Arc::clone(&spawner)));

// 4. session → HarnessBuilder → harness（现有逻辑，不变）

// 5. run_repl / run_one_shot（传入 spawner）
if interactive {
    run_repl(&harness, Some(spawner)).await
} else {
    run_one_shot(&harness, &prompt).await
}
```

### 测试策略

**`MockSubAgentSpawner`**（放在 `#[cfg(test)]` 或 `tests/`）：

```rust
struct MockSubAgentSpawner {
    result: SubAgentResult,
}

impl SubAgentSpawner for MockSubAgentSpawner {
    fn spawn(&self, _opts: SpawnOptions)
        -> BoxFuture<'_, Result<SubAgentHandle, AgentError>>
    {
        let result = self.result.clone();
        Box::pin(async move {
            let join = tokio::spawn(async move { Ok(result) });
            Ok(SubAgentHandle { join, abort: CancellationToken::new() })
        })
    }
}
```

**核心测试用例**：

| 层 | 用例 |
|----|------|
| `SpawnAgentTool` | mock spawner 返回结果，验证 ToolResult 格式正确 |
| `SpawnAgentTool` | parent abort → sub_abort.cancel() 被调用 |
| `HarnessSubAgentSpawner` | 已知 alias → 正确 (client, model) |
| `HarnessSubAgentSpawner` | 未知 alias → `AgentError` |
| `HarnessSubAgentSpawner` | `ProviderSelection::Inherit` → 使用 default |
| `HarnessSubAgentSpawner` | `ContextMode::Inject(text)` → prompt 前缀正确 |
| REPL 解析 | `/sub-agent "..." --provider fast` → SpawnOptions 字段正确 |

---

## 六、未纳入范围（v0.3+）

| 功能 | 原因 |
|------|------|
| sub-agent 递归派发（sub-agent 再派 sub-agent） | 需要解决循环 Arc 引用，用 Weak 方案，Phase 1 不需要 |
| sub-agent 自定义工具集 | SpawnOptions 加 `tools_override` 字段，现阶段继承 sub_plugin_factory 即可 |
| sub-agent 结果流式回传 | 目前等待完成后一次性返回，流式需要框架级别的跨 harness 事件桥接 |
| alias 从配置文件读取 | 目前在 main.rs 硬编码，配置文件方案放 Phase 2 |
| `ContextMode::Full`（传完整父 session 历史） | 需要 session 序列化和 token 预算管理 |

---

## 实现过程中的注意事项

1. **SubAgentResult 需加 `#[derive(Clone)]`**：Mock 和测试会调用 `.clone()`（见第 421 行伪代码）。

2. **run_repl() 签名变化**：examples/coding-agent/src/main.rs 中 `run_repl()` 签名从 `(harness: &Arc<AgentHarness>)` 改为加 `spawner: Option<Arc<dyn SubAgentSpawner>>` 参数。测试代码需同步（如 plugin.rs 中的测试调用）。

3. **abort token 机制依赖 AgentHarness 支持**：spawn() 流程 step 7 中需调用 `harness.set_abort_token(abort_clone)` 或类似机制，确保外部 abort 信号能到达 harness 内部的 agent loop。若 AgentHarness 尚不支持，需在 llm-harness-agent/src/harness.rs 添加对应方法。

4. **Session 存储不应写死后端**：`HarnessSubAgentSpawner` 通过 `SessionFactory` trait 抽象 session 创建，生产环境使用 `JsonlSessionFactory`，测试环境可使用 `InMemorySessionRepo`。默认使用 NoOpSessionFactory（未设置时 panic），避免对 `JsonlSessionRepo` 的隐式绑定。

## 七、文件变更索引

```
新增  crates/llm-harness-agent/src/sub_agent.rs
新增  crates/llm-harness-runtime/src/spawner.rs
新增  crates/llm-harness-runtime-sandbox-os/src/env_factory.rs
新增  examples/coding-agent/src/tools/spawn_agent.rs
修改  examples/coding-agent/src/plugin.rs
修改  examples/coding-agent/src/main.rs
删除  crates/llm-harness-runtime/src/subagent.rs
修改  crates/llm-harness-runtime/src/lib.rs (移除 pub use subagent::SubAgentSpawner)
```

**变更说明：**
- 现有 `crates/llm-harness-runtime/src/subagent.rs` 中的 `SubAgentSpawner` trait 是 v0.1 占位实现，签名为 `fn spawn(&self, opts: SubAgentSpawnOpts, env: Arc<dyn ExecutionEnv>) -> BoxFuture<'_, Result<SubAgentResult>>`。
- 新设计将 `SubAgentSpawner` trait 和所有相关类型（`SpawnOptions`、`SubAgentHandle`、`SubAgentResult`、`ProviderSelection`、`ContextMode`）移至 `crates/llm-harness-agent/src/sub_agent.rs`（命名统一为下划线风格），新签名为 `fn spawn(&self, opts: SpawnOptions) -> BoxFuture<'_, Result<SubAgentHandle, AgentError>>`。
- 删除原 `subagent.rs`，清理 `lib.rs` 中对应的 `pub use` re-export。
- 旧 trait 的所有依赖方都应改为导入 `llm-harness-agent::sub_agent::*`。
