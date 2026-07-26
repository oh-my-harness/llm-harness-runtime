# Roadmap

> 基于 harness engineering & RSI 研究综述与 AutoHarness 论文，经逐项验证与讨论后确定的演进路线。
> 详见 [docs/harness-engineering-and-rsi-implications.md](docs/harness-engineering-and-rsi-implications.md)。

## 现状

runtime 已具备 harness engineering 的核心机制（15 项 ✅）：

- **workflow design / loop engineering** — `WorkflowEngine`：staged workflow + back-edge + retry
- **evaluation** — `StepTransitionJudge` trait + `StepResult.structured`
- **permission controls** — `BeforeToolCallHook` + `WorkflowGateHook` + `RuleBasedApprovalHook`
- **persistent state management** — `TaskStore` + `Checkpoint` + `WorkflowState`
- **memory / context management** — `WorkflowContext` (blackboard) + `Session` + compaction
- **tools** — `Tool` trait + `Plugin` 注册 + `McpToolAdapter`
- **planning** — `PlanWorkflowTool` + LLM step
- **action** — `StepExecutor` + `Sandbox`
- **file system as persistent memory** — `Sandbox` cwd + `shell` executor
- **sub-agent spawn + parallel** — `spawn_async` + `MessageBus`
- **agent delegation tools** — `SpawnAgentTool` / `MessageSubagentTool` / `MessageMainTool` / `AwaitSubagentReplyTool` / `AwaitMainMessageTool` / `QuerySubagentTool` / `AbortSubagentTool`
- **audit trail** — `AuditSink` + SHA-256 hash chain
- **cost / budget control** — `CostAggregate` + `BudgetControlAdapter`
- **crash recovery** — `WorkflowEngine::restore()` + `restore_from_step()` + `TaskStore`
- **event-driven pause/resume** — `EventStream` + `WaitForExternalEventTool`（含超时）
- **mechanism/policy separation** — trait 声明在 types，实现在 runtime/后端 crate，策略靠 trait 回调注入

## P0：AutoHarness 模式

> 论文：*AutoHarness: improving LLM agents by automatically synthesizing a code harness*（Google DeepMind, 2026）

### 核心思想

LLM agent 在结构化环境中频繁输出非法动作（如 78% 的国际象棋失败源于非法走棋）。AutoHarness 让模型自动生成代码校验框架（harness），通过环境反馈迭代优化代码，消除非法动作。

### 三级梯度架构

1. **Action Filter**：代码生成全部合法动作 → LLM 排序选最优
2. **Action Verifier**（主方案）：LLM 提议 → 代码校验 → 非法则带报错重试
3. **Action Policy**：纯代码策略，推理阶段不调 LLM

### 迭代搜索流程

```
Thompson 采样树搜索
  ├── 选节点：从搜索树按 Thompson 采样选代码版本
  ├── Rollout：并行 N 个环境运行当前 harness，收集失败样本
  ├── Critic：汇总非法动作/错误/奖励
  ├── Refiner：LLM 接收旧代码 + 错误日志 → 修复后新代码
  └── 更新树：新代码作为子节点，更新启发值（合法动作率）
```

### runtime 已有机制映射

| AutoHarness 组件 | runtime 对应 |
|---|---|
| 迭代循环（rollout → critic → refine） | `WorkflowEngine` back-edge + retry + LoopConfig |
| 并行环境 rollout | `spawn_async` + `MessageBus` |
| harness 代码执行 | `Sandbox`（bash/python executor） |
| Critic 评估 | `StepTransitionJudge` trait |
| Refiner（LLM 修代码） | `Step::Llm` |
| 搜索树节点持久化 | `Checkpoint` + `TaskStore` |

### 实现方案：混合（runtime 加机制 + 应用层实现）

外层搜索循环（Thompson 采样 / 树节点管理 / rollout 协调）在**应用层**实现，基于 `WorkflowEngine` + `spawn_async` + `Sandbox`。

runtime 层需新增的机制待详细设计时确定，候选方向：

- 环境接口抽象（如 `EnvironmentRunner` trait：`step(action) → obs/reward/done`）
- 搜索树分支元数据（`Checkpoint` 的 stage/parent 关系）
- 环境交互内置 tool（让 LLM step 调用外部环境）
- cost/tokens 持久化（搜索树节点记录代价）

**设计原则约束**：runtime 只提供机制（环境抽象、持久化、执行），不提供策略（Thompson 采样、树搜索协调、代码修复 prompt）。搜索算法和 prompt 设计由应用层实现。

## 近期

### P1：step_history 按需检索（Q1）

**问题**：`format_step_history_summary` 把所有历史 step 截断后等权注入 prompt（output 前 1000 字符 + structured 前 4000 字符 + 总预算 20000 字符裁剪）。LLM 拿到的是平铺文本，不能按 step_id / stage / 指标检索特定历史 step 的完整 `StepResult`。长 workflow 中信息丢失。

**方案**：提供内置 `query_history` tool，让 LLM step agent 可查询 `step_history` 中特定 step 的完整 `structured` 结果。

**边界**：runtime 提供检索机制。查询什么、怎么用检索结果，是策略层。

