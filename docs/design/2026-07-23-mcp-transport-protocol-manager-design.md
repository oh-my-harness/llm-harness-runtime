# MCP Transport, Protocol & Manager Design

**Date**: 2026-07-23  
**Issue**: [#78](https://github.com/oh-my-harness/llm-harness-runtime/issues/78)  
**Scope**: `crates/llm-harness-runtime-mcp` 内部基础设施——传输层 + 协议层 + manager + 配置解析 + 类型定义 + adapter 改造  
**不含**: adapter 内容处理 bug 修复、builder/engine 接线、Senza PyO3 绑定（各自独立 spec）

## 背景

runtime 的 MCP 集成目前是 trait-only 的半成品：`McpClientHandle` trait 只有 `list_tools` + `call_tool` 两个方法，没有传输层、没有协议实现、没有连接管理。`runtime-mcp` crate 在自身单测之外没有调用者。用户无法连接任何真实 MCP server。

本 spec 从零实现 MCP 协议层 + 传输层 + 连接管理，参考 oh-my-pi（TypeScript）的成熟实现，用 Rust 重写。不使用任何 MCP SDK。

## 设计决策

| 决策 | 选择 | 理由 |
|------|------|------|
| 传输层选型 | 从零实现，不用 SDK | runtime 已有自己的 trait 抽象；oh-my-pi 自实现成功；协议层不复杂（~500 行），transport 是工作量大头但可控 |
| 传输层覆盖 | stdio + Streamable HTTP + legacy SSE | 覆盖 MCP 生态全部 transport 类型 |
| 连接管理 | 完整生命周期 | 并行连接、快速启动 gate、deferred fallback、自动重连 + 熔断、工具缓存——对齐 oh-my-pi |
| Crate 结构 | 全部在 `runtime-mcp` | 一个 crate 搞定所有 MCP 相关逻辑 |
| Trait 设计 | 三层分离（Transport → Session → Manager），弃用旧 `McpClientHandle` | 不考虑向后兼容，直接拆分分层 |
| 协议版本 | `2025-11-25` 为首选版本，显式协商兼容版本 | 使用当前正式版；不再假设不同版本天然兼容 |
| 配置格式 | JSON（mcp.json 兼容） | 兼容 Claude Desktop / Cursor / oh-my-pi 生态 |
| `roots/list` | 空列表 stub | 成本极低，避免 server 报错 |
| Adapter 改造 | 依赖从 `McpClientHandle` → `McpSession`/`McpManager` | 旧 trait 删除 |

## 不在范围内

以下 MCP spec 能力**不做**——不在 runtime 的职责范围或 MVP 不需要：

| 能力 | 判断 | 理由 |
|------|------|------|
| `sampling/createMessage` | 不做 | MCP server 向 client 请求 LLM 推理；runtime 本身是 LLM 宿主，自指 |
| `completion/complete` | 不做 | IDE 的 argument autocomplete 职责 |
| `roots/list` 真正实现 | 空列表 stub | 不维护 roots 模型，但响应请求避免 server 报错 |
| `logging` | 不做 | runtime 已有 tracing 基础设施 |
| notifications（工具列表变更） | 不做 | 冲击动态工具模型，复杂度过高 |
| pagination | 实现细节 | 在 session 方法内循环翻页，不作为独立能力 |
| PyO3 绑定 | 独立 spec | 依赖 runtime 侧 MCP 先可用，是 Senza 的职责 |
| builder/engine 接线 | 独立 spec | `.mcp_server()` builder 方法、config 驱动注入 |
| adapter 内容处理 bug | 独立 spec | `McpToolAdapter` 丢弃非 text 内容块 |

## 架构总览

```
mcp.json 配置
      ↓
McpManager (lifecycle: 并行连接/超时/重连/缓存/熔断/deferred)
      ↓
McpSession (protocol: initialize → capabilities → tools/resources/prompts)
      ↓
McpTransport (framing: stdio JSONL / HTTP POST+SSE / legacy SSE)
      ↓
JsonRpc (message types: Request / Response / Notification)
```

## Crate 结构

```
crates/llm-harness-runtime-mcp/
├── Cargo.toml
└── src/
    ├── lib.rs              # adapter 层 + 公共导出
    ├── json_rpc.rs         # JSON-RPC 2.0 消息类型 + 编解码
    ├── transport/
    │   ├── mod.rs          # McpTransport trait + 工厂
    │   ├── stdio.rs        # StdioTransport
    │   ├── http.rs         # HttpTransport (Streamable HTTP)
    │   └── sse.rs          # LegacySseTransport (HTTP+SSE)
    ├── session.rs          # McpSession: 协议会话
    ├── manager.rs          # McpManager: 多 server 生命周期
    ├── config.rs           # mcp.json 配置解析
    └── types.rs            # MCP 协议类型 (capabilities, tool defs, content, etc.)
```

### Cargo.toml 新增依赖

```toml
[dependencies]
# 现有
llm-harness-runtime.workspace = true
llm-harness-types.workspace   = true
anyhow.workspace              = true
futures.workspace             = true
serde_json.workspace          = true
# 新增
serde.workspace               = true
tokio = { workspace = true, features = ["process", "io-util", "net", "time", "sync"] }
tokio-util.workspace          = true
reqwest.workspace             = true
tracing.workspace             = true
thiserror.workspace           = true
url = "2"
```

`reqwest` 已在 workspace（rustls-tls），HTTP transport 复用。`tokio` 需要加 `process`（stdio 子进程）、`io-util`（读写管道）、`net`（HTTP 连接）。

### 旧代码清理

- `McpClientHandle` trait（`runtime/src/platform/mcp.rs`）删除
- `McpToolDef` / `McpResult` / `McpContent` / `McpError`（同文件）删除——由 `runtime-mcp/src/types.rs` 取代
- `runtime/src/platform/mcp.rs` 文件删除
- `runtime/src/platform/mod.rs` 移除 `pub mod mcp;`
- `StubMcpClient`（`runtime-mcp/src/lib.rs` 测试中）删除
- `McpToolSource` / `McpToolAdapter` / `McpPromptSource` 改造（见 Adapter 层章节）

## 1. JSON-RPC 层 (`json_rpc.rs`)

定义 JSON-RPC 2.0 协议消息骨架，被 transport 和 session 共享。

### 类型定义

```rust
/// MCP/JSON-RPC request id。入站必须同时接受 string 和 number；
/// 本客户端生成的出站 id 使用单连接内自增 u64。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    String(String),
    Number(serde_json::Number),
}

/// JSON-RPC 2.0 消息——三种变体。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Response(JsonRpcResponse),
    Notification(JsonRpcNotification),
}

/// 请求——有 id，期望响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,   // 固定 "2.0"
    pub id: RequestId,
    pub method: String,
    #[serde(default = "Value::Null")]
    pub params: Value,
}

/// 响应——匹配请求 id。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    /// 协议/解析错误可能无法关联请求，此时允许 null。
    pub id: Option<RequestId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// 通知——无 id，无需响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default = "Value::Null")]
    pub params: Value,
}

/// JSON-RPC 错误对象。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}
```

### 设计要点

- **ID 类型**：入站接受 MCP 规定的 `string | number`；客户端出站把单连接内
  自增 `u64` 转成 `serde_json::Number`。不需要全局唯一。
- **编解码**：`serde_json` 直接序列化/反序列化。stdio 是 JSONL（每行一个消息），HTTP 是单个 JSON body 或 SSE event。
- **请求关联**：不在此层做——关联是 transport 的职责（维护 `HashMap<id, pending>`）。
- **MCP 语义**：`JsonRpcMessage` 只管消息形状，不含任何 MCP 语义（`tools/list` 等方法名在 session 层使用）。
- **`jsonrpc` 字段**：序列化时固定 `"2.0"`，反序列化时不校验（兼容性宽松）。

### 错误码

标准 JSON-RPC 错误码（参考）：

| 码 | 含义 |
|----|------|
| -32700 | Parse error |
| -32600 | Invalid request |
| -32601 | Method not found |
| -32602 | Invalid params |
| -32603 | Internal error |

## 2. Transport 层 (`transport/`)

### Trait 定义

```rust
/// MCP 传输层抽象——抽象 stdio/http/sse 的帧传输细节。
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// 分配单连接 request id，发送请求并等待匹配响应。
    async fn request(&self, method: &str, params: Value) -> Result<Value>;

    /// 发送通知（无需响应）。
    async fn notify(&self, method: &str, params: Value) -> Result<()>;

    /// initialize 完成版本协商后设置。HTTP 后续请求必须携带
    /// MCP-Protocol-Version；stdio 实现只保存或 no-op。
    fn set_protocol_version(&self, version: &str);

    /// 连接是否可用。
    fn is_connected(&self) -> bool;

    /// 关闭传输。
    async fn close(&self) -> Result<()>;

    /// 注册 server→client 请求回调（roots/list, ping 等）。
    fn on_request(
        &self,
        handler: Arc<
            dyn Fn(JsonRpcRequest) -> BoxFuture<'static, Result<Value>> + Send + Sync
        >,
    );

    /// 注册通知回调（工具列表变更等）。
    fn on_notification(&self, handler: Arc<dyn Fn(JsonRpcNotification) + Send + Sync>);

    /// 注册连接关闭回调。
    fn on_close(&self, handler: Arc<dyn Fn() + Send + Sync>);
    /// 启动后台 SSE listener（仅 HTTP transport 有效，其他实现为 no-op）。
    /// 在 initialize 响应后、notifications/initialized 前调用。
    async fn start_sse_listener(&self) -> Result<()>;
}
```

### 工厂函数

```rust
/// 根据 server 配置创建对应的 transport。
pub async fn create_transport(config: &McpServerConfig) -> Result<Box<dyn McpTransport>> {
    match config {
        McpServerConfig::Stdio { .. } => Ok(Box::new(StdioTransport::connect(config).await?)),
        McpServerConfig::HttpOrSse { .. } if config.is_sse() => {
            Ok(Box::new(LegacySseTransport::connect(config).await?))
        }
        McpServerConfig::HttpOrSse { .. } => {
            Ok(Box::new(HttpTransport::connect(config).await?))
        }
    }
}
```

### 2.1 StdioTransport (`transport/stdio.rs`)

**原理**：newline-delimited JSON over 子进程 stdio。

**状态**：

```rust
pub struct StdioTransport {
    process: Mutex<Option<tokio::process::Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    next_id: AtomicU64,
    connected: AtomicBool,
    // 回调
    on_request: RwLock<Option<Arc<dyn Fn(JsonRpcRequest) -> BoxFuture<'static, Result<Value>> + Send + Sync>>>,
    on_notification: RwLock<Option<Arc<dyn Fn(JsonRpcNotification) + Send + Sync>>>,
    on_close: RwLock<Option<Arc<dyn Fn() + Send + Sync>>>,
    // 配置
    timeout_ms: u64,  // 0 = 禁用
}
```

**生命周期**：

- `connect()`:
  1. spawn 子进程（command + args + env + cwd），设置 `kill_on_drop(true)`
  2. 标记 `connected = true`
  3. 启动 stdout 读循环（`read_jsonl`）
  4. 启动 stderr 循环（读取并丢弃，或 `tracing::debug!`）

- `close()` / `handle_close()`:
  1. 标记 `connected = false`
  2. 拒绝所有 pending 请求（`Transport closed`）
  3. kill 子进程
  4. detach 读循环（不 await，可能 hang）
  5. 触发 `on_close` 回调

- **读循环异常退出**：`finally` 触发 `handle_close()`，执行与主动 close 相同的清理。

**请求关联**：

- 出站请求：序列化为 JSON + `\n`，写入 stdin
- `pending: HashMap<u64, oneshot::Sender>` 存储在途请求
- 读循环解析 JSONL 每行为 `JsonRpcMessage`
- 有匹配 `id` → 解析为 `JsonRpcResponse`，resolve 对应 `oneshot`
- 有 `method` 无 `id` → 通知，调 `on_notification`
- 有 `method` + `id` → server→client 请求，调 `on_request`；无 handler 则回复 `-32601 Method not found`
- 未知 response id → 忽略

**超时**：

- 每个请求 `tokio::time::timeout`（默认 30s，`OMP_MCP_TIMEOUT_MS` 或 config 覆盖，`0` 禁用）
- 超时/abort → reject pending promise，清理 map entry

**背压**：

- `request()` await `stdin.write()` + flush，broken-pipe 失败 reject 请求
- `notify()` 写入并 flush；尤其 `notifications/initialized` 必须保证送达
- 无显式队列或高水位管理
- 入站处理 stream 驱动（`for await` over `read_jsonl`），逐消息处理

**异常处理**：

- 每行 JSONL 在 `try/catch` 中解析，malformed 行丢弃（`tracing::warn!`），循环继续
- 流解析异常 → `on_error`（如果仍 connected），然后 close

### 2.2 HttpTransport (`transport/http.rs`)

**原理**：Streamable HTTP JSON-RPC over POST，可选 SSE 响应/监听。

**状态**：

```rust
pub struct HttpTransport {
    url: String,
    client: reqwest::Client,
    headers: HashMap<String, String>,
    session_id: RwLock<Option<String>>,       // Mcp-Session-Id
    protocol_version: RwLock<Option<String>>,  // 协商后的 MCP-Protocol-Version
    connected: AtomicBool,
    sse_listener_handle: Mutex<Option<JoinHandle<()>>>,
    // 回调（同 StdioTransport）
    on_request: RwLock<Option<...>>,
    on_notification: RwLock<Option<...>>,
    on_close: RwLock<Option<...>>,
    timeout_ms: u64,
    // OAuth refresh
    auth_refresh: Option<Arc<dyn Fn() -> BoxFuture<'static, Result<HashMap<String, String>>> + Send + Sync>>,
}
```

**连接语义**：

- `connect()`：标记 `connected = true`（无 socket/session 握手，HTTP 无状态）
- `connected` 含义："transport 可用"，不是"持久流已建立"

**请求路径**：

- `request()`：POST JSON body，生成 `id`
- 所有 POST 带 `Accept: application/json, text/event-stream`
- initialize 之后的 POST/GET/DELETE 带协商后的 `MCP-Protocol-Version`
- 非 SSE 响应（`Content-Type: application/json`）：解析单个 JSON-RPC response，返回 `result` / throw on `error`
- SSE 响应（`Content-Type: text/event-stream`）：消费 SSE 流，找匹配 `id` 的第一个消息
- SSE 中 `method` + 无 `id` → 通知，调 `on_notification`
- SSE 中 `method` + `id` → server→client 请求，POST 回复 JSON-RPC response
- SSE 流结束前未匹配 → `No response received for request ID ...`

**Session header**：

- POST 响应有 `Mcp-Session-Id` → 存储
- 后续请求/通知带上 `Mcp-Session-Id`
- `close()` 尝试 HTTP DELETE 终止 session；失败忽略

**后台 SSE listener**（可选）：

- GET 请求打开 SSE 流，接收 server 主动通知/请求
- `connectToServer()` 在 `initialize` 后、`notifications/initialized` 前启动（由 session 层控制）
- 启动等待最多 1s（或请求超时极小时缩短）；`timeout: 0` 禁用启动 deadline
- GET 返回 405 / 非 OK / 无 body / 超时 → 静默禁用 listener
- GET listener 正常结束不等于 MCP session 断开，不触发整 session 重连
- 如果 SSE event 带 `id`，保存最后一个 event id；server 按 polling 语义关闭流时，
  按 `retry` 延迟用 `Last-Event-ID` 恢复
- 只有 session 明确失效（例如带 session id 的请求返回 404）、认证失效且刷新失败，
  或底层 transport 被显式关闭时才触发 `on_close`

**超时与取消**：

- `request()`: `tokio::time::timeout`（`OMP_MCP_TIMEOUT_MS` / `config.timeout` / 30s；`0` 禁用）
- 外部 abort signal 合并（如果调用方提供）

**OAuth 401/403 重试**：

- 配置了 `auth_refresh` 回调的 HTTP server
- 请求返回 401/403 → 调 `auth_refresh` 获取新 headers → 重试一次
- 刷新失败 → 原始错误传播

**HTTP 错误传播**：

- 非 2xx：错误包含 `HTTP <status>: <text>`
- 有 `WWW-Authenticate` / `Mcp-Auth-Server` → 追加 auth hints
- JSON-RPC error object → `MCP error <code>: <message>`
- JSON 解析失败 → parse exception 传播

### 2.3 LegacySseTransport (`transport/sse.rs`)

**原理**：MCP protocol revision 2024-11-05 HTTP+SSE。

**生命周期**：

- `connect()`：GET 打开配置 URL（`Accept: text/event-stream`）
- 第一个 `endpoint` event 是控制数据，其 `data` 值解析为 JSON-RPC POST 端点（相对 URL 解析为绝对）
- `request()` / `notify()`：POST JSON-RPC frame 到发现的端点
- JSON-RPC 响应、通知、server→client 请求从 `event: message` 流读取，按 id 关联
- 流结束 → pending 请求失败 `Legacy SSE stream closed` → `on_close` 触发重连

**与 HttpTransport 的区别**：

- 不使用 `Mcp-Session-Id`，不发送 DELETE
- POST 端点从 SSE 流动态发现，不是配置的 URL
- SSE 流是持久的（从 `connect` 到 `close`），不是 per-request

### 2.4 重试/重连职责边界

**Transport 层不做**：

- 重试普通失败请求（HTTP OAuth refresh 单次重试除外）
- stdio 进程退出后重连
- SSE listener 自行重连
- 断连后重发在途请求

Transport 层 fail fast，错误上抛。重连由 manager 层处理。

## 3. Session 层 (`session.rs`)

### McpSession

```rust
pub struct McpSession {
    transport: Box<dyn McpTransport>,
    capabilities: RwLock<Option<McpServerCapabilities>>,
    server_info: RwLock<Option<McpImplementation>>,
    protocol_version: RwLock<Option<String>>,
}
```

### 连接握手

```rust
impl McpSession {
    /// 建立连接：initialize → capabilities 协商 → notifications/initialized
    pub async fn connect(transport: Box<dyn McpTransport>) -> Result<Self> {
        // 1. 注册默认 server→client 请求 handler
        //    - roots/list → 返回 {"roots": []}
        //    - ping → 返回 {}
        transport.on_request(Arc::new(handle_server_request));

        // 2. 发送 initialize 请求
        let init_result = transport.request("initialize", serde_json::json!({
                "protocolVersion": PREFERRED_PROTOCOL_VERSION,  // "2025-11-25"
                "capabilities": {
                    "roots": { "listChanged": false }  // 声明支持 roots/list 但不通知变更
                },
                "clientInfo": { "name": "llm-harness-runtime-mcp", "version": env!("CARGO_PKG_VERSION") }
            })).await?;

        // 3. 校验并存储 server capabilities / info / protocolVersion
        let init: McpInitializeResult = serde_json::from_value(init_result)?;
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&init.protocol_version.as_str()) {
            transport.close().await?;
            return Err(McpError::UnsupportedProtocolVersion(init.protocol_version));
        }
        transport.set_protocol_version(&init.protocol_version);
        // 4. 对于 HTTP transport，此时启动后台 SSE listener
        //    （transport 需要暴露 start_sse_listener() 方法，session 在 init 后调用）
        // 5. 发送 notifications/initialized
        transport.notify("notifications/initialized", json!({})).await?;

        Ok(Self { transport, capabilities: init.capabilities, ... })
    }
}
```

### MCP 方法调用

```rust
impl McpSession {
    // ── Tools ──
    pub async fn list_tools(&self) -> Result<Vec<McpToolDefinition>> {
        self.require_capability(|c| c.tools.is_some())?;
        self.paginate("tools/list", "tools").await
    }

    pub async fn call_tool(&self, name: &str, args: Value) -> Result<McpToolCallResult> {
        self.require_capability(|c| c.tools.is_some())?;
        let result = self.transport.request("tools/call", json!({
            "name": name, "arguments": args
        })).await?;
        Ok(serde_json::from_value(result)?)
    }

    // ── Resources ──
    pub async fn list_resources(&self) -> Result<Vec<McpResource>> {
        self.require_capability(|c| c.resources.is_some())?;
        self.paginate("resources/list", "resources").await
    }

    pub async fn read_resource(&self, uri: &str) -> Result<McpResourceReadResult> {
        self.require_capability(|c| c.resources.is_some())?;
        let result = self.transport.request("resources/read", json!({
            "uri": uri
        })).await?;
        Ok(serde_json::from_value(result)?)
    }

    // ── Prompts ──
    pub async fn list_prompts(&self) -> Result<Vec<McpPrompt>> {
        self.require_capability(|c| c.prompts.is_some())?;
        self.paginate("prompts/list", "prompts").await
    }

    pub async fn get_prompt(&self, name: &str, args: HashMap<String, String>) -> Result<McpGetPromptResult> {
        self.require_capability(|c| c.prompts.is_some())?;
        let result = self.transport.request("prompts/get", json!({
            "name": name, "arguments": args
        })).await?;
        Ok(serde_json::from_value(result)?)
    }

    // ── 生命周期 ──
    pub async fn disconnect(&self) -> Result<()> {
        self.transport.close().await
    }

    pub fn capabilities(&self) -> Option<McpServerCapabilities> {
        self.capabilities.read().clone()
    }
}
```

### 设计要点

- **Capability 检查**：每个方法调用前检查 server 是否声明了对应能力。不支持的方法返回 `MethodNotSupported` 错误，不发请求。避免无谓的网络往返。
- **`roots/list` stub**：在 `connect()` 时注册到 transport 的 `on_request`，匹配 `roots/list` 返回 `{"roots": []}`，匹配 `ping` 返回 `{}`。不维护真正的 roots 模型。
- **Protocol version**：首选 `"2025-11-25"`。server 返回版本必须位于
  `SUPPORTED_PROTOCOL_VERSIONS`；否则关闭连接并返回 `UnsupportedProtocolVersion`。
  不用“忽略差异继续通信”的宽松策略。
- **SSE listener 启动时机**：HTTP transport 的后台 SSE listener 在 `initialize` 响应后、`notifications/initialized` 前启动。这需要 transport 暴露一个 `start_sse_listener()` 方法供 session 调用。
- **Pagination**：`tools/list`、`resources/list`、`prompts/list` 统一循环读取
  `nextCursor`，并设置最大页数保护，避免恶意或错误 server 无限分页。

## 4. Manager 层 (`manager.rs`)

对齐 oh-my-pi 的完整连接生命周期管理。

### 状态模型

```rust
pub struct McpManager {
    /// 完全连接的 server。
    connections: RwLock<HashMap<String, ManagedConnection>>,
    /// 握手中的 server。
    pending_connections: RwLock<HashMap<String, watch::Receiver<ConnectionAttempt>>>,
    /// 工具加载中的 server（connected but tools still loading）。
    pending_tool_loads: RwLock<HashMap<String, JoinHandle<Vec<McpToolDefinition>>>,
    /// 重连中的 server。
    pending_reconnections: RwLock<HashMap<String, JoinHandle<Option<ManagedConnection>>>>,
    /// 原始配置（保存以便重连时重新解析凭据）。
    server_configs: RwLock<HashMap<String, McpServerConfig>>,
    /// 当前暴露的工具视图。
    tools: RwLock<Vec<ExposedTool>>,
    /// 重连历史（用于熔断）。
    reconnect_history: RwLock<HashMap<String, Vec<Instant>>>,
    /// 工具缓存。
    tool_cache: RwLock<McpToolCache>,
}

struct ManagedConnection {
    session: Arc<McpSession>,
    server_name: String,
    source: SourceMeta,
}

struct ExposedTool {
    server_name: String,
    full_name: String, // mcp__{sanitized_server}_{sanitized_tool}
    tool_def: McpToolDefinition,
    /// 活跃连接的 session（None = deferred，需等待连接完成）。
    session: Option<Arc<McpSession>>,
    /// 重连回调（deferred tool 调用时尝试重连）。
    reconnect: Option<Arc<dyn Fn() -> BoxFuture<'static, Option<Arc<McpSession>>> + Send + Sync>>,
}

#[derive(Clone)]
enum ConnectionAttempt {
    Pending,
    Ready(Arc<ManagedConnection>),
    Failed(Arc<McpError>),
}
```

`futures::future::Shared` 要求输出可 `Clone`，而 `anyhow::Error` 不是
`Clone`。因此 pending 连接使用 `watch` 广播可克隆的状态；多个 deferred tool
可以等待同一个连接尝试，不复制底层错误或 future。

### 连接状态

```rust
pub enum ConnectionStatus {
    Connected,
    Connecting,
    Disconnected,
}

impl McpManager {
    pub fn get_connection_status(&self, name: &str) -> ConnectionStatus {
        if self.connections.read().contains_key(name) { Connected }
        else if self.pending_connections.read().contains_key(name)
          || self.pending_tool_loads.read().contains_key(name)
          || self.pending_reconnections.read().contains_key(name) { Connecting }
        else { Disconnected }
    }
}
```

### 4.1 连接建立流程

```rust
impl McpManager {
    /// 发现并连接所有 MCP server。
    pub async fn discover_and_connect(&self, configs: HashMap<String, McpServerConfig>) -> Result<()> {
        // 1. 存储所有 server_configs
        // 2. 对每个 server 调 connect_server()
        // 3. 等待快速启动 gate（250ms）
    }

    /// 连接单个 server。
    async fn connect_server(&self, name: &str, config: &McpServerConfig) {
        // 1. store/update source metadata
        // 2. skip if already connected/pending/reconnecting
        // 3. validate transport fields
        // 4. resolve auth/shell substitutions
        // 5. call create_transport(config) + McpSession::connect(transport)
        // 6. wire transport.on_close → reconnect_server(name)
        // 7. call list_tools(connection)
        // 8. cache tool definitions (best-effort)
        // 9. best-effort load resources/prompts after tools
    }
}
```

### 4.2 快速启动 gate + deferred fallback

```rust
const STARTUP_TIMEOUT_MS: u64 = 250;

impl McpManager {
    async fn connect_servers(&self) -> ConnectResult {
        // spawn per-server connect + list_tools tasks
        let tasks = /* ... */;

        // race: all tasks settled vs 250ms timeout
        tokio::select! {
            _ = wait_all_settled(&tasks) => {
                // all done: fulfilled → live tools, rejected → errors
            }
            _ = tokio::time::sleep(Duration::from_millis(STARTUP_TIMEOUT_MS)) => {
                // gate elapsed:
                // - fulfilled → live tools
                // - rejected → errors
                // - pending:
                //   - cache hit → DeferredTool (from MCPToolCache)
                //   - cache miss → no tools at startup; background continuation
                //     updates manager state when ready; running harness 不动态替换工具
            }
        }
    }
}
```

**混合启动模型**：

- 有缓存的 pending server → 立即返回 deferred tool（可被 agent 调用，调用时等待连接完成）
- 无缓存的 pending server → 启动时不提供工具；后台 continuation 只更新 manager
  状态。后续新 build/显式 refresh 可看到工具，运行中的 harness 不静默换表
- 慢 server 不阻塞启动

### 4.3 后台完成

每个 pending `toolsPromise` 有后台 continuation：

```rust
// 后台 continuation:
// 1. 等待 connect + list_tools 完成
// 2. 替换 manager state 中该 server 的工具切片（replace_server_tools）
// 3. 写入缓存
// 4. 启动后失败只 log（allow_background_logging）
```

动态更新已经运行中的 `AgentHarness` 不属于 manager 自身职责，需要 runtime
提供通用 dynamic-tool contribution API。该 API 完成前，manager 不直接持有或调用
`AgentHarness::set_tools()`。

### 4.4 工具调用

```rust
// 活跃连接的工具 → 通过 session.call_tool() 调用
// DeferredTool → 先 waitForConnection(server)，再调用
// 两者都对可重试的连接错误尝试一次重连 + 单次重试
```

### 4.5 自动重连

```rust
impl McpManager {
    async fn reconnect_server(&self, name: &str) -> Option<ManagedConnection> {
        // 1. 熔断检查：30s 内 >5 次重连 → 暂停，log warning
        // 2. teardown stale connection
        // 3. re-resolve auth/config values（不泄漏已解析的 tokens）
        // 4. retry with backoff: 500ms → 1000ms → 2000ms → 4000ms
        // 5. 成功 → reload tools → 替换 tools 中的工具切片
        // 6. 保留旧工具（stale but visible）直到重连成功
        // 7. 通知 consumers 工具已更新
    }
}
```

**重连 backoff 序列**：`[500, 1000, 2000, 4000]` ms，第 5 次起保持 4000ms。

**熔断**：`reconnect_history` 记录每次重连时间戳。30s 窗口内 >5 次 → 暂停自动重连，需手动 reconnect（`reconnect_server(name)` 可被外部调用重置历史）。

### 4.6 工具缓存

```rust
pub struct McpToolCache {
    cache: RwLock<HashMap<String, Vec<McpToolDefinition>>>,
    cache_path: Option<PathBuf>,  // 持久化路径
}

impl McpToolCache {
    pub fn get(&self, server_name: &str) -> Option<Vec<McpToolDefinition>>;
    pub async fn set(&self, server_name: &str, tools: Vec<McpToolDefinition>);  // best-effort 写盘
    pub async fn load_from_disk(&self) -> Result<()>;  // 启动时加载
    pub async fn flush(&self) -> Result<()>;           // 写盘
}
```

- 连接成功后写入缓存
- 快速启动 gate 时，pending server 如果有缓存 → 创建 deferred tool
- 缓存存储在 runtime 的 state 目录（`~/.omp/agent/mcp-cache.json` 或通过 config 指定）
- 持久化是 best-effort，失败只 log warning

### 4.7 刷新/重载

```rust
impl McpManager {
    /// 添加或替换一个 server 配置并连接。
    pub async fn add_server(&self, name: String, config: McpServerConfig) -> Result<()>;

    /// 重载所有 server：disconnect all → rediscover → connect。
    pub async fn reload_all(&self) -> Result<()>;

    /// 重连单个 server。
    pub async fn reconnect(&self, name: &str) -> Result<()>;

    /// 断开单个 server。
    pub async fn disconnect_server(&self, name: &str) -> Result<()>;

    /// 断开所有 server。
    pub async fn disconnect_all(&self) -> Result<()>;

    /// 同步取消 manager 创建的后台连接/重连/listener task。
    /// Drop 兜底使用；不执行需要 await 的协议级 shutdown。
    pub fn cancel_background_tasks(&self);
}
```

**`disconnect_server(name)`**：

1. 移除 pending entries、source metadata、saved config
2. 移除 resource refresh/subscription 状态
3. detach `on_close`（避免触发重连）
4. close transport（如果 connected）
5. 移除 tools 中 `mcp__${name}_` 前缀的工具

**`disconnect_all()`**：

1. detach 所有活跃 transport 的 `on_close`
2. `Promise.allSettled` 关闭所有 transport
3. 清空 pending maps、sources、configs、connections、subscriptions、tools

`disconnect_server()`、`disconnect_all()` 和
`cancel_background_tasks()` 必须幂等；显式 shutdown、失败清理和 Drop 兜底可以重复调用。

### 4.8 失败模式

| 场景 | 行为 | 严重性 |
|------|------|--------|
| Discovery 异常 | 返回空工具集 + 一个 synthetic error | best-effort session startup |
| 无效 server config | 跳过该 server，记录 validation error | best-effort per server |
| 连接超时/init 失败 | 记录 error，其他 server 继续 | best-effort per server |
| `tools/list` pending + 有缓存 | 立即返回 deferred tools | best-effort fast startup |
| `tools/list` pending + 无缓存 | 启动时不提供工具；后台注册 | best-effort late registration |
| 后台 tool-load 失败 | 启动后 log | best-effort logging |
| 传输断连 | manager 尝试重连；旧工具保留；后续调用可能重试一次或失败 | best-effort automatic recovery |

## 5. 配置层 (`config.rs`)

### mcp.json 格式

兼容 Claude Desktop / Cursor / oh-my-pi 格式：

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
      "env": { "FOO": "bar" },
      "cwd": "/path/to/dir"
    },
    "remote-api": {
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer ..." }
    },
    "legacy-sse": {
      "type": "sse",
      "url": "https://example.com/sse"
    }
  }
}
```

### Rust 类型

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct McpConfigFile {
    #[serde(rename = "mcpServers")]
    pub mcp_servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum McpServerConfig {
    /// Stdio server: spawn 子进程。有 `command` 字段时匹配。
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        timeout: Option<u64>,
    },
    /// HTTP 或 SSE server：有 `url` 字段时匹配。
    /// `type` 字段决定具体 transport：`"sse"` → Sse，其他 → Http。
    HttpOrSse {
        url: String,
        #[serde(default)]
        #[serde(rename = "type")]
        transport_type: Option<String>,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        timeout: Option<u64>,
    },
}

impl McpServerConfig {
    /// 判断是否为 SSE transport。
    pub fn is_sse(&self) -> bool {
        matches!(self, Self::HttpOrSse { transport_type: Some(t), .. } if t == "sse")
    }
}
```

