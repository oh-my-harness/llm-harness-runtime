# 基础设施 Crate 参考

> 本文档覆盖 7 个基础设施 crate——它们是编排层之下的基础类型、平台 trait 和后端实现。上层 crate（subagents/workflow、agent、loop、strategy）有各自的文档。

---

## llm-harness-types

核心类型与 trait 定义 crate。不依赖任何内部 crate，是整个栈的根基。

### 模块

| 模块 | 关键导出 |
|------|---------|
| `tool` | `Tool` trait、`ToolContext`、`ToolResult`、`ToolFailure`、`ToolLevel`、`ToolExecutionMode`、`ToolSessionProjection`、`Tool::report_duration()` |
| `content` | `ContentBlock`（含 `Document` 文档块）、`DataBlock`、`ImageSource`（`Base64`/`Url`）、`BinarySource`（`Blob`/`Url`） |
| `hooks` | `BeforeToolCallHook`、`AfterToolCallHook`（含 `duration` 字段）、`BeforeTurnHook`、`AfterTurnHook`、`ShouldStopHook`、`TransformContextHook`、`PrepareNextTurnHook`、`BeforeRunHook`、`AfterRunHook`、`OnAbortHook`、`BeforeCompactHook`、`BeforeProviderRequestHook`、`AfterProviderResponseHook` |
| `messages` | `AgentMessage`、`UserMessage`、`AssistantMessage`、`ContentBlock`、`DataBlock` |
| `env` | `ExecutionEnv` trait、`ShellOptions`、`ShellOutput`、`EnvError`、`UnsupportedEnv` |
| `run` | `RunContext`、`RunRequest`、`RunExtensions`、`RunId` |
| `cost` | `CostAggregate`、`ModelCost`、`PricingProvider`、`TokenPrice`、`BudgetExceededHook` |
| `audit` | `AuditEntry`、`AuditSink` trait、`AuditEventType`、`AuditDecision` |
| `event_stream` | `EventStream` trait、`Event` |
| `resources` | `ResourceProvider` trait、`ResourceContent`、`ResourceDescriptor` |
| `skills` | `Skill`、`SkillMetadata` |
| `compaction` | `CompactionError`、`CompactionSummaryMessage` |
| `events` | `AgentEvent`、`AgentError` |
| `identity` | 身份与凭证相关类型 |

### Hook 合成语义

| 类型 | 语义 | Hook 示例 |
|------|------|----------|
| All-Execute（观测型） | 所有 hook 依次执行，返回第一个 hook 的结果 | `BeforeProviderRequestHook`、`AfterProviderResponseHook`、`AfterTurnHook`、`AfterRunHook` |
| First-Non-Allow（决策型） | 顺序执行，第一个返回 `Allow` 以外的决策立即短路 | `BeforeToolCallHook`（`Allow`/`Deny`/`Modify`） |
| Chain（链式修改型） | 按序执行，每个 hook 看到前一个 hook 的修改结果 | `AfterToolCallHook`（`Passthrough`/`Patch`） |

`AfterRunHook` 在 run_loop 结束、harness 回到 Idle 后调用（可 spawn 等待 task，如等子 agent 完成后 `continue_run`）；`OnAbortHook` 在 `harness.abort()` 时先于 `AfterRunHook` 调用（非阻塞、幂等），供等待方置标志跳过。

---

## llm-harness-platform

横切 trait 定义 crate。后端 crate（sandbox、audit-jsonl、trace-otel）依赖此 crate 实现 trait。

### 模块

| 模块 | 关键导出 | 说明 |
|------|---------|------|
| `sandbox` | `Sandbox` trait、`SandboxConfig`、`NetRule`、`ResourceLimits` | 沙箱抽象：start/env/reset/shutdown；Bwrap 后端支持 cgroup v2 CPU/进程/内存上限 |
| `audit` | re-export from `types` | 审计 trait |
| `tracer` | `TraceExporter` trait、`SpanEvent`、`SpanId`、`TraceId`、`SpanKind`、`SpanStatus` | 追踪导出抽象 |
| `tracing_adapter` | `SpanContextMap` | 把 core hooks 桥接为 span 事件 |
| `prompt` | `PromptSource` trait、`PromptSourceTemplate`、`ArgumentDef` | 提示模板源 |
| `env` | 环境辅助 | 环境抽象辅助 |

