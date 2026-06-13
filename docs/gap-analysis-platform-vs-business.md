# 通用平台差距分析：从"生产级 Multi-Agent Harness"文章对照 llm-harness-runtime v0.2

## 修订说明（2026-06-13）

### 第一次修订（核心立场调整）

1. 初稿把参考文章当作 spec，修订版加入对文章主张的批判性审视（§3）
2. 初稿只看"平台缺什么"，修订版补上"平台已有、文章未涉及的能力"（§4）
3. 引入 trait 价值四问框架（§5）贯穿后续判断
4. 将原"高优先级 9 项"大幅收缩，区分平台核心 / 独立 optional crate / 框架层 / 业务层四级归属
5. 补上文章遗漏但生产真正需要的方向（§14）
6. 每个"建议添加 trait"都附我的判断

### 第二次修订（深入 core 源码后的事实修正）

**口径调整**：平台 = `llm-harness-core` + `llm-harness-runtime` 两个 crate 共同构成。用户在 Cargo.toml 同时引入即可，不需要 runtime 做 re-export。文章里的需求由 core 还是 runtime 满足，都算"平台已具备"。

基于这个口径，第一次修订的几处事实错误：

- §4.4 写"execution_mode 三种"——实际只有 `Parallel`/`Sequential` 两种，已改
- §4.6 写"CancellationToken v0.2 已规划"——实际 core 已全面贯穿（每个 ExecutionEnv 操作 + ToolContext.abort + HarnessInner.current_abort），已改
- §4 列表遗漏了 core 已有的关键能力：Compaction 子系统、Session/分支管理、Skills/PromptTemplate、Streaming 事件流、Steer/Follow-up 注入队列。已补充
- §4 列表遗漏了 runtime 已有的能力：`Sandbox` trait + 3 backend（bwrap / seatbelt / os）、`ResourceProvider`/`ResourceInjector`、`PromptSource`。已补充
- §10.2 c) 写"ContextCompressor 不应进平台"——结论可以保留（runtime 不需要新 trait），但**理由是错的**：实际上 core 已经有完整的 Compaction 系统（`BeforeCompactHook` + `CompactionSettings` + `compact()` + 默认摘要 prompt），覆盖了文章"Context Compression"的需求。已改正理由
- §14 写"Tool 沙箱需评估"——实际 runtime 已有 `Sandbox` trait + 3 backend，已改
- §14 写"Streaming 中途取消语义需明确"——实际 AgentEvent 已含 `TextDelta`/`ThinkingDelta`/`ToolExecutionUpdate`，且 CancellationToken 全程贯穿。已改

核心立场（不变）：**平台的价值不在于覆盖文章列出的所有项，而在于提供一组正交、可组合的机制（trait + Hook + 合成语义），让业务用最少的代码表达任意策略。**

---

## 1. 分析目的

对照《从零设计生产级 Multi-Agent Harness》提出的要求，辩证地评估：

- 文章哪些主张适合作为平台层 trait
- 哪些应作为独立 optional crate
- 哪些属于框架层（构建在 platform 之上的工程层）
- 哪些是业务层策略，根本不该平台化

核心原则（保持不变）：**平台提供机制，业务提供策略。**

新增推论：**仅当机制无法通过现有 trait + Hook 组合表达，且抽象稳定多实现时，才引入新 trait。**

---

## 2. 信息来源

- **文章来源**：`《从零设计生产级 Multi-Agent Harness：架构、评估、记忆、成本与 MCP 工具接入全拆解》`，李伟山，发布于腾讯云开发者公众号，2026-05-12。去噪后的纯技术内容保存在 `docs/reference-production-multi-agent-harness.md`。
- **设计文档**：`docs/runtime-v0.2-design.md`
- **源代码**：`crates/llm-harness-runtime/src/` 下的 trait 定义与默认实现
- **项目规则**：`CLAUDE.md`

---

## 3. 对参考文章主张的批判性审视

文章是腾讯云开发者公众号的厂商技术博客，不是 RFC 也不是论文。框架性观点合理，但下列具体主张需要警惕。

### 3.1 数字与清单缺乏量化依据

| 文章主张 | 我的看法 |
|---|---|
| "工具数 5–12 个最佳"、"<8 / 8–15 / >20 错误率分层" | 无出处。作为参考默认值可以，不应固化为约束 |
| Budget 四区阈值（绿>50% / 黄 20-50% / 红 5-20% / 熔断<5%） | 直觉数字，生产中要按 workflow 校准 |
| 工具元信息"九项" | 前 3 项必需（name/description/schema），后 6 项是治理建议，不应都作为必填字段 |
| Eval "必须" 四层 | 4 层是一种分层方法，不是唯一方法 |

