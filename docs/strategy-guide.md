# 策略层指南

> `llm-harness-strategy` 是可选的策略实现 crate。runtime 机制层不依赖此 crate；用户按需引入，通过 `Plugin` 或 `Hook` 注册。
>
> 所有策略通过 `HarnessBuilder::install(&plugin)` 一行接入，或作为单独 Hook 注册到 `HarnessHooks`。

---

## 安全防护（Safety）

**模块**：`safety/`

`SafetyDefaultsPlugin` 提供 bash 命令黑名单和路径遍历防护。

| 检查项 | 覆盖工具 | 行为 |
|--------|---------|------|
| 命令黑名单 | `bash` | 拦截 `rm -rf`、`sudo`、`chmod 777` 等危险命令 |
| 路径遍历 | `write`/`edit`/`read` | 阻止 `../../etc` 等路径逃逸，静态检查 + canonicalize 校验 |

```rust
use llm_harness_strategy::SafetyDefaultsPlugin;

builder.install(&SafetyDefaultsPlugin::new());
```

注册的 Hook：`BeforeToolCallHook`（Deny 危险操作，Allow 安全操作）。

---

## 循环安全（Loop Safety）

**模块**：`loop_safety/`

`LoopSafetyPlugin` 打包 5 个防护 Hook，防止 Agent 进入病态循环。

| 防护 | 配置类型 | 默认值 | Hook 类型 | 行为 |
|------|---------|--------|----------|------|
| 重复调用检测 | `RepetitionGuardConfig` | 3 次连续相同参数 | `BeforeToolCallHook` | 同一工具同一参数连续调用 N 次 → Deny |
| 连续失败熔断 | `FailureCircuitBreakerConfig` | 3 次连续失败 | `BeforeToolCallHook` + `AfterToolCallHook` + `ShouldStopHook` | 同一工具连续失败 N 次 → Deny |
| 截断续写 | `TruncationContinuationConfig` | 最多 2 次续写 | `ShouldStopHook` + `TransformContextHook` | 模型回复被 `max_tokens` 截断时自动发续写 prompt |
| 死亡螺旋防护 | `DeathSpiralGuardConfig` | 5 轮连续错误 | `PrepareNextTurnHook` + `ShouldStopHook` | 连续 N 轮出现工具失败 → 停止 loop |
| 全局轮数上限 | `MaxTurnsConfig` | 50 轮 | `ShouldStopHook` + `PrepareNextTurnHook` | 硬上限，到达即停 |

```rust
use llm_harness_strategy::{LoopSafetyPlugin, LoopSafetyConfig};

// 全部启用（默认配置）
builder.install(&LoopSafetyPlugin::new(LoopSafetyConfig::default_enabled()));

// 自定义：只启用 max_turns 和 death_spiral
let config = LoopSafetyConfig {
    max_turns: Some(MaxTurnsConfig { max_turns: 30 }),
    death_spiral_guard: Some(DeathSpiralGuardConfig { max_error_turns: 3 }),
    ..LoopSafetyConfig::disabled()
};
builder.install(&LoopSafetyPlugin::new(config));
```

---

## 状态栏与待办（Status Panel & TODO）

**模块**：`status_panel/`

`StatusPanelPlugin` 提供 `<agent_status>` XML 注入、TODO 工具、工具调用计数器。

| 组件 | 类型 | 行为 |
|------|------|------|
| 状态栏注入 | `TransformContextHook` | 在上下文末尾注入 `<agent_status>` XML，包含当前任务、花费、预算剩余、TODO 列表 |
| TODO 工具 | `TodoWriteTool` | LLM 可调用的 `todo_write` 工具，维护待办列表（pending/in_progress/done） |
| 工具调用计数 | `ToolCallCounterHook` | `AfterToolCallHook`，统计工具调用次数 |

```rust
use llm_harness_strategy::{StatusPanelPlugin, StatusPanelConfig};

builder.install(&StatusPanelPlugin::new(StatusPanelConfig::default()));
```

---

## 事件流（Event Streams）

**模块**：`event_stream/`

提供 `EventStream` trait 的内置实现，供 `WaitForExternalEventTool` 使用。

| 实现 | 类型 | 用途 |
|------|------|------|
| `TimerStream` | 定时器 | 一次性延迟（`TimerKind::Once`）或周期性触发（`TimerKind::Interval`） |
| `HeartbeatStream` | 心跳 | 周期性心跳事件，可外部控制停止 |
| `FilterStream` | 过滤器 | 包装内层 stream，按 `source`/`channel`/`field` 过滤事件 |
| `WebhookStream` | Webhook | HTTP 接收器，将外部 webhook 请求转为事件 |
| `ShellMonitorStream` | Shell 监控 | 监控长运行 shell 进程的输出，转为事件 |
| `EventDetails` / `EventContext` | 模板 | 统一事件 payload 结构，使 `FilterStream` 可靠过滤 |

