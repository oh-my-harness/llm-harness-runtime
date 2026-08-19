# MCP 集成指南

> `llm-harness-mcp` 将 [Model Context Protocol](https://modelcontextprotocol.io) 服务器接入 Agent。MCP 是一个开放协议，让 Agent 连接外部工具服务（如 GitHub、数据库、文件系统等）。
>
> 本 crate 实现完整的 MCP 协议层：传输（stdio/HTTP/SSE）、会话管理、多服务器生命周期、工具适配、提示与资源发现、版本锁定、凭证作用域。

---

## 快速开始

```rust
use llm_harness_agent::HarnessBuilder;
use llm_harness_mcp::{HarnessBuilderMcpExt, McpServerConfig};

let harness = HarnessBuilder::new("claude-3-5-sonnet")
    .with_mcp()
    .mcp_server("filesystem", McpServerConfig::Stdio {
        command: "npx".into(),
        args: vec!["@modelcontextprotocol/server-filesystem".into(), "/tmp".into()],
        env: Default::default(),
        cwd: None,
        timeout: None,
        version_policy: Default::default(),
        credentials: None,
    })
    .build(env)
    .await?;
```

MCP 工具自动注册为 Agent 可调用的 `Tool`。工具名为 `mcp__{server}_{tool}`（如 `mcp__filesystem_read_file`）。

---

## 服务器配置

`McpServerConfig` 是一个 `enum`，支持两种传输方式：

### Stdio（子进程）

```rust
McpServerConfig::Stdio {
    command: "npx".into(),
    args: vec!["@modelcontextprotocol/server-github".into()],
    env: HashMap::from([("GITHUB_TOKEN".into(), token.into())]),
    cwd: None,
    timeout: Some(30_000),  // ms, None = 默认 30s
    version_policy: Default::default(),
    credentials: None,
}
```

### HTTP / SSE（远程）

```rust
McpServerConfig::HttpOrSse {
    url: "https://mcp.example.com/sse".into(),
    transport_type: Some("sse".into()),  // "http" | "sse", None = 自动检测
    headers: HashMap::from([("Authorization".into(), "Bearer ...".into())]),
    timeout: None,
    version_policy: Default::default(),
    credentials: None,
}
```

### 配置文件

也支持从 JSON 配置文件加载（`.mcp.json` 或 `mcp.json`）：

```json
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "${ENV:GITHUB_TOKEN}" }
    }
  }
}
```

```rust
builder.with_mcp()
    .mcp_config_file(".mcp.json")
    .build(env).await?;
```

---

## 连接管理

`McpManager` 管理多个 MCP 服务器的连接生命周期。

| 能力 | 方法 | 说明 |
|------|------|------|
| 连接 | `connect_server(name, config)` | 建立连接 + initialize 握手 + 发现工具 |
| 断开 | `disconnect(name)` / `disconnect_all()` | 关闭连接 |
| 重连 | 内置 | 自动重连（30 秒窗口内最多 5 次，指数退避） |
| 工具发现 | `get_tools()` | 返回所有已连接服务器的工具列表 |
| 工具缓存 | `McpToolCache` | 工具定义缓存 + 持久化到磁盘，减少重复 `tools/list` |
| 状态查询 | `connection_status(name)` | `Connected` / `Connecting` / `Disconnected` |

```rust
use llm_harness_mcp::McpManager;

let manager = Arc::new(McpManager::new());
manager.connect_server("github", github_config).await?;
let tools = manager.get_tools();  // Vec<ExposedTool>
```

`ExposedTool` 包含 `server_name`、`full_name`（`mcp__{server}_{tool}`）、`tool_def`（`McpToolDefinition`）。

---

## 工具适配

`McpToolSource` 把 `McpManager` 的工具视图适配为核心 `Tool` trait 实现。

```rust
use llm_harness_mcp::McpToolSource;

let source = McpToolSource::new(manager.clone());
let adapters = source.into_adapters().await?;  // Vec<McpToolAdapter>
for adapter in adapters {
    builder.tool(adapter);
}
```

> 通常不需要手动调用——`McpHarnessBuilder::build()` 会自动完成适配和注册。

---

## 提示与资源

### 提示发现

`McpPromptRegistry` 发现和管理 MCP 服务器提供的提示模板。

```rust
// McpHarnessBuilder 自动注册 prompt registry
let mcp_harness = builder.with_mcp()
    .mcp_server("server", config)
    .build(env).await?;

// 通过 registry 列出和调用提示
let prompts: Vec<McpPromptMeta> = mcp_harness.prompts.list().await?;
```

### 资源注入

`McpResourceInjectionHook` 按策略将 MCP 资源内容注入到 system prompt。

```rust
use llm_harness_mcp::{McpResourceInjectionPolicy, McpResourceSelector};

let policy = McpResourceInjectionPolicy {
    selectors: vec![
        McpResourceSelector::Prefix { server: "docs".into(), uri_prefix: "file:///docs/".into() },
    ],
    max_resources: 16,
    max_bytes_per_resource: 256 * 1024,
    max_total_bytes: 1024 * 1024,
    max_total_chars: 200_000,
    strict: false,
};

builder.with_mcp()
    .mcp_server("docs", config)
    .resource_injection(policy)
    .build(env).await?;
```

---

## 版本锁定

版本锁定防止 MCP 服务器静默更新改变工具行为。

| 类型 | 说明 |
|------|------|
| `McpLockFile` | 锁文件根结构，含每个服务器的快照 |
| `ServerInfoSnapshot` | 服务器名称 + 版本 |
| `ToolSnapshotEntry` | 工具名 + description hash + input_schema hash（SHA-256） |
| `ToolDiff` | 工具变更差异：added / removed / modified_desc / modified_schema |

### 版本策略

`VersionPolicy` 配置版本不匹配时的行为：

| 策略 | `on_mismatch`（服务器版本变） | `on_tool_change`（工具定义变） |
|------|--------------------------|---------------------------|
| `FailClosed`（默认） | 拒绝连接 | 拒绝连接 |
| `AllowWithWarning` | 警告但继续 | 警告但继续 |
| `AllowAndUpdate` | 继续并更新锁文件 | 继续并更新锁文件 |

```rust
use llm_harness_mcp::config::VersionPolicy;

builder.with_mcp()
    .mcp_server("github", config)
    .lock_file(".mcp-lock.json")
    .version_policy("github", VersionPolicy {
        lock_file: Some(".mcp-lock.json".into()),
        on_mismatch: MismatchPolicy::AllowWithWarning,
        on_tool_change: ToolChangePolicy::FailClosed,
    })
    .build(env).await?;
```

---

## 凭证作用域

MCP 使用进程级凭证模型（env/headers 在 server 内所有工具间共享）。凭证作用域提供**检测**（而非强制隔离）。

### 凭证声明

```json
{
  "credentials": {
    "scoped_env": {
      "GITHUB_TOKEN": {
        "value": "${ENV:GITHUB_TOKEN}",
        "allowed_tools": ["create_pr", "create_issue"]
      }
    }
  }
}
```

- `value` 支持纯文本、`${ENV:VAR}`、`${ENV:VAR:-default}`、`vault://...`
- `allowed_tools` 为 `None` = 所有工具可用；`Some(list)` = 仅列表中的工具可用

### 凭证提供者

| 提供者 | 说明 |
|--------|------|
| `EnvCredentialProvider`（默认） | 从进程环境变量解析 `${ENV:...}` |
| `CredentialProvider` trait | 用户实现，支持 `vault://` 等自定义引用 |

### 凭证保护 Hook

`McpCredentialGuardHook`（在 `llm-harness-strategy` 中）检查工具调用是否在凭证作用域内。详见 [策略层指南](strategy-guide.md#mcp-凭证保护mcp-credential-guard)。

---

## 传输层

`McpTransport` trait 定义传输抽象，三种实现：

| 传输 | 文件 | 用途 |
|------|------|------|
| `StdioTransport` | `transport/stdio.rs` | 子进程 stdin/stdout 通信 |
| `HttpTransport` | `transport/http.rs` | HTTP 请求/响应（MCP Streamable HTTP） |
| `LegacySseTransport` | `transport/sse.rs` | 旧版 SSE 传输（兼容 `2024-11-05` 协议） |

默认超时：30 秒（`DEFAULT_TIMEOUT_MS`）。

---

## Builder 集成

`HarnessBuilderMcpExt` trait 为 `HarnessBuilder` 添加 MCP 支持。

| 方法 | 说明 |
|------|------|
| `.with_mcp()` | 进入 `McpHarnessBuilder` |
| `.mcp_server(name, config)` | 添加一个 MCP 服务器 |
| `.mcp_config_file(path)` | 从 JSON 文件加载服务器配置 |
| `.with_mcp_manager(manager)` | 使用外部共享的 `McpManager` |
| `.resource_injection(policy)` | 配置资源注入策略 |
| `.lock_file(path)` | 设置锁文件路径（默认 `.mcp-lock.json`） |
| `.version_policy(server, policy)` | 设置特定服务器的版本策略 |

`build()` 返回 `McpAgentHarness`，包含：
- `harness: AgentHarness` — 标准 harness（已注册 MCP 工具）
- `runtime: McpRuntimeHandle` — MCP 运行时句柄（可 `shutdown_if_owned()`）
- `prompts: McpPromptRegistry` — 提示注册表

```rust
let mcp_harness = builder.with_mcp()
    .mcp_server("github", github_config)
    .mcp_server("docs", docs_config)
    .resource_injection(policy)
    .lock_file(".mcp-lock.json")
    .build(env)
    .await?;

// 使用 AgentHarness
mcp_harness.harness.run(request).await?;

// 关闭时
mcp_harness.runtime.shutdown_if_owned().await?;
```

---

## 协议版本

| 版本 | 状态 |
|------|------|
| `2025-11-25` | 首选版本（`PREFERRED_PROTOCOL_VERSION`） |
| `2025-06-18` | 接受 |
| `2025-03-26` | 接受 |
| `2024-11-05` | 接受（旧版 SSE 兼容） |

握手时按优先级协商，服务器返回的版本必须在 `SUPPORTED_PROTOCOL_VERSIONS` 列表中。