### 3.2 "声明式 Plan vs 命令式调用"被过度简化

文章把"声明式"等同"Harness 可介入"，但：

- 复杂条件分支、循环、动态参数在纯声明 DAG 中难以表达
- 命令式 + 拦截 Hook（如 `BeforeToolCallHook`）同样能实现"介入"
- 真实系统往往采取混合形式：高层 DAG + 节点内命令式 + 拦截 Hook

不存在"必须声明式"的强约束。Plan 是一种工具，不是平台 trait 的强制依赖。

### 3.3 "Trajectory Eval 是 Multi-Agent 最大创新点"被夸大

序列/轨迹校验在传统软件测试（SQL plan 回归、状态机覆盖）里早有。Agent 轨迹的特殊性在于**轨迹不确定**，不在"看轨迹"本身。Trajectory Eval 有价值，但更接近 CI 工程问题，不是必须的平台 trait。

### 3.4 Memory 五层不必照搬

文章的 Working / Session / Execution Log / Episodic / Semantic 是一种分类。多数实际系统：

- 只用 Working + Execution Log（无跨任务记忆需求）
- 或只用 Session（短会话不需要长期记忆）

把五层全部强制实现是过度工程。

### 3.5 文章对"Harness 自身成本"沉默

文章鼓吹的所有机制都有 meta-cost：

- Planner Agent 生成计划本身烧 token
- LLM-as-Judge 评估烧 token
- Memory retrieval + embedding 也烧 token
- Trace + audit I/O 开销

这些 meta-cost 在生产中常常 dwarf "节省下来的钱"。引用文章建议时要做成本核算。

### 3.6 文章完全没提的生产真实痛点

任务取消、多租户隔离、工具沙箱、Context window 硬约束、Streaming 中途取消语义。这些放在 §14 单列。

---

## 4. 平台已有但文章未涉及的核心能力

初稿只看"文章 → 平台缺什么"，单向化。这一节补上平台（**core + runtime 一起**）已有的、文章里没有对应概念的能力。这些是平台的设计 IP。

### 4.1 三段 hook 体系：Auth / Budget / Infrastructure（runtime）

文章未涉及。平台通过 `AuthHook`（core 定义、runtime 暴露 adapter）、`BudgetExceededHook`、`InfrastructureHook` 实现"框架与业务的解耦边界"。文章的"Tool Registry + RBAC"思路在平台里通过 hook 实现，比单一注册表更灵活。

### 4.2 CompositeHook 三种合成语义（runtime）

文章完全没有这个概念，但生产中多个 hook 共存是常态：

- All-Execute（观测型）
- First-Non-Allow（决策型）
- Last-Non-Passthrough（修改型）

平台把"合成语义"作为一等概念，避免了"手写 hook 链组合"的混乱。

### 4.3 Hook vs Event 的明确分离（core）

文章用"事件订阅"、"hook 拦截"等术语混用，平台明确区分：

- **Hook**：构造期注入、参与决策、有返回值（core 有 12 个 hook trait + AuthHook/ConvertToLlmHook 在 AgentHarnessOptions 顶层）
- **Event**：运行时订阅、纯通知、无返回值（`AgentEvent` / `AgentHarnessEvent`）

类型层面就清晰"什么时候可改写、什么时候只观测"。

### 4.4 Tool execution_mode 两模式 + 分批并发（core）

`ToolExecutionMode::Parallel` / `Sequential`，loop 按 Sequential 为分割点切批，组内并发、组间顺序。这比文章"超时 + 速率限制"细粒度。

### 4.5 Trait 依赖方向硬约束（core/runtime 都遵循）

`llm-harness-types` / `llm-harness-traits` 不依赖任何外部 crate。用户可以只引入 trait 包做单元测试，无需拖入 tokio / serde。

### 4.6 CancellationToken 已全面贯穿（core）

文章未提"任务取消"。core 已在每个 `ExecutionEnv` 操作、`ToolContext.abort`、`HarnessInner.current_abort`、`HarnessOptions.stream_options` 上接 CancellationToken。`abort()` 是一等操作。这是生产硬需求，且已具备。

### 4.7 hash 链审计与脱敏（runtime）

`AuditSink` + hash 链防篡改、`SpanRedactor` 脱敏、`CaptureLevel`。文章只提"审计要保留"，平台给出了具体的不可篡改与脱敏机制。

### 4.8 Compaction 子系统（core，文章意义上的"Context Compression"）

文章作为策略二提及；core 已是一等子系统：

