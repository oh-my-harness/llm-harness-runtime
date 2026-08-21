# 基础设施 Crate 参考

> 本文档覆盖 7 个基础设施 crate——它们是 runtime 层之下的基础类型、平台 trait 和后端实现。上层 crate（runtime、agent、loop、strategy）有各自的文档。

---

## llm-harness-types

核心类型与 trait 定义 crate。不依赖任何内部 crate，是整个栈的根基。

### 模块

| 模块 | 关键导出 |
|------|---------|
| `tool` | `Tool` trait、`ToolContext`、`ToolResult`、`ToolFailure`、`ToolLevel`、`ToolExecutionMode`、`ToolSessionProjection` |
| `hooks` | `BeforeToolCallHook`、`AfterToolCallHook`、`BeforeTurnHook`、`AfterTurnHook`、`ShouldStopHook`、`TransformContextHook`、`PrepareNextTurnHook`、`BeforeRunHook`、`BeforeCompactHook`、`BeforeProviderRequestHook`、`AfterProviderResponseHook` |
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
| All-Execute（观测型） | 所有 hook 依次执行，返回第一个 hook 的结果 | `BeforeProviderRequestHook`、`AfterProviderResponseHook`、`AfterTurnHook` |
| First-Non-Allow（决策型） | 顺序执行，第一个返回 `Allow` 以外的决策立即短路 | `BeforeToolCallHook`（`Allow`/`Deny`/`Modify`） |
| Chain（链式修改型） | 按序执行，每个 hook 看到前一个 hook 的修改结果 | `AfterToolCallHook`（`Passthrough`/`Patch`） |

---

## llm-harness-platform

横切 trait 定义 crate。后端 crate（sandbox、audit-jsonl、trace-otel）依赖此 crate 实现 trait。

### 模块

| 模块 | 关键导出 | 说明 |
|------|---------|------|
| `sandbox` | `Sandbox` trait、`SandboxConfig`、`NetRule`、`ResourceLimits` | 沙箱抽象：start/env/reset/shutdown |
| `audit` | re-export from `types` | 审计 trait |
| `tracer` | `TraceExporter` trait、`SpanEvent`、`SpanId`、`TraceId`、`SpanKind`、`SpanStatus` | 追踪导出抽象 |
| `tracing_adapter` | `SpanContextMap` | 把 core hooks 桥接为 span 事件 |
| `prompt` | `PromptSource` trait、`PromptSourceTemplate`、`ArgumentDef` | 提示模板源 |
| `env` | 环境辅助 | 环境抽象辅助 |

> 消费者直接依赖 `llm-harness-platform` 访问这些模块（不再通过 runtime re-export）。

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
