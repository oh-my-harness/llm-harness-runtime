# llm-harness-runtime 设计文档

> 参考：`@earendil-works/pi-coding-agent`（`packages/coding-agent/src/core/`）
> 状态：草稿，待 review

---

## 1. 定位

```
coding-agent / eda-agent / other-agent   ← 具体任务层（各自独立仓库）
─────────────────────────────────────────────────────────────────
llm-harness-runtime                      ← 本仓库
─────────────────────────────────────────────────────────────────
llm-harness-core                         ← agent 框架
llm-api-adapter                          ← provider 适配
```

`llm-harness-core` 解决的是"一次对话如何跑起来"——loop、session、compaction、skills、harness。

`llm-harness-runtime` 解决的是"一个可实际部署的 agent 还需要什么"：

- **通用工具集**：read / bash / edit / write / grep / find / ls，任何操作系统类 agent 都能用
- **模型管理**：知道有哪些模型、哪些 provider 可用、API key 从哪来
- **凭证存储**：把 API key 安全地存到本地磁盘、支持 OAuth
- **配置系统**：全局 + 项目两层配置，深度合并，覆盖 harness-core 里的基础 settings
- **扩展系统**：第三方可以注入自定义工具、hook、slash command，不需要改源码
- **AgentRuntime**：把上面所有东西组装起来，对上层（具体 agent）提供简洁接口

> **不包含的内容（已在 llm-harness-core 里）：**
> agent_loop、session 存储、compaction 算法、skills/PromptTemplate 加载、AgentHarness 本体。
> runtime 依赖 harness-core，在其之上添加能力，不重复实现。

---

## 2. Crate 结构

```
llm-harness-runtime/
├── Cargo.toml                (workspace)
└── crates/
    ├── runtime-types/        (trait 定义 + 公共类型，零 IO)
    ├── runtime-tools/        (read/bash/edit/write/grep/find/ls 实现)
    └── runtime/              (model registry + auth + config + extension + AgentRuntime)
```

依赖方向：

```
runtime → runtime-tools → runtime-types
runtime → runtime-types
runtime → llm-harness-core (外部)
runtime → llm-api-adapter  (外部)
```

**硬性规则：**
- `runtime-types` 零外部依赖，只依赖标准库和 `llm-harness-types`
- `runtime-tools` 只依赖 `runtime-types`，不依赖 `runtime`（工具不感知上层）
- 具体 agent（`coding-agent`、`eda-agent`）只依赖 `runtime`，不直接依赖 `llm-harness-core`

---

## 3. 模块设计

### 3.1 通用工具集（`runtime-tools`）

**职责：** 提供所有操作系统类 agent 都会用到的文件/命令工具，通过可插拔 Operations 接口支持本地和远程两种执行环境。

```rust
// runtime-types/src/tool_ops.rs

/// Bash 命令执行后端（可替换为 SSH、容器、沙箱）
pub trait BashOperations: Send + Sync {
    fn exec<'a>(
        &'a self,
        command: &'a str,
        cwd: &'a Path,
        opts: BashExecOptions,
    ) -> BoxFuture<'a, Result<BashOutput, ToolError>>;
}

/// 文件 I/O 后端（可替换为 SFTP、VFS）
pub trait FileOperations: Send + Sync {
    fn read_file<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<Vec<u8>, ToolError>>;
    fn write_file<'a>(&'a self, path: &'a Path, content: &'a [u8]) -> BoxFuture<'a, Result<(), ToolError>>;
    fn read_dir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<Vec<DirEntry>, ToolError>>;
    fn metadata<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<FileMetadata, ToolError>>;
}
```

内置工具：

| 工具 | 描述 | 关键入参 |
|------|------|---------|
| `read` | 读文件内容（支持按行偏移/截断） | `path`, `offset?`, `limit?` |
| `bash` | 执行 shell 命令，捕获 stdout/stderr | `command`, `timeout?` |
| `edit` | 字符串精确替换（old → new） | `path`, `old_string`, `new_string` |
| `write` | 覆盖写入文件 | `path`, `content` |
| `grep` | 正则搜索文件内容 | `pattern`, `path?`, `glob?` |
| `find` | 按名称模式搜索文件 | `pattern`, `path?` |
| `ls` | 列出目录内容 | `path` |
| `ask_user` | 暂停流程等待用户输入（`llm-interactive` 模式） | `question`, `options?` |