### Transport 类型推断

- 有 `command` 字段 → `Stdio`
- 有 `url` + `"type": "sse"` → `Sse`（`HttpOrSse` 变体的 `transport_type == Some("sse")`）
- 有 `url`（无 `type` 或 `"type": "http"`）→ `Http`（`HttpOrSse` 变体的 `transport_type` 为 `None` 或非 `"sse"`）

`#[serde(untagged)]` 先尝试匹配 `Stdio`（有 `command` 字段），再尝试 `HttpOrSse`（有 `url` 字段）。`HttpOrSse` 是统一变体，通过 `transport_type` 字段在运行时区分 HTTP 和 SSE。工厂函数 `create_transport` 根据 `is_sse()` 选择 `HttpTransport` 或 `LegacySseTransport`。

### 配置加载

```rust
impl McpConfigFile {
    /// 从 JSON 文本解析。
    pub fn from_json(text: &str) -> Result<Self>;

    /// 从文件加载。
    pub async fn from_file(path: &Path) -> Result<Self>;

    /// 从目录发现 mcp.json / .mcp.json。
    pub async fn discover(dir: &Path) -> Result<Option<Self>>;
}
```

## 6. MCP 类型定义 (`types.rs`)

从 oh-my-pi 的 `types.ts` 移植，Rust 化。

