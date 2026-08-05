# Roadmap

> 基于 [AI Agents in Depth 分层梳理](docs/ai-agent-book-layer-analysis.md) 28 个 view 的分析结果。
> 三层模型：**机制层**（trait/hook/引擎基础设施）· **runtime 策略层**（默认 hook 实现/内置工具/模板）· **用户策略层**（应用层决定）。

---

## 第一层：功能全景

### 图例

| 标记 | 含义 |
|------|------|
| ✅ 已支持 | 机制层有基础设施 + 策略层有默认实现，端到端可用 |
| ⚠️ 部分支持 | 机制层有 trait/hook 点，但策略层缺默认实现或机制层有缺口 |
| ❌ 待实现 | 机制层和策略层均缺失 |

### 功能矩阵

| 功能领域 | 机制层 | 策略层 | 整体 | 说明 |
|---------|--------|--------|------|------|
| **Loop 引擎** | ✅ | — | ✅ | sync loop + async tools，RetryConfig 指数退避 |
| **Workflow 引擎** | ✅ | — | ✅ | staged workflow + back-edge + retry + crash recovery |
| **Tool 系统** | ✅ | — | ✅ | Tool trait + Plugin 注册 + ToolSessionProjection (Full/Projected/Ephemeral) |
| **并行工具执行** | ✅ | — | ✅ | join_all + ToolExecutionMode (Parallel/Sequential) |
| **工具审批** | ✅ | ⚠️ | ⚠️ | BeforeToolCallHook + RuleChain + WorkflowGateHook 机制完整；缺默认危险命令黑名单、路径遍历防护规则 |
| **文件编辑** | ✅ | — | ✅ | EditTool 行号+hashline 定位，WriteTool，ReadTool offset/limit+行号 |
| **搜索工具** | ❌ | ❌ | ❌ | grep + glob 工具未实现（跨平台差异需独立工具） |
| **BashTool 截断** | ⚠️ | ❌ | ⚠️ | ShellExecutor 有 TruncationStrategy 但 BashTool 未使用 |
| **上下文压缩** | ⚠️ | ⚠️ | ⚠️ | compaction 基础设施完整，但 prompt 硬编码、无熔断器、无消息级不可压缩标记 |
| **上下文注入** | ✅ | ⚠️ | ⚠️ | ContextMode (Append/Replace/Inject) + TransformContextHook 机制完整；Inject 已改为 Vec\<ContextBlock\> 结构化来源标注；缺状态栏默认模板 |
| **状态栏** | ✅ | ❌ | ⚠️ | TransformContextHook/PrepareNextTurnHook 可注入；缺默认 XML 格式模板和内容收集器 |
| **TODO 工具** | ❌ | ❌ | ❌ | 无 TODO write 工具 |
| **成本/预算** | ✅ | ✅ | ✅ | CostAccumulatorHook + BudgetControlAdapter + UsageLedger（策略层代码待迁移到 strategy crate） |
| **审计日志** | ✅ | ⚠️ | ⚠️ | AuditSink + SHA-256 hash chain；缺默认结构化日志格式 |
| **沙箱** | ✅ | — | ✅ | Sandbox trait + os/bwrap/seatbelt 后端 |
| **子 Agent** | ✅ | ⚠️ | ⚠️ | spawn + MessageBus + 7 个委派工具完整；SpawnOptions 支持 per-spawn system_prompt/thinking_level/role/description 覆盖；QuerySubagentTool 返回 role/description；ContextMode 结构化来源标注已实现 |
| **MCP 集成** | ⚠️ | ⚠️ | ⚠️ | McpToolAdapter 基本可用；缺版本锁定、最小权限凭证管理、工具描述审查 |
| **Skill 系统** | ✅ | ⚠️ | ⚠️ | SkillReadTool + format_skills_for_system_prompt 存在但无生产调用者；缺 BeforeRunHook 自动注入默认实现 |
| **HITL** | ❌ | ❌ | ❌ | 无 request_human_approval/input 内置工具 |
| **通知** | ❌ | ❌ | ❌ | 无 NotificationChannel trait，无 notify_user 工具 |
| **事件驱动** | ✅ | ⚠️ | ⚠️ | EventStream trait + WaitForExternalEventTool（含超时）；缺默认事件过滤规则、Cron/Heartbeat 实现 |
| **故障恢复** | ⚠️ | ⚠️ | ⚠️ | RetryConfig 退避完整；缺抖动、空闲看门狗、轨迹修复、连续失败计数器、死亡螺旋防护、全局 max_turns |
| **Abort/取消** | ✅ | — | ✅ | CancellationToken (3 检查点) + AbortCascadeHook + phase_guard RAII + session 持久化 |
| **提示工程** | ✅ | ⚠️ | ⚠️ | PromptTemplate + ResourceInjector 机制完整；缺注入短语黑名单、来源标签包裹默认策略 |
| **提示注入防御** | ✅ | ⚠️ | ⚠️ | ContextMode 结构化来源标注已实现；缺来源约束默认策略 |
| **项目指令文件** | ✅ | ❌ | ⚠️ | BeforeRunHook 可注入；缺自动扫描 CLAUDE.md 等的默认实现 |
| **持久终端会话** | ❌ | ❌ | ❌ | OsEnv::execute_shell 每次 `sh -c` 新进程，无 PTY |
| **流式工具执行** | ❌ | ❌ | ❌ | loop_fn 消费完整 stream 后才 dispatch 工具 |
| **写入后 lint** | ✅ | ❌ | ⚠️ | AfterToolCallHook::Replace 可支持；无默认实现 |
| **主动工具发现** | ❌ | ❌ | ❌ | 无 discover_tools 元工具 |