> **为什么 Operations 接口要可替换？** EDA agent 需要在远端计算服务器上执行命令（PanGen 通过 SSH 提交），
> coding-agent 在本地跑。通过替换 `BashOperations` / `FileOperations` 的后端实现，工具本身代码不变。
>
> **`BashOperations` 与 `FileOperations` 都需要远程后端：**
> EDA 场景下，命令提交走 SSH（`SshBashOps`），结果文件读取走 NFS 挂载或 SFTP（`SshFileOps`）。
> 两个 trait 各自独立可替换，允许"本地命令 + 远程文件"或"远程命令 + 远程文件"等混合模式。
>
> **`ask_user` 工具** 用于 `llm-interactive` 模式——LLM 提出参数建议后暂停，
> 等用户确认再继续。EDA 场景价值尤其高：EDA 任务耗时数小时，
> 用户希望在任务启动前审批参数，启动后可以离开。

---

### 3.1.1 工具中间进度事件（`runtime-types`）

**背景：** EDA agent 的 `run_eda_job` 工具会轮询等待数小时，期间需要把
`EdaIterationCompleted`、`EarlyConvergenceDetected` 等进度推送给 UI，
但这些信息不应该发给 LLM（会污染上下文）。

**方案：** 在 `ToolContext` 中增加可选的 `progress_tx` 通道：

```rust
// runtime-types/src/tool_context_ext.rs

/// 工具向外推送中间进度事件（不发给 LLM，仅用于 UI/观测）。
pub struct ToolProgressEvent {
    pub tool_name: String,
    pub payload: serde_json::Value,   // 工具自定义结构
}

// ToolContext 扩展字段（在 llm-harness-types 的 ToolContext 之上）
pub struct RuntimeToolContext {
    pub base: ToolContext,            // harness-core 的 ToolContext
    pub progress_tx: Option<mpsc::Sender<ToolProgressEvent>>,
}
```

`AgentRuntime` 创建 `progress_tx` 通道，把 receiver 接入 `AgentHarnessEvent`
广播（包装成 `AgentHarnessEvent::ToolProgress`），UI 订阅后渲染进度。

> **为什么不放在 `llm-harness-types` 的 `ToolContext` 里？**
> `ToolContext` 是 harness-core 的概念，应保持简洁。进度事件是 runtime 层的关切，
> 通过 `RuntimeToolContext` 包装，不污染 core 的类型定义。

---

### 3.2 模型注册表（`runtime` / `model`）

**职责：** 知道有哪些 provider 可用、每个 provider 有哪些模型、以及如何获取对应的 API key 或 token。

```rust
pub struct ModelRegistry {
    providers: Vec<Box<dyn ProviderPlugin>>,
}

pub trait ProviderPlugin: Send + Sync {
    fn name(&self) -> &str;
    fn models(&self) -> &[ModelInfo];
    /// 尝试从本地凭证或环境变量解析出可用的 LlmClient
    fn resolve_client(&self, auth: &AuthStorage) -> Option<Arc<dyn LlmClient>>;
}

pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub context_window: u32,
    pub supports_thinking: bool,
    pub supports_tool_use: bool,
}
```

内置 provider：Anthropic、OpenAI。扩展系统可注册自定义 provider（参见 3.5）。

> **为什么需要这层而不直接用 llm-api-adapter？**
> llm-api-adapter 解决的是"如何发请求"，不管理"用哪个 key"、"有哪些模型可选"。
> ModelRegistry 是服务发现层——知道本地存了哪些凭证、哪些 provider 当前可用。

---

### 3.3 凭证存储（`runtime` / `auth`）

**职责：** 把 API key 和 OAuth token 安全地持久化到本地磁盘，支持并发安全的读写。

```rust
pub struct AuthStorage {
    path: PathBuf,    // ~/.config/llm-harness/auth.json
}

impl AuthStorage {
    pub async fn get_api_key(&self, provider: &str) -> Option<String>;
    pub async fn set_api_key(&self, provider: &str, key: String) -> Result<()>;
    pub async fn get_oauth_token(&self, provider: &str) -> Option<OAuthToken>;
    pub async fn set_oauth_token(&self, provider: &str, token: OAuthToken) -> Result<()>;
}

pub struct OAuthToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}
```