```rust
use llm_harness_strategy::{TimerStream, TimerKind, FilterStream, by_source};

// 定时器：每 60 秒触发
let timer = TimerStream::new(TimerKind::Interval, std::time::Duration::from_secs(60));

// 过滤器：只放行 source = "timer" 的事件
let filtered = FilterStream::new(Box::new(timer), by_source("timer"));
```

---

## 注入防御（Injection Defense）

### 注入短语过滤

**模块**：`injection_filter.rs`

`InjectionFilterPlugin` 过滤工具返回和上下文中的提示注入短语。

| 检查点 | Hook 类型 | 行为 |
|--------|----------|------|
| 工具返回后 | `AfterToolCallHook` | 过滤 `ToolResult` 中的注入短语，session 持久化内容也被过滤 |
| 上下文转换 | `TransformContextHook` | 扫描已积累的 `ToolResultMessage` 文本块 |

内置默认模式覆盖中英文常见注入短语（如 "ignore all previous instructions"、"忽略之前的所有指令"）。

```rust
use llm_harness_strategy::InjectionFilterPlugin;

builder.install(&InjectionFilterPlugin::default());
```

### 来源标记

**模块**：`source_tag.rs`

`SourceTagPlugin` 给外部内容工具的返回结果包裹 `<external_content source="...">` 标签。

```rust
use llm_harness_strategy::{SourceTagPlugin, SourceTagEntry};

builder.install(&SourceTagPlugin::new(vec![
    SourceTagEntry { tool: "web_search".into(), label: "webpage".into() },
    SourceTagEntry { tool: "read_url".into(),   label: "webpage".into() },
]));
```

注册的 Hook：`AfterToolCallHook`（包裹 `DataBlock::Text` 内容，model + session 双重包裹）。

### 持久记忆防线

**模块**：`memory_defense.rs`

`MemoryDefensePlugin` 扫描写入持久记忆的内容（`memory_write` 工具 + write/edit 到 `MEMORY.md` 等文件），防止注入短语潜伏在长期记忆中。

| 动作模式 | 行为 |
|---------|------|
| `Deny`（默认） | 拒绝写入，返回 ToolFailure 告知 Agent 内容包含注入短语 |
| `Sanitize` | 替换注入短语为 `[FILTERED]`，继续写入修改后的内容 |
| `Warn` | 记录警告但允许写入 |

```rust
use llm_harness_strategy::{MemoryDefensePlugin, MemoryDefensePluginBuilder, MemoryDefenseAction};

// 默认 Deny 模式
builder.install(&MemoryDefensePlugin::new());

// Sanitize 模式 + 自定义记忆文件
let plugin = MemoryDefensePluginBuilder::new()
    .action(MemoryDefenseAction::Sanitize)
    .memory_files(vec!["MEMORY.md", "NOTES.md"])
    .build();
builder.install(&plugin);
```

---

## 通知（Notification）

**模块**：`notify.rs`

`NotifyPlugin` 提供 `notify_user` 工具和 `NotificationChannel` trait。

| 组件 | 类型 | 行为 |
|------|------|------|
| `NotifyUserTool` | `Tool` | LLM 可调用的 `notify_user` 工具，不结束 run |
| `NotificationChannel` | trait | 用户实现，定义通知投递方式（IM/邮件/短信） |
| `Urgency` | enum | `Low`/`Normal`/`High`，传递给 channel 作为元数据 |

```rust
use llm_harness_strategy::{NotifyPlugin, NotificationChannel, NotificationMessage, NotificationError};
use std::sync::Arc;
use futures::future::BoxFuture;

struct ConsoleChannel;
impl NotificationChannel for ConsoleChannel {
    fn send(&self, msg: NotificationMessage) -> BoxFuture<'_, Result<(), NotificationError>> {
        println!("[notify] {}", msg.text);
        Box::pin(async { Ok(()) })
    }
}

builder.install(&NotifyPlugin::new());

// 每个 run 通过 RunExtensions 传入 channel
let request = RunRequest::from_text("...")
    .with_extension(Arc::new(ConsoleChannel) as Arc<dyn NotificationChannel>);
```

---

## 工具输出保护（Tool Output Guard）

**模块**：`tool_output_guard.rs`

`ToolOutputGuardPlugin` 是 `AfterToolCallHook`，当工具返回的 `Full` 投影过大时自动截断。

| 配置 | 默认值 | 说明 |
|------|--------|------|
| `max_lines` | 200 | 超过此行数触发截断 |
| `max_bytes` | 10,000 | 超过此字节数触发截断 |
| `head_lines` | 50 | 保留头部行数 |
| `tail_lines` | 50 | 保留尾部行数 |
| `skip_tools` | `[]` | 跳过的工具名列表 |

截断行为：完整内容持久化到临时文件 → `model_content` 替换为截断版本 + 文件路径 → `session_projection` 切换为 `Projected`。

```rust
use llm_harness_strategy::{ToolOutputGuardPlugin, ToolOutputGuardConfig};

builder.install(&ToolOutputGuardPlugin::new(env.clone()));

// 自定义阈值
let config = ToolOutputGuardConfig {
    max_lines: 100,
    max_bytes: 5_000,
    ..Default::default()
};
builder.install(&ToolOutputGuardPlugin::with_config(env.clone(), config));
```