- `BeforeCompactHook`（`Proceed` / `Skip` / `Override` 决策）
- `CompactionSettings`（reserve_tokens / keep_recent_tokens / summary_model）
- `prepare_compaction()` + `compact()` 函数（分离决策与异步摘要）
- 默认摘要 prompt（含 Goal / Progress / Decisions / Next Steps / Critical Context 五段结构）
- 自动触发：`total_tokens > context_window - reserve_tokens` 时触发
- `CompactionResult` 含 `file_operations` 跟踪（被压缩区间的文件读写记录）
- `TransformContextHook` 作为 compaction 接入 LLM 调用链的钩子

文章主张的"事实型任务保留原始引用、合规型任务关键证据不可压缩"等策略，由业务在 `BeforeCompactHook::Override` 里实现。

### 4.9 Session + 分支管理（core）

文章未涉及，但企业级 agent 强需求：

- `SessionRepo` trait（create / open / list / delete / **cross-session fork**）
- `SessionStorage` trait（17 + 方法，含 entries / 树形结构 / cursor / 多 leaf）
- `InMemorySessionRepo` 与 `JsonlSessionRepo` 两种持久化
- 分支事件：`BranchForked` / `BranchSwitched` / `BranchDeleted` / `BranchSummarized`
- AI 自动生成的 `BranchSummary`
- `active_cursor` 管理多分支并行
- 不可变 entry 树（`UUIDv7` 时序）

这一整套是文章"Execution Log（不可变）"+"轨迹回放"的超集。

### 4.10 Streaming 事件流（core）

文章未深入。core 的 `AgentEvent` 是 5 层完整流：

- Agent 生命周期（`AgentStart` / `AgentEnd`）
- Turn 生命周期（`TurnStart` / `TurnEnd`）
- 消息级（`MessageStart` / `MessageUpdate` / `MessageEnd`）
- Token 级（`TextDelta` / `ThinkingDelta` / `ToolCallArgsDelta`）—— **`ThinkingDelta` 支持流式推理内容（CoT 流）**
- 工具执行（`ToolExecutionStart` / `ToolExecutionUpdate` / `ToolExecutionEnd`）—— **`ToolExecutionUpdate` 支持长工具推送中间结果**

这些都不在文章涉及范围内。

### 4.11 运行中注入消息：Steer / Follow-up（core）

`HarnessInner` 持有 `steer_tx` / `follow_up_tx` 两条 mpsc 通道，允许在 agent 运行中向 agent 队列注入消息。文章未提及，但实际产品（如对话式 agent）需要。

### 4.12 Sandbox 抽象 + 3 个 backend（runtime）

文章在"权限"主题上比较笼统。Runtime 提供：

- `Sandbox` trait + `SandboxConfig`（fs_allowlist / fs_denylist / net_allowlist / `ResourceLimits` cpu/memory/disk/timeout / work_dir）
- 三个 backend crate：`sandbox-bwrap`（Linux bubblewrap）、`sandbox-seatbelt`（macOS sandbox-exec）、`sandbox-os`（无沙箱直通）
- 与 core 的 `ExecutionEnv` 复合：Sandbox 包装 ExecutionEnv，给 tool 调用加权限边界

这比文章 "tool 风险等级 + 人工审批" 实质性强得多。

### 4.13 资源与 Prompt 抽象（runtime）

- `ResourceProvider` / `ResourceInjector`：把 MCP/file 资源注入 system prompt
- `PromptSource` / `PromptTemplateCompiler`：第三方 prompt 模板（如 MCP prompts）编译为 core `PromptTemplate`

文章未提结构化资源管理。

### 4.14 Skills 系统（core）

`Skill` / `SourcedSkill` / `PromptTemplate` / `load_skills` / `format_skills_for_system_prompt`：从 `SKILL.md` 文件加载技能，注入系统提示或通过 `invoke_skill()` 显式调用。文章未提技能管理。

### 4.15 RetryConfig 与 transient error 处理（core）

`RetryConfig` 用于 provider 调用层的 transient error 退避，比 runtime `RetryPolicy`（task 级）更底层。两者层级互补。

**结论：** 平台（core + runtime）并非"到处是缺口"。文章侧重"必须存在哪些组件"，平台侧重"组件之间如何安全组合"。**很多文章列出的需求（Context Compression、Execution Log、人工审批、沙箱、子任务 spawning）实际上已经在 core 或 runtime 里被覆盖，只是名称和接口形态与文章不同。**

---

## 5. trait 价值评估的四问框架

后续每个"建议添加 trait"都按此过滤：

1. **类型必要性**：接口需要类型安全（vs 配置 / 字符串 / 闭包）吗？
2. **多实现性**：是否会出现 ≥2 种实质不同的实现？
3. **接口稳定性**：接口是否已稳定（vs 仍在演化，过早固化反害）？
4. **用户实现成本**：不在平台提供，用户自己写有多痛？

