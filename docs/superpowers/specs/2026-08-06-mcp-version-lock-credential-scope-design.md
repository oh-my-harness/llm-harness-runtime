# MCP 服务器版本锁定与最小权限凭证管理

**Date**: 2026-08-06
**Scope**: `crates/llm-harness-runtime-mcp/`（机制层）+ `crates/llm-harness-strategy/`（策略层）
**来源**: ai-agent-book-layer-analysis.md view 17 改进点 4、5
**教程依据**: ai-agent-book ch4 line 147-151

## 背景

教程 ch4 line 147-151 提出 MCP 信任模型的四类风险，其中两条缓解原则需要 runtime 机制层支持：

1. **锁定服务器版本**，拒绝静默更新，升级时重新审查（改进点 4，机制层）
2. 为每个服务器配置**最小权限凭证**——只授最小范围、设有效期、不复用高权限个人凭证（改进点 5，机制层）

### runtime 现状

| 维度 | 现状 | 缺口 |
|------|------|------|
| 协议版本 | `SUPPORTED_PROTOCOL_VERSIONS` 协商 + `UnsupportedProtocolVersion` 拒绝 | 这是 MCP 协议版本，不是服务器实现版本 |
| 服务器身份 | `McpInitializeResult.server_info` = `{name, version}` 已解析并存储在 `McpSession.server_info` | 存了但零校验——不锁定、不比对、不告警 |
| 工具定义 | `tools/list` 返回后直接 `replace_server_tools` 全量暴露 | 收到 `notifications/tools/list_changed` 就静默刷新，无版本比对、无审查 |
| 凭证 | `McpServerConfig` 只有 `env: HashMap<String,String>` 和 `headers: HashMap<String,String>` | 无凭证作用域、无有效期、无来源标注；env/headers 原样传给子进程/HTTP |

**关键风险点**：当前 `tools/list_changed` 通知触发 `replace_server_tools` 静默刷新工具列表（manager.rs:654-676）——服务器可在握手后推送 `tools/list_changed`，runtime 毫无防备地接受新工具定义（可能含投毒 description）。这正是教程说的"静默更新"风险的实际发生点。

## 设计决策

| 决策点 | 选择 | 理由 |
|--------|------|------|
| 版本锁定范围 | 服务器实现版本 + 工具定义快照双重锁定 | server_info.version 防服务器升级，工具快照防运行时工具定义变更 |
| 不匹配策略 | 可配置：默认 fail-closed，用户可按变更类型调策略 | 符合 view 4 "故障安全默认值"原则 |
| 凭证作用域 | 服务器级默认 + 可选工具白名单 | 平衡安全性与复杂性 |
| 凭证来源 | 引用式（`${ENV:...}` / `vault://...`），值不落盘 | 凭证不出现在配置文件中 |
| 锁定声明 | 首次连接自动锁定到本地锁文件 | 用户无需手写快照 |
| 架构分层 | 机制层（runtime-mcp）+ 策略层（llm-harness-strategy）| 机制层提供数据与拦截点，策略层提供默认策略实现 |

## 一、版本锁定

### 1.1 锁文件

新增 `.mcp-lock.json`，与 `.mcp.json` 同目录发现。结构：

```json
{
  "version": 1,
  "servers": {
    "github": {
      "server_info": { "name": "github-mcp-server", "version": "1.2.3" },
      "protocol_version": "2025-11-25",
      "tools_snapshot": [
        {
          "name": "search_repos",
          "description_hash": "sha256:abc123...",
          "input_schema_hash": "sha256:def456..."
        }
      ]
    }
  }
}
```

**设计要点**：
- 不存完整工具定义，只存 hash——锁文件是安全校验手段，不是工具缓存（那是 `McpToolCache` 的职责）。避免重复存储和同步问题。
- `description_hash` + `input_schema_hash` 分开——工具描述投毒（改 description）和参数变更（改 schema）是不同风险类型，diff 时需区分。
- 锁文件路径可配置（builder API），默认 `.mcp-lock.json`。

### 1.2 握手时版本锁定

