# Agent Inspector Web API — 设计文档

> Issue: [#131](https://github.com/oh-my-harness/llm-harness-runtime/issues/131)
> Branch: `feat/inspector-web-api`
> Date: 2026-08-22 (updated 2026-08-24)

## 1. 背景

runtime 已有 EventStream（含 `MessageUpdate` 流式 token）、Session、TaskStore、AuditSink 等数据通道，但没有 Web API 层对外暴露。开发者缺少"看见 agent 在干什么"的工具。

Inspector 是一个独立 crate，通过 Web API + WebSocket 让开发者实时观察 agent 状态和行为，并支持 agent 退出后回放。

## 2. 定位

- **开发者调试工具**（非运维监控、非终端用户面板）
- **独立 crate**（`llm-harness-inspector`），不侵入 Plugin trait，运行时挂载
- **只提供 Web API**（REST + WebSocket），不绑定前端实现
- **独立于 senza-studio**，任何基于 runtime 的 agent 应用都能用

## 3. 架构

### 3.1 挂载方式

Builder 模式，可选数据源按需注入。`AgentHarness` 以 `Arc` 传入——axum server 是 `'static` 的，不能持有非 `'static` 引用：

```rust
use llm_harness_inspector::Inspector;

// P0 最小路径：只需 harness，立即获得实时事件流 + 配置 + 当前 session
let inspector = Inspector::builder(Arc::clone(&harness))
    .port(8080)
    .mount()
    .await?;

// 完整路径：传入可选数据源，解锁跨 session 列表等端点
let inspector = Inspector::builder(Arc::clone(&harness))
    .session_repo(repo)       // 启用 GET /api/sessions, GET /api/sessions/:id
    .task_store(ts)           // P1: workflow 状态
    .audit_sink(audit)        // P1: 审计日志
    .port(8080)
    .mount()
    .await?;
```

> **为什么是 `Arc<AgentHarness>` 而非 `&AgentHarness`**：`mount()` 启动的 axum server 生命周期超越 builder，server 内部的 route handler 需要长期持有 harness 引用来响应 REST 请求和订阅事件。`&'a AgentHarness` 无法满足 axum 的 `'static` 约束。`AgentHarness` 内部全部使用 `Arc`/`Mutex`，已经是 `Send + Sync`，包在 `Arc` 里零额外成本。调用者需 `Arc::new(harness)` 或 `harness.into()`。

**环境变量（零代码调试）：**

```bash
# 任何 live-test example 都可以挂载 Inspector UI
SENZA_INSPECT=8080 cargo run -p llm-harness-live-tests --example 02_tool_calling
# 🔧 Inspector running at http://127.0.0.1:8080
# 浏览器打开即可实时观察 agent 事件流
```

`from_env()` 读 `SENZA_INSPECT` 环境变量（值为端口号），有则返回预填 port 的 `InspectorBuilder`，无则返回 `None`。调用者可链式追加可选数据源再 `.mount()`：

```rust
if let Some(builder) = Inspector::from_env(Arc::clone(&harness)) {
    let inspector = builder
        .session_repo(repo)
        .mount()
        .await?;
}
```

详细集成方案见 §3.7。

### 3.2 数据源

不新建数据通道，复用 `AgentHarness` 已有的公开 API + 可选外部句柄：

| 数据源 | 获取方式 | 用途 | 必须 |
|---|---|---|---|
| 事件流 | `harness.subscribe()` → `broadcast::Receiver<Arc<AgentHarnessEvent>>` | 实时事件全量透传（含 `TextDelta`、`ThinkingDelta`、`MessageUpdate`、`ToolExecutionUpdate`、`RetryAttempt` 等） | ✅ |
| 配置快照 | `harness.state()` → `HarnessState` | model、thinking_level、tools、system_prompt、phase、streaming_message 等 | ✅ |
| Cost/budget | `harness.usage()` → `CostAggregate` | token 用量与成本 | ✅ |
| 当前 Session | `harness.read_active_path()` / `harness.read_all_entries()` / `harness.list_branches()` | 当前 session 的消息历史与分支树 | ✅ |
| Session 元数据 | `harness.session_metadata()` → `SessionMetadata`（**待实现 accessor**） | 当前 session ID、创建时间、active cursor | ✅ |
| SessionRepo | builder `.session_repo(repo)` | 跨 session 列表与任意 session 详情 | ❌ 可选 |
| TaskStore | builder `.task_store(ts)` | Workflow 状态（current_step、step_history、WorkflowContext） | ❌ 可选，P1 |
| AuditSink | builder `.audit_sink(sink)` | SHA-256 哈希链审计日志 | ❌ 可选，P1 |

> **待实现：`AgentHarness::session_metadata()`**：暴露 `self.session.metadata()` 的代理方法，返回 `Result<SessionMetadata, HarnessError>`。当前 `AgentHarness` 没有此方法——`harness.session` 字段是 `pub(super)`，crate 外不可访问。此 accessor 是 P0 第 2 项，在实现 Inspector 前需先对 `llm-harness-agent` crate 添加。

Inspector 不依赖 workflow / knowledge / memory / session-recall 等 crate。可选数据源通过 trait object 传入（`Arc<dyn SessionRepo>`、`Arc<dyn TaskStore>`、`Arc<dyn AuditSink>`），不直接依赖具体 crate。

### 3.3 运行模式

**实时模式**：agent 运行时，WebSocket 推送 `AgentHarnessEvent` 全量事件流。

> **连接初始化——快照 + 增量**：`tokio::broadcast` 的新订阅者收不到订阅前的事件。WS 连接建立后，Inspector 先下发一条初始快照消息（`harness.state()` + `harness.usage()` 的当前状态），然后切换到增量事件推送。前端用快照初始化 UI，后续按增量事件更新。格式见 §5.2。

包含 `TextDelta`/`ThinkingDelta`（逐 token 流式）、`MessageUpdate`（完整快照）、`ToolExecutionUpdate`（长时工具进度）、`RetryAttempt`（重试可见性）等。前端自行选择用 delta 拼接还是用 snapshot 替换。

**回放模式**：agent 退出后，从 SessionRepo + TaskStore 持久化数据重建完整历史。前端通过 REST API 分页查询。

### 3.4 技术选型

- HTTP server：**axum**（与 runtime Tokio 栈一致）
- 实时推送：**WebSocket**
- 依赖控制：cargo feature `inspect`，不启用时不引入 axum 依赖
- 前端：**内置 minimal static HTML demo**（`include_str!` 单文件，serve at `/`），连接 WebSocket 实时渲染事件流。核心交付物是 API，HTML demo 仅用于开箱即用的调试体验，生产使用者可自行实现前端
- API 文档：附带 OpenAPI/JSON schema 描述所有端点

### 3.5 事件序列化边界

`AgentHarnessEvent` 和 `AgentEvent` 当前只有 `#[derive(Debug)]`，没有 `Serialize`。直接给它们加 `#[derive(Serialize)]` **编译不过**——`AgentEvent::TurnEnd` 和 `ToolExecutionEnd` 携带 `ToolResult`，而 `ToolResult` 包含 `ToolResultExtensions`（`HashMap<TypeId, Arc<dyn Any + Send + Sync>>`），这是进程内 typed extension map，**根本不可序列化**。同理 `ToolFailure` 和 `ToolProgress` 也缺少 `Serialize`。

**解决方案**：Inspector crate 定义 `SerializableAgentHarnessEvent`——对每个 variant 做投影映射，跳过不可序列化的字段。这是**投影**（跳过进程内字段），不是**翻译**（不改变事件语义、不裁剪事件类型、不重命名字段）。

具体投影规则：

| 原始类型 | 投影处理 |
|---|---|
| `ToolResult` | 序列化 `model_content` + `details` + `terminate` + `session_projection`；**丢弃 `extensions`**（`TypeId`-keyed trait object map，进程内专用，无跨进程语义） |
| `ToolFailure` | Inspector 内定义 `SerializableToolFailure`（`code` + `model_message` + `retryable` + `details` + `session_projection`） |
| `ToolProgress` | Inspector 内定义 `SerializableToolProgress`（`content` + `details`） |
| 其他子类型（`AgentMessage`、`AssistantMessage`、`ContentBlock`、`TokenUsage`、`DataBlock` 等） | **已有 `Serialize`**，直接序列化 |

> **不修改 `llm-harness-types`**：不在底层 crate 加 `Serialize` derive，避免 `ToolResultExtensions` 问题。序列化边界画在 Inspector crate 内，保持底层类型不变。

### 3.6 并发模型与线程安全

- `harness.state()`：`inner.lock().unwrap().clone()`——获取 `Mutex` 锁后 clone 整个 `HarnessState`。axum 多任务并发调用安全：每次调用独立加锁、clone、释放。
- `harness.usage()`：`inner.lock().unwrap().cost.clone()` → `cost.lock().unwrap().clone()`——两级锁，锁顺序固定（先 `inner` 后 `cost`），无死锁风险。
- `harness.subscribe()`：`broadcast::Sender::subscribe()`——返回 `broadcast::Receiver<Arc<AgentHarnessEvent>>`。`broadcast::Receiver` 是 `Send`，可在 axum task 中持有。
- `harness.read_active_path()` 等 async 方法：内部通过 `Arc<dyn SessionStorage>` 调用，trait object 是 `Send + Sync`，多任务并发安全。

结论：所有 `AgentHarness` 公开 API 在 axum 多任务并发环境下安全使用，无需额外同步原语。

### 3.7 Live-tests 集成：全局可选 Web UI

`llm-harness-live-tests` 的 24 个 example 共享一套工厂函数（`make_harness` / `make_harness_with` / `make_harness_with_env` / `make_harness_with_session`）。在这些工厂函数内部检查 `SENZA_INSPECT` 环境变量——有值则自动挂载 Inspector，**所有 example 零改动即可获得 Web UI**。

**工厂函数改造：**

返回类型从 `(AgentHarness, TempDir)` 变为 `(Arc<AgentHarness>, TempDir)`。`Arc<AgentHarness>` 通过 `Deref` 透明支持 `harness.prompt()`、`EventCapture::subscribe(&harness)` 等所有现有调用——example 代码不需要改。

```rust
// crates/llm-harness-live-tests/src/lib.rs

async fn maybe_mount_inspector(
    harness: AgentHarness,
) -> (Arc<AgentHarness>, Option<Inspector>) {
    let harness = Arc::new(harness);
    let inspector = match std::env::var("SENZA_INSPECT") {
        Ok(port_str) => {
            let port: u16 = port_str.parse().unwrap_or(8080);
            let inspector = Inspector::builder(Arc::clone(&harness))
                .port(port)
                .mount()
                .await
                .expect("inspector mount failed");
            println!("🔧 Inspector running at http://127.0.0.1:{port}");
            Some(inspector)
        }
        Err(_) => None,
    };
    (harness, inspector)
}

pub async fn make_harness(entry: &LiveProviderEntry) -> (Arc<AgentHarness>, tempfile::TempDir) {
    let (harness, tmp) = make_harness_with(entry, |b| b).await;
    let (harness, _inspector) = maybe_mount_inspector(harness).await;
    (harness, tmp)
}
```

**Inspector 生命周期：**

`Inspector` 持有 `Arc<AgentHarness>` 的 clone，所以 harness 不会被 drop 直到 inspector 也 drop。`Inspector::shutdown` 是 async 的，但 `Drop` 不能 await——`Inspector` 的 `Drop` 实现里通过 `oneshot::Sender` 触发 server 停止：

```rust
impl Drop for Inspector {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}
```

工厂函数返回的 `_inspector` 在函数作用域结束时 drop，触发 server 关闭。example 中 `let (harness, _tmp) = make_harness(entry).await;` 的 `_tmp` 已经是同样的模式——inspector 随 `_tmp` 一起在 main 函数结束时清理。

> **注意**：当前工厂函数返回的是元组，inspector 被丢弃在 `maybe_mount_inspector` 内部。需要调整：工厂函数返回 `(Arc<AgentHarness>, InspectorGuard, TempDir)`，其中 `InspectorGuard` 是一个 `Option<Inspector>` 的新类型，随 harness 一起返回，确保生命周期绑定。或者更简单——把 inspector 存在一个 `OnceLock` 或随 harness 返回的结构里。实现时确定具体方案。

**不设 `SENZA_INSPECT` 时**：`maybe_mount_inspector` 返回 `(Arc::new(harness), None)`，行为与现在完全一致，零额外开销。

**`EventCapture` 与 Inspector 并行**：`EventCapture::subscribe(&harness)` 和 Inspector 的 WS 端点各自独立调用 `harness.subscribe()`，获得独立的 `broadcast::Receiver`。两者不干扰——终端 println 和浏览器 UI 同时工作。

**依赖关系**：`llm-harness-live-tests` 的 `Cargo.toml` 新增 `llm-harness-inspector` 为 optional dependency，仅在 `live-llm` feature 下启用：

```toml
[dependencies]
llm-harness-inspector = { workspace = true, optional = true }

[features]
live-llm = ["dep:llm-harness-inspector"]
```

## 4. Crate 结构

```
llm-harness-inspector/
  Cargo.toml          — feature: inspect → axum, tokio-ws
  src/
    lib.rs            — Inspector, InspectorBuilder, 公开 API
    server.rs         — axum router, HTTP server 生命周期
    routes/
      mod.rs          — 路由注册（按可选数据源条件注册）
      config.rs       — GET /api/agent/config, GET /api/agent/session
      sessions.rs     — GET /api/sessions, GET /api/sessions/:id（需 session_repo）
      events.rs       — WS /api/events
    state.rs          — InspectorState: 持有 Arc<AgentHarness> + 可选数据源句柄
    event_ser.rs      — SerializableAgentHarnessEvent 投影映射
  static/
    index.html        — minimal Web UI（include_str! 内联，单文件含内联 JS/CSS）
```

### 依赖关系

```
llm-harness-inspector
  → llm-harness-agent    (AgentHarness, AgentHarnessEvent, HarnessState, SessionRepo)
  → llm-harness-types    (AuditSink trait, Tool 等类型)
  → llm-harness-workflow (TaskStore trait — P1，仅在接入 task_store 时引入)
  → axum                 (feature = "inspect")
  → tokio                (已有 workspace 依赖)
```

> **TaskStore 依赖说明**：`TaskStore` trait 定义在 `llm-harness-workflow` crate。P1 接入 `task_store` 数据源时，Inspector 需新增对 `llm-harness-workflow` 的依赖。P0 不涉及此依赖。替代方案是在 P1 实现前将 `TaskStore` trait 上移到 `llm-harness-types`，届时再决定。

## 5. API 设计

### 5.1 REST 端点

#### `GET /api/agent/config`

返回 agent 当前配置快照。只需 `Arc<AgentHarness>`。

```json
{
  "session_id": "sess_abc123",
  "model": "deepseek-chat",
  "thinking_level": "medium",
  "temperature": 0.7,
  "tools": ["bash", "read_file", "write_file"],
  "active_tools": null,
  "final_answer_mode": "auto",
  "system_prompt": "You are a helpful assistant...",
  "phase": "Idle",
  "budget": {
    "usage_reports": { "provider_calls": 3, "reported_valid": 2, "missing": 1 },
    "total_input_tokens": 42000,
    "total_output_tokens": 3800,
    "total_cost": 0.15
  }
}
```

字段映射自 `HarnessState` + `CostAggregate`（`harness.usage()`）。`session_id` 来自 `harness.session_metadata()`。

#### `GET /api/agent/session`

返回当前 agent 绑定的 session 的消息历史与分支信息。只需 `Arc<AgentHarness>`，不需要 `SessionRepo`。

```json
{
  "metadata": {
    "id": "sess_abc123",
    "created_at": "2026-08-22T10:30:00Z",
    "active_cursor": "entry_005"
  },
  "branches": [
    { "leaf_id": "entry_005", "label": null, "message_count": 12 }
  ],
  "active_path": [
    {
      "id": "entry_001",
      "kind": "message",
      "message": {
        "role": "user",
        "content": "Run the calibration pipeline",
        "timestamp": "2026-08-22T10:30:00Z"
      }
    },
    {
      "id": "entry_004",
      "kind": "message",
      "message": {
        "role": "assistant",
        "content": "I'll start the calibration...",
        "tool_calls": [...],
        "timestamp": "2026-08-22T10:30:05Z"
      }
    }
  ]
}
```

数据来自 `harness.session_metadata()` + `harness.list_branches()` + `harness.read_active_path()`。`active_path` 中的 `SessionEntry` 结构直接序列化，不做翻译。

#### `GET /api/sessions`（需 `session_repo`）

返回所有 session 列表。仅在 builder 传入了 `session_repo` 时注册此路由。

```json
{
  "sessions": [
    {
      "id": "sess_abc123",
      "name": "calibration-run-1",
      "created_at": "2026-08-22T10:30:00Z",
      "durability": "durable",
      "active_cursor": "entry_005"
    }
  ]
}
```

数据来自 `repo.list(ListSessionOptions::default())`，返回 `Vec<SessionMetadata>` 直接序列化。

#### `GET /api/sessions/:id`（需 `session_repo`）

返回指定 session 的消息历史。仅在 builder 传入了 `session_repo` 时注册此路由。

```json
{
  "metadata": { "id": "sess_abc123", ... },
  "active_path": [ ... ]
}
```

数据来自 `repo.open(id)` → `Session::new(storage)` → `session.metadata()` + `session.read_active_path()`。未传入 `session_repo` 时此路由不存在，请求返回 404。

### 5.2 WebSocket 端点

#### `WS /api/events`

实时推送 agent 事件流。连接建立后，所有 `AgentHarnessEvent` 经投影后推送给前端——Inspector **不翻译事件语义、不裁剪事件类型、不筛选事件**。仅跳过不可序列化的进程内字段（见 §3.5）。

**连接初始化——快照消息：**

`tokio::broadcast` 的新订阅者收不到订阅前的事件。WS 连接建立后，Inspector 先下发一条初始快照：

```json
{
  "type": "Snapshot",
  "inner_type": null,
  "timestamp": "2026-08-22T10:30:05.123Z",
  "data": {
    "state": <HarnessState 序列化>,
    "usage": <CostAggregate 序列化>
  }
}
```

前端用此快照初始化 UI 状态。之后切换到增量事件推送。

**增量事件消息格式：**

```json
{
  "type": "<AgentHarnessEvent variant 名>",
  "inner_type": "<内层 AgentEvent variant 名，或 null>",
  "timestamp": "2026-08-22T10:30:05.123Z",
  "data": <SerializableAgentHarnessEvent 投影后的内容>
}
```

- `type`：`AgentHarnessEvent` enum variant 名（如 `"Agent"`、`"PhaseChange"`、`"CompactionStart"`）
- `inner_type`：当 `type = "Agent"` 时，内层 `AgentEvent` 的 variant 名（如 `"TextDelta"`、`"MessageUpdate"`、`"Error"`）。其他 variant 为 `null`。前端可直接按 `inner_type`（有值时）或 `type`（`inner_type` 为 null 时）路由渲染逻辑。
- `timestamp`：Inspector 推送时的时间戳
- `data`：`AgentHarnessEvent` 经 `SerializableAgentHarnessEvent` 投影后的完整内容（见 §3.5），不裁剪事件字段

**事件分类指引**（前端渲染参考，非完整列表——权威定义见 `llm-harness-agent` crate 的 `event.rs`）：

| 分类 | type | inner_type | 说明 |
|---|---|---|---|
| 快照 | `Snapshot` | `null` | 连接初始化时下发，含 state + usage |
| Agent 生命周期 | `Agent` | `AgentStart` / `AgentEnd` | 一次完整运行的开始/结束 |
| Turn 生命周期 | `Agent` | `TurnStart` / `TurnEnd` | 单轮对话边界，`TurnEnd` 携带完整 assistant message + tool results |
| 消息级 | `Agent` | `MessageStart` / `MessageUpdate` / `MessageEnd` | assistant message 边界。`MessageUpdate` 是完整快照（覆盖前一次） |
| Token 级 | `Agent` | `TextDelta` / `ThinkingDelta` | 增量文本片段，前端可用 delta 拼接或忽略改用 `MessageUpdate` 快照 |
| 工具调用（LLM 发起） | `Agent` | `ToolCallStart` / `ToolCallArgsDelta` / `ToolCallEnd` | LLM 输出 tool_use block 的流式过程 |
| 工具执行（harness 执行） | `Agent` | `ToolExecutionStart` / `ToolExecutionUpdate` / `ToolExecutionEnd` | Rust 层面执行 tool 的生命周期 |
| 工具调用（harness 级） | `ToolCallStart` / `ToolCallEnd` | `null` | harness 级工具调用记录，含 `args` 和 `result` |
| 错误与重试 | `Agent` | `Error` / `RetryAttempt` | `RetryAttempt` 在退避睡眠前推送，含 attempt 编号和延迟 |
| 阶段变化 | `PhaseChange` | `null` | harness phase 变化（Idle → Running 等） |
| 压缩 | `CompactionStart` / `CompactionEnd` | `null` | 上下文压缩事件 |
| 分支 | `BranchForked` / `BranchSwitched` / `BranchDeleted` / `BranchSummarized` | `null` | session 分支操作 |
| 配置变化 | `ModelUpdate` / `ThinkingLevelUpdate` / `ToolsUpdate` / `ActiveToolsUpdate` / `ResourcesUpdate` | `null` | 运行时配置变更 |
| 其他 | `Settled` / `Aborted` / `SavePoint` / `QueueUpdate` / `SessionInfoUpdate` | `null` | harness 级杂项事件 |

> **`ToolCallStart` 命名冲突说明**：`AgentEvent::ToolCallStart`（LLM 流式阶段，只有 `tool_use_id` + `name`）和 `AgentHarnessEvent::ToolCallStart`（harness 执行阶段，含 `args` + `tool_name`）同名。通过 `type` + `inner_type` 区分：前者 `type = "Agent"`, `inner_type = "ToolCallStart"`；后者 `type = "ToolCallStart"`, `inner_type = null`。前端按 `(type, inner_type)` 组合路由渲染。

### 5.3 内置 Web UI

`GET /` — 内置 minimal HTML demo，`include_str!("static/index.html")` 单文件，含内联 JS/CSS，无构建工具依赖。

**UI 功能：**

1. **配置面板**：页面加载时 `GET /api/agent/config`，展示 model、tools、phase、budget
2. **Session 历史**：页面加载时 `GET /api/agent/session`，展示消息列表和分支树
3. **实时事件流**：连接 `WS /api/events`，按事件类型渲染：
   - 第一条 `Snapshot` 消息 → 初始化配置面板和状态指示器
   - `Agent(TextDelta)` → 追加文本到当前 assistant 消息气泡
   - `Agent(MessageStart/MessageEnd)` → 创建/关闭消息气泡
   - `Agent(ToolCallStart)` / `Agent(ToolExecutionStart)` → 工具调用卡片
   - `PhaseChange` → 顶部状态指示器变色
   - `Agent(Error)` / `Agent(RetryAttempt)` → 红色警告条
   - `CompactionStart/End` → 压缩进度条

前端通过 `(type, inner_type)` 组合路由渲染逻辑。生产使用者可忽略此 HTML，自行实现前端消费同一套 API。

## 6. Inspector API

```rust
pub struct Inspector {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

pub struct InspectorBuilder {
    harness: Arc<AgentHarness>,
    port: u16,
    session_repo: Option<Arc<dyn SessionRepo>>,
    task_store: Option<Arc<dyn TaskStore>>,
    audit_sink: Option<Arc<dyn AuditSink>>,
}

impl Inspector {
    /// 创建 builder，必须传入 Arc<AgentHarness>
    pub fn builder(harness: Arc<AgentHarness>) -> InspectorBuilder;

    /// 从环境变量检查并返回预填 port 的 builder
    pub fn from_env(harness: Arc<AgentHarness>) -> Option<InspectorBuilder>;

    /// 关闭 inspector（显式关闭，等待 server 完全停止）
    pub async fn shutdown(self);
}

impl Drop for Inspector {
    /// Drop 时通过 oneshot 触发 server 停止，不 await（async Drop 不可用）
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

impl InspectorBuilder {
    pub fn port(mut self, port: u16) -> Self;
    pub fn session_repo(mut self, repo: Arc<dyn SessionRepo>) -> Self;
    pub fn task_store(mut self, ts: Arc<dyn TaskStore>) -> Self;
    pub fn audit_sink(mut self, sink: Arc<dyn AuditSink>) -> Self;

    /// 启动 HTTP server，消费 builder
    pub async fn mount(self) -> Result<Inspector, InspectorError>;
}
```

`InspectorBuilder` 不再有生命周期参数——`Arc<AgentHarness>` 是 `'static` 的，可以直接被 axum server 持有。`from_env` 读 `SENZA_INSPECT` 环境变量（值为端口号），有则返回预填 port 的 `InspectorBuilder`，无则返回 `None`。调用者可在 `.mount()` 前链式追加可选数据源。

## 7. 依赖控制

`Cargo.toml`:

```toml
[features]
inspect = ["dep:axum", "dep:tower", "dep:tower-http"]
default = []

[dependencies]
axum = { workspace = true, optional = true }
tower = { workspace = true, optional = true }
tower-http = { workspace = true, optional = true }
tokio = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
```

不启用 `inspect` feature 时，crate 编译为空壳（或不存在），零额外依赖。

## 8. MVP 范围

### P0（必须实现）

1. `Inspector::builder()` + `Inspector::from_env()` + `InspectorBuilder::mount()`
2. `AgentHarness::session_metadata()` accessor（对 llm-harness-agent crate 的改动，**待实现**）
3. `SerializableAgentHarnessEvent` 投影映射（含 `ToolResult` / `ToolFailure` / `ToolProgress` 的可序列化包装）
4. `GET /api/agent/config` — 配置快照（含 session_id、budget）
5. `GET /api/agent/session` — 当前 session 消息历史与分支
6. `WS /api/events` — 实时事件流（连接初始化快照 + 增量事件透传）
7. 内置 Web UI（`static/index.html`，`GET /` 提供）
8. Live-tests 集成：`make_harness*` 工厂函数检查 `SENZA_INSPECT` 自动挂载 Inspector，所有 24 个 example 零改动获得 Web UI

### P1（延后）

- `GET /api/sessions` + `GET /api/sessions/:id`（需 `session_repo`，实现已设计但延后到 P0 稳定后）
- `task_store` / `audit_sink` 数据源接入
- 回放模式（agent 退出后从持久化数据重建历史）
- Workflow DAG 可视化（节点状态、当前执行位置）
- 内嵌 Trace 查看（不依赖 Jaeger）

### 不做

- Plugin trait 扩展
- 前端绑定
- 运维监控面板
- 配置热修改 / 手动触发 / 断点调试

## 9. 测试
- 单元测试：InspectorBuilder 生命周期（builder 构造、mount / shutdown）
- 单元测试：`SerializableAgentHarnessEvent` 投影映射——验证所有 variant 可序列化，`ToolResult.extensions` 被正确跳过
- 集成测试：启动 inspector + 真实 `AgentHarness`（in-memory + MockLlmClient），验证 REST 端点返回正确数据
- WebSocket 测试：连接 `/api/events`，验证初始快照消息格式、增量事件格式（`type` + `inner_type` + `timestamp` + `data`）和事件透传完整性
- 条件路由测试：无 `session_repo` 时 `/api/sessions` 返回 404，有时返回列表
- Feature gate 测试：不启用 `inspect` feature 时编译正常，无额外依赖
- Live Web UI 验证：`SENZA_INSPECT=8080 cargo run -p llm-harness-live-tests --example 01_prompt_streaming`，浏览器手动验证事件流渲染（非自动化，需 API key + 浏览器）