判定标准：至少 3 条肯定，且"用户成本高"与"多实现"必占其一。

---

## 6. 架构编排

### 6.1 平台已提供

| 机制 | 位置 | 文章对应 |
|------|------|---------|
| `TaskState` 状态机（7 种状态） | `task.rs` | 02 节：任务生命周期状态机 |
| `TaskRunner` trait + `TaskRunnerImpl` | `task.rs` | 02 节：Orchestrator 决策权 |
| 11 个 hook 插槽的 `HarnessHooks` | core 提供，runtime 注入 adapter | 02 节：Harness 全局控制 |
| `RetryPolicy` | `task.rs` | 02 节：失败处理 |
| 硬终止（`ShouldStopHook` + core `max_steps`） | `budget.rs` + core | 02 节：硬终止 |
| `Checkpoint` 恢复机制 | `task.rs` | 02 节：checkpoint resume |

### 6.2 平台欠缺 / 我的判断

**a) Plan 抽象 — 不应进平台核心**

初稿建议引入 `Plan` / `PlanStep` / `Planner` / `PlanExecutor` 四个 trait。

四问检验：

| 问 | 判断 |
|---|---|
| 类型必要性 | ✗ Plan 结构差异巨大（coding ≠ research ≠ 数据分析），强行抽象只能得到"最小公共子集" |
| 多实现性 | ✓ 确实有多种 planner |
| 稳定性 | ✗ Plan DSL 在业界尚未收敛 |
| 用户成本 | 低 — 业务直接定义自己的 PlanStep 即可 |

**我的判断：** 不进平台层。可考虑独立 crate `llm-harness-planning`，但优先级低，待业界 DSL 收敛后再启动。

**b) AgentCapability + AgentRouter — 不应进平台**

`role` / `supported_intents` / `required_tools` / `preferred_model` 全是业务策略字段。

**我的判断：** 不进平台层。

**c) TaskConstraints 统一配置 struct — 应进平台**

四问：类型必要 ✓（各种 limit 类型不同）/ 稳定 ✓（max_steps / max_tokens / max_duration / max_tool_calls 已收敛）/ 用户成本高 ✓。

**我的判断：** v0.3 进核心，作为 struct（不是 trait）。

### 6.3 业务层负责

- 定义 PlanStep 形态、实现 Planner
- 定义 Agent 角色体系与路由规则
- 设定 TaskConstraints 阈值
- 实现 TaskVerifier

---

## 7. 工具治理

### 7.1 平台已提供

| 机制 | 位置 | 文章对应 |
|------|------|---------|
| `ToolRegistry` trait | `tool_registry.rs` | 03 节：Tool Registry 统一关口 |
| `ToolSource` trait | `tool_registry.rs` | 03 节：工具来源抽象 |
| `ToolDefinition`（name + description + Schema + execution_mode） | `tool_registry.rs` | 03 节 |
| `ToolMetadata`（name + label + description） | `tool_registry.rs` | 03 节：仅三项 |
| 两层过滤（subset + active_tools） | `tool_registry.rs` + core | 03 节：RBAC |
| `InMemoryToolRegistry` | `tool_registry.rs` | — |
| `HumanApprovalWrapper` | `human_approval.rs` | 03 节：人工确认 |
| T0-T3 风险分层（文档约定） | 设计文档 §8.2.1 | 03 节：风险等级 |

### 7.2 平台欠缺 / 我的判断

**a) ToolMetadata 字段扩展 — v0.2 可补，但分两组处理**

初稿建议平铺 6 个新字段。我把它们拆开：

**应作为一等字段（平台真的会消费）：**

- `timeout: Option<Duration>` — 平台调度需要
- `rate_limit_per_minute: Option<u32>` — 平台限流需要

**应放入通用扩展位（业务策略，平台不消费）：**

- `risk_level` / `require_approval` / `allowed_roles` / `audit_policy`

**我的设计建议：**

```rust
pub struct ToolMetadata {
    pub name: String,
    pub label: String,
    pub description: String,
    // —— 平台一等字段（v0.2 新增）——
    pub timeout: Option<Duration>,
    pub rate_limit_per_minute: Option<u32>,
    // —— 业务自由扩展位 ——
    pub properties: HashMap<String, serde_json::Value>,
}
```

业务把 `risk_level` / `audit_policy` 等塞进 `properties`，避免平台被业务字段污染。

**b) RateLimiter trait — 应进平台**

