# llm-harness-runtime v0.2 设计



## 目录

1. [设计原则](#1-设计原则)
2. [整体架构与 crate 拓扑](#2-整体架构与-crate-拓扑)
3. [Platform 层（traits + 基础设施）](#3-platform-层traits--基础设施)
   - 3.1 E — 沙箱抽象 trait
   - 3.2 T — Tool registry + ToolSource
   - 3.3 T — MCP 适配器
   - 3.4 T — ResourceProvider trait
   - 3.5 T — Prompt template 集成
   - 3.6 L — Task runner
   - 3.7 L — Sub-agent spawning 扩展点
   - 3.8 O — TraceExporter trait + OTel 默认实现
   - 3.9 O — 成本聚合
   - 3.10 G — AuthHook 实现
   - 3.11 G — 人类介入 hook（H4）
   - 3.12 G — 审计日志
4. [默认实现层](#4-默认实现层)
5. [Core 修复分工](#5-core-修复分工)
6. [v0.2 不做清单](#6-v02-不做清单)
7. [实现顺序建议](#7-实现顺序建议)
8. [跨层关注点详述](#8-跨层关注点详述)
   - 8.1 Hook 耦合与组合语义
   - 8.2 能力-控制权衡
   - 8.3 成本-质量-速度三难

---

## 1. 设计原则

### 1.1 两层架构

Runtime 分为两层：

| 层 | 职责 | 示例 |
|---|------|------|
| **Platform** | trait 定义 + 编排核心 + 基础设施接口 | `Sandbox` trait、`ToolRegistry`、TaskRunner、`TraceExporter` trait |
| **默认实现** | 具体后端，独立 crate，按需引入 | `sandbox-os`、`mcp`、`trace-otel`、`audit-jsonl`、`auth` |

Platform 层无运行时依赖（纯 traits + 少量核心逻辑），所有实现层业务按需组装。

### 1.2 模块化

- 每个功能维度一个独立 crate
- 业务层 `Cargo.toml` 精确声明所需组件，不用的不引入
- 不强调"开箱即用"，强调"精确组装"

### 1.3 不改 Core，但充分使用 Core 的扩展点

Runtime 不修改 `llm-harness-core` 的接口定义。Core 已有的 `AgentHarness`、`Agent`、`Tool`、`ExecutionEnv` 等原样保留，runtime 在上层组装。

**"不改 Core" ≠ "不用 Core 的扩展点"**。Core 已暴露 12 个 hook trait，它们正是为上层注入行为而设计的。Runtime 应优先通过实现这些 hook 来组合行为，而不是引入平行概念：

| Runtime 概念 | 与 Core 的正确对接方式 |
|-------------|----------------------|
| HumanApprovalWrapper | 实现 `BeforeToolCallHook`，不创建新 hook 点 |
| TracingHookAdapter | 通过 `AfterProviderResponseHook` + `AfterToolCallHook` + `AfterTurnHook` 采集 span 数据 |
| BudgetControlAdapter | 通过 `AfterProviderResponseHook` 检查成本、`PrepareNextTurnHook` 阻止继续 |
| PromptSourceTemplate | 编译为 core `PromptTemplate`，统一模板引擎 |

### 1.4 不强加策略

Trait + hook 模式优先。Runtime 提供机制，业务层决定具体策略。唯一的例外是安全默认值（预算超限默认开环；人类介入**有 hook 时超时默认 Deny**，无 hook 时默认放行）。

### 1.5 跨层关注点

以下三个交叉关注点贯穿整个设计，详见 §8。

1. **Hook 耦合**：各 hook adapter 单独工作时正常，组合时需约定注册顺序与短路语义——详见 §8.1。
2. **能力-控制权衡**：工具集越大任务覆盖越广，但工具选择错误面也越大——详见 §8.2。
3. **成本-质量-速度三难**：沙箱、tracing、审计普遍增加延迟和成本，通过部署配置权衡——详见 §8.3。

---

## 2. 整体架构与 crate 拓扑

### 2.1 角色定位

```
┌─────────────────────────────────────────────────────┐
│                  业务 Agent（coding agent / EDA agent） │
│  工具定义 | TaskVerifier | PricingProvider 等        │
├─────────────────────────────────────────────────────┤
│              llm-harness-runtime                     │ ⊂ platform 层
│  Sandbox trait | ToolRegistry | TaskRunner          │
│  TraceExporter | AuditSink | AuthHook               │
│  McpClientHandle | ResourceProvider | PromptSource  │
│  SubAgentSpawner | etc.                              │
├─────────────────────────────────────────────────────┤
│              llm-harness-core                        │
│  AgentHarness | Session | Compaction | execution    │
│  loop (来自 crates: llm-harness, llm-harness-loop, │
│  llm-harness-types)                                 │
├─────────────────────────────────────────────────────┤
│              llm_adapter (外部 git dep)              │
│  Provider | ChatRequest/Response | StreamEvent      │
│  OpenAI | Anthropic | DeepSeek                       │
└─────────────────────────────────────────────────────┘
```

### 2.2 依赖方向

```
llm-harness-runtime
  └→ llm-harness（core）
       └→ llm-harness-loop → llm_adapter
       └→ llm-harness-types

llm-harness-runtime-mcp
  └→ llm-harness-runtime（McpClientHandle trait → ToolSource 实现）
  └→ llm-harness-types（ContentBlock, ToolResult 等）

llm-harness-runtime-sandbox-*
  └→ llm-harness-runtime（Sandbox trait + ExecutionEnv impl）

llm-harness-runtime-trace-otel
  └→ llm-harness-runtime（TraceExporter trait → OTel impl）
  └→ opentelemetry crate

llm-harness-runtime-audit-jsonl
  └→ llm-harness-runtime（AuditSink trait）
  └→ tokio

llm-harness-runtime-auth
  └→ llm-harness-runtime（AuthHook trait）
```

### 2.3 Crate 清单

| Crate | 类型 | 依赖 llm-harness-runtime? | 生产方式 |
|-------|------|--------------------------|---------|
| `llm-harness-runtime` | 主 crate (platform) | — | workspace 成员 |
| `llm-harness-runtime-mcp` | MCP client 适配 | 是 | workspace 成员 |
| `llm-harness-runtime-sandbox-os` | OsEnvSandbox | 是 | workspace 成员 |
| `llm-harness-runtime-sandbox-bwrap` | BwrapSandbox (Linux) | 是 | workspace 成员 |
| `llm-harness-runtime-sandbox-seatbelt` | SeatbeltSandbox (macOS) | 是 | workspace 成员 |
| `llm-harness-runtime-trace-otel` | OTel TraceExporter | 是 | workspace 成员 |
| `llm-harness-runtime-audit-jsonl` | JSONL AuditSink | 是 | workspace 成员 |
| `llm-harness-runtime-auth` | EnvAuthHook + FileAuthHook | 是 | workspace 成员 |

业务层 Cargo.toml 示例：

```toml
[dependencies]
llm-harness-runtime = "0.2"
llm-harness-runtime-mcp = "0.2"
llm-harness-runtime-sandbox-os = "0.2"          # ~ 开发环境
llm-harness-runtime-sandbox-bwrap = "0.2"        # = 生产部署（Linux）
llm-harness-runtime-trace-otel = "0.2"
llm-harness-runtime-audit-jsonl = "0.2"
llm-harness-runtime-auth = "0.2"
```

---

## 3. Platform 层（traits + 基础设施）

### 3.1 E — 沙箱抽象 trait

#### 3.1.1 设计

```rust
/// 沙箱权限边界声明（v0.2 最小集）
struct SandboxConfig {
    /// 文件系统路径白名单（读写）
    pub fs_allowlist: Vec<PathBuf>,
    /// 文件系统路径黑名单（以上白名单的例外）
    pub fs_denylist: Vec<PathBuf>,
    /// 网络访问规则
    pub net_allowlist: Vec<NetRule>,
    /// 资源限制
    pub resource_limits: ResourceLimits,
    /// 沙箱工作目录。
    /// start() 成功后，env().working_dir() 必须返回此路径（若设置），
    /// 否则返回沙箱后端的默认工作目录。
    pub work_dir: Option<PathBuf>,
}

struct NetRule {
    pub host: String,
    pub port_range: Option<(u16, u16)>,
}

struct ResourceLimits {
    pub max_cpus: Option<usize>,
    pub max_memory_mb: Option<usize>,
    pub max_disk_mb: Option<usize>,
    pub timeout: Option<Duration>,
}

/// 沙箱抽象 — 包裹 ExecutionEnv，增加生命周期管理。
///
/// ## work_dir 语义约定
/// `start()` 成功后，`env().working_dir()` 的返回值由以下规则决定：
/// - 若 `SandboxConfig.work_dir` 已设置 → 必须返回该路径（后端应将沙箱 cwd 设置为此路径）
/// - 若未设置 → 返回沙箱后端的默认工作目录（如 `/tmp/sandbox-<id>` 或 `/home/agent`）
/// 后端不得在 `start()` 之后静默修改 `working_dir()`。
trait Sandbox: Send + Sync {
    /// 启动或验证沙箱已经就绪
    async fn start(&self) -> Result<()>;

    /// 返回沙箱内的执行环境
    fn env(&self) -> Arc<dyn ExecutionEnv>;

    /// 重置到初始状态
    async fn reset(&self) -> Result<()>;

    /// 关闭沙箱，释放资源
    async fn shutdown(self: Box<Self>) -> Result<()>;

    /// 返回当前沙箱的配置
    fn config(&self) -> &SandboxConfig;

    /// 沙箱是否正在运行
    fn is_running(&self) -> bool;
}

/// snapshot/restore —— v0.3
/// trait SandboxSnapshot { snapshot() -> SnapshotId; restore(id) -> Result<()> }
```

#### 3.1.2 决策记录

| 决策 | 选项 | 依据 |
|------|------|------|
| trait 层次 | Sandbox 包 ExecutionEnv（B） | Core 的 ExecutionEnv 只管"沙箱内做什么"，Sandbox 管"沙箱在哪、谁管它" |
| 权限声明 | SandboxConfig 静态传入（A） | 构造时锁定边界；后端可以忽略不支持的字段，返回 warning |
| work_dir 语义 | 文档约定 + trait 注释（v0.2 新增） | 消除 SandboxConfig.work_dir 与 ExecutionEnv::working_dir() 的歧义 |
| snapshot | v0.3（B） | 涉及存储抽象，过早设计可能错；reset 覆盖 v0.2 需求 |

---

### 3.2 T — Tool registry + ToolSource

#### 3.2.1 设计

```rust
/// Tool 元数据
struct ToolMetadata {
    pub name: String,
    pub label: String,
    pub description: String,
}

/// 工具注册表
trait ToolRegistry: Send + Sync {
    /// 注册工具
    fn register(&self, tool: Arc<dyn Tool>);
    /// 注销工具
    fn unregister(&self, name: &str);
    /// 按名查找
    fn get(&self, name: &str) -> Option<Arc<dyn Tool>>;
    /// 列出所有已注册
    fn list(&self) -> Vec<ToolMetadata>;
    /// 按名列表组装 LoopConfig.tools
    fn subset(&self, names: &[&str]) -> Vec<Arc<dyn Tool>>;
}

/// Tool 来源（MCP、本地文件、HTTP API 等）
trait ToolSource: Send + Sync {
    /// 发现并返回可用的工具列表
    async fn discover(&self) -> Result<Vec<ToolDefinition>>;
}

/// ToolSource 产出的原始定义，由 registry 适配成 Tool 后注册
struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,      // JSON Schema
    pub execution_mode: ToolExecutionMode,
    pub custom_converter: Option<Box<dyn Fn(Value) -> Result<Value> + Send + Sync>>,
}
```

#### 3.2.2 两层过滤：ToolRegistry vs active_tools

`ToolRegistry` 和 core 的 `active_tools` 分别工作在不同平面，职责不重叠：

| 平面 | 组件 | 职责 | 运行时机 |
|------|------|------|---------|
| **管理平面** | `ToolRegistry` | 注册/发现/注销工具；维护全集 | AgentHarness 启动前 |
| **运行时平面** | `AgentHarness::set_active_tools()` | 每 turn 过滤激活子集；传给 `LoopConfig.tools` | 每 turn 开始时，由 `DefaultPrepareNextTurn` 调用 |

调用顺序：

```
ToolSource::discover()                           // 从 MCP/本地发现原始定义
  → ToolRegistry::register()                     // 注册到 registry 全集
  → ToolRegistry::subset(&["read", "write"])     // 产出 Vec<Arc<dyn Tool>>
  → AgentHarness::set_tools(tools)              // 设置本次 session 的工具集
  → AgentHarness::set_active_tools(names)       // per-turn 激活子集（可选）
```

典型场景：`ToolRegistry.subset()` 按角色/任务类型做粗粒度过滤（"coding tools" vs "browsing tools"），`active_tools` 在 session 内做细粒度动态调整（如某 turn 禁用网络工具）。

#### 3.2.3 使用模式

```rust
// 业务层
let registry = InMemoryToolRegistry::new();
registry.register(my_local_tool);

// 从 MCP source 拉取并注册
let mcp_source = McpSource::new(client_handle);
for def in mcp_source.discover().await? {
    let tool = ToolSourceAdapter::from_definition(def);
    registry.register(tool);
}

// 组装给 session（管理平面过滤）
let tools = registry.subset(&["read_file", "write_file", "search_code"]);
harness.set_tools(tools);
// per-turn 激活子集（运行时平面过滤，可选）
harness.set_active_tools(&["read_file", "write_file"]);
```

#### 3.2.4 决策记录

| 决策 | 选项 | 依据 |
|------|------|------|
| 与 LoopConfig 关系 | 组装器模式，不改 core 接口（A） | Core 已有 `active_tools` subset 过滤 |
| 单例 | per-instance（A） | 业务层决定共享粒度 |
| ToolSource 抽象 | 引入 trait（B） | 与 ResourceProvider/PromptSource 对称，MCP 第一个 source 值得抽象 |
| 两层分工文档 | v0.2 新增说明（A） | 消除使用混乱 |

---

### 3.3 T — MCP 适配器

#### 3.3.1 设计

```rust
/// 业务层实现的 MCP client 句柄
/// runtime 不绑定任何 MCP SDK
trait McpClientHandle: Send + Sync {
    /// 拉取工具列表（启动时调用一次）
    async fn list_tools(&self) -> Result<Vec<McpToolDef>>;
    /// 调用工具
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<McpResult, McpError>;
}

struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

struct McpResult {
    pub content: Vec<McpContent>,
    pub is_error: bool,
}

/// protocol error（基础设施问题）vs tool error（工具语义失败）
enum McpError {
    Protocol { source: Box<dyn Error + Send> },   // 连接断、超时等 → 上传到 runtime
    ToolExecution { content: Vec<McpContent> },    // 工具执行失败 → 给 agent 重试
}
```

#### 3.3.2 MCP 服务集成

```
业务层控制：
  1. 启动 MCP server 子进程（或连远程 host）
  2. 创建 McpClientHandle 实例
  3. 传入 runtime

Runtime 控制（启动时一次）：
  1. tools/list → ToolSource::discover（McpToolSource 是 ToolSource 的一个实现）
  2. 适配成 Tool 注册到 registry

未覆盖（v0.3+）：
  - MCP Resources → 走 ResourceProvider trait
  - MCP Prompts → 走 PromptSource trait（编译为 core PromptTemplate）
  - 运行时 tools/list 重新发现（v0.2 不做）
  - 工具签名验证（MCP ETDI，v0.3+）
```

#### 3.3.3 决策记录

| 决策 | 选项 | 依据 |
|------|------|------|
| MCP client 接口 | runtime 定义 trait，不绑 SDK（A） | Platform 层不替业务做 SDK 选型 |
| 错误映射 | protocol error 上传，tool error 给 agent（B） | Protocol error 是基础设施问题，agent 不应处理 |
| Server 生命周期 | 业务层管理（B） | Runtime 不启动/监控子进程 |
| Resource 集成 | 走 ResourceProvider trait（B） | 不直接操作 MCP Resource 协议 |
| 发现时机 | 启动时一次（A） | 简单够用，动态发现 v0.3+ |

---

### 3.4 T — ResourceProvider trait

#### 3.4.1 设计

```rust
struct ResourceDescriptor {
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: String,
}

enum ResourceContent {
    Text(String),
    Blob(Vec<u8>),
}

/// 上下文资源提供者
trait ResourceProvider: Send + Sync {
    /// 列出可用资源
    async fn list(&self) -> Result<Vec<ResourceDescriptor>>;
    /// 读取指定资源
    async fn read(&self, uri: &str) -> Result<ResourceContent>;
}
```

#### 3.4.2 注入时机与位置

```rust
/// 资源注入器 — runtime 提供的 helper
struct ResourceInjector {
    // 依赖 core 的 TransformContextHook 实现
}

impl ResourceInjector {
    /// 注入全部提供者的全部资源到 system prompt 尾部（带 cache_control）
    pub async fn inject_all(
        &self,
        providers: &[Arc<dyn ResourceProvider>],
        context: &mut AgentContext,
    ) -> Result<()>;

    /// 选择性注入指定 URI
    pub async fn inject(
        &self,
        provider: &dyn ResourceProvider,
        uri: &str,
        context: &mut AgentContext,
    ) -> Result<()>;
}
```

注入行为：
- **位置**：system prompt 尾部，cache_control 标记该 block
- **机制+默认实现**：runtime 提供 `ResourceInjector`（基于 `TransformContextHook`），同时提供一个 `AutoInjectAll` 默认策略
- **业务层覆盖**：通过 hook 自定义注入选择和时机

#### 3.4.3 决策记录

| 决策 | 选项 | 依据 |
|------|------|------|
| trait 形态 | list + read（A） | 对齐 MCP Resource 协议语义 |
| 注入策略 | 机制 + 默认实现（C） | Platform 层不强加策略，但提供默认便利 |
| 注入位置 | system prompt 尾部 + cache_control（C） | KV-cache 友好；需要 adapter 支持 cache_control |

---

### 3.5 T — Prompt template 集成

#### 3.5.1 设计

v0.2 **不引入** `NamedPromptTemplate` 和 `NamedTemplateRegistry`，统一使用 core 的 `PromptTemplate`。

```rust
/// 提示模板来源（MCP prompts、文件系统等）
trait PromptSource: Send + Sync {
    /// 发现模板
    async fn discover(&self) -> Result<Vec<PromptSourceTemplate>>;
}

/// PromptSource 产出的原始模板
struct PromptSourceTemplate {
    pub name: String,
    pub description: String,
    /// 模板正文，使用 {{param_name}} 命名占位符
    pub template: String,
    /// 参数定义，顺序决定编译后的位置参数 $1, $2, ...
    pub arguments: Vec<ArgumentDef>,
}

struct ArgumentDef {
    pub name: String,
    pub description: String,
    pub required: bool,
}
```

#### 3.5.2 编译为 core PromptTemplate

`PromptSourceTemplate` 在注册时由 `PromptTemplateCompiler` 编译为 core 的 `PromptTemplate`（位置参数语法），统一使用 core 的 `invoke_template()` 执行：

```rust
/// 将 PromptSourceTemplate 编译为 core PromptTemplate
pub struct PromptTemplateCompiler;

impl PromptTemplateCompiler {
    /// 按 arguments 声明顺序将 {{param_name}} 映射为 $1, $2, ...
    ///
    /// 示例：
    ///   arguments = [ArgumentDef { name: "lang" }, ArgumentDef { name: "code" }]
    ///   "Review {{code}} in {{lang}}" → "Review $2 in $1"
    pub fn compile(source: &PromptSourceTemplate) -> PromptTemplate {
        let mut template_str = source.template.clone();
        for (idx, arg) in source.arguments.iter().enumerate() {
            let placeholder = format!("{{{{{}}}}}", arg.name);
            let position = format!("${}", idx + 1);
            template_str = template_str.replace(&placeholder, &position);
        }
        PromptTemplate::new(template_str)
    }
}
```

注册流程：

```rust
let source = McpPromptSource::new(client_handle);
for raw in source.discover().await? {
    let core_template = PromptTemplateCompiler::compile(&raw);
    template_registry.register(raw.name.clone(), core_template);
}

// 调用统一使用 core 的 invoke_template
let rendered = invoke_template(&template_registry, "review_code", &["Rust", "fn main() {}"])?;
```

#### 3.5.3 与 core PromptTemplate 的关系

| 模板系统 | 语法 | 用途 | v0.2 后状态 |
|---------|------|------|------------|
| Core `PromptTemplate` | `$1`, `$@` 位置参数 | skills、内置命令 | **保留，不变** |
| Runtime `PromptSourceTemplate` | `{{param}}` 命名占位符 | 注册/发现阶段中间表示 | 编译后消费 |
| ~~NamedPromptTemplate~~ | ~~`{{param}}`~~ | ~~MCP prompts~~ | **不引入**（v0.1 中不存在） |
| ~~NamedTemplateRegistry~~ | — | ~~模板仓库~~ | **不引入**（v0.1 中不存在） |

#### 3.5.4 决策记录

| 决策 | 选项 | 依据 |
|------|------|------|
| PromptSource trait | 引入（A） | 与 ToolSource 对称；MCP 第一个 source |
| 命名参数处理 | 编译为 core PromptTemplate（A） | 消除两套模板引擎；MCP prompts 只是来源，不应引入新引擎 |
| 编译时机 | 注册时一次性编译（eager） | 失败早发现；运行时 invoke 路径干净 |

---

### 3.6 L — Task runner

#### 3.6.1 设计

```rust
/// 任务状态机
enum TaskState {
    Pending,
    Running {
        session_id: EntryId,        // 当前 AgentHarness session
        started_at: DateTime<Utc>,
    },
    Paused { reason: String },
    AwaitingHumanInput { request: ApprovalRequest },
    Failed {
        error: TaskError,
        retries: usize,
        ended_at: DateTime<Utc>,
    },
    Succeeded {
        result: TaskResult,
        ended_at: DateTime<Utc>,
    },
    Cancelled { reason: String },
}

/// 任务完成判定（业务层实现）
trait TaskVerifier: Send + Sync {
    /// 判定任务是否成功
    async fn verify(&self, trace: &TaskTrace) -> TaskVerdict;
}

enum TaskVerdict {
    Pass,
    Fail { reason: String },
    Indeterminate { reason: String },  // 需要人工判断
}

/// 任务日志 trace（传给 verifier 的摘要数据）
struct TaskTrace {
    pub task_id: TaskId,
    pub session_ids: Vec<EntryId>,
    pub checkpoints: Vec<Checkpoint>,
    pub final_state: Value,
}

/// Task-level checkpoint（独立于 session，额外快照）
struct Checkpoint {
    pub seq: usize,
    pub saved_at: DateTime<Utc>,
    pub description: String,
    pub payload: Value,        // 业务层自定义快照内容
}
```

#### 3.6.2 与 AgentHarness 的关系

```
TaskRunner（生命周期：整个任务）
  ├─ checkpoints[] （task-level 额外快照）
  ├─ state (pending → running → succeeded/failed)
  ├─ retry_policy
  └─ current_harness: Option<AgentHarness>

一个 task 跨越多次 harness session：
  Task: Pending
    → Running: start harness session 1
      → agent runs... → adapter crash → harness fails
    → Failed: retries=0
    → Running: start harness session 2（checkpoint resume）
      → agent continues from last checkpoint
    → Succeeded
```

#### 3.6.3 决策记录

| 决策 | 选项 | 依据 |
|------|------|------|
| 层次 | TaskRunner 上层封装 AgentHarness（A） | AgentHarness 管"对话"，TaskRunner 管"任务" |
| 状态机 | 富状态机（B） | 覆盖 AwaitingHumanInput / Paused / Cancelled |
| 完成判定 | 业务层 TaskVerifier hook（A） | V 层留给业务，runtime 不内置 verifier |
| Checkpoint | 额外 task-level checkpoint（B） | 比仅复用 session 更精确的 recovery |

---

### 3.7 L — Sub-agent spawning 扩展点

#### 3.7.1 设计

```rust
/// sub-agent 的 spawn 配置
struct SubAgentSpawnOpts {
    pub prompt: String,
    pub model: Option<String>,
    /// registry 里的工具子集（仅名称）；子 agent 独立工具上下文
    pub tools: Option<Vec<String>>,
    pub max_turns: Option<usize>,
    pub timeout: Option<Duration>,
    /// 子 agent 的资源上限（独立于父 agent）
    pub resource_limits: Option<ResourceLimits>,
}

/// sub-agent 执行结果
struct SubAgentResult {
    /// 1-2K token 文字摘要，是子 agent 向父 agent 返回的唯一信息通道
    pub summary: String,
    pub token_usage: Option<TokenUsage>,
    /// 是否因 max_turns/timeout/resource_limits 被截断
    pub truncated: bool,
}

/// sub-agent spawner — 在 ToolContext 上暴露
trait SubAgentSpawner: Send + Sync {
    async fn spawn(
        &self,
        opts: SubAgentSpawnOpts,
        env: Arc<dyn ExecutionEnv>,
    ) -> Result<SubAgentResult>;
}
```

```rust
// v0.3+ API 预览 — 此方法不在 v0.2 中实现
// v0.2 中 ToolContext 不添加 sub_agent 字段（§3.7.3 决策：v0.2 明确不实现）
// 以下代码展示 v0.3 的预期 API 设计，仅供参考：
impl ToolContext {
    pub fn sub_agent_spawner(&self) -> Option<&dyn SubAgentSpawner> {
        self.sub_agent.as_deref()
    }
}
```

#### 3.7.2 上下文隔离

子 agent 遵循以下隔离原则（与论文 §10 中"新鲜上下文窗口"对齐）：

- **上下文独立**：子 agent 拥有全新的空上下文窗口，不继承父 agent 的对话历史
- **工具独立**：`SubAgentSpawnOpts.tools` 是工具名列表，由 spawner 实现从 registry 重新组装；子 agent 不自动继承父 agent 的工具集
- **资源独立**：`resource_limits` 独立配置；父 agent 的配额不自动共享给子 agent
- **信息单向**：子 agent 只能通过 `SubAgentResult.summary`（文字摘要）向父 agent 返回信息；直接对象引用不穿越边界

- v0.2 默认 `None`（`ToolContext.sub_agent = None`），调用即返回 `Err(Unsupported)`
- 业务层未来 inject 实现

#### 3.7.3 决策记录

| 决策 | 选项 | 依据 |
|------|------|------|
| 扩展点位置 | ToolContext（A） | 与论文"工具触发 sub-agent"实践一致 |
| 返回语义 | text 摘要（A） | 对齐"1-2K 摘要回到 orchestrator"最佳实践 |
| resource_limits | 独立字段（v0.2 新增） | 子 agent 独立资源配额；避免父子共享导致不可预期的资源抢占 |
| v0.2 默认 | None（A） | 明确不实现；纯净扩展点 |

---

### 3.8 O — TraceExporter trait + OTel 默认实现

#### 3.8.1 设计

```rust
/// Span 定义
struct SpanEvent {
    pub span_id: SpanId,
    pub trace_id: TraceId,
    pub parent_span_id: Option<SpanId>,
    pub name: String,                   // e.g. "llm_call", "tool_exec", "turn"
    pub kind: SpanKind,                 // Internal, Client, Server
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub attributes: HashMap<String, AttributeValue>,
    pub status: SpanStatus,
}

/// 追踪导出器 trait
trait TraceExporter: Send + Sync {
    async fn export(&self, events: &[SpanEvent]) -> Result<()>;
    async fn flush(&self) -> Result<()>;
}
```

#### 3.8.2 Span 层次结构

严格遵循 OTel GenAI 语义约定：

```
Task span                    attributes: {task_id, task_state}
  └─ Session span           attributes: {session_id, model, system_prompt_hash}
       └─ Turn span         attributes: {turn_index, stop_reason}
            ├─ LLM span     attributes: {gen_ai.system, gen_ai.model, gen_ai.usage.*}
            │               （遵循 GenAI 语义约定）
            └─ Tool span    attributes: {tool_name, tool_use_id, tool_is_error}
```

Agent 专属扩展属性：

| 属性 | 类型 | span |
|------|------|------|
| `agent.session_id` | string | Session / Turn / LLM / Tool |
| `agent.task_id` | string | Task / Session / Turn |
| `agent.turn_index` | int | Turn / LLM / Tool |
| `agent.tool_name` | string | Tool |
| `agent.stop_reason` | string | Turn |

#### 3.8.3 默认 OTel 实现

`llm-harness-runtime-trace-otel` crate 提供两个实现：

```rust
/// 测试用内存 exporter — 收集 SpanEvent 供单元/集成测试断言，不连接外部 collector
pub struct InMemoryTraceExporter {
    spans: tokio::sync::Mutex<Vec<SpanEvent>>,
}

/// 生产用 OTLP exporter — 将 SpanEvent 推送到 OTel collector
pub struct OtlpExporter {
    provider: opentelemetry::sdk::TracerProvider,
}
```

`InMemoryTraceExporter` 用于集成测试；`OtlpExporter` 用于生产部署（通过 `OTEL_EXPORTER_OTLP_ENDPOINT` 配置）。

#### 3.8.4 Capture 策略

三档配置，逐 session 设定：

```rust
enum CaptureLevel {
    /// 仅元数据：token 数、延迟、stop_reason、错误
    MetadataOnly,
    /// 元数据 + LLM prompt/response 原文
    Full,
}

struct SpanCaptureConfig {
    pub level: CaptureLevel,
    /// 脱敏 hook（应用在 Full level 下）
    pub redactor: Option<Arc<dyn SpanRedactor>>,
}

trait SpanRedactor: Send + Sync {
    fn redact(&self, span: &mut SpanEvent);
}
```

默认：`MetadataOnly`。

#### 3.8.5 决策记录

| 决策 | 选项 | 依据 |
|------|------|------|
| 耦合度 | trait + 默认 OTel 实现（B） | Platform 层纯 trait，可换后端 |
| Span 层次 | 严格遵循 GenAI 语义约定（A） | Langfuse/Phoenix 可消费 |
| 全文捕获 | 三档 + redaction hook（C） | 隐私优先，灵活调优 |

#### 3.8.6 Span 数据采集架构（v0.2 新增）

Span 不是凭空产生的，数据来自 core 已有的 hook 调用链。Runtime 提供 `TracingHookAdapter`，同时实现多个 core hook，通过 `HarnessHooks` 注入到 `AgentHarness`：

```rust
/// 同时实现多个 core hook，将事件边界转换为 span
pub struct TracingHookAdapter {
    exporter: Arc<dyn TraceExporter>,
    capture_config: SpanCaptureConfig,
    // 活跃 span 上下文，by session_id / turn_index
    active_spans: Mutex<SpanContextMap>,
}

impl BeforeProviderRequestHook for TracingHookAdapter {
    // → 记录 LLM 请求发出时刻（start_time），存入 active_spans.llm_spans
    // 与 AfterProviderResponseHook 配对构成完整 LLM span
}

impl AfterProviderResponseHook for TracingHookAdapter {
    // → 完成 LLM span：从 active_spans 取出 BeforeProviderRequestHook 记录的 start_time，
    //   计算真实 latency，记录 gen_ai.usage.*
}

impl AfterToolCallHook for TracingHookAdapter {
    // → Tool span（tool_name, tool_use_id, is_error）
}

impl AfterTurnHook for TracingHookAdapter {
    // → 完成 Turn span：从 begin_turn() 存入 active_spans 的 turn_spans 取出 start_time
}
```

**LLM span start_time 采集**：`BeforeProviderRequestHook` 在请求发出前记录 `start_time`，`AfterProviderResponseHook` 在响应到达时取出并构成完整 LLM span。若只实现 `AfterProviderResponseHook`，`start_time` 为响应到达时间（错误）。两个 hook **必须成对注册**。

**Task/Session span 生命周期**：Task 和 Session span 不来自 core hook，而是由 `TaskRunner` 显式调用 `TracingHookAdapter` 的配套方法驱动：

```rust
impl TracingHookAdapter {
    // 任务启动时调用 — 在 active_spans 中创建 Task span（不立即导出）
    pub fn begin_task(&self, task_id: &str) { ... }

    // 任务结束时调用 — 从 active_spans 取出 Task span，设置 end_time，推入 completed 并导出
    pub fn end_task(&self, task_id: &str, status: SpanStatus) { ... }

    // Session 启动/结束同理
    pub fn begin_session(&self, session_id: &str, model: &str) { ... }
    pub fn end_session(&self, session_id: &str, status: SpanStatus) { ... }

    // Turn 启动 — 存入 active_spans.turn_spans；结束由 AfterTurnHook 处理
    pub fn begin_turn(&self, turn_index: u32) { ... }
}
```

**关键约束**：`begin_task()` 只在 `active_spans` 中创建 span state，**不推入 `completed`**。`end_task()` 才设置 `end_time` 并导出。否则 Task span 的 duration 为零（begin/end 时间戳相同）。同理适用于 Session span。

各 span 类型与数据来源的对应关系：

| Span 类型 | 数据来源 | 采集内容 |
|-----------|---------|---------|
| LLM span | `BeforeProviderRequestHook`（开始）+ `AfterProviderResponseHook`（结束） | usage、latency_ms（真实值）、stop_reason |
| Tool span | `AfterToolCallHook` | tool_name、tool_use_id、is_error |
| Turn span | `begin_turn()`（开始）+ `AfterTurnHook`（结束） | turn_index、stop_reason、token 小计 |
| Session span | `begin_session()`（开始）+ `end_session()`（结束） | session_id、model |
| Task span | `begin_task()`（开始）+ `end_task()`（结束） | task_id、state |

注入方式（`HarnessHooks` 每类只有一个槽，多 adapter 须用 CompositeHook 包装，详见 §8.1）：

```rust
let adapter = Arc::new(TracingHookAdapter::new(exporter, capture_config));
let hooks = HarnessHooks {
    before_provider_request: Some(adapter.clone() as Arc<dyn BeforeProviderRequestHook>),
    after_provider_response: Some(Arc::new(CompositeAfterProviderResponseHook::new(vec![
        adapter.clone() as Arc<dyn AfterProviderResponseHook>,
        budget_adapter.clone() as Arc<dyn AfterProviderResponseHook>,
    ]))),
    after_tool_call: Some(adapter.clone() as Arc<dyn AfterToolCallHook>),
    after_turn:      Some(adapter.clone() as Arc<dyn AfterTurnHook>),
    should_stop:     Some(budget_adapter.clone() as Arc<dyn ShouldStopHook>),
    ..HarnessHooks::none()
};
harness.set_hooks(hooks);
```

---

### 3.9 O — 成本聚合

#### 3.9.1 设计

```rust
struct TokenPrice {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cache_read_per_mtok: f64,        // 可选的
    pub cache_write_per_mtok: f64,       // 可选的
}

/// 价格表提供者（业务层注入）
trait PricingProvider: Send + Sync {
    fn price_for(&self, model: &str, provider: &str) -> Option<TokenPrice>;
}
```

#### 3.9.2 聚合结果

```rust
struct CostAggregate {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_write_tokens: u64,
    pub total_cost: f64,
    pub by_model: HashMap<String, ModelCost>,
}

struct ModelCost {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost: f64,
    pub call_count: u64,
}
```

- `AgentHarness` 内置维护 `current_cost()`（per-session 聚合）
- `TaskRunner` 内置维护 `total_cost()`（跨 session 聚合 + 汇总到 task-level）
- `PricingProvider::price_for` 返回 `None` 时不计成本（默认行为）

#### 3.9.3 预算控制（v0.2 修订）

预算控制通过两层 core hook 实现，业务层只需实现 `BudgetExceededHook`：

```rust
trait BudgetExceededHook: Send + Sync {
    /// 预算超限时调用；返回 true 表示继续，false 表示 task 进入 Failed
    async fn on_budget_exceeded(
        &self,
        session_cost: &CostAggregate,
        task_cost: &CostAggregate,
        budget_limit: f64,
    ) -> bool;
}
```

Runtime 内部提供 `BudgetControlAdapter`，将业务层 hook 接入 core 的扩展点：

```rust
/// 将 BudgetExceededHook 适配为 core hook 对：
/// AfterProviderResponseHook（累积成本 + 超限检测）
/// ShouldStopHook（超限时阻断下一 turn）
struct BudgetControlAdapter {
    pricing: Arc<dyn PricingProvider>,
    budget_limit: f64,
    exceeded_hook: Option<Arc<dyn BudgetExceededHook>>,
    // 内部累积成本
    session_cost: Mutex<CostAggregate>,
    // AfterProviderResponseHook 写，ShouldStopHook 读
    budget_exceeded: Mutex<bool>,
}

impl AfterProviderResponseHook for BudgetControlAdapter {
    // 累积 token 用量；若超限则调用 exceeded_hook，
    // 根据返回值设置 budget_exceeded = true（应停止）
}

impl ShouldStopHook for BudgetControlAdapter {
    // 返回 *self.budget_exceeded.lock() — core 在每 turn 开始前调用，
    // 返回 true 时终止循环
}
```

注入方式：

```rust
let budget_adapter = Arc::new(BudgetControlAdapter::new(pricing, max_cost, Some(hook)));
let hooks = HarnessHooks {
    after_provider_response: Some(budget_adapter.clone() as Arc<dyn AfterProviderResponseHook>),
    should_stop:             Some(budget_adapter.clone() as Arc<dyn ShouldStopHook>),
    ..HarnessHooks::none()
};
harness.set_hooks(hooks);
```

- Budget 在 TaskRunner 构造时配置（`max_total_cost: f64`）
- 无 hook → 不做预算控制（开环）

**为什么是 ShouldStopHook 而非 PrepareNextTurnHook**：`PrepareNextTurnHook` 返回 `NextTurnDirective`，其所有字段均为 `Option`，全为 `None` 时语义为"继续正常执行"，无法表达"停止"。`ShouldStopHook` 返回 `bool`，是 core 为此场景专门设计的扩展点。

#### 3.9.4 决策记录

| 决策 | 选项 | 依据 |
|------|------|------|
| 价格表 | 业务层注入 PricingProvider（B） | 价格表不是 runtime 职责 |
| 聚合粒度 | runtime 内置 per-session + per-task（B） | 程序内实时可用，与 TaskRunner 紧耦合 |
| 预算超限 | BudgetControlAdapter 接 core AfterProviderResponseHook + ShouldStopHook（v0.2 修订） | PrepareNextTurnHook 无法表达"停止"语义；ShouldStopHook 是 core 为此设计的专用扩展点 |

---

### 3.10 G — AuthHook 实现

Core 已有 `AuthHook` trait，runtime 要接通并提供默认实现。

#### 3.10.1 AuthHook 与 core 对齐

Runtime **不重新定义** `AuthHook` trait，直接 re-export core 的定义，避免签名漂移：

```rust
// 在 crates/llm-harness-runtime/src/auth.rs
pub use llm_harness_types::{AuthHook, AuthInfo};

// Core 定义（位于 llm_harness_types::hooks）：
//
//   pub trait AuthHook: Send + Sync {
//       fn resolve<'a>(&'a self) -> BoxFuture<'a, Result<AuthInfo, AgentError>>;
//   }
//
//   pub struct AuthInfo {
//       pub api_key: Option<String>,
//       pub headers: Vec<(String, String)>,
//   }
```

**行为：**
- 每次 LLM 调用都调 `AuthHook::resolve()`（不缓存）
- Auth 失败 → `TaskError::AuthError` → Task state 进入 `AwaitingHumanInput`
- 业务层在 hook 内部可选择缓存（通过 `FileAuthHook::with_cache_ttl` 等）

#### 3.10.2 默认实现

`llm-harness-runtime-auth` crate 提供：

```rust
/// 从环境变量读：ANTHROPIC_API_KEY, OPENAI_API_KEY 等
struct EnvAuthHook;

/// 从 YAML/TOML 配置文件读
struct FileAuthHook { path: PathBuf }
```

#### 3.10.3 决策记录

| 决策 | 选项 | 依据 |
|------|------|------|
| 默认实现 | EnvAuthHook + FileAuthHook（B） | 覆盖 95% 场景 |
| 缓存 | 每次都调（A） | Hook 内部可自缓存，runtime 无 policy |
| 失败处理 | 进 AwaitingHumanInput（C） | 与富状态机一致；auth 是 recoverable failure |

---

### 3.11 G — 人类介入 hook（H4）（v0.2 重构）

v0.2 **删除** `HumanInterventionHook` trait（与 core `BeforeToolCallHook` 重复），改为 `HumanApprover` + `HumanApprovalWrapper` 模式。

#### 3.11.1 设计

```rust
/// 轻量人类审批接口（业务层实现）
/// 只关注"问人"这一步，不涉及 core hook 协议
trait HumanApprover: Send + Sync {
    /// 询问人类是否授权；返回 None 表示超时或无人响应
    async fn ask(&self, request: &ApprovalRequest) -> Option<ApprovalDecision>;
}

/// 需要人类确认的请求
struct ApprovalRequest {
    pub request_id: String,
    pub tool_name: String,
    pub arguments: Value,
    /// 提供足够上下文让人做判断（当前任务描述、上一步结果等）
    pub context: String,
}

/// 人类的决策（极简；allow-once/allow-always 等 UX 由业务层在 HumanApprover 内维护）
enum ApprovalDecision {
    Approve,
    Deny { reason: String },
}

/// 超时策略
enum TimeoutPolicy {
    Approve,
    Deny { reason: String },
}

/// 将 HumanApprover 适配为 core BeforeToolCallHook
pub struct HumanApprovalWrapper {
    approver: Arc<dyn HumanApprover>,
    /// ask() 返回 None 时的默认处理。
    /// 默认 Deny（fail-secure）：注册了 HumanApprover 说明明确需要人工监督，
    /// 超时 = 监督者未响应，应阻断而非放行。
    /// 业务层可覆盖为 Approve（如低风险工具的宽松策略）。
    on_timeout: TimeoutPolicy,
}

impl BeforeToolCallHook for HumanApprovalWrapper {
    async fn before_tool_call(
        &self,
        ctx: &ToolCallContext,
    ) -> BeforeToolCallDecision {
        let request = ApprovalRequest {
            request_id: Uuid::new_v4().to_string(),
            tool_name: ctx.tool_name().to_owned(),
            arguments: ctx.arguments().clone(),
            context: ctx.task_description().unwrap_or_default().to_owned(),
        };
        match self.approver.ask(&request).await {
            Some(ApprovalDecision::Approve) => BeforeToolCallDecision::Allow,
            Some(ApprovalDecision::Deny { reason }) => {
                BeforeToolCallDecision::Deny(ToolResult::error(reason))
            }
            None => match &self.on_timeout {
                TimeoutPolicy::Approve => BeforeToolCallDecision::Allow,
                TimeoutPolicy::Deny { reason } => {
                    BeforeToolCallDecision::Deny(ToolResult::error(reason.clone()))
                }
            },
        }
    }
}
```

#### 3.11.2 注入方式

```rust
// 业务层实现 HumanApprover（如 CLI 交互、Slack bot、Web UI callback）
let my_approver: Arc<dyn HumanApprover> = Arc::new(CliApprover::new());

// 包装为 BeforeToolCallHook
let wrapper = Arc::new(HumanApprovalWrapper {
    approver: my_approver,
    on_timeout: TimeoutPolicy::Deny { reason: "human approval timeout".into() },   // fail-secure
});

// 通过 HarnessHooks 注入
let hooks = HarnessHooks {
    before_tool_call: Some(Arc::new(wrapper) as Arc<dyn BeforeToolCallHook>),
    ..HarnessHooks::none()
};
harness.set_hooks(hooks);
```

无 hook → 什么都不做（默认放行）。

#### 3.11.3 为何不直接使用 BeforeToolCallDecision::Modify

Core 的 `BeforeToolCallDecision::Modify(Value)` 允许在放行前修改工具参数，这是 `HumanApprover` 不具备的能力。`HumanApprovalWrapper` 故意只暴露 Approve/Deny 的简化接口。若业务层需要 Modify 语义（如"审批后修改参数再执行"），可直接实现 `BeforeToolCallHook`，跳过 `HumanApprovalWrapper`。

#### 3.11.4 与 TaskRunner 状态机的联动

当 `HumanApprovalWrapper.on_timeout = Deny` 且超时发生时，`BeforeToolCallHook` 返回 `Deny`，agent 收到工具拒绝结果。若需要将此场景上报为 `TaskState::AwaitingHumanInput`，由 TaskRunner 层的 `AfterToolCallHook` 检测特定 deny reason 并驱动状态变更。

#### 3.11.5 决策记录

| 决策 | 选项 | 依据 |
|------|------|------|
| 触发位置 | BeforeToolCallHook（core 已有） | 消除与 core 的概念重复；Modify 能力保留 |
| 业务接口 | HumanApprover（极简 ask）| allow-once/always 等 UX 由业务层在 HumanApprover 内维护 |
| 超时策略 | TimeoutPolicy 可配置，**默认 Deny**（fail-secure） | 注册了监督 hook 而超时 ≠ 同意；无 hook 时全部放行与此无关 |
| Modify 语义 | HumanApprovalWrapper 不暴露 | 直接实现 BeforeToolCallHook 可获得完整语义 |

---

### 3.12 G — 审计日志

#### 3.12.1 设计

```rust
enum AuditEventType {
    ToolCall,
    Auth,
    HumanApproval,
    BudgetExceeded,
    StateTransition,        // task state 变更
}

struct AuditEntry {
    pub timestamp: DateTime<Utc>,
    pub trace_id: String,                   // 链回 OTel span
    pub session_id: EntryId,
    pub task_id: Option<TaskId>,
    pub principal: String,                  // user / agent / system
    pub event_type: AuditEventType,
    pub payload: Value,                     // event-specific 详情
    pub decision: Option<AuditDecision>,    // allow / deny / modify
    pub cost_delta: Option<CostDelta>,      // 本事件产生的开销
    pub prev_hash: Option<String>,          // hash 链：上一条 entry 的 sha256
    pub hash: String,                       // 本条 entry 的 sha256（content + prev_hash）
}

enum AuditDecision {
    Allow,
    Deny { reason: String },
    Modify { before: Value, after: Value },
}

/// 审计 sink
trait AuditSink: Send + Sync {
    async fn record(&self, entry: AuditEntry) -> Result<()>;
    async fn flush(&self) -> Result<()>;
}
```

Hash 链算法：
```
hash_n = SHA256(serialize(content_n) + ":" + hash_{n-1})
```
每条 entry 的 `hash` 域记录本条摘要。日志尾部被篡改时，hash 链断裂可检测。
初始 entry 的 `prev_hash = ""`。

#### 3.12.2 注入位置

审计事件从多路汇入：

```
agent_loop（TurnEnd）
  ├→ 工具执行结果 → AuditEventType::ToolCall
  ├→ AuthHook → AuditEventType::Auth
  ├→ HumanApprovalWrapper → AuditEventType::HumanApproval
  ├→ BudgetControlAdapter → AuditEventType::BudgetExceeded
  └→ TaskRunner state change → AuditEventType::StateTransition
```

#### 3.12.3 决策记录

| 决策 | 选项 | 依据 |
|------|------|------|
| 与已有数据关系 | 独立日志（A） | 合规要求独立保留、防篡改、与 OTel 隔离 |
| 完整性保护 | hash 链（B） | 无密钥负担，篡改可检测 |
| 默认后端 | AuditSink trait + JSONL 实现（C） | 与其他 trait 风格一致 |
| hash 链限制（v0.2 新增说明） | — | 无密钥意味着任何能写入日志的人都可以重新计算链。"篡改可检测"仅在验证者拥有日志独立副本时成立。HMAC 签名是 v0.3+ 的增强选项，v0.2 可接受此限制。 |

---

## 4. 默认实现层

### 4.1 Crate 接口对照表

| Crate | 实现的 trait | 依赖 | 平台 |
|-------|-------------|------|------|
| `sandbox-os` | `Sandbox` + `ExecutionEnv` | 无（纯 tokio fs + shell） | 所有 |
| `sandbox-bwrap` | `Sandbox` + `ExecutionEnv` | bubblewrap CLI | Linux |
| `sandbox-seatbelt` | `Sandbox` + `ExecutionEnv` | macOS sandbox APIs | macOS |
| `mcp` | `ToolSource`, `PromptSource`（via `McpClientHandle`） | 无（仅 trait） | 所有 |
| `trace-otel` | `TraceExporter` | `opentelemetry` | 所有 |
| `audit-jsonl` | `AuditSink` | `tokio::fs` | 所有 |
| `auth` | `AuthHook` | 无（仅 OS env/file IO） | 所有 |

### 4.2 默认实现说明

**无隔离沙箱（sandbox-os）：**
```rust
/// 纯 OsEnv 包装，无安全隔离
/// 适合开发/测试
/// 生产部署需切换到 sandbox-bwrap 或 sandbox-seatbelt
pub struct OsEnvSandbox {
    env: Arc<OsEnv>,
    config: SandboxConfig,
}
```

**Linux 沙箱（sandbox-bwrap）：**
```rust
/// 基于 bubblewrap(1) 的 OS-level permission sandbox
/// SandboxConfig.fs_allowlist → bwrap --ro-bind / --bind
/// SandboxConfig.net_allowlist → 结合本地 proxy 或 seccomp
/// SandboxConfig.resource_limits → setrlimit + timeout
pub struct BwrapSandbox { ... }
```

**macOS 沙箱（sandbox-seatbelt）：**
```rust
/// 基于 macOS sandbox(7) / seatbelt 的权限沙箱
/// SandboxConfig → 编译为 .sb 策略文件
pub struct SeatbeltSandbox { ... }
```

---

## 5. Core 修复分工

### 5.1 阻塞项（v0.2 必须先修）

| # | 问题 | 负责方 | 方案 |
|---|------|--------|------|
| 1 | `OsEnv` 被 `#[cfg(test)]` 保护 | **Runtime 重写** | Runtime 在 `sandbox-os` crate 里重写一份 `OsEnv`，不依赖 core 解除限制 |
| 2 | `StreamOptions` 未传递给 adapter | **先修 adapter** | 扩展 llm_adapter 接口接收完整 `StreamOptions`（timeout / headers / metadata / cache_config），再让 core 接通 |
| 7 | `AuthHook` 在 `compact()` 被 ignored（"v1 ignores it"） | **Core 修** | Core 将 AuthHook 传入 `compact()` 调用链 |

### 5.2 非阻塞项（v0.2 可以做，不急）

| # | 问题 | 方案 |
|---|------|------|
| 3 | `ThinkingLevel` → adapter 映射待确认 | 确认 `req_b.extended_thinking_budget()` 是否完整 |
| 4 | `split_turn_prefix` v1 始终 None | v2 实现跨 turn 边界 split |
| 5 | `HarnessPhase::Branching` 未使用 | branch 操作加固 |
| 6 | compaction token 估算粗糙 | 接入 tokenizer 或 provider token count API |

---

## 6. v0.2 不做清单

| 领域 | 项目 | 原因 |
|------|------|------|
| V 层 | Verification harness | 业务定义，runtime 不做 |
| V 层 | 任务成功标准定义 | 业务流程决策 |
| V 层 | 轨迹级评估 | 业务层或平台层后续 v0.3+ |
| C 层 | 长期记忆具体后端 | 仅定义 MemoryStore trait（v0.3） |
| C 层 | Context drift 检测 | 学术界未解问题，不应阻塞 runtime |
| C 层 | KV-cache 感知强保障 | 仅通过 cache_control 注入位置做轻量保障 |
| L 层 | Multi-agent 编排具体模式 | 仅留 sub-agent 扩展点 |
| E 层 | Snapshot/restore | v0.3 评估场景 |
| G 层 | YAML constitution | 过早抽象 |
| G 层 | H1 输入守卫（prompt injection 检测器） | 业务层可选实现 |
| G 层 | H3 信息流控制（taint tracking） | CaMeL 类，v0.3+ |
| G 层 | AuditSink HMAC 签名 | Hash 链已够用；HMAC v0.3+ |
| T 层 | MCP 运行时动态发现 | 启动时发现够用 |
| T 层 | MCP 工具签名验证（ETDI） | v0.3+ |

---

## 7. 实现顺序建议

### Phase 0 — Core 阻塞项修复

```
修复 core #7：AuthHook 在 compact() 中被 ignored
  ← Runtime 依赖 core，必须先修阻塞项
```

### Phase 1a — 主 crate Trait 定义（全部 trait + 基础类型）

```
llm-harness-runtime 主 crate：
  Sandbox trait + SandboxConfig + ResourceLimits
  ToolRegistry trait + ToolSource trait + ToolDefinition
  McpClientHandle trait
  ResourceProvider trait
  PromptSource trait + PromptSourceTemplate + PromptTemplateCompiler
  TraceExporter trait + SpanEvent
  AuditSink trait + AuditEntry
  AuthHook trait（对齐 core）
  HumanApprover trait + ApprovalRequest + HumanApprovalWrapper
  BudgetExceededHook trait
  SubAgentSpawner trait + SubAgentSpawnOpts
  TaskRunner + TaskState（状态机骨架）
  ← 所有 impl crate 依赖这些 trait；必须先稳定
```

### Phase 1b — 基础设施实现

```
sandbox-os crate           ← OsEnv 重写，实现 Sandbox + ExecutionEnv
sandbox-bwrap crate        ← 基础 Linux 沙箱
auth crate                 ← EnvAuthHook + FileAuthHook
  ← 依赖 Phase 1a 的 trait
```

### Phase 2 — 工具层实现

```
mcp crate                  ← McpClientHandle trait → ToolSource impl
resource crate             ← ResourceProvider trait + ResourceInjector
prompt crate               ← PromptSource trait + PromptTemplateCompiler + 对接 core
  ← 依赖 Phase 1a 的 trait
```

### Phase 3 — 编排核心

```
llm-harness-runtime 主 crate（TaskRunner 状态机完整实现 + checkpoint）
audit-jsonl crate          ← AuditSink JSONL impl
HumanApprovalWrapper       ← built into runtime（BeforeToolCallHook 实现）
BudgetControlAdapter       ← built into runtime（AfterProviderResponseHook + PrepareNextTurnHook）
```

### Phase 4 — 可观测性

```
trace-otel crate           ← TraceExporter → OTel impl
TracingHookAdapter         ← built into runtime（AfterProviderResponseHook + AfterToolCallHook + AfterTurnHook）
成本聚合                    ← built into TaskRunner + AgentHarness 扩展
```

### Phase 5 — 集成测试（最小端到端）

```
第一个端到端 milestone:
  runtime + sandbox-os + auth + audit-jsonl + trace-otel
  跑通一个最简 task:
    Task: "create a file and write hello"
    TaskRunner → start AgentHarness → tool call read_file/write_file
    → TracingHookAdapter → OTel spans 生成
    → AuditSink 记录 ToolCall + StateTransition
    → Task state → Succeeded
  验证目标：
    - HumanApprovalWrapper 可插拔（无 hook 正常跑通）
    - BudgetControlAdapter 超限后 task 进入 Failed
    - TracingHookAdapter span 层次结构正确（Task > Session > Turn > LLM/Tool）
    - audit hash 链断裂检测（手动篡改一条 entry 后 validator 报错）
```

---

## 8. 跨层关注点详述

### 8.1 Hook 耦合与组合语义

#### 8.1.1 Core 的单槽约束与 CompositeHook 模式

Core `HarnessHooks` 每个 hook 类型只有一个 `Option<Arc<dyn Trait>>` 槽位：

```rust
pub struct HarnessHooks {
    pub before_tool_call:           Option<Arc<dyn BeforeToolCallHook>>,
    pub after_tool_call:            Option<Arc<dyn AfterToolCallHook>>,
    pub after_turn:                 Option<Arc<dyn AfterTurnHook>>,
    pub after_provider_response:    Option<Arc<dyn AfterProviderResponseHook>>,
    pub before_provider_request:    Option<Arc<dyn BeforeProviderRequestHook>>,
    pub prepare_next_turn:          Option<Arc<dyn PrepareNextTurnHook>>,
    pub should_stop:                Option<Arc<dyn ShouldStopHook>>,
    // ... 共 11 个槽
    pub fn none() -> Self { ... }  // 唯一工厂方法（无 builder）
}
```

若两个 adapter（如 `TracingHookAdapter` 和 `BudgetControlAdapter`）都需要实现同一 hook，**后赋值者会静默覆盖前者**。

**解决方案：CompositeHook 包装器**

Runtime 在主 crate 的 `composite.rs` 中提供 `CompositeHook<T>` 系列包装器，将多个实现链为一个：

```rust
/// 将多个 AfterProviderResponseHook 顺序执行（全部执行，观测型语义）
pub struct CompositeAfterProviderResponseHook {
    hooks: Vec<Arc<dyn AfterProviderResponseHook>>,
}

impl CompositeAfterProviderResponseHook {
    pub fn new(hooks: Vec<Arc<dyn AfterProviderResponseHook>>) -> Self {
        Self { hooks }
    }
}

impl AfterProviderResponseHook for CompositeAfterProviderResponseHook {
    fn after_response<'a>(&'a self, info: &'a ProviderResponseInfo) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for hook in &self.hooks {
                hook.after_response(info).await;
            }
        })
    }
}

/// 将多个 BeforeToolCallHook 顺序执行（第一个非 Allow 立即短路，决策型语义）
pub struct CompositeBeforeToolCallHook {
    hooks: Vec<Arc<dyn BeforeToolCallHook>>,
}
```

执行语义按 hook 类型分三类：

| Hook 类型 | CompositeHook 执行规则 | 短路条件 |
|-----------|----------------------|---------|
| 观测型（`AfterProviderResponse`、`AfterTurn`、`BeforeProviderRequest`） | 全部顺序执行，忽略单个异常 | 无（全部运行） |
| 决策型（`BeforeToolCall`） | 顺序执行，第一个非 `Allow` 结果立即短路 | 任意 `Deny` |
| 修改型（`AfterToolCall`） | 全部顺序执行；最后一个非 `Passthrough` 的结果生效（last-non-passthrough-wins） | 无（全部运行） |

**核心约束**：CompositeHook 内部的注册顺序决定执行顺序，从而决定短路发生的位置。不同顺序可能产生截然不同的行为。

#### 8.1.2 已知的耦合问题与缓解方案

**耦合点 1：`AfterProviderResponseHook` 的执行顺序**

`TracingHookAdapter` 和 `BudgetControlAdapter` 都实现该 hook。

```
错误顺序：BudgetControlAdapter → TracingHookAdapter
  问题：Budget 超限时，BudgetControlAdapter 标记"停止"，
        控制流仍继续到 TracingHookAdapter，这本身 OK；
        但若 BudgetControlAdapter 的实现错误地在此处抛出 panic/early return，
        TracingHookAdapter 的 span 会丢失。

正确顺序：TracingHookAdapter → BudgetControlAdapter
  理由：先记录 span（不依赖预算决策），再做预算决策；
        即使 budget 超限导致 task 终止，span 已经写入 exporter。
```

规则：**观测型 adapter 先于决策型 adapter 注册**。

**耦合点 2：`BeforeToolCallHook` 中 `HumanApprovalWrapper` 的位置**

```
方案 A：HumanApprovalWrapper 最后注册（推荐）
  执行：机器验证 hook1 → hook2 → ... → HumanApprovalWrapper
  优点：机器过滤先于人工干预，减少不必要的人类打扰；
        hook1/hook2 Deny 时，人类不会被打扰（Deny 已短路）。

方案 B：HumanApprovalWrapper 最先注册
  执行：HumanApprovalWrapper → hook1 → hook2
  缺点：即使后续机器检查会 Deny，人类已被提问（浪费人类注意力）。
```

规则：**`HumanApprovalWrapper` 应最后注册**，让机器验证先过滤，再才请求人类确认。

**耦合点 3：`AuditSink` 的数据完整性**

`AuditSink` 从 `HumanApprovalWrapper`、`BudgetControlAdapter` 等处收集事件。若某个 adapter 因异常未发出审计事件，日志出现空洞。

缓解：所有 adapter 在 finally/drop 语义上保证审计写入，不允许 panic 阻断审计路径。`AuditSink::record()` 内部错误应写入本地 fallback（stderr 或 sidecar），不向调用方传播。

**耦合点 4：`BudgetControlAdapter` 与 `TracingHookAdapter` 的 span 状态**

Budget 超限时，`BudgetControlAdapter` 通过 `PrepareNextTurnHook` 阻断下一 turn。此时 Turn span 可能已经开始（`BeforeTurnHook` 已触发）但还未结束（因为 turn 被中止）。

缓解：`TracingHookAdapter` 在 `TaskRunner` 的状态变更事件中关闭未完成 span，设置 `status = SpanStatus::Error("budget_exceeded")`。

#### 8.1.3 推荐注册顺序

```rust
// HarnessHooks 每个槽位只接受一个实现；多 adapter 通过 CompositeHook 包装后赋值。
let hooks = HarnessHooks {
    // AfterProviderResponseHook：观测优先，决策靠后（通过 CompositeHook 链接）
    after_provider_response: Some(Arc::new(CompositeAfterProviderResponseHook::new(vec![
        tracing_adapter.clone() as Arc<dyn AfterProviderResponseHook>,  // 1. 先记录 span
        budget_adapter.clone()  as Arc<dyn AfterProviderResponseHook>,  // 2. 再做预算决策
    ]))),

    // AfterToolCallHook：观测优先
    after_tool_call: Some(Arc::new(CompositeAfterToolCallHook::new(vec![
        tracing_adapter.clone() as Arc<dyn AfterToolCallHook>,           // 1. 先记录 span
        audit_adapter.clone()   as Arc<dyn AfterToolCallHook>,           // 2. 再写审计
    ]))),

    // BeforeToolCallHook：机器检查优先，人工介入最后（通过 CompositeHook 链接）
    before_tool_call: Some(Arc::new(CompositeBeforeToolCallHook::new(vec![
        validation_hook.clone()        as Arc<dyn BeforeToolCallHook>,   // 1. 参数校验
        rate_limit_hook.clone()        as Arc<dyn BeforeToolCallHook>,   // 2. 速率限制
        human_approval_wrapper.clone() as Arc<dyn BeforeToolCallHook>,   // 3. 人工确认（最后）
    ]))),

    // BeforeProviderRequestHook（单实现，直接赋值）
    before_provider_request: Some(tracing_adapter.clone() as Arc<dyn BeforeProviderRequestHook>),

    // ShouldStopHook：预算超限时阻断
    should_stop: Some(budget_adapter.clone() as Arc<dyn ShouldStopHook>),

    ..HarnessHooks::none()
};

#### 8.1.4 集成测试矩阵

Phase 5 集成测试必须覆盖以下组合场景：

| 场景 | 验证要点 |
|------|---------|
| Tracing only | Span 层次结构完整；Task > Session > Turn > LLM/Tool；begin_task/end_task 配对 |
| Budget only | 超限后 task 进入 Failed；ShouldStopHook 返回 true 阻断生效 |
| HumanApproval only | Approve → 工具执行；Deny → 工具返回错误；Timeout → Deny（fail-secure） |
| Tracing + Budget | 超限时 span 仍然写入（tracing 不被 budget 短路） |
| Tracing + HumanApproval | Deny 时 Tool span 记录 is_error=true |
| Budget + HumanApproval | 超限后 HumanApprovalWrapper 不再被调用（turn 已被阻断） |
| Tracing + Budget + HumanApproval | 审计日志完整；span 完整；状态机正确转换 |
| HumanApproval timeout → AwaitingHumanInput | TaskState 正确变更；audit 记录超时事件 |
| 沙箱 + 所有 hooks | 沙箱启动延迟不影响 hook 注册；shutdown 后 audit flush 完整 |

---

### 8.2 能力-控制权衡

#### 8.2.1 工具风险分层

按操作的可逆性和外部副作用将工具分为四个风险层级：

| 层级 | 特征 | 典型工具 | 推荐门控 |
|------|------|---------|---------|
| **T0 读取型** | 无副作用，完全可逆 | `read_file`, `list_dir`, `search_code`, `get_url` | 无 gate，直接放行 |
| **T1 可逆写入** | 有副作用，可通过 git/backup 恢复 | `write_file`, `create_dir`, `append_log` | 可选 gate（开发环境通常省略） |
| **T2 外部副作用** | 不可逆或影响外部系统 | `delete_file`, `http_post`, `send_email`, `run_command` | 推荐 `HumanApprovalWrapper` |
| **T3 特权操作** | 高权限，影响面大 | `deploy`, `db_write`, `exec_sudo`, `rm -rf` | 必须 `HumanApprovalWrapper` + `AuditSink` |

业务层在构建 `ToolRegistry` 时应按此分层标注每个工具，并在注册 `HumanApprovalWrapper` 时按工具名过滤：

```rust
// HumanApprover 实现示例：只对 T2/T3 工具触发人工确认
struct TieredApprover {
    t2_t3_tools: HashSet<String>,
    inner: Arc<dyn HumanApprover>,
}

impl HumanApprover for TieredApprover {
    async fn ask(&self, request: &ApprovalRequest) -> Option<ApprovalDecision> {
        if self.t2_t3_tools.contains(&request.tool_name) {
            self.inner.ask(request).await
        } else {
            Some(ApprovalDecision::Approve)  // T0/T1 直接放行
        }
    }
}
```

#### 8.2.2 工具集大小与错误面

工具选择错误率与工具集大小正相关。根据已有 coding agent 实测经验：

- **< 8 个工具**：agent 几乎不会调用不相关工具；错误主要来自参数错误
- **8–15 个工具**：偶发工具选择错误；agent 有时调用功能相近但语义不同的工具
- **> 20 个工具**：工具选择错误率显著上升；agent 在不确定时倾向于"探索性"调用不熟悉的工具

建议：**每个 session 的 active 工具集控制在 5–12 个**。利用 `ToolRegistry.subset()` 在启动时按角色/任务类型粗过滤，利用 `active_tools` 在运行时按任务阶段细过滤。

#### 8.2.3 两层过滤的组合策略

```
角色层（session 粒度，ToolRegistry.subset）：
  coding_agent → ["read_file", "write_file", "search_code", "run_tests",
                   "git_diff", "git_commit"]
  browser_agent → ["get_url", "screenshot", "click", "fill_form"]
  data_agent   → ["query_db", "read_csv", "plot_chart", "export"]

任务阶段层（turn 粒度，active_tools via PrepareNextTurnHook）：
  planning phase     → 只开放读取类工具（只看不动）
  implementation phase → 开放写入类工具
  verification phase   → 只开放读取 + 测试工具（run_tests）
  cleanup phase        → 只开放 git 工具
```

**阶段动态切换** 由 `PrepareNextTurnHook` 实现：hook 根据对话历史判断当前阶段，动态生成 `LoopConfig.tools`。这是 `active_tools` 的主要使用场景。

#### 8.2.4 最小权限演进策略

对于权限不确定的新任务类型，建议渐进式扩展：

1. **初始配置**：只给 T0 工具，观察 agent 行为和错误模式
2. **按需扩展**：根据任务失败日志（audit + tracing）识别缺失工具，逐步添加 T1
3. **高危门控**：T2/T3 需求出现时，先加 `HumanApprovalWrapper` 观察实际调用频率，再决定是否自动化

---

### 8.3 成本-质量-速度三难

#### 8.3.1 各组件的开销分析

| 组件 | 额外延迟 | CPU/内存 | 外部依赖 | 对"质量"的贡献维度 |
|------|---------|---------|---------|-----------------|
| `OsEnvSandbox` | ~0ms | 极小 | 无 | 无安全隔离（质量隐患） |
| `BwrapSandbox` (Linux) | 启动 50–200ms；per-command ~5ms | 低 | bubblewrap | 安全隔离质量 ↑ |
| `SeatbeltSandbox` (macOS) | 启动 30–100ms | 低 | macOS kernel | 安全隔离质量 ↑ |
| `TracingHookAdapter` MetadataOnly | per-turn ~1–3ms；batch export ~10ms | 小 | OTel collector | 可观测质量 ↑（调试、回溯） |
| `TracingHookAdapter` Full | per-turn ~5–20ms（序列化 prompt/resp） | 中（大 prompt 时） | OTel + 存储 | 可观测质量 ↑↑（完整回放） |
| `AuditSink` JSONL | per-entry ~0.5–2ms（async IO） | 极小 | 磁盘 | 合规质量 ↑（不可抵赖） |
| `HumanApprovalWrapper` | 人类响应时间（秒～分钟） | 极小 | 人类 + 通知渠道 | 决策质量 ↑（防高危操作） |
| `BudgetControlAdapter` | per-turn <0.1ms | 极小 | 无 | 成本控制 ↑（防超支） |

**关键观察**：
- 沙箱的主要开销是**启动延迟**，对长任务的单 turn 影响极小（摊薄）；对短任务（< 5 turns）影响显著
- `TracingHookAdapter Full` 在处理大 system prompt（> 10K tokens）时序列化开销明显，建议超过 32K tokens 的 prompt 配合 `SpanRedactor` 截断
- `HumanApprovalWrapper` 是唯一引入**人类等待**的组件，会彻底打断 agent 的异步 flow；应仅对 T2/T3 工具启用

#### 8.3.2 三种部署配置

**Dev（本地开发）**
```toml
sandbox = "os"                   # 无隔离，快速启动
tracing = "off"                  # 或 MetadataOnly，减少噪声
audit = "off"
human_approval = "off"
budget = "off"
```
目标：最低开销，快速迭代，不关心安全边界。

**Staging（集成/测试环境）**
```toml
sandbox = "bwrap" / "seatbelt"   # 隔离，接近生产
tracing = "full"                  # 完整捕获，用于调试回溯
audit = "jsonl"                   # 本地文件
human_approval = "off"            # 自动化测试不中断
budget = "soft-limit"             # 触发告警但不停止
```
目标：完整可观测，找 bug；不引入人工等待。

**Prod（生产）**
```toml
sandbox = "bwrap" / "seatbelt"
tracing = "metadata-only" + redactor   # 隐私优先
audit = "jsonl" → 外部存储（S3/GCS）
human_approval = "T2/T3 工具开启"
budget = "hard-limit"                  # 超限直接 Failed
```
目标：安全 + 合规 + 成本可控；tracing 降档以降低延迟和存储。

#### 8.3.3 权衡决策指导

```
问：任务跑得慢，怎么降延迟？
答：
  1. 优先检查 TracingHookAdapter Full → 换 MetadataOnly（最有效）
  2. 检查 HumanApprovalWrapper 是否对 T0/T1 工具开启了（不应该）
  3. 沙箱启动延迟：若任务 < 10 turns，考虑 sandbox reset（重用沙箱）而非每次 start/shutdown

问：成本超预期，怎么控制？
答：
  1. 启用 BudgetControlAdapter 的 hard-limit（当前设置是否为 off？）
  2. 查看 CostAggregate.by_model：是否某个模型用量异常高？
  3. 检查 TracingHookAdapter Full 的存储成本（OTel backend + 存储费用）

问：合规审计要求保留所有操作记录，存储成本太高怎么办？
答：
  1. AuditSink 只记录决策型事件（ToolCall + HumanApproval + StateTransition）
     工具的参数细节走 TracingHookAdapter，不重复存入 audit
  2. audit-jsonl 的外部存储接冷存储（Glacier/Nearline），降低长期存储成本
  3. hash 链保证可验证性，不需要实时可访问

问：安全团队要求"所有 T2/T3 工具必须人工审批"，但人工响应太慢影响 SLA？
答：
  1. 按工具细化 TimeoutPolicy：高频 T2 工具（如 http_post 到受控域名）超时后 Approve（宽松）
     罕见 T3 工具（如 deploy）超时后 Deny + 告警
  2. HumanApprover 实现中做批量审批（一次确认多个同类请求）
  3. 引入审批缓存：同工具+同参数模式在 TTL 内无需重复审批
```

#### 8.3.4 Platform 层的中立立场

这三组权衡的决策权完全在业务层：
- **Platform 层不强制任何组件**：`HarnessHooks::none()` 默认全空——没有 tracing、没有 audit、没有 budget
- **业务层通过选择注入哪些 adapter 来表达权衡立场**
- **运行时开销完全可预测**：每个组件的额外路径独立，不相互依赖（除了 §8.1 中的注册顺序约定）

这是 Platform 层设计的核心价值：提供**可组合的机制**，而不是捆绑的策略。