### P1：Session 续接（Gap-1）

**问题**：`run_llm_step` 每次 `Transition::Retry` 时创建全新 `Session::new(storage)`。LLM 只拿到 `step_history` 文本摘要 + `WorkflowContext` KV 前缀，看不到上一轮 retry 的对话内容。ACE 的 Reflector iterative refinement、ADAS 的 self-refine 等需要 step 内多轮改进的模式无法保持对话连续性。

**方案**：在 `Step::Llm` 定义中增加 `session_mode: SessionMode` 字段：

- `Fresh`（默认，当前行为）：每次创建新 session。
- `Persistent`：retry 时复用上一轮 session，LLM 看到完整对话历史。

**边界**：只提供机制（session 复用 or 不复用），是否复用由 workflow 定义者（策略层）决定。

**影响范围**：`workflow/model.rs`（Step 枚举扩展）、`workflow/engine/runner.rs`（`run_llm_step` 按 session_mode 选择 session 构造方式）。

## 中期

### P2：WorkflowContext 大小限制（Q2）

**问题**：`WorkflowContext` 是 `serde_json::Map<String, Value>` 纯 KV 黑板。`format_context_vars` 把所有 KV 全量格式化进 prompt，无截断、无大小限制。如果应用层往 context 塞大对象，prompt 会膨胀。

**方案**：`format_context_vars` 增加总大小上限 + 单值截断。超过上限时保留按 key 排序的前 N 个，或标记截断。

**边界**：runtime 提供大小控制机制。限制多少、截断策略，是配置参数。

### P2：Tool 声明式权限级别（借鉴 BoxAgnts）

**问题**：`Tool` trait 无 `permission_level()` 方法。`RuleBasedApprovalHook` 的 `RuleChain` 未命中时 fallback 默认 Deny（fail-secure），缺少中间地带——Read 类工具（如 `read`/`glob`）也需要显式写规则才放行，应用层负担重。

**方案**：在 `Tool` trait 增加默认方法 `fn permission_level(&self) -> PermissionLevel { PermissionLevel::Read }`，新增 `PermissionLevel` 枚举（`Read`/`Write`/`Execute`/`Network`）。在 `runtime/src/rules/` 新增 `PermissionLevelHook`（实现 `BeforeToolCallHook`），作为 `RuleChain` 未命中时的默认策略：按 `tool.permission_level()` 派生决策（Read→Allow, Write/Execute/Network→Deny），保持 fail-secure。

**边界**：runtime 只提供声明性元数据 + 默认派生机制。`PermissionMode` 降级（BypassPermissions/AcceptEdits/Plan）、`Ask` 交互决策、规则持久化（Session/Persistent scope）是应用层策略，通过组合现有 `BeforeToolCallHook` 实现。

**影响范围**：`llm-harness-types/src/tool.rs`（trait + enum）、`runtime/src/rules/`（新 hook）、内置 tool（bash→Execute, write/edit→Write, read→Read）。