四问：类型必要 ✓ / 多实现 ✓（token bucket / leaky bucket / 分布式 redis） / 稳定 ✓ / 用户成本中。

**我的判断：** v0.3 进核心。

**c) ToolRegistry 事件通知 — 不必加**

文章未明确要求。需要时可通过现有 hook 实现。

### 7.3 业务层负责

- 填写工具的业务策略字段
- 实现 RateLimiter
- 选择 MCP SDK 并管理子进程生命周期

---

## 8. 状态与记忆

### 8.1 平台已提供

| 机制 | 位置 | 文章对应 |
|------|------|---------|
| Working State | core `AgentHarness` session | 04 节 |
| Execution Log | `AuditSink` + `TracingHookAdapter` | 04 节 |
| `Checkpoint` | `task.rs` | 04 节 |

### 8.2 平台欠缺 / 我的判断

**核心判断：Memory 不应进平台核心，应作为独立 optional crate。**

理由：

1. Memory 后端高度多样（Redis / pgvector / Qdrant / 文件系统）
2. Memory 是策略密集型（什么值得记、怎么打分、怎么遗忘）
3. 不是所有 Agent 都需要长期 Memory
4. 引入 Memory 会拉入 embedding / vector 等依赖，污染平台核心

**推荐做法：** 独立 crate `llm-harness-memory`，可选依赖。最小接口：

```rust
trait MemoryStore: Send + Sync {
    type Memory: Serialize + DeserializeOwned;
    fn search(&self, query: &str, top_k: usize)
        -> BoxFuture<'_, Vec<Self::Memory>>;
    fn store(&self, memory: Self::Memory) -> BoxFuture<'_, ()>;
}
```

把 `Memory` 设为 associated type，业务定义自己的 EpisodicMemory / SemanticMemory 结构，平台不限定字段（这点和初稿不同——初稿规定了具体字段，我认为是过度规定）。

`ForgettingPolicy` 同样在该 crate，配置导向、非 trait。

`MemoryInjector` 不必单独 trait，注入时机由业务在 PrepareNextTurnHook 里实现即可。

**我的判断：** 不进 v0.3 核心，作为独立 crate 与平台主版本松耦合演进。

### 8.3 业务层负责

- 选择记忆后端
- 实现向量嵌入
- 定义什么值得记住
- 设定遗忘阈值
- 实现记忆巩固策略
- 管理 Session State 后端

---

## 9. 评估体系

### 9.1 平台已提供

| 机制 | 位置 | 文章对应 |
|------|------|---------|
| `TaskVerifier` trait | `task.rs` | 第三层：Task Completion Eval |
| `TaskTrace` | `task.rs` | — |
| `SpanEvent` 层次结构 | `tracer.rs` | 可用于轨迹回放 |

### 9.2 平台欠缺 / 我的判断

**核心判断：Eval 是 CI 工程层面的事，不是运行时 trait。**

`TrajectoryEvaluator` / `EvalCase` / `EvalRunner` / `EvalJudge` 这类 trait 应在独立 crate `llm-harness-eval` 中实现，或留给业务/CI 框架。

理由：

1. Eval 通常**离线**运行，不在请求路径上
2. Eval 框架不需要与 Agent runtime 共享核心 trait
3. EvalCase 字段（forbidden_tools / expected_output_pattern / DeterministicCheck）是策略

**唯一可进平台的：** 现有的 `TaskTrace` + `SpanEvent` 已经能让外部 Eval 工具读取轨迹。无需新加 trait。

**我的判断：** 全部不进 v0.3 核心。建议作为 `llm-harness-eval` 独立 crate 或留给业务/CI 框架。

### 9.3 业务层负责

- 构建 Eval 数据集
- 定义确定性检查
- 实现 LLM-as-Judge prompt
- 建立 Eval CI 流水线
- 人工抽检校准

---

## 10. 成本控制

### 10.1 平台已提供

| 机制 | 位置 | 文章对应 |
|------|------|---------|
| `PricingProvider` trait | `cost.rs` | 06 节：价格表提供者 |
| `CostAggregate` + `ModelCost` | `cost.rs` | 06 节：聚合 |
| `TokenPrice` | `cost.rs` | 06 节：按模型定价 |
| `BudgetControlAdapter` | `budget.rs` | 06 节：实时调度 |
| `BudgetExceededHook` trait | `budget.rs` | 06 节：超限回调 |
| 二元超限检测 | `budget.rs` | 06 节 |

### 10.2 平台欠缺 / 我的判断

**a) 分级降级 — 部分可进平台，但不固化"四区"**

初稿建议引入 `BudgetZone` 四区枚举 + `DegradationPolicy` trait。我认为四区阈值（50%/20%/5%）是策略，不应固化在 enum 里。