`McpSession::connect()` 握手成功后，`McpManager` 接管校验逻辑：

```
connect() → initialize() → 拿到 server_info + protocol_version
                                    ↓
                        McpManager::verify_and_lock()
                                    ↓
                    ┌─ 锁文件无该服务器记录 → 首次连接，写入快照
                    │
                    └─ 锁文件有记录 → 比对：
                        ├─ server_info.version 匹配？
                        ├─ protocol_version 匹配？
                        └─ tools/list 快照比对 → 见 1.3
```

### 1.3 运行时工具定义变更锁定

当前 `wire_session` 注册了 `notifications/tools/list_changed` 回调（manager.rs:654-676），收到通知就 `session.list_tools()` + `replace_server_tools()` 静默刷新。

改为：

```
tools/list_changed 通知
        ↓
session.list_tools() 取新工具定义
        ↓
与锁文件快照 diff：
  ├─ 新增工具（added）              → 变更类型 A
  ├─ 删除工具（removed）            → 变更类型 R
  ├─ description 变更（modified_desc）   → 变更类型 D（投毒风险）
  └─ input_schema 变更（modified_schema） → 变更类型 S（参数变更）
        ↓
按策略决定：
  ├─ fail_closed（默认）→ 丢弃新工具列表，保留旧快照，告警
  ├─ allow_added        → 接受新增，其余类型 fail_closed
  └─ allow_all          → 接受全部变更，更新锁文件（需显式 opt-in）
        ↓
若接受变更 → replace_server_tools + 更新锁文件
若拒绝变更 → 保留旧工具列表，记录告警
```

### 1.4 配置接口

`.mcp.json` 新增可选字段：

```json
{
  "mcpServers": {
    "github": {
      "command": "...",
      "version_policy": {
        "lock_file": ".mcp-lock.json",
        "on_mismatch": "fail_closed",
        "on_tool_change": "fail_closed"
      }
    }
  }
}
```

- `on_mismatch`（版本不匹配）：`fail_closed`（默认）| `allow_with_warning` | `allow_and_update`
- `on_tool_change`（工具定义变更）：`fail_closed`（默认）| `allow_added` | `allow_all`

Builder API 对应：`builder.version_policy(server, VersionPolicy { ... })`

未声明 `version_policy` 时，默认行为：
- `on_mismatch` = `fail_closed`
- `on_tool_change` = `fail_closed`
- `lock_file` = `.mcp.json` 同目录下 `.mcp-lock.json`

## 二、凭证管理

### 2.1 凭证引用语法

`McpServerConfig` 的 `env` 和 `headers` 值支持三种引用形式，`create_transport` 前由 `CredentialResolver` 解析：

| 语法 | 含义 | 示例 |
|------|------|------|
| `${ENV:VAR_NAME}` | 从环境变量读取 | `${ENV:GITHUB_TOKEN}` |
| `${ENV:VAR_NAME:-default}` | 带默认值 | `${ENV:API_KEY:-fallback}` |
| `vault://path/to/secret` | 从 vault 读取（机制层定义 trait，具体后端由用户注入） | `vault://prod/github/token` |
| 不含引用的纯文本 | 原样使用（向后兼容当前行为） | `application/json` |

**设计要点**：
- 纯文本向后兼容——现有 `.mcp.json` 的 `env: {"FOO": "bar"}` 继续工作。
- `vault://` 不内建任何具体后端——机制层定义 `CredentialProvider` trait，用户提供实现。runtime 只负责调用 trait 拿值。
- 解析在 `create_transport` 之前、`McpManager::start_connection` 内部完成——凭证值只存在于 transport 的运行时内存中，不写入锁文件、不写入工具缓存、不落盘。

### 2.2 CredentialProvider trait

```rust
/// Mechanism-layer trait: resolve a credential reference to its value.
/// Concrete backends (env, file, vault) are provided by the consumer.
pub trait CredentialProvider: Send + Sync {
    /// Resolve a credential reference. Returns None if not found.
    fn resolve(&self, reference: &str) -> BoxFuture<'_, Option<String>>;
}
```