> 消费者直接依赖 `llm-harness-platform` 访问这些模块（编排层 crate 不再 re-export）。

---

## llm-harness-sandbox-remote

远程沙箱协议客户端，实现 `Sandbox` / `ExecutionEnv` 到远程 gateway 的适配。它负责 HTTPS/Bearer 认证、guest path 校验、JSON 控制面、文件操作、SSE 流式 shell、abort/timeout 上限和错误映射。

### 关键类型

| 类型 | 说明 |
|------|------|
| `RemoteSandboxClientConfig` | 已验证的 gateway endpoint、token、超时和响应大小限制；`Debug` 隐藏 token |
| `RemoteSandboxClient` | HTTP/SSE 协议客户端 |
| `RemoteSandbox` | `Sandbox` 实现，持有 gateway 返回的 sandbox 生命周期 |
| `RemoteEnv` | `ExecutionEnv` 实现，暴露远程文件和 shell 操作 |

协议、生产 gateway 要求和当前边界见 [remote-sandbox-backend.md](design/2026-09-15-remote-sandbox-backend.md)。

---

## llm-harness-audit-jsonl

JSONL 文件审计后端，实现 `AuditSink` trait。

### 关键类型

| 类型 | 说明 |
|------|------|
| `JsonlAuditSink` | JSONL 文件审计 sink，每条记录一行 JSON |
| `JsonlAuditSink::validate(path)` | 校验哈希链完整性，返回有效条目数 |

### 哈希链

每条 `AuditEntry` 写为一行 JSON。哈希计算方式：
- `hash_0 = SHA256(json_without_hash_field)` — 第一条
- `hash_n = SHA256(json_without_hash_field + ":" + prev_hash)` — 后续

篡改可通过 `validate()` 检测。

```rust
use llm_harness_audit_jsonl::JsonlAuditSink;
use std::sync::Arc;

let sink = Arc::new(JsonlAuditSink::new("audit.jsonl"));

// 校验
let valid = JsonlAuditSink::validate(std::path::Path::new("audit.jsonl")).await?;
```

---

## llm-harness-trace-otel

OpenTelemetry 追踪后端，实现 `TraceExporter` trait。

### 关键类型

| 类型 | 说明 |
|------|------|
| `InMemoryTraceExporter` | 内存追踪导出器，用于测试 |
| `OtlpTraceExporter` | OTLP HTTP 导出器，发送到 Jaeger/Tempo 等收集器 |
| `OtlpConfig` | OTLP 配置 |

### OTLP 配置

| 字段 | 默认值 | 说明 |
|------|--------|------|
| `endpoint` | `http://localhost:4318/v1/traces` | OTLP HTTP 端点 |
| `service_name` | `llm-harness` | 服务名 |
| `timeout` | 10s | 导出超时 |

```rust
use llm_harness_trace_otel::{OtlpTraceExporter, OtlpConfig};

let exporter = OtlpTraceExporter::new(&OtlpConfig {
    endpoint: "http://localhost:4318/v1/traces".into(),
    service_name: "my-agent".into(),
    ..Default::default()
})?;
```

---

## llm-harness-knowledge

后端无关的知识访问契约。定义知识源能暴露什么，以及受信身份如何授权。

### 模块

| 模块 | 关键导出 | 说明 |
|------|---------|------|
| `source` | `KnowledgeSource` trait、`KnowledgeCapability`、`FreshnessClass`、`KnowledgeRef`、`KnowledgeHit`、`KnowledgeContent` | 知识源协议 |
| `access` | `KnowledgeAccessContext`、`KnowledgeAccessControl`、`KnowledgeAction` | 权限与身份 |
| `citation` | 引用类型 | 引用管理 |
| `evidence` | 证据注册与验证 | 证据系统 |
| `registry` | `KnowledgeRegistry` | 多源注册与路由 |
| `tools` | `knowledge_search`/`knowledge_read` Tool | LLM 可调用工具 |
| `plugin` | `KnowledgePlugin` | 一行安装 |

### KnowledgeSource trait

```rust
pub trait KnowledgeSource: Send + Sync {
    fn id(&self) -> &str;
    fn capabilities(&self) -> &[KnowledgeCapability];
    fn search(&self, query: &str, ctx: &KnowledgeRequestContext) -> BoxFuture<Vec<KnowledgeHit>>;
    fn read(&self, reference: &KnowledgeRef, ctx: &KnowledgeRequestContext) -> BoxFuture<Result<KnowledgeContent>>;
}
```