**我的设计建议：**

```rust
struct BudgetStatus {
    spent: f64,
    limit: f64,
    ratio: f64,    // spent / limit
}

trait BudgetDegradationHook: Send + Sync {
    fn on_budget_update(&self, status: BudgetStatus) -> Option<DegradationAction>;
}

enum DegradationAction {
    Continue,
    RequestStop,
    Custom(serde_json::Value),
}
```

- 平台提供 `BudgetStatus` 与 hook 调用机制
- 业务在 hook 实现里自己决定"在 30% / 5% 时做什么"，不强加四区命名

**我的判断：** v0.3 进核心，但**不固化"绿黄红熔断"和"具体动作"**。

**b) ModelRouter — 不应进平台**

完全是策略层。平台需要的"切换 LLM client"能力通过 `LlmClient` 抽象已经具备。

**c) ContextCompressor — 不需要新 trait，core 已有 Compaction 子系统**

文章策略二（上下文压缩）实际上**已被 core 的 Compaction 子系统完整覆盖**：

- `BeforeCompactHook`（`Proceed` / `Skip` / `Override` 决策）
- `CompactionSettings`（reserve_tokens / keep_recent_tokens / summary_model）
- `prepare_compaction()` + `compact()` 默认实现 + 内置摘要 prompt
- 自动触发：`total_tokens > context_window - reserve_tokens`
- `CompactionResult` 含 `file_operations` 跟踪
- `TransformContextHook` 作为接入 LLM 调用链的钩子

文章主张的"事实型任务保留原始引用、合规型任务关键证据不可压缩"等业务策略，由业务在 `BeforeCompactHook::Override` 里实现自定义摘要逻辑。

**我的判断：** runtime 不需要新增 `ContextCompressor` trait。文章对应需求由 core 现有机制满足，业务策略走 hook。

### 10.3 业务层负责

- 实现 `PricingProvider`
- 设定预算阈值与降级动作
- 实现 ModelRouter
- 实现 ContextCompressor
- 配置监控看板
- 定义成本 SLA

---

## 11. MCP 工具接入

### 11.1 平台已提供

| 机制 | 位置 | 文章对应 |
|------|------|---------|
| `McpClientHandle` trait | `mcp.rs` | 07 节：MCP 客户端句柄 |
| `McpToolDef` / `McpResult` / `McpError` | `mcp.rs` | 07 节 |
| `ToolSource` → `ToolRegistry` 适配 | `tool_registry.rs` | 07 节：MCP → Tool 适配 |
| Protocol error vs tool error 分离 | `mcp.rs` | 07 节 |

### 11.2 平台欠缺 / 我的判断

**a) per-MCP-server 配额 — 不必单独加 trait**

可通过 `RateLimiter`（§7） + `ToolRegistry.subset` + `HumanApprovalWrapper` 组合表达。引入 `McpServerManager` 是抽象冗余。

**b) MCP 白名单 — 已能用现有机制**

`ToolRegistry.subset()` 已提供过滤。所谓"声明式配置"是配置层而非 trait 层。

**我的判断：** 不引入 MCP 专属新 trait。

### 11.3 业务层负责

- 选择 MCP SDK
- 管理 MCP server 进程生命周期
- 配置每个 MCP server 的工具白名单
- 按业务域配置超时和熔断阈值
- 审计 MCP 调用

---

## 12. 可观测性

### 12.1 平台已提供

| 机制 | 位置 | 文章对应 |
|------|------|---------|
| `TraceExporter` trait + `SpanEvent` | `tracer.rs` | 08 节 |
| `TracingHookAdapter`（5 层 span） | `tracing_adapter.rs` | 08 节：Span 层次 |
| `CaptureLevel` + `SpanRedactor` | `tracer.rs` | 08 节：脱敏 |
| OTel GenAI 语义约定 | `tracer.rs` | 08 节 |
| `AuditSink` + hash 链 | `audit.rs` | 08 节：审计 |
| `InMemoryTraceExporter` + `OtlpExporter` | `trace-otel/` | 08 节 |
| `CostAggregate` | `cost.rs` | 08 节 |

### 12.2 平台欠缺 / 我的判断

**a) MetricsCollector trait — 应进平台**

四问：类型必要 ✓ / 多实现 ✓（Prometheus / StatsD / OTel）/ 稳定 ✓（counter / histogram / gauge 三种 metric 类型公认）/ 用户成本高 ✓。

**我的判断：** v0.3 进核心，作为门面 trait（实际导出走 OTel SDK）。

**b) AlertSender — 不应进平台**

