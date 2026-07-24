# MCP Builder/Engine Wiring

**Date**: 2026-07-23  
**Issue**: [#78](https://github.com/oh-my-harness/llm-harness-runtime/issues/78) §3  
**前置 spec**: [MCP Transport, Protocol & Manager](./2026-07-23-mcp-transport-protocol-manager-design.md)  
**Scope**: `HarnessBuilder` / `WorkflowEngine` 接线 + config 驱动注入  
**不含**: Senza PyO3 绑定（独立 spec）

## 背景

`HarnessBuilder`（1387 行）和 `WorkflowEngine` 中搜索 `mcp` 零匹配。没有 `.mcp_server()` 或 `.with_mcp_client()` builder 方法。用户要手动调 `McpToolSource::into_adapters()` 拿到 `Vec<McpToolAdapter>`，再逐个 `builder.tool(adapter)`——没有 helper，没有配置驱动。`ResourceInjector` 和 `InMemoryPromptRegistry` 从未被 builder/engine 调用。

## 设计

### 1. Crate 边界：MCP 包装 Builder

```rust
// crates/llm-harness-runtime-mcp/src/builder.rs

/// runtime 的 HarnessBuilder 保持 MCP 无关。本类型位于 runtime-mcp，
/// 单向依赖并包装 HarnessBuilder，避免 Cargo 循环依赖。
pub struct McpHarnessBuilder {
    inner: HarnessBuilder,
    /// MCP server 配置（延迟连接，build 时统一处理）。
    mcp_servers: Vec<(String, McpServerConfig)>,
    /// 延迟到 async build 阶段读取，保持配置 API 同步可链式调用。
    mcp_config_files: Vec<PathBuf>,
    /// 已创建的 MCP manager（如果用户直接传入）。
    mcp_manager: Option<Arc<McpManager>>,
    /// 仅注入显式选择的资源；默认禁用。
    resource_policy: Option<McpResourceInjectionPolicy>,
}

/// 让普通 builder 显式进入 MCP 包装层。
pub trait HarnessBuilderMcpExt {
    fn with_mcp(self) -> McpHarnessBuilder;
}

impl HarnessBuilderMcpExt for HarnessBuilder {
    fn with_mcp(self) -> McpHarnessBuilder {
        McpHarnessBuilder::new(self)
    }
}

impl McpHarnessBuilder {
    /// 添加一个 MCP server（编程式 API）。
    /// build 时自动连接，工具注入到 tool registry。
    pub fn mcp_server(mut self, name: impl Into<String>, config: McpServerConfig) -> Self {
        self.mcp_servers.push((name.into(), config));
        self
    }

    /// 从 mcp.json 文件加载 MCP server 配置。
    pub fn mcp_config_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.mcp_config_files.push(path.into());
        self
    }

    /// 直接传入已创建的 McpManager（高级 API）。
    /// 跳过 builder 内部的 manager 创建，用户自行管理生命周期。
    pub fn with_mcp_manager(mut self, manager: Arc<McpManager>) -> Self {
        self.mcp_manager = Some(manager);
        self
    }

    /// 配置要注入 system prompt 的资源。未调用时不自动读取任何资源。
    pub fn resource_injection(mut self, policy: McpResourceInjectionPolicy) -> Self {
        self.resource_policy = Some(policy);
        self
    }
}
```

使用方式：

```rust
let harness = HarnessBuilder::new(model)
    .provider(model, client)
    .system_prompt("...")
    .with_mcp()
    .mcp_server("filesystem", config)
    .build(env)
    .await?;
```

依赖方向保持为：

```text
llm-harness-runtime-mcp → llm-harness-runtime → llm-harness-agent
```

`llm-harness-runtime`、`llm-harness-agent` 都不引用 `McpManager`、
`McpSession`、`McpResourceProvider` 等具体 MCP 类型。

### 2. 异步 Build 与接线

MCP 包装层的两个 build 方法都是异步的。它先完成 MCP 连接和接线，再调用
runtime 原有 builder；runtime 的 `HarnessBuilder::build_with_session()` 保持同步，
不引入破坏性 API 变更。

```rust
impl McpHarnessBuilder {
    pub async fn build(
        self,
        env: Arc<dyn ExecutionEnv>,
    ) -> Result<McpAgentHarness> {
        let prepared = self.prepare().await?;
        let harness = prepared.inner.build(env).await?;
        prepared.finish(harness)
    }

    pub async fn build_with_session(
        self,
        env: Arc<dyn ExecutionEnv>,
        session: Session,
    ) -> Result<McpAgentHarness> {
        let prepared = self.prepare().await?;
        let harness = prepared.inner.build_with_session(env, session)?;
        prepared.finish(harness)
    }
}
```

`prepare()` 的固定顺序：

```rust
async fn prepare(mut self) -> Result<PreparedMcpBuild> {
    // a. 异步读取所有 mcp.json，并与编程式配置合并。
    //    同名 server 采用后注册覆盖；重复项记录 warning。
    self.load_config_files().await?;

    // b. 确定 manager 所有权：外部传入 = Shared；内部创建 = Owned
    let (mcp_manager, ownership) = self.resolve_manager().await?;

    // c. Tools：构造 adapter，并在创建 AgentHarness 之前注入 inner builder
    let tool_source = McpToolSource::new(mcp_manager.clone());
    let mcp_adapters = tool_source.into_adapters().await?;
    for adapter in mcp_adapters {
        self.inner = self.inner.tool(Arc::new(adapter));
    }

    // d. Resources：只有显式策略存在时才注册 BeforeRunHook
    if let Some(policy) = self.resource_policy {
        let hook = McpResourceInjectionHook::new(mcp_manager.clone(), policy);
        let mut hooks = HarnessHooks::none();
        hooks.before_run.push(Arc::new(hook));
        self.inner = self.inner.hooks(hooks);
    }

    // e. MCP prompt registry 保留在包装层，不塞入 AgentHarnessOptions
    let prompts = Arc::new(McpPromptRegistry::discover(mcp_manager.clone()).await?);

    Ok(PreparedMcpBuild {
        inner: self.inner,
        manager: mcp_manager,
        ownership,
        prompts,
    })
}
```

### 3. Resource 注入点

MCP resource 默认**不自动注入**。`resources/list` 可能返回大量文件、数据库对象
或二进制资源；全部读取既不安全，也可能直接耗尽模型上下文。

调用方必须显式配置 URI 或 URI 前缀，并设置硬上限：

```rust
pub struct McpResourceInjectionPolicy {
    /// 显式 server + URI 选择器；默认空。
    pub selectors: Vec<McpResourceSelector>,
    /// 单次 run 最多注入的资源数。
    pub max_resources: usize,          // default: 16
    /// 单个资源解码后的最大字节数。
    pub max_bytes_per_resource: usize, // default: 256 KiB
    /// 单次 run 所有资源的最大总字节数。
    pub max_total_bytes: usize,        // default: 1 MiB
    /// 注入 system prompt 的最大字符数，作为 token budget 的确定性近似上限。
    pub max_total_chars: usize,        // default: 200_000
    /// 单个资源失败时是否终止 run；默认 false（记录 warning 后继续）。
    pub strict: bool,
}

pub enum McpResourceSelector {
    Exact {
        server: String,
        uri: String,
    },
    Prefix {
        server: String,
        uri_prefix: String,
    },
}
```

注入通过 `runtime-mcp` 中的 `McpResourceInjectionHook` 实现
`BeforeRunHook`，不修改 `AgentHarnessOptions`，也不让 agent crate 依赖 runtime：

```rust
impl BeforeRunHook for McpResourceInjectionHook {
    fn before_run<'a>(&'a self, ctx: BeforeRunCtx<'a>)
        -> BoxFuture<'a, Result<BeforeRunResult, AgentError>>
    {
        Box::pin(async move {
            // 1. Exact 直接 read；Prefix 才对指定 server list 并筛选
            // 2. 按 selector/URI 稳定排序并截断 max_resources
            // 3. read 前后检查单项/总量上限
            // 4. 只把通过校验的 text 内容追加到 ctx.system_prompt
            // 5. blob 只写元信息，不把 base64/原始二进制放进 prompt
            Ok(BeforeRunResult {
                additional_messages: vec![],
                system_prompt: None,
            })
        })
    }
}
```

资源读取发生在 `before_run`，因此每次 run 的上下文是确定的，不采用“后台加载后
不确定何时进入 prompt”的竞态模型。没有配置 policy 时，不调用
`resources/list` / `resources/read`。

### 4. Prompt Registry 挂载

MCP `prompts/get` 返回异步消息列表，不是 runtime 静态
`PromptTemplate` 使用的 `{{param}}` 字符串。因此不修改
`InMemoryPromptRegistry`，也不往 `AgentHarnessOptions` 塞 MCP registry。

`McpPromptRegistry` 由 `McpAgentHarness` 包装层持有：

```rust
pub struct McpPromptRegistry {
    sessions: Vec<Arc<McpSession>>,
    prompts: RwLock<HashMap<String, PromptMeta>>,  // discover 缓存的元信息
}

impl McpPromptRegistry {
    pub async fn discover(&self) -> Result<()> {
        for session in &self.sessions {
            let prompts = session.list_prompts().await?;
            for p in prompts {
                self.prompts.write().insert(p.name.clone(), PromptMeta { ... });
            }
        }
    }

    pub async fn invoke(&self, name: &str, args: HashMap<String, String>) -> Result<String> {
        let session = self.find_session_for_prompt(name)?;
        let result = session.get_prompt(name, args).await?;
        // 将 messages 转为文本
        Ok(self.format_prompt_messages(result.messages))
    }

    fn format_prompt_messages(&self, messages: Vec<McpPromptMessage>) -> String {
        messages.into_iter().map(|m| {
            match m.content {
                McpPromptContent::Text { text } => format!("{}: {}", m.role, text),
                McpPromptContent::Image { mime_type, .. } => format!("{}: [Image: {}]", m.role, mime_type),
                McpPromptContent::Audio { mime_type, .. } => format!("{}: [Audio: {}]", m.role, mime_type),
                McpPromptContent::ResourceLink { uri, name, .. } => {
                    format!("{}: [Resource link: {} ({})]", m.role, name, uri)
                }
                McpPromptContent::Resource { resource } => format!("{}: [Resource: {}]", m.role, resource.uri),
            }
        }).collect::<Vec<_>>().join("\n")
    }
}
```

包装层公开：

```rust
impl McpAgentHarness {
    pub fn list_mcp_prompts(&self) -> Vec<McpPromptMeta>;

    pub async fn prompt_from_mcp(
        &self,
        name: &str,
        args: HashMap<String, String>,
    ) -> Result<()> {
        let text = self.prompts.invoke(name, args).await?;
        self.harness.prompt(text).await?;
        Ok(())
    }
}
```

### 5. WorkflowEngine 接线

`WorkflowEngine` 保持 MCP 无关，不在 `WorkflowEngineConfig` 中加入
`Arc<McpManager>`。`runtime-mcp` 提供 `McpWorkflowEngine` 包装层：

```rust
pub struct McpWorkflowEngine {
    inner: WorkflowEngine,
    runtime: Arc<McpRuntimeHandle>,
}

impl McpWorkflowEngine {
    /// 先连接/发现 MCP，再把当时可用的 adapter 和 resource hook 作为
    /// 通用 tool/hook contribution 配置到 WorkflowEngine。
    pub async fn new(
        config: WorkflowEngineConfig,
        mcp: McpWorkflowConfig,
    ) -> Result<Self>;

    pub async fn run(&self) -> Result<TaskResult, WorkflowError> {
        self.inner.run().await
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.runtime.shutdown_if_owned().await
    }
}
```

Workflow 中所有 step 共享同一 manager，不为每一步重复建立 MCP 连接。普通
`WorkflowEngine` 和现有 `BuilderCustomize` API 不发生破坏性变化。

第一阶段只承诺 build 时已发现工具的稳定快照。deferred tool、后台新增/删除工具
需要单独的通用 dynamic-tool contribution API；在该 API 完成前，不在运行中的
Workflow 上静默替换工具集合。

### 6. Manager 生命周期

- **创建**：`McpHarnessBuilder::build()` 时创建，或用户传入。
- **使用**：`McpAgentHarness` / `McpWorkflowEngine` 包装层持有 manager，保证
  harness/engine 使用期间连接存活。
- **Owned**：包装层内部创建。调用 `shutdown().await` 时执行
  `disconnect_all()`。
- **Shared**：用户传入。包装层不主动断开，由用户管理生命周期。
- **Drop 兜底**：只同步取消后台 task，并依靠 stdio child 的
  `kill_on_drop(true)` 防止进程泄漏；不在 Drop 中调用 `tokio::spawn`，也不保证
  HTTP DELETE 完成。

```rust
pub enum McpManagerOwnership {
    Owned,
    Shared,
}

pub struct McpAgentHarness {
    harness: AgentHarness,
    runtime: Arc<McpRuntimeHandle>,
    prompts: Arc<McpPromptRegistry>,
}

impl Deref for McpAgentHarness {
    type Target = AgentHarness;

    fn deref(&self) -> &Self::Target {
        &self.harness
    }
}

pub struct McpRuntimeHandle {
    manager: Arc<McpManager>,
    ownership: McpManagerOwnership,
}

impl McpRuntimeHandle {
    pub async fn shutdown_if_owned(&self) -> Result<()> {
        if matches!(self.ownership, McpManagerOwnership::Owned) {
            self.manager.disconnect_all().await?;
        }
        Ok(())
    }
}

impl McpAgentHarness {
    pub async fn shutdown(&self) -> Result<()> {
        self.runtime.shutdown_if_owned().await
    }
}

impl Drop for McpRuntimeHandle {
    fn drop(&mut self) {
        self.manager.cancel_background_tasks();
    }
}
```

### 7. 工具命名

MCP 工具注入 tool registry 时，命名为 `mcp__{server_name}_{tool_name}`，对齐 oh-my-pi。`McpToolAdapter::name()` 返回此格式。

```rust
impl McpToolAdapter {
    pub fn name(&self) -> &str {
        &self.full_name  // "mcp__filesystem_read"
    }
}
```

命名由 `McpToolSource::into_adapters()` 在创建 adapter 时设置：

```rust
fn create_mcp_tool_name(server_name: &str, tool_name: &str) -> String {
    format!("mcp__{}_{}", sanitize(server_name), sanitize(tool_name))
}
```

### 8. 测试

| 测试 | 覆盖 |
|------|------|
| `builder.with_mcp().mcp_server()` | 包装层配置存储、build 时连接、无循环依赖 |
| `McpHarnessBuilder::mcp_config_file()` | mcp.json 加载、多 server |
| `McpHarnessBuilder::with_mcp_manager()` | Shared manager 传入、不自动 disconnect |
| `build_with_session().await` | MCP 包装层异步准备后调用 runtime 同步 build |
| build 后 tool registry | MCP 工具以 `mcp__` 前缀注册 |
| resource 默认关闭 | 无 policy 时不调用 list/read |
| resource allowlist + limits | 只注入命中项，数量/单项/总量/字符上限生效 |
| prompt registry | `McpPromptRegistry::invoke()` 调用 `session.get_prompt()` |
| owned shutdown | 显式 shutdown 调 `disconnect_all()` |
| shared shutdown | 包装层不关闭外部 manager |
| drop fallback | 不依赖当前 Tokio runtime，不泄漏 stdio child |
| McpWorkflowEngine | 所有 step 共享 manager，普通 WorkflowEngine 不依赖 MCP |