**参考**：[BoxAgnts](https://github.com/guyoung/boxagnts) `workspace/src/permissions.rs` 的 `PermissionLevel` + `PermissionManager::evaluate`。

## 远期

### P3：compaction 分级压缩（借鉴 BoxAgnts）

**问题**：当前 compaction 是单层触发——`try_auto_compact` 在 estimated_tokens 超过阈值时全量压缩到 `keep_recent_tokens`。在阈值边界附近会产生振荡：context 刚好低于阈值时不压缩、持续膨胀，超过阈值时一次性大压缩导致信息丢失。

**方案**：`CompactionThresholdConfig` 增加双阈值：`micro_compact_threshold: Option<f64>`（如 0.75，轻量压缩最旧部分）+ `full_compact_threshold`（当前隐式阈值）。`try_auto_compact` 按双阈值选择压缩力度：micro 阈值触发只压缩最旧 N 条（保留更大 `keep_recent_tokens`），full 阈值触发当前完整压缩。具体压缩策略可通过 `BeforeCompactHook` 注入。

**边界**：runtime 提供双阈值配置机制。压缩多少、怎么压缩是策略，留给 hook。

**影响范围**：`llm-harness-agent/src/compaction.rs`（`CompactionThresholdConfig` 扩展）、`harness/compaction_ops.rs`（双阈值逻辑）。

**参考**：[BoxAgnts](https://github.com/guyoung/boxagnts) `query/src/compact.rs` 的 `MicroCompactConfig` + `micro_compact_if_needed`。

## 设计原则

以下原则在 runtime 后续开发中坚持（源自 [harness engineering & RSI 综述](docs/harness-engineering-and-rsi-implications.md) §5）：

1. **机制与策略分离**（最高优先级）— runtime 提供机制（workflow 遍历、hook 组合、spawn、persist、sandbox、audit），不提供策略（怎么路由、什么时候分支、怎么评估、怎么搜索）。策略通过 trait 回调注入。检验标准：runtime 一行不改即可服务不同 agent。
2. **刻意简单和通用** — 不预判模型能力边界，不预判领域需求。检验标准：资深工程师会觉得这过于复杂吗？如果是，简化。
3. **File system as first-class memory** — context 不是垃圾桶。全量状态存文件，context 只留当前需要的。`WorkflowContext` 应保持轻量，`Sandbox` 文件系统是主要持久记忆。
4. **可序列化、可比较、可恢复** — harness 配置必须可被导出为文件、可被重新加载执行、可被外层系统比较。
5. **安全层在 evolution loop 之外** — permission control、evaluator、human review 不在 harness 自我改进的循环内。editable surface 必须被显式界定。
6. **OS 式封装** — harness 像 OS：trait 是系统调用，runtime 是内核，后端 crate 是驱动，应用层 agent 是用户程序。

## 明确不做的事

以下经讨论确认**不在 runtime 职责内**，由上层建筑实现或当前设计已够用：

| 不做 | 理由 |
|---|---|
| Checkpoint 阶段元数据（Q3） | checkpoint 关系管理是应用层职责，runtime 不应预设树结构语义 |
| sub-agent 结构化回传（Q4） | sub-agent 通过文件系统回传结构化数据（写文件，主 agent 读），不需要专用 tool |
| workflow context compaction（Q5） | session 级 compaction 已够，workflow context 应保持轻量（文件系统存全量） |
| AuditSink 查询接口（Q6） | 查询是应用层职责，runtime 只管写，读全量 JSONL 自行解析即可 |
| 多维 budget（Q7） | cost + max_steps + max_retries 三者独立够用，应用层可自行组合 |
| Sandbox 资源计费（Q8） | 资源计费是上层建筑职责，runtime 只管隔离不管计量 |
| EventStream trait 超时（Q9） | tool 层已有超时机制（`WaitForExternalEventTool::with_timeout`），trait 层不需要改 |
| Context provenance tracking（Gap-2） | 应用层可在 `StepResult.structured` 里手动标注来源 |
| 完整 harness 配置序列化（Gap-3） | 完整配置序列化是上层建筑职责，runtime 只需保证 Workflow 可序列化 |
| 并行 fan-out / fan-in（Gap-4） | `spawn_async` + `MessageBus` 已足够，并行编排是应用层职责 |
| Editable surface 标记（Gap-5） | 上层建筑（harness 自我改进系统）的职责，runtime 不需要预设这层抽象 |
| Context lifecycle 管理（Gap-6） | lifecycle 管理是应用层职责，runtime 只提供 KV 存储 |
| 失败模式聚合（Gap-7） | 失败分析是策略层职责，runtime 只提供数据（step_history + AuditSink） |
| `resume_agent` 工具 | sub-agent 完成后不需要恢复，新任务应 spawn 新 agent |
| 子图嵌套（Issue #38） | 当前 stage + back-edge + LoopConfig 已够用，子图嵌套增加复杂度但收益不明 |
| 时间旅行 / checkpoint history（Issue #39） | `restore_from_step` 的 rerun 能力已够，fork 分支是上层建筑职责 |
| State reducer（Issue #40） | 依赖并行 fan-out（已决定不做），没有独立价值 |
| 运行时动态 workflow 修改 | workflow 应在运行前定义好，动态修改与声明式设计哲学冲突 |
| Meta-Harness / ADAS / AFlow 外层搜索循环 | 这是"优化 harness 的系统"，不是 harness 本身。runtime 是被优化的对象，不是优化器 |
| Skill 数据库 + crossover agent | MCE 的 outer loop。runtime 提供 skill 注册/执行机制，不提供 skill 演化策略 |
| Pareto frontier 选择 | 多目标评估策略。runtime 提供评估数据，选择算法是策略 |
| 模型权重更新 | SIA 的 joint optimization。完全在 runtime 之外（训练系统） |
| 领域评估逻辑 | QoR 指标计算等。runtime 提供 `StepExecutor` + `StepTransitionJudge` trait，具体评估由应用层实现 |
| WASM 沙箱工具执行（BoxAgnts A） | `Sandbox` trait 已覆盖（os/bwrap/seatbelt），WASM 是新后端非新机制，引入 wasmtime 增编译体积 |
| ManagedAgentConfig 声明式编排（BoxAgnts D） | 与 spawn+MessageBus 机制重复，manager-executor 是并行编排策略，属应用层 |
| compaction 熔断器（BoxAgnts E-1） | 不采纳——provider 故障应由应用层 retry 策略处理，runtime 不预设故障处理模式 |
| API round 分组 + topic_hint（BoxAgnts E-3） | 树结构 session + 现有切点逻辑（`find_valid_cut_boundary`）已等价 |
| Session 分支/检查点快照（BoxAgnts F） | 树结构 session（`fork_branch`/`navigate_to`）已是扁平+快照方案的严格上位 |
| Skill `list` 工具发现（BoxAgnts G） | 现有 `SkillReadTool` + `format_skills_for_system_prompt` 已是严格上位 |
| feature_gates 环境变量 flag（BoxAgnts H） | 配置应显式可序列化（`AgentHarnessOptions`），环境变量 flag 与设计原则有张力 |
| ToolSearchTool 工具发现（BoxAgnts C） | 当前工具数量少全量注入更简单，ROI 低；需要时应用层可自行实现 |