告警系统差异巨大（Slack / PagerDuty / Email / 自研）。平台用户直接接入告警 SDK 即可。

### 12.3 业务层负责

- 选择 OTel 后端
- 配置脱敏规则
- 搭建监控看板
- 设置告警规则
- 定义 CaptureLevel 策略

---

## 13. Multi-Agent 协作

### 13.1 平台已提供

| 机制 | 位置 | 文章对应 |
|------|------|---------|
| `SubAgentSpawner` trait（v0.2 默认 None） | `subagent.rs` | 02 节：Sub-agent spawning |
| `SubAgentSpawnOpts` + `SubAgentResult` | `subagent.rs` | — |
| 上下文隔离保证（文档约定） | `subagent.rs` | — |

### 13.2 平台欠缺 / 我的判断

**a) AgentBus / AgentMessage — 不应进平台**

Agent 间通信协议是上层框架的事。平台提供 `SubAgentSpawner` 已足够覆盖"父→子单向 spawn + summary 返回"的核心场景。

**b) AgentQualityScore — 不应进平台**

这是 Eval 体系的产物，归 §9。

### 13.3 业务层负责

- 定义 Agent 角色体系
- 实现 SubAgentSpawner
- 定义协作模式

---

## 14. 文章未提但生产真正需要

补上文章遗漏、生产 Agent 真正会踩坑的方向：

| 维度 | 平台位置 | 状态 | 我的判断 |
|---|---|---|---|
| **任务取消传播** | core `CancellationToken` | **已具备**（每个 Env 操作 + ToolContext + HarnessInner 全程贯穿） | 平台底层已扎实 |
| **超时硬截断** | TaskConstraints.max_duration | v0.3 应补 | 与 §6 TaskConstraints 统一处理 |
| **多租户隔离** | 暂无统一抽象 | 需评估 | 企业用户硬需求，应专项调研 |
| **Tool 沙箱执行** | runtime `Sandbox` trait + 3 backend（bwrap / seatbelt / os） | **已具备** | 比文章"risk_level 字段"实质性强得多 |
| **Context window 硬约束** | core `CompactionSettings.reserve_tokens` 自动触发 | **已具备**（compaction 子系统覆盖） | 业务可通过 `BeforeCompactHook` 定制 |
| **Streaming（含中途取消）** | core `AgentEvent`（TextDelta / ThinkingDelta / ToolExecutionUpdate）+ CancellationToken | **已具备** | ThinkingDelta 还支持流式推理（CoT 流） |
| **运行中注入消息** | core Steer / Follow-up 队列 | **已具备** | 对话式 agent 必需 |
| **Session + 分支管理** | core `SessionRepo` / `SessionStorage` + Branch 事件 | **已具备**（含跨 session fork） | 是文章"Execution Log"的超集 |
| **审计日志保留与轮转** | runtime `AuditSink` 接口已具 | 接口已备 / 保留策略归业务 | 平台只管接口 |

**核心观察：** 文章未直接提及的能力里，平台**真正缺的**只有 "多租户隔离" 和 "TaskConstraints 统一"。其他大部分（沙箱、压缩、流式、取消、Session/分支）都已在 core 或 runtime 落地。

这些事实在 §15 的优先级清单里需要据此重排。

---

## 15. 修订后的优先级清单

### 已具备（无需再做）

| 项 | 位置 | 说明 |
|---|---|---|
| Context Compression | core Compaction 子系统 | 文章策略二已被覆盖 |
| Tool 沙箱 | runtime `Sandbox` + 3 backend | 文章"风险等级"的实质替代 |
| 任务取消传播 | core `CancellationToken` 全程贯穿 | 文章未提，但已扎实 |
| Streaming（含 CoT 流、tool 中间结果） | core `AgentEvent` 5 层 | 文章未深入 |
| Session + 跨 session fork + 分支 | core `SessionRepo` / `SessionStorage` | 文章"Execution Log"的超集 |
| Skills / PromptTemplate | core | 文章未提 |
| 运行中消息注入 | core Steer / Follow-up 队列 | 文章未提 |
| 审计 hash 链 + 脱敏 | runtime `AuditSink` + `SpanRedactor` | 文章"审计"具体化 |

### v0.2 可补（不引入新 trait，仅字段扩展）

| 项 | 说明 |
|---|---|
| ToolMetadata 增加 `timeout` 字段 | 平台调度需要 |
| ToolMetadata 增加 `rate_limit_per_minute` 字段 | 平台限流需要 |
| ToolMetadata 增加 `properties: HashMap<String, Value>` | 业务自由扩展位（risk_level / audit_policy 等塞这里） |

### v0.3 核心（少而精，4 项）