### 旧 roadmap 中的功能（仍有效）

| 功能领域 | 机制层 | 策略层 | 整体 | 说明 |
|---------|--------|--------|------|------|
| **持久状态管理** | ✅ | — | ✅ | TaskStore + Checkpoint + WorkflowState |
| **规划** | ✅ | — | ✅ | PlanWorkflowTool + LLM step |
| **崩溃恢复** | ✅ | — | ✅ | WorkflowEngine::restore() + restore_from_step() + TaskStore |

---

## 第二层：下一版本详细计划

分三个阶段执行。阶段间有依赖：阶段一是前置条件（策略 crate 存在后才能放策略代码），阶段二和阶段三可部分并行。

### 阶段一：提取策略代码到 `llm-harness-strategy` crate

将混杂在机制层 crate 中的策略层代码迁移到独立 crate，只依赖机制层 public API。编译器强制边界。

| # | 待迁移代码 | 当前位置 | 说明 |
|---|-----------|---------|------|
| 1 | `BudgetControlAdapter` | `runtime/src/control/budget.rs` | ShouldStopHook 实现 |
| 2 | `CostAccumulatorHook` | `runtime/src/control/cost_hook.rs` | AfterProviderResponseHook 实现 |
| 3 | `RuleBasedApprovalHook` + `RuleChain`/`Rule`/`Predicate`/内置 predicates | `runtime/src/rules/` | BeforeToolCallHook 实现 + 规则匹配框架 |
| 4 | `ResourceInjector` | `runtime/src/platform/resource.rs` | 往 system_prompt 追加资源的默认策略 |
| 5 | `format_skills_for_system_prompt` | `agent/src/skills.rs` | Skill 元数据格式化模板 |
| 6 | `SUMMARIZATION_SYSTEM_PROMPT`/`SUMMARY_REQUEST` | `agent/src/compaction.rs` | 压缩 prompt 模板 |

**保留在机制层**：`UsageLedger`/`CostAggregate`（共享数据结构）、`WorkflowGateHook`（与 workflow 引擎紧耦合）。

**迁移后**：`HarnessBuilder` 的 `budget()`/`pricing()` 方法改为委托到 strategy crate 的工厂函数，或保留 builder 方法但 hook 实现来自 strategy crate。

### 阶段二：补齐机制层缺失

需改 loop_fn/session/dispatch 内部逻辑，不能仅靠 hook 实现。

| # | 缺失项 | 涉及 view | 改动位置 | 优先级 |
|---|--------|---------|---------|--------|
| 1 | 自定义压缩 prompt 通路 | view 9 | `compaction.rs` | 高 |
| 2 | 压缩熔断器（连续失败计数+停止） | view 9, 26 | `compaction_ops.rs` | 高 |
| 3 | 标记消息为不可压缩 | view 9 | `messages.rs`/`compaction.rs` | 中 |
| 4 | ~~ContextMode 结构化来源标注~~ ✅ 已实现 | view 19 | `spawn_types.rs`：Inject(String)→Inject(Vec\<ContextBlock\>) | ~~高~~ |
| 5 | ~~SpawnOptions 配置继承~~ ✅ 已实现 | view 3, 12 | `spawn_types.rs`/`spawner.rs` | ~~高~~ |
| 6 | ~~QuerySubagentTool 返回 role/description~~ ✅ 已实现 | view 12 | `spawn/tools.rs` | ~~中~~ |
| 7 | NotificationChannel trait | view 13 | `types/` | 高 |
| 8 | 退避抖动 (jitter) | view 26 | `config.rs` RetryConfig | 中 |
| 9 | 流式空闲看门狗 | view 26 | `loop_fn.rs` | 中 |
| 10 | 轨迹完整性自动修复 | view 26 | `loop_fn.rs` | 中 |
| 11 | MCP 服务器版本锁定 | view 17 | `runtime-mcp/` | 低 |
| 12 | MCP 最小权限凭证管理 | view 17 | `runtime-mcp/` | 低 |
| 13 | 截断后完整输出保存到临时文件 | view 18 | `tool.rs` ToolSessionProjection | 中 |