- 存储格式：JSON，文件权限 `0600`
- 写入使用文件锁（`fd-lock`）防止并发竞态
- 读取时优先检查环境变量（`ANTHROPIC_API_KEY` 等），其次读文件

---

### 3.4 配置系统（`runtime` / `config`）

**职责：** 两层配置（全局 + 项目），深度合并，项目配置优先。

```rust
pub struct RuntimeSettings {
    // 模型
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub default_thinking_level: Option<ThinkingLevel>,

    // 工具行为
    pub bash_timeout_ms: Option<u64>,
    pub read_max_lines: Option<usize>,

    // 会话
    pub session_dir: Option<String>,

    // 资源路径
    pub skill_dirs: Option<Vec<String>>,
    pub template_dirs: Option<Vec<String>>,
    pub extension_dirs: Option<Vec<String>>,

    // 功能开关
    pub compaction: Option<CompactionSettings>,
    pub retry: Option<RetrySettings>,
    pub branch_summary: Option<BranchSummarySettings>,
}

pub struct SettingsManager {
    global: RuntimeSettings,   // ~/.config/llm-harness/settings.json
    project: RuntimeSettings,  // <cwd>/.llm-harness/settings.json
    merged: RuntimeSettings,   // 深度合并结果（project 优先）
}
```

> **为什么不复用 llm-harness-core 里的 settings？**
> harness-core 的 `AgentHarnessOptions` 是构造时传入的不可变配置，没有分层合并的概念。
> `RuntimeSettings` 是运行时可动态读取的分层配置，语义不同。

---

### 3.5 扩展系统（`runtime` / `extension`）

**职责：** 允许第三方在不修改源码的情况下注入工具、hook、slash command、自定义 provider。

```rust
/// 扩展包描述符（从目录或 .toml 加载）
pub struct ExtensionManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub entry: String,   // 动态库路径 (.so/.dylib) 或脚本路径
}

/// 扩展运行时接口
#[async_trait]
pub trait Extension: Send + Sync {
    fn manifest(&self) -> &ExtensionManifest;

    fn tools(&self) -> Vec<Arc<dyn Tool>>;
    fn providers(&self) -> Vec<Box<dyn ProviderPlugin>>;
    fn slash_commands(&self) -> Vec<SlashCommand>;

    async fn on_hook(&self, hook: HookEvent, ctx: &HookContext) -> Result<HookOutcome>;
}

pub enum HookEvent {
    SessionStart,
    SessionEnd,
    BeforeToolCall { name: String },
    AfterToolCall  { name: String, result: ToolResult },
    BeforeCompact,
    BeforeFork,
}

pub enum HookOutcome {
    Continue,
    Abort { reason: String },
    /// AfterToolCall 专用：修改工具结果
    PatchResult(ToolResultPatch),
}
```

> **第一阶段可以只实现 hook 注册，不实现动态加载。**
> 扩展先以"编译时注入"的形式存在（构造 `AgentRuntime` 时传入 `Vec<Arc<dyn Extension>>`），
> 动态加载（dlopen / WASM）作为后续阶段实现。

---

### 3.6 AgentRuntime（`runtime` / `agent_runtime`）

**职责：** 把 model registry、auth、config、tools、extensions、harness 组装在一起，对上层（具体 agent）提供统一接口。

```rust
pub struct AgentRuntime {
    harness: AgentHarness,
    model_registry: ModelRegistry,
    auth: Arc<AuthStorage>,
    settings: Arc<SettingsManager>,
    extensions: Vec<Arc<dyn Extension>>,
}

pub struct AgentRuntimeOptions {
    pub cwd: PathBuf,
    pub tools: Vec<Arc<dyn Tool>>,          // 具体 agent 提供的领域工具
    pub system_prompt: String,
    pub extra_extensions: Vec<Arc<dyn Extension>>,
    pub settings_override: Option<RuntimeSettings>,
}

impl AgentRuntime {
    /// 从当前环境自动解析 model、auth，构建完整 runtime。
    pub async fn build(opts: AgentRuntimeOptions) -> Result<Self, RuntimeError>;

    /// 发送 prompt，等待完成（自动处理 compaction、retry）。
    pub async fn prompt(&self, text: &str) -> Result<(), RuntimeError>;

    /// 订阅事件流。
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<AgentHarnessEvent>>;

    /// 当前激活的模型 + provider 信息。
    pub fn active_model(&self) -> &ModelInfo;

    /// 切换模型。
    pub async fn set_model(&self, model_id: &str) -> Result<(), RuntimeError>;

    /// 重新加载扩展和 skill 资源。
    pub async fn reload_resources(&self) -> Result<(), RuntimeError>;

    /// 暴露底层 harness（高级用法）。
    pub fn harness(&self) -> &AgentHarness;
}
```