| 项 | 类型 | 理由 |
|---|---|---|
| TaskConstraints 统一 struct | struct | 各 limit 类型已收敛；max_duration 走 CancellationToken |
| RateLimiter trait | trait | 接口稳定，多实现 |
| BudgetDegradationHook（不固化 Zone） | trait | 与现有 BudgetExceededHook 对偶，但更细粒度 |
| MetricsCollector trait | trait | 与 tracing 对称，门面抽象 |

### 独立 optional crate（与核心松耦合演进）

| crate | 包含 | 优先级 |
|---|---|---|
| `llm-harness-memory` | MemoryStore trait（associated type）| 中 |
| `llm-harness-eval` | TrajectoryEvaluator / EvalCase / EvalRunner | 中 |
| `llm-harness-planning` | Plan / Planner 抽象 | 低（待业界 DSL 收敛） |

### 不进平台（业务/框架层自决）

| 项 | 不进的理由 |
|---|---|
| Plan / PlanStep / Planner / PlanExecutor 进核心 | Plan 结构业务高度耦合 |
| AgentCapability / AgentRouter | 字段全是业务策略 |
| ModelRouter | 业务策略，LlmClient 已够 |
| ContextCompressor | **core Compaction 已覆盖**，业务策略走 `BeforeCompactHook` |
| McpServerManager 独立 trait | RateLimiter + subset 可组合替代 |
| AgentBus | 上层框架职责 |
| AgentQualityScore | Eval 产物 |
| AlertSender | 业务直接接告警 SDK |

### 应专项评估（v0.3 之外的方向性研究）

| 项 | 说明 |
|---|---|
| 多租户隔离抽象 | 企业级硬需求，文章未提，是平台真正没解决的 |
| Audit 保留 / 轮转策略 | 接口已具，落地策略由业务定 |

---

## 16. 业务层始终需做的工作（平台无法替代）

| 维度 | 业务层职责 |
|------|----------|
| **领域建模** | 定义 Agent 角色、工具集、任务类型、Plan 模板 |
| **Prompt 工程** | 编写 System Prompt、工具描述、评估标准 |
| **策略配置** | 设定预算阈值、降级动作、遗忘策略参数 |
| **基础设施** | 选择/部署向量数据库、OTel 后端、MCP SDK、消息队列 |
| **数据建设** | 构建 Eval 数据集、标注 ground truth、记忆种子数据 |
| **模型选型** | 选择主模型/小模型/路由规则、维护价格表 |
| **安全合规** | 定义脱敏规则、审计保留策略、权限矩阵 |
| **运维** | 监控看板、告警规则、SLA 定义、成本核算 |
| **持续优化** | A/B 测试 Prompt、校准 LLM-as-Judge、调整路由策略 |

---

## 17. 核心洞察

初稿洞察："平台补全后，业务层的核心工作从'造轮子'变成'做决策'。"

修订洞察：

> **平台的价值不在于覆盖文章列出的所有项，而在于通过 trait + Hook + 合成语义提供一组正交、可组合的机制，让业务用最少的代码表达任意策略。**

判定标准：每加一个 trait 都应回答 ——

> "这个 trait 是否让业务的代码量显著减少，且让业务实现的接口更稳定？"

如果不能，就让业务自己写。**少即是多**，对 trait 抽象尤其如此。

---

## 参考文件

- `docs/runtime-v0.2-design.md` — v0.2 总体设计
- `crates/llm-harness-runtime/src/lib.rs` — 主 crate 模块结构
- `crates/llm-harness-runtime/src/tool_registry.rs` — ToolRegistry / ToolSource / ToolMetadata
- `crates/llm-harness-runtime/src/task.rs` — TaskRunner / TaskState / TaskVerifier
- `crates/llm-harness-runtime/src/budget.rs` — BudgetControlAdapter / BudgetExceededHook
- `crates/llm-harness-runtime/src/cost.rs` — PricingProvider / CostAggregate / TokenPrice
- `crates/llm-harness-runtime/src/subagent.rs` — SubAgentSpawner
- `crates/llm-harness-runtime/src/tracer.rs` — TraceExporter / SpanEvent / CaptureLevel
- `crates/llm-harness-runtime/src/tracing_adapter.rs` — TracingHookAdapter
- `crates/llm-harness-runtime/src/audit.rs` — AuditSink / hash 链
- `crates/llm-harness-runtime/src/human_approval.rs` — HumanApprover / HumanApprovalWrapper
- `crates/llm-harness-runtime/src/composite.rs` — CompositeHook 三种合成语义
- 外部文章：《从零设计生产级 Multi-Agent Harness》，李伟山，腾讯云开发者，2026-05