### 阶段三：补齐 runtime 策略层缺失

在 `llm-harness-strategy` crate 中实现默认策略。只需注册 hook 或实现 trait。

| # | 缺失项 | 涉及 view | 实现方式 | 优先级 |
|---|--------|---------|---------|--------|
| 1 | 默认危险命令黑名单 | view 11 | BeforeToolCallHook + RuleChain | 高 |
| 2 | 默认路径遍历防护 | view 11 | BeforeToolCallHook + canonicalize | 高 |
| 3 | 状态栏 XML 模板 + 内容收集器 | view 5, 8 | TransformContextHook 默认实现 | 高 |
| 4 | Skill 元数据注入 BeforeRunHook | view 7 | BeforeRunHook 默认实现 | 高 |
| 5 | notify_user 内置工具 | view 13 | Tool trait + NotificationChannel | 高 |
| 6 | HITL 工具 + 超时降级 | view 12, 20 | Tool trait + timeout→Deny | 高 |
| 7 | 项目指令文件自动注入 | view 25 | BeforeRunHook 扫描 CLAUDE.md 等 | 高 |
| 8 | 全局 max_turns 上限 | view 26 | ShouldStopHook + 轮数计数 | 高 |
| 9 | BashTool 截断策略 | view 18 | BashTool 改用 TruncationStrategy | 高 |
| 10 | grep + glob 工具 | view 15 | runtime-tools 新增 | 高 |
| 11 | 默认审计日志格式 | view 11 | AfterToolCallHook + 结构化模板 | 中 |
| 12 | TODO write 工具 | view 5 | Tool trait + TransformContextHook | 中 |
| 13 | 自动包裹来源标签 | view 6 | AfterToolCallHook 默认实现 | 中 |
| 14 | 状态栏内容来源约束 | view 6 | TransformContextHook 校验 | 中 |
| 15 | 命令语义解析 | view 24 | BeforeToolCallHook + Shell 语义分析 | 中 |
| 16 | 持久记忆跨会话防线 | view 24 | BeforeToolCallHook 写入审查 | 中 |
| 17 | 重复调用指纹检测 | view 26 | ShouldStopHook + tool+params 指纹 | 中 |
| 18 | 连续失败计数器 | view 26 | ShouldStopHook + per-path 计数 | 中 |
| 19 | 输出触顶降级接续 | view 26 | BeforeProviderRequestHook + 续写 | 中 |
| 20 | 死亡螺旋防护 + 递归深度计数器 | view 26 | AfterRunHook/ShouldStopHook | 中 |
| 21 | 压缩 prompt 默认模板（上下文感知） | view 9 | 自定义压缩 prompt 的默认实现 | 中 |
| 22 | 保留优先级默认规则 | view 9 | 不可压缩消息的策略层规则 | 中 |
| 23 | 截断标注引导 Agent read 临时文件 | view 18 | 截断标注模板 | 中 |
| 24 | discover_tools 元工具 | view 17, 23 | Tool trait 实现 | 中 |
| 25 | 工具层次化组织 | view 17, 23 | 工具分类+分组策略 | 中 |
| 26 | 注入短语黑名单模板 | view 6 | TransformContextHook 默认实现 | 低 |
| 27 | 批量 steer 消息编号+汇总 | view 22 | TransformContextHook 默认实现 | 低 |
| 28 | 工具描述审查机制 | view 17 | MCP adapter wrapper 审计 | 低 |
| 29 | 事件过滤规则 + 载荷上下文模板 | view 14 | EventStream wrapper | 低 |
| 30 | Cron/Heartbeat EventStream | view 14 | TimerStream/HeartbeatStream | 低 |
| 31 | 工具结果投影默认策略 | view 9, 10 | ToolSessionProjection 默认阈值 | 低 |

---

## 明确不做的事

| 不做 | 涉及 view | 理由 |
|------|----------|------|
| 流式工具执行 | view 27 | 实现复杂度高（需 stream 中增量解析 ToolUse） |
| 持久化终端会话 | view 28 | 需要 PTY/长运行 shell 进程管理 |
| 写入后自动 lint | view 28 | AfterToolCallHook::Replace 可支持，但无内置实现计划 |
| 动态 schema 追加 + KV Cache 优化 | view 23 | 太复杂；当前全量注入 tools 参数对数十个工具足够 |
| mid-turn steer | view 21 | 有意设计选择——sync loop 模型更简单，匹配 LLM 训练范式 |
| 优先级队列 | view 20 | 太复杂 |
| 多渠道通知 | view 13/20 | 太复杂，NotificationChannel trait 先做单通道 |