- 机制层提供 `EnvCredentialProvider` 作为默认实现（解析 `${ENV:...}`）。
- `vault://` 前缀的引用交给用户注入的 `CredentialProvider` 处理。如果未注入，`vault://` 引用解析失败 → 连接被拒（fail-closed）。
- `McpManager::new()` 接受 `Option<Arc<dyn CredentialProvider>>`，默认 `EnvCredentialProvider`。

### 2.3 凭证作用域声明

`.mcp.json` 新增可选的 `credentials` 字段，与 `env`/`headers` 并列：

```json
{
  "mcpServers": {
    "github": {
      "command": "...",
      "env": { "GITHUB_TOKEN": "${ENV:GITHUB_TOKEN}" },
      "credentials": {
        "scoped_env": {
          "GITHUB_TOKEN": {
            "value": "${ENV:GITHUB_TOKEN}",
            "allowed_tools": ["search_repos", "get_file_contents"]
          }
        }
      }
    }
  }
}
```

**设计要点**：
- `credentials.scoped_env` 与 `env` 互斥管理——同一个 key 不能同时出现在两处（validate 时检查）。`env` 是无作用域的凭证（所有工具可用），`credentials.scoped_env` 是有限作用域的凭证。
- `allowed_tools` 列出该凭证可用于的工具名（`sanitize_name` 前的原始 `tool_def.name`）。未列出的工具调用不会获得该凭证值。
 - 机制层职责：解析声明、存储作用域映射、在 `McpToolAdapter` 上暴露 `authorized_credentials` 供策略层检查。不决定"白名单外的工具调用怎么办"——那是策略。
 
 **`authorized_credentials` 数据通路**：`McpManager` 在 `ManagerInner` 中持有 `credential_scopes: RwLock<HashMap<String, HashMap<String, Vec<String>>>>`（server_name → credential_key → allowed_tools）。`McpToolSource::into_adapters()`（lib.rs:43-55）构造 `McpToolAdapter` 时，从 `manager` 查询该服务器下所有声明了 `allowed_tools` 的凭证，反向计算当前 `full_name`（`sanitize_name` 后）对应的原始 `tool_def.name` 是否在各凭证的 `allowed_tools` 中，将命中的凭证 key 列表填入 `authorized_credentials`。

### 2.4 作用域强制——机制层拦截点与策略层 hook

机制层在 `McpToolAdapter` 上暴露凭证作用域信息：

```rust
pub struct McpToolAdapter {
    full_name: String,
    description: String,
    parameters_schema: Value,
    manager: Arc<McpManager>,
    // 新增：该工具被授权使用的凭证 key 列表
    authorized_credentials: Vec<String>,
}
```

策略层 `McpCredentialGuardHook`（`BeforeToolCallHook`，放在 `llm-harness-strategy`）：
- 从 `ToolContext` 取到 `McpToolAdapter` 的 `authorized_credentials`。
- 检查当前调用的工具是否在所有已声明作用域的凭证白名单内。
- 默认行为：**告警**（不 Deny）——当工具调用不在任何已声明凭证的 `allowed_tools` 内时，记录告警日志。
- 高安全场景：用户可自定义 hook 实现 Deny 策略。

**关键取舍**：MCP 协议下，凭证通过 env/headers 传给服务器进程，不是按工具调用传递的。服务器进程拿到 env 后，所有工具理论上都能访问。真正的按工具凭证隔离需要按工具粒度拆分 transport 连接，代价过高。务实方案是机制层记录声明 + 策略层告警/可选 Deny——"检测而非阻断"，因为 MCP transport 级别的按工具凭证隔离需要服务器配合。

## 三、数据流与错误处理

### 3.1 整体数据流