`KnowledgeCapability`：`Search`、`Read`、`Revisioned`、`SectionRead`、`LineRead`、`ChunkRead`。

---

## llm-harness-knowledge-local

`KnowledgeSource` 的本地 Markdown/Text 实现，使用 BM25 搜索。

### 模块

| 模块 | 说明 |
|------|------|
| `source` | `LocalDocumentSource` — 实现 `KnowledgeSource` trait |
| `fs` | 文件系统扫描 |
| `index` | BM25 全文索引 |
| `parser` | Markdown/Text 解析（section 拆分） |
| `revision` | 文件修订追踪 |

```rust
use llm_harness_knowledge_local::LocalDocumentSource;

let source = LocalDocumentSource::new("/path/to/docs", Default::default())?;
// 注册到 KnowledgeRegistry 或直接作为 KnowledgeSource 使用
```

---

## llm-harness-memory

长期记忆系统。记忆分为只读 `KnowledgeSource` 和独立 `MemoryStore`，`MemoryService` 在受信边界组合两者。

### 模块

| 模块 | 关键导出 | 说明 |
|------|---------|------|
| `store` | `MemoryStore` trait、`MemoryWriteIntent`、`MemoryMutationOrigin`、`MemorySessionId` | 写协议 |
| `service` | `MemoryService` | 读+写+策略组合 |
| `policy` | `MemoryWritePolicy` | 写入策略 |
| `safe_policy` | 安全策略 | 注入防御集成 |
| `mutation_gate` | `MemoryMutationGate` | 变更网关 |
| `tools` | `memory_write`/`memory_forget` Tool | LLM 可调用工具 |
| `plugin` | `MemoryPlugin` | 一行安装 |

### 设计要点

- **读写分离**：`MemoryStore`（写）不继承 `KnowledgeSource`（读），`MemoryService` 内部持有两者
- **受信来源**：`MemoryMutationOrigin`（`ExplicitTool`/`Application`/`Capture`）由运行时注入，不从模型参数反序列化
- **注入防御**：与 `MemoryDefensePlugin`（strategy crate）配合，扫描写入内容中的注入短语

---

## llm-harness-inspector

Agent Inspector：独立调试 crate，通过 Web API + WebSocket 实时观察 agent 状态与事件。只依赖 `llm-harness-agent` 与 `llm-harness-types`，不侵入 Plugin trait；全部实现 feature-gated（`inspect`），不启用时零成本。

### REST 端点

| 端点 | 方法 | 说明 |
|------|------|------|
| `/` | GET | 内置 Web UI（单文件 HTML，WebSocket 实时渲染） |
| `/api/agent/config` | GET | 配置快照（model/tools/phase/budget） |
| `/api/agent/session` | GET | 当前 session 消息历史与分支 |
| `/api/agent/usage` | GET | token 用量与成本 |
| `/api/agent/skills` | GET | skill 列表 |
| `/api/inject` | POST | 注入 steer 消息（`harness.steer()`） |
| `/api/abort` | POST | 中止当前 turn（`harness.abort()`） |
| `/api/prompt` | POST | 发起新 prompt |
| `/api/events` | WS | 实时事件流（首条 Snapshot 快照 + 增量透传） |

事件消息格式：`{ "type": "<AgentHarnessEvent variant>", "inner_type": "<内层 AgentEvent variant 或 null>", "timestamp": "...", "data": {...} }`。不可序列化的进程内字段（如 `ToolResult.extensions`）在投影层跳过，不改变事件语义。

### 使用

```rust
use llm_harness_inspector::Inspector;

// 最小路径：harness 即可获得实时事件流 + 配置 + session
let inspector = Inspector::builder(Arc::clone(&harness))
    .port(8080)
    .mount()
    .await?;

// 零代码挂载：SENZA_INSPECT=8080 环境变量
if let Some(builder) = Inspector::from_env(Arc::clone(&harness)) {
    builder.mount().await?;
}
```

`Inspector` 通过 `Drop` 触发 server 优雅关闭。详细设计见 `docs/design/2026-08-22-inspector-web-api-design.md`，使用指南见 [inspector-guide.md](inspector-guide.md)。