`AgentRuntime::build` 内部做的事：

```
1. 加载 SettingsManager（global + project）
2. 初始化 AuthStorage
3. 扫描 extension_dirs，加载扩展（阶段一：编译时注入，无动态加载）
4. 扩展注册额外的 provider 到 ModelRegistry
5. 根据 settings.default_model 解析出 LlmClient（通过 ModelRegistry + AuthStorage）
6. 合并工具：opts.tools + 扩展注入的工具
7. 加载 skills / templates（调用 harness-core 的 reload_resources）
8. 构建 AgentHarness
9. 注册扩展 hook 到 HarnessHooks
```

> **为什么具体 agent 传入 tools 而不是 runtime 内置？**
> read/bash/grep 这些工具虽然"通用"，但 eda-agent 可能不需要全部，
> 也可能需要用自定义的 `BashOperations`（远程执行）替换默认实现。
> 由具体 agent 决定挂载哪些工具，runtime 提供工具实现，不强制绑定。

---

## 4. 具体 agent 如何使用

```rust
// eda-agent/src/main.rs
let tools = vec![
    runtime_tools::read_tool(cwd.clone(), Default::default()),
    runtime_tools::bash_tool(cwd.clone(), BashOptions {
        operations: Arc::new(RemoteBashOps::new(&ssh_config)),  // EDA 在远端跑
        ..Default::default()
    }),
    Arc::new(RunPanGenTool::new(job_dir.clone())),   // EDA 专属工具
    Arc::new(ParseAMCResultTool::new()),             // EDA 专属工具
];

let runtime = AgentRuntime::build(AgentRuntimeOptions {
    cwd,
    tools,
    system_prompt: build_eda_system_prompt(&context),
    extra_extensions: vec![],
    settings_override: None,
}).await?;

runtime.prompt("开始 FindOptics 阶段").await?;
```

---

## 5. 阶段规划

| 阶段 | 内容 | 交付物 |
|------|------|--------|
| **Phase 1** | 通用工具集（runtime-tools）+ BashOperations/FileOperations 接口 | 7 个工具 + ops trait |
| **Phase 2** | AuthStorage + ModelRegistry（Anthropic + OpenAI） | 凭证存储 + 模型解析 |
| **Phase 3** | SettingsManager（两层配置 + 深度合并） | RuntimeSettings |
| **Phase 4** | AgentRuntime（组装 harness + registry + auth + settings） | AgentRuntime::build |
| **Phase 5** | Extension system（编译时注入版，含 hook + 工具注册） | Extension trait |
| **Phase 6** | 动态扩展加载（dylib / WASM） | ExtensionLoader |

> **Phase 1-4 足够支撑 coding-agent 和 eda-agent 迁移过来。**
> Phase 5-6 是扩展生态，优先级靠后。

---

## 6. 与 llm-harness-core 的边界

| 能力 | 在哪里 | 说明 |
|------|--------|------|
| agent_loop | harness-core | runtime 不重复实现 |
| Session 存储 + 分支 | harness-core | runtime 通过 harness 间接使用 |
| Compaction 算法 | harness-core | runtime 通过 harness 触发 |
| Skills / PromptTemplate 加载 | harness-core | runtime 提供目录，harness 加载 |
| AgentHarness | harness-core | AgentRuntime 持有并包装它 |
| 通用工具实现 | **runtime-tools** | |
| 模型注册 / 凭证 | **runtime** | |
| 两层配置系统 | **runtime** | |
| 扩展 hook 系统 | **runtime** | |
| AgentRuntime 组装 | **runtime** | |
| 领域专属工具 | **coding-agent / eda-agent** | |
| 领域 system prompt | **coding-agent / eda-agent** | |