---

## MCP 凭证保护（MCP Credential Guard）

**模块**：`credential_guard.rs`

`McpCredentialGuardHook` 检查 MCP 工具调用是否在其服务器的凭证作用域内。

| 模式 | 行为 |
|------|------|
| `warning`（默认） | 记录警告，允许调用 |
| `denying` | 拒绝调用，返回凭证作用域违规错误 |

```rust
use llm_harness_strategy::McpCredentialGuardHook;

// Warning 模式
let hook = McpCredentialGuardHook::warning(lookup_fn);

// Fail-closed 模式
let hook = McpCredentialGuardHook::denying(lookup_fn);

builder.hooks.before_tool_call.push(Arc::new(hook));
```

> 注意：MCP 使用进程级凭证模型（env/headers 在 server 内所有工具间共享），此 Hook 提供检测而非强制隔离。

---

## 上下文压缩（Compaction）

**模块**：`compaction.rs`

提供 context-aware compaction prompt 模板。编码 5 级保留优先级：

1. 架构决策和关键约束 — 逐字保留
2. 修改文件列表和关键变更记录 — 完整保留
3. 验证状态（通过/失败） — 必须保留
4. 未解决 TODO 和回滚说明 — 必须保留
5. 工具输出 — 可删除，只保留结论

```rust
use llm_harness_strategy::context_aware_prompt_spec;
use llm_harness_agent::CompactionModelConfig;

let compaction = CompactionModelConfig {
    summary: llm_harness_agent::CompactionSummaryConfig {
        prompt: Some(context_aware_prompt_spec()),
        ..Default::default()
    },
    ..Default::default()
};
```

---

## 资源注入（Resource Injection）

**模块**：`resource.rs`

`ResourceInjector` 把 `ResourceProvider` 提供的资源内容追加到 system prompt 尾部。

```rust
use llm_harness_strategy::ResourceInjector;

let injector = ResourceInjector::new();
injector.inject_all(&providers, &mut context).await?;
```

---

## 项目指令注入（Project Instruction）

**模块**：`project_instruction.rs`

`ProjectInstructionPlugin` 自动扫描 `CLAUDE.md`、`AGENTS.md`、`.cursorrules` 等文件，注入到 system prompt 尾部。

| 配置 | 默认值 | 说明 |
|------|--------|------|
| `file_names` | `["CLAUDE.md", "AGENTS.md", ".cursorrules", "SOUL.md"]` | 扫描的文件名，按顺序处理 |
| `max_depth` | `None`（遍历到根） | 向上遍历的最大父目录层数 |
| `max_bytes` | 64 KiB | 单个文件最大读取字节数 |

从 `working_dir()` 向上遍历，子目录在前（更具体），父目录在后（更通用），XML 标签包裹每个文件。

```rust
use llm_harness_strategy::{ProjectInstructionPlugin, ProjectInstructionConfig};

builder.install(&ProjectInstructionPlugin::new(ProjectInstructionConfig::default()));
```

注册的 Hook：`BeforeRunHook`（每个 run 开始时扫描并注入）。

---

## 审计策略（Audit）

**模块**：`audit.rs`

`AuditPlugin` 把每次工具调用记录到 `AuditSink`。

| 组件 | 行为 |
|------|------|
| `ToolCallAuditHook` | `BeforeToolCallHook` 记录开始时间，`AfterToolCallHook` 构造 `AuditEntry`（含时长）写入 `AuditSink` |
| `AuditPlugin` | 包装为 Plugin，一行安装 |

```rust
use llm_harness_strategy::AuditPlugin;
use std::sync::Arc;

builder.install(&AuditPlugin::new(Arc::new(audit_sink)));
```

> 安全注意：工具调用的完整 `args` 会记录到审计 payload 中，不做脱敏。如果参数可能包含密钥，需在 `AuditSink` 后端做访问控制。

---

## 预算控制（Budget Control）

**模块**：`budget.rs`

`BudgetControlAdapter` 是纯决策 Hook，实现 `ShouldStopHook`。

| 模式 | 行为 |
|------|------|
| `exceeded_hook = None` | 监视模式：只统计不停 |
| `exceeded_hook = Some(h)` | 超预算时咨询 `BudgetExceededHook`：`true` = 继续，`false` = 停止 |

计费违规和未知成本响应始终停止，不可覆盖。

```rust
use llm_harness_strategy::BudgetControlAdapter;
use llm_harness_types::BudgetExceededHook;

// 通常由 HarnessBuilder 自动注入，不需要手动创建
// 此处展示手动配置：
let adapter = BudgetControlAdapter::new(
    shared_cost_arc,
    5.0,  // $5.00 预算上限
    None, // 监视模式
);
```

> 通常通过 `HarnessBuilder::budget(limit)` 配置，builder 会自动创建 `CostAccumulatorHook` + `BudgetControlAdapter`。