### Capabilities

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerCapabilities {
    #[serde(default)]
    pub tools: Option<ToolsCapability>,
    #[serde(default)]
    pub resources: Option<ResourcesCapability>,
    #[serde(default)]
    pub prompts: Option<PromptsCapability>,
    #[serde(default)]
    pub logging: Option<()>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolsCapability {
    #[serde(default)]
    pub list_changed: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResourcesCapability {
    #[serde(default)]
    pub subscribe: bool,
    #[serde(default)]
    pub list_changed: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PromptsCapability {
    #[serde(default)]
    pub list_changed: bool,
}
```

### Implementation Info

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct McpImplementation {
    pub name: String,
    pub version: String,
}
```

### Initialize

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct McpInitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: McpServerCapabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: McpImplementation,
}
```

### Tools

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct McpToolDefinition {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(default, rename = "outputSchema")]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub annotations: Option<McpToolAnnotations>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpToolAnnotations {
    #[serde(default, rename = "readOnlyHint")]
    pub read_only_hint: Option<bool>,
    #[serde(default, rename = "destructiveHint")]
    pub destructive_hint: Option<bool>,
    #[serde(default, rename = "idempotentHint")]
    pub idempotent_hint: Option<bool>,
    #[serde(default, rename = "openWorldHint")]
    pub open_world_hint: Option<bool>,
}
```

### Content Types

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum McpContent {
    Text {
        text: String,
    },
    Image {
        data: String,      // base64
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    Audio {
        data: String,      // base64
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    #[serde(rename = "resource_link")]
    ResourceLink {
        uri: String,
        name: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default, rename = "mimeType")]
        mime_type: Option<String>,
    },
    Resource {
        resource: McpResourceContent,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResourceContent {
    pub uri: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub blob: Option<String>,  // base64
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpToolCallResult {
    pub content: Vec<McpContent>,
    #[serde(default, rename = "structuredContent")]
    pub structured_content: Option<Value>,
    #[serde(default)]
    pub is_error: bool,
}
```

`annotations`、`_meta` 等不影响 runtime 执行的扩展字段允许反序列化并按需保留；
serde 默认忽略未知字段，以便兼容同一协议版本内的向后兼容扩展。

### Resources

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct McpResource {
    pub uri: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpResourceReadResult {
    pub contents: Vec<McpResourceContentItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpResourceContentItem {
    pub uri: String,
    #[serde(default)]
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub blob: Option<String>,
}
```

### Prompts

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct McpPrompt {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub arguments: Vec<McpPromptArgument>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpPromptArgument {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpGetPromptResult {
    #[serde(default)]
    pub description: Option<String>,
    pub messages: Vec<McpPromptMessage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpPromptMessage {
    pub role: String,  // "user" | "assistant"
    pub content: McpPromptContent,
}

/// MCP 2025-11-25 中 prompt message 和 tool result 共享 ContentBlock union。
pub type McpPromptContent = McpContent;
```

### Notification Methods

```rust
pub mod notification_methods {
    pub const TOOLS_LIST_CHANGED: &str = "notifications/tools/list_changed";
    pub const RESOURCES_LIST_CHANGED: &str = "notifications/resources/list_changed";
    pub const RESOURCES_UPDATED: &str = "notifications/resources/updated";
    pub const PROMPTS_LIST_CHANGED: &str = "notifications/prompts/list_changed";
    pub const LOGGING: &str = "notifications/message";
}
```

## 7. Adapter 层改造 (`lib.rs`)

### 旧代码清理

- `McpClientHandle` trait 删除
- `McpToolDef` / `McpResult` / `McpContent` / `McpError`（旧类型）删除
- `StubMcpClient` 删除

### 改造后的 adapter

```rust
use std::sync::Arc;
use llm_harness_runtime::platform::resource::{ResourceContent, ResourceDescriptor, ResourceProvider};
use llm_harness_runtime::platform::prompt::{PromptSource, PromptSourceTemplate};

/// 从 McpManager 获取工具，适配为 `Tool` trait 对象。
pub struct McpToolSource {
    manager: Arc<McpManager>,
}

impl McpToolSource {
    pub fn new(manager: Arc<McpManager>) -> Self {
        Self { manager }
    }

    /// 获取所有已连接 server 的工具，适配为 McpToolAdapter。
    pub async fn into_adapters(&self) -> Result<Vec<McpToolAdapter>> {
        let tools = self.manager.get_tools();
        // 将 ExposedTool 转为 McpToolAdapter
        // ...
    }
}

/// 单个 MCP 工具适配器。
pub struct McpToolAdapter {
    session: Arc<McpSession>,
    tool_name: String,
    description: String,
    parameters_schema: Value,
}

impl Tool for McpToolAdapter {
    // name() / description() / parameters_schema() 不变
    // execute(): dispatch 到 session.call_tool()，内容处理逻辑
    // 在 adapter-content-fix spec 中修复（保留所有 content block）
    // 此处做最小适配：新 McpContent enum 替换旧 flat struct，
    // execute() 暂时只提取 text 变体，非 text 降级为 "[Image: ...]" / "[Resource: ...]"
    // （即 adapter-content-fix spec 的内容，此处直接实现）
}

/// MCP prompt 源——从 McpSession 发现 prompt 模板。
pub struct McpPromptSource {
    session: Arc<McpSession>,
}

impl PromptSource for McpPromptSource {
    fn discover(&self) -> BoxFuture<'_, Result<Vec<PromptSourceTemplate>>> {
        // 调 session.list_prompts()
        // 转换 McpPrompt → PromptSourceTemplate
    }
}

/// MCP resource provider——桥接 resources/list + resources/read。
pub struct McpResourceProvider {
    session: Arc<McpSession>,
}

impl ResourceProvider for McpResourceProvider {
    fn list(&self) -> BoxFuture<'_, Result<Vec<ResourceDescriptor>>> {
        // 调 session.list_resources()
        // 转换 McpResource → ResourceDescriptor
    }

    fn read(&self, uri: &str) -> BoxFuture<'_, Result<ResourceContent>> {
        // 调 session.read_resource(uri)
        // 转换 McpResourceContentItem → ResourceContent
    }
}
```

### `McpPromptSource::discover()` 实现细节

```rust
impl PromptSource for McpPromptSource {
    fn discover(&self) -> BoxFuture<'_, Result<Vec<PromptSourceTemplate>>> {
        let session = self.session.clone();
        Box::pin(async move {
            let prompts = session.list_prompts().await?;
            // 每个 McpPrompt 转为 PromptSourceTemplate：
            // - name → name
            // - description → description
            // - arguments → arguments (ArgumentDef)
            // - template: 需要 prompts/get 获取内容
            //   但 prompts/get 需要参数，discover 时不传参数
            //   方案：只注册 prompt 的元信息，template 内容延迟到 invoke 时获取
            //   或者：对无参数的 prompt，立即 get 获取 template
            Ok(prompts.into_iter().map(|p| {
                PromptSourceTemplate {
                    name: p.name,
                    description: p.description,
                    arguments: p.arguments.into_iter().map(|a| ArgumentDef {
                        name: a.name,
                        description: a.description,
                    }).collect(),
                    template: String::new(),  // 延迟获取，见下文
                }
            }).collect())
        })
    }
}
```

**注意**：MCP prompts 的 `prompts/get` 返回的是消息列表（`messages: Vec<McpPromptMessage>`），不是模板字符串。这与 runtime 的 `PromptSourceTemplate`（期望 `{{param}}` 占位符的模板字符串）语义不同。这需要在 builder 接线 spec 中解决——`McpPromptSource` 可能需要特殊处理，不经过 `PromptTemplateCompiler`。本 spec 只负责桥接 `list_prompts`，template 内容的转换留到接线 spec。

### `McpResourceProvider` 实现细节

```rust
impl ResourceProvider for McpResourceProvider {
    fn list(&self) -> BoxFuture<'_, Result<Vec<ResourceDescriptor>>> {
        let session = self.session.clone();
        Box::pin(async move {
            let resources = session.list_resources().await?;
            Ok(resources.into_iter().map(|r| ResourceDescriptor {
                uri: r.uri,
                name: r.name,
                description: r.description,
                mime_type: r.mime_type.unwrap_or_default(),
            }).collect())
        })
    }

    fn read(&self, uri: &str) -> BoxFuture<'_, Result<ResourceContent>> {
        let session = self.session.clone();
        let uri = uri.to_owned();
        Box::pin(async move {
            let result = session.read_resource(&uri).await?;
            // 取第一个 content item
            let item = result.contents.into_iter().next()
                .ok_or_else(|| anyhow!("empty resource"))?;
            if let Some(text) = item.text {
                Ok(ResourceContent::Text(text))
            } else if let Some(blob) = item.blob {
                Ok(ResourceContent::Blob(base64::decode(&blob)?))
            } else {
                Err(anyhow!("resource has no text or blob content"))
            }
        })
    }
}
```

## 8. 协议版本

规范基线：

- [MCP 2025-11-25 specification](https://modelcontextprotocol.io/specification/2025-11-25)
- [MCP versioning and negotiation](https://modelcontextprotocol.io/docs/learn/versioning)

```rust
pub const PREFERRED_PROTOCOL_VERSION: &str = "2025-11-25";

/// 按优先级排列。只有经过本项目兼容性测试的版本才能加入。
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];
```

- **首选版本**：初始化请求发送当前正式版 `"2025-11-25"`。
- **协商**：server 返回同版本时直接使用；返回其他版本时，仅当它位于
  `SUPPORTED_PROTOCOL_VERSIONS` 才继续，否则断开。
- **HTTP header**：initialize 之后的所有 HTTP 请求都带协商后的
  `MCP-Protocol-Version`。
- **兼容承诺**：旧版本不是“天然兼容”。每个兼容版本至少通过握手、
  tools/list、tools/call、resources 和 prompts 的契约测试后才能进入支持列表。
- **Legacy SSE**：保留 `2024-11-05` 是为了兼容仍使用旧 HTTP+SSE transport
  的 server；新 server 默认使用 Streamable HTTP。

## 9. 超时配置

```rust
pub fn resolve_timeout_ms(env_override: Option<&str>, config_timeout: Option<u64>) -> u64 {
    // 1. OMP_MCP_TIMEOUT_MS 环境变量
    if let Some(env) = env_override {
        if let Ok(ms) = env.parse::<u64>() {
            return ms;  // 0 = 禁用
        }
        // 无效值忽略，log warning
    }
    // 2. config.timeout
    if let Some(ms) = config_timeout {
        return ms;
    }
    // 3. 默认 30s
    30000
}
```

环境变量名沿用 `OMP_MCP_TIMEOUT_MS` 以保持生态一致。`0` 禁用客户端超时。

## 10. 测试策略

### 单元测试

| 层 | 测试方式 | 覆盖 |
|----|---------|------|
| `json_rpc.rs` | 直接序列化/反序列化 | string/number request id、消息类型编解码、untagged 枚举、错误码 |
| `transport/` | Mock `McpTransport` impl（内存 channel） | 请求关联、超时、通知 flush、回调、close |
| `session.rs` | Mock transport | 握手序列、版本接受/拒绝、capability 检查、分页、方法调用 |
| `manager.rs` | Mock session/transport | 并行连接、gate 超时、deferred、重连 backoff、熔断 |
| `config.rs` | 直接解析 JSON | 三种 config 类型推断、缺失字段默认值 |
| `types.rs` | 直接反序列化样本 JSON | MCP 协议类型覆盖 |

### 集成测试

| 测试 | 方式 | 覆盖 |
|------|------|------|
| stdio transport | spawn 一个 echo MCP server（用 Rust 写一个最小 MCP server 二进制，或用 `npx` 启动 `@modelcontextprotocol/server-everything`） | 完整握手 → tools/list → tools/call |
| HTTP transport | 同上但 HTTP 模式 | Streamable HTTP、Accept/version/session headers、SSE polling/resume |
| manager 多 server | 两个 stdio server | 并行连接、一个失败不影响另一个、deferred tool |
| 协议兼容矩阵 | 各版本 fixture/minimal server | `2025-11-25` 首选；每个兼容版本完成握手和 tools/resources/prompts 契约 |

### 不做 mock 的原则

- transport 测试用真实子进程或真实 HTTP server（本地启动），不用 mock 网络
- manager 测试可以用 mock transport（因为测试的是生命周期管理，不是传输正确性）

## 11. 实现阶段

本 spec 的实现按以下阶段顺序执行，每个阶段可独立验证：

1. **类型层**：`types.rs` + `json_rpc.rs` → 类型定义和编解码测试
2. **Config 层**：`config.rs` → mcp.json 解析测试
3. **Transport 层**：`transport/mod.rs` + `stdio.rs` → stdio transport + 单元测试 + 集成测试
4. **Transport 层**：`http.rs` + `sse.rs` → HTTP/SSE transport + 集成测试
5. **Session 层**：`session.rs` → 握手 + 方法调用 + 单元测试
6. **Manager 层**：`manager.rs` → 并行连接 + gate + deferred + 重连 + 缓存 + 熔断
7. **Adapter 改造**：`lib.rs` → 删除旧代码、改造 adapter 依赖 `McpSession`/`McpManager`
8. **清理**：删除 `runtime/src/platform/mcp.rs`，更新 `mod.rs`