```
.mcp.json + .mcp-lock.json
        ↓
McpManager::start_connection
        ↓
CredentialResolver 解析 env/headers 引用
        ↓
凭证引用解析成功?
  ├─ 否 → 拒绝连接: CredentialUnresolved
  └─ 是 → create_transport + McpSession::connect
        ↓
initialize 握手 → server_info + protocol_version
        ↓
VersionLock::verify_and_lock
  ├─ 锁文件无记录 → 首次连接: 写入快照
  └─ 锁文件有记录 → 版本匹配?
      ├─ 否 → on_mismatch 策略
      │   ├─ fail_closed → 拒绝连接: VersionMismatch
      │   ├─ allow_with_warning → 告警 + 继续
      │   └─ allow_and_update → 更新锁文件 + 继续
      └─ 是 → tools/list 快照比对
          ↓
      工具定义变更?
        ├─ 无变更 → 暴露工具
        └─ 有变更 → on_tool_change 策略
            ├─ fail_closed → 保留旧工具列表 + 告警
            ├─ allow_added → 接受新增 + 拒绝其余 + 更新锁文件
            └─ allow_all → 接受全部 + 更新锁文件
        ↓
replace_server_tools: 暴露工具
        ↓
wire_session: 注册 tools/list_changed 回调
        ↓
运行中: tools/list_changed 通知 → 重新走 diff + 策略路径
```

### 3.2 错误类型

新增错误变体到 `McpManagerError`：

```rust
#[derive(Debug, Error)]
pub enum McpManagerError {
    // ... 现有变体 ...

    #[error("MCP server '{server}' version mismatch: expected {expected}, got {actual}")]
    VersionMismatch {
        server: String,
        expected: String,
        actual: String,
    },

    #[error("MCP server '{server}' credential reference '{reference}' could not be resolved")]
    CredentialUnresolved {
        server: String,
        reference: String,
    },

    #[error("MCP server '{server}' tool definition changed: {summary}")]
    ToolDefinitionChanged {
        server: String,
        summary: String,
    },
}
```

`VersionMismatch` 和 `ToolDefinitionChanged` 在 `fail_closed` 策略下作为连接错误向上传播——`McpManager::start_connection` 返回 `Err`，该服务器不进入 `connections`，工具不暴露。与现有连接失败行为一致（manager.rs:634-637 的 `Failed` 路径）。

### 3.3 锁文件并发与持久化

- 锁文件读写通过 `tokio::fs` 异步操作，与 `McpToolCache` 的持久化模式一致。
- 写入时机：首次连接写入快照、`allow_and_update`/`allow_added`/`allow_all` 策略下接受变更后更新。不做 debounce——锁文件写入频率低（仅连接和变更时），无需 `McpToolCache` 的 flush 任务机制。
- 并发：多服务器同时连接时各自写自己的 key，用 `RwLock<McpLockFile>` 保护内存中的锁文件结构，落盘时整体序列化。

### 3.4 凭证解析失败的处理

- `${ENV:VAR}` 解析不到且无默认值 → `CredentialUnresolved`，连接被拒（fail-closed）。
- `vault://` 引用但未注入 `CredentialProvider` → `CredentialUnresolved`，连接被拒。
- 纯文本值（无引用语法）→ 原样使用，不经过 `CredentialResolver`。

## 四、模块边界

| 模块 | 位置 | 职责 |
|------|------|------|
| `VersionLock` | `runtime-mcp` 机制层 | 锁文件读写、版本比对、工具定义 diff |
| `CredentialResolver` + `CredentialProvider` trait | `runtime-mcp` 机制层 | 引用解析、trait 定义、`EnvCredentialProvider` 默认实现 |
| `McpCredentialGuardHook` | `llm-harness-strategy` 策略层 | 凭证作用域检查（默认告警，可自定义 Deny） |
| `VersionPolicy` 配置 | `.mcp.json` + builder API | on_mismatch / on_tool_change 策略声明 |
| `credentials.scoped_env` 声明 | `.mcp.json` | 凭证值引用 + allowed_tools 白名单 |

## 五、不做的项

- 按工具粒度拆分 transport 连接（MCP transport 级凭证隔离）——代价过高，需要服务器配合
- 凭证有效期自动轮转——凭证值来自外部（env/vault），有效期由来源系统管理
- 凭证使用审计日志——属策略层审计 hook 范畴，view 17 未标记
