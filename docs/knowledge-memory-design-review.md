# 对 Knowledge/Memory 目标架构设计的 Review 意见

> 评审对象：《Agent 业务知识与 Memory 目标架构》
>
> 评审日期：2026-07-22
>
> 核心结论：设计的技术方向正确，但落地路径需要重构——Phase 0 太大，导致零 Knowledge 价值交付直到 runtime 重写完成。建议改为增量路径，先让 Knowledge 核心能力跑起来，runtime 升级推后。

---

## 〇、实现原则：宏大设计，小步快跑

> **设计可以宏大，实现必须小步。**
>
> 目标架构作为方向指引保留，但落地时必须拆成能尽快合并进主干的小步骤，每一步都独立可验证、独立可合并。严禁全做完了搞一个超大 merge。

具体要求：

1. **每个 Phase 内部再拆成可独立合并的小 PR**。一个 Phase 不是一次大 merge，而是多个小 PR 的序列。例如 Phase A 应至少拆为：(1) KnowledgeSource trait + 类型定义；(2) LocalDocumentSource 实现；(3) knowledge_search / knowledge_read Tool 注册。每个 PR 都能编译、能测试、能合并。
2. **优先合并到主干，而不是长期 feature branch**。过渡方案也合并进主干，用 feature flag 或配置项控制开关。不要在分支上攒几个月的代码。
3. **每一步都要有可验证的交付物**。不是"框架就绪"，而是"Agent 能搜文档并带引用回答"这样的端到端可观测行为。
4. **过渡方案不是技术债，是验证手段**。Phase C 的 message 注入、Phase A 的全文进 Session，都是在真实使用中验证假设的方式。Phase D 替换它们时，已经有数据支撑替换决策。
5. **拒绝"等全部做完再合并"**。如果某个 PR 依赖未完成的重构，先找不依赖该重构的最小切片，合并它，再迭代。宁可合并一个粗糙但能跑的版本，也不要等待一个完美但永远不来的大 merge。
6. **每做完一个小功能，用 `../llm-tutor` 通过功能迁移来验证**。llm-tutor 是基于 llm-harness-runtime 构建的 AI 学习工作区，已有 RAG 知识库（`tutor-rag`）和 Agent（`tutor-agent`）。每完成一个 Knowledge/Memory 小功能，就把 llm-tutor 的对应能力迁移到新协议上——例如 Phase A 做完 `knowledge_search`/`knowledge_read`，就让 llm-tutor 的文档搜索从现有 RAG 迁移到 `KnowledgeSource` 协议。用真实业务场景验证，不是写个 demo 自测就完事。这既是验证手段，也是迁移压力测试：如果 llm-tutor 迁移不过去，说明协议设计有问题。

---

## 一、设计的亮点（应保留）

以下决策是正确的，建议在修订中保留：

1. **Search/Read 分离**：避免一次 top-k 全文注入上下文，让 Agent 逐步缩小范围。
2. **Evidence 与 Citation 分离**："查到过某篇文档" ≠ "答案有证据"，概念区分到位。
3. **权限属于 run 不属于 Plugin**：fail-closed，不依赖 prompt 文本猜测身份。
4. **明确拒绝的方案章节**：防止 scope creep，团队对齐。
5. **LocalDocumentSource 作为首个垂直切片**：不需要外部基础设施，验证协议端到端。
6. **Knowledge 与 Memory 分层**：只读协议和可写能力分离，职责清晰。

---

## 二、核心问题：Phase 0 阻塞所有价值交付

### 问题描述

当前设计的分阶段实现是：

```
Phase 0: 重写 run 生命周期 → Phase 1: Knowledge 协议 → Phase 2: 路由与自动召回 → Phase 3: Memory
```

Phase 0 要求一次性完成：
- `RunRequest` / `RunContext` / `RunRecord` / `RunOutcome`
- 原子状态机（Idle → Preparing → Turning → Finalizing）
- runtime-owned `RunHandle` / `RunCompletion`
- typed `RunExtensions` + `RunLocalState`
- `DataBlock` / `ContextAttachment` 与 provider conversion
- `ToolResult` / `ToolFailure` Session Projection
- 带 `RunContext` 的 Hook / `ToolContext`
- per-run usage delta

**在 Phase 0 完成前，零 Knowledge 价值交付。**

### 当前代码库现状

对照现有代码：

| 设计要求 | 当前代码 | 差距 |
|---|---|---|
| `RunRequest` / `RunContext` | `prompt(text)` 直接进 loop | 完全没有 |
| 原子状态机 (Preparing→Turning→Finalizing) | `AgentPhase::Idle \| Running` | 状态太少 |
| typed `RunExtensions` | 无 | 完全没有 |
| `DataBlock` / `ContextAttachment` | `AgentContext { system_prompt, messages }` | 完全没有 |
| `ToolResult` Session Projection | `ToolResult { content, details, terminate }` | 完全没有 |
| `ToolContext` 带 `RunContext` | `ToolContext { env, abort, tool_use_id, ... }` | 没有 run 上下文 |
| `BeforeRunHook` 带结构化输入 | 从 messages 里猜 `prompt_text` | 原始 |

Phase 0 本质上是 runtime v2 重写。生命周期 + 状态机 + provider conversion + session projection 都是容易延期的模块。如果 Phase 0 延期，整个 Knowledge/Memory 全部被阻塞。

### 核心洞察

设计自己说：

> 如果业务只有一个简单知识库，Agent 最终看到的仍然可以只是 `knowledge_search` 和 `knowledge_read` 两个 Tool。

而 `knowledge_search` / `knowledge_read` 作为普通 `Tool` impl，走现有 `Tool` trait，**完全可以在当前 `AgentHarness` 上实现**，不需要重写生命周期、不需要 transient attachment、不需要 Session Projection。

当前 `loop_driver.rs` 中 Tool 结果写入 Session 的逻辑是：

```rust
// ToolExecutionEnd: 直接把 result.content 全量写入 ToolResultMessage → Session
let (content, is_error) = match result {
    Ok(r) => (r.content.clone(), false),
    ...
};
let msg = AgentMessage::ToolResult(ToolResultMessage { tool_use_id, content, is_error, timestamp });
self.push_pending_write(SessionEntryPayload::Message(msg));
```

Knowledge Tool 返回 `ToolResult { content, details, terminate }`，走这条现成路径即可。文档全文会进 Session（bloat 问题留到后期 Session Projection 解决），但功能完全跑得通。

---

## 三、建议的增量路径

```
Phase A: KnowledgeSource + Tools（最小核心）
    ↓ 交付：Agent 能搜文档、读章节、带引用回答
Phase B: Memory
    ↓ 交付：Agent 能记住/遗忘用户偏好
Phase C: 自动召回
    ↓ 交付：问题进来自动召回相关资料
Phase D: Runtime 升级（替换过渡方案为目标架构）
    ↓ 交付：transient attachment + session projection + 原子生命周期
```

### Phase A：KnowledgeSource + 工具（最小核心）

**做什么**：
- 新 crate `llm-harness-runtime-knowledge`
- 定义 `KnowledgeSource` trait、`KnowledgeRef`、`KnowledgeHit`、`KnowledgeContent`、`KnowledgeError`
- `knowledge_search` / `knowledge_read` 作为普通 `Tool` impl（走现有 `Tool` trait）
- `LocalDocumentSource`（Markdown/Text + BM25 + section read）
- 简单 citation：Tool 返回带 ref 的内容，模型在答案中引用
- **单租户，不做 auth**：所有文档对所有人可见，等 Phase D 再加 `KnowledgeAccessContext`

**不做什么**：
- 不做 transient attachment（工具结果走现有 `ToolResult.content`）
- 不做 Session Projection（沿用当前行为，全文进 session）
- 不做 Router / Planner / Coordinator / Assembler（单源不需要）
- 不做自动召回

**交付价值**：Agent 能搜本地文档、读指定章节、带引用回答。纯文档场景端到端跑通。

**改动范围**：纯新增 crate + 通过 `AgentHarnessOptions.tools` 注册两个 Tool。不修改现有 runtime 代码。

**拆分示例**（体现"小步快跑"原则）：
1. PR1：`KnowledgeSource` trait + `KnowledgeRef`/`KnowledgeHit`/`KnowledgeContent`/`KnowledgeError` 类型定义 + 单元测试
2. PR2：`LocalDocumentSource` 实现（文件读取 + Markdown section 解析 + BM25 搜索）
3. PR3：`knowledge_search` / `knowledge_read` Tool impl + 注册示例 + 端到端测试

### Phase B：Memory

**做什么**：
- `MemoryStore` trait——**建议不继承 `KnowledgeSource`**，改为组合关系（详见问题二）
- `memory_write` / `memory_forget` 作为普通 Tool
- 简单文件或 SQLite 后端
- Memory 作为 `KnowledgeSource` 注册，可被 search/read

**交付价值**：Agent 能跨 session 记住和遗忘用户偏好。

### Phase C：自动召回

**做什么**：
- 用现有 `BeforeRunHook` 做自动检索
- 检索结果作为 additional messages 注入（过渡方案）
- 或者在 `AgentContext` 上加 `attachments: Vec<ContextAttachment>` 字段（小改动）

**交付价值**：问题进来自动召回相关资料，不需要 Agent 显式调 Tool。

**关于过渡方案的说明**：设计明确拒绝了"把检索结果伪装成 additional message"。但过渡方案的价值在于验证检索质量和召回策略，同时不阻塞 runtime 重写。Phase D 完成后替换为目标 transient attachment。

### Phase D：Runtime 升级

**做什么**：
- `RunRequest` / `RunContext` / `RunRecord`
- 原子状态机
- 真正的 transient attachment（替代 Phase C 的过渡方案）
- `ToolResult` Session Projection
- `RunHandle` / `RunCompletion`
- `KnowledgeAccessContext` + 多租户 auth

**交付价值**：安全性、可观测性、并发正确性。把前三个阶段的过渡方案替换为目标架构。

**代价**：Phase D 时要替换 Phase C 的过渡方案，有一次性迁移工作。但此时已经验证了检索质量和业务价值，迁移有明确动力。

### 增量路径 vs 原设计对比

| 维度 | 原设计 | 增量路径 |
|---|---|---|
| 首个价值交付点 | Phase 0 完成后（runtime 重写完成） | Phase A 完成后（新增 crate） |
| 是否有过渡层 | 无，一步到位 | 有（Phase C 的 message 注入） |
| 风险 | Phase 0 延期 → 全部阻塞 | Phase A 独立交付，不依赖 runtime 重写 |
| 迁移成本 | 无 | Phase D 替换过渡方案（一次性） |

---

## 四、其他设计问题

### 问题 1：Session Projection 强制所有 Tool 改造

设计说"每个 Tool 必须显式构造 Session Projection，核心不为旧 Tool 提供隐式 `Full` 默认"。

**问题**：当前 `ToolResult` 只有 `{ content, details, terminate }`。要求所有现有 Tool（BashTool、ReadTool、EditTool、WriteTool 等）都新增 Session Projection 字段并理解 `Full`/`Projected`/`Ephemeral` 三种投影，是 big bang 迁移。设计没有提到迁移策略。

**建议**：
- 提供隐式 `Full` 默认，保持现有 Tool 不变
- 只要求 Knowledge Tool 显式构造 Projection
- Phase D 再考虑是否收紧为强制显式

### 问题 2：MemoryStore trait 继承 KnowledgeSource 可能过度耦合

```rust
pub trait MemoryStore: KnowledgeSource {
    fn upsert(...) -> ...;
    fn delete(...) -> ...;
}
```

**问题**：这要求每个 MemoryStore 实现都必须同时实现 search/read/upsert/delete。但如果 Memory 后端只支持写入和简单遍历（比如 KV store），不支持复杂 search？或者 read 和 write 使用不同后端（读用 Elasticsearch，写用 PostgreSQL）？

**建议**：改为组合关系，`MemoryService` 内部持有 `KnowledgeSource`（读）和 `MemoryStore`（写）两个 trait，分别实现。

### 问题 3：Subagent delegation 机制未定义

设计说"子 Agent 的 KnowledgeAccessContext 必须通过 Authorizer 生成 delegation，并且只能收窄"，但 `KnowledgeAuthorizer` trait 只有 `authorize(access, action, resource)` 方法，没有 `delegate` 方法。怎么生成收窄后的 access context？

**建议**：补充 `delegate` 方法或独立 `Delegator` trait，明确收窄语义和实现方式。

### 问题 4：Evidence 信任边界模糊

Domain Tool 通过 `ToolResult.extensions` 放入 `KnowledgeEvidenceSet`，Knowledge Plugin 的 AfterToolCallHook 按类型读取。但任何 Tool 都可以产生 evidence。如果不可信 Tool 伪造 evidence，它会获得 citation handle 吗？

**建议**：明确 evidence 来源的信任级别。只有注册了 KnowledgeEvidenceProvider 能力的 Tool 才能产生 evidence，或增加来源签名机制。

### 问题 5：缺少迁移和测试策略

- **迁移**：设计明确说"不以保持现有内部 API 兼容为约束"，但当前代码库有真实的 Tool、Hook 和 Session 消费者。没有迁移计划。
- **测试**：验收标准有 18 条很好，但安全属性（"权限泄漏必须为零"）怎么测？没有提到 fuzzing、property-based testing 或对抗性测试。

**建议**：
- 补充现有 Tool 和 Hook 的迁移计划
- 补充安全属性的测试策略（property-based testing、对抗性测试用例）
- 增量路径下，每个 Phase 都应有可独立验证的验收标准

---

## 五、总结

设计的技术方向和概念模型是对的，问题在于**落地路径**：

1. **Phase 0 太大**：重写 runtime 生命周期是 Knowledge 的硬前置，导致零价值交付直到重写完成。
2. **应该增量**：KnowledgeSource 协议和 `knowledge_search`/`knowledge_read` Tool 可以在现有 `AgentHarness` 上先跑起来，不需要等 runtime 重写。
3. **过渡方案可接受**：Phase C 的 message 注入虽然不是目标架构，但能验证检索质量和召回策略，值得为它付一次性迁移成本。
4. **几个技术细节需要补充**：Session Projection 迁移策略、MemoryStore trait 设计、Subagent delegation 机制、Evidence 信任边界、测试策略。
5. **实现必须小步快跑**：设计可以宏大，但每个 Phase 内部必须再拆成可独立合并的小 PR，优先合并到主干，拒绝超大 merge。
6. **用 llm-tutor 迁移验证**：每做完一个小功能，就把 `../llm-tutor` 的对应能力迁移到新协议上。llm-tutor 已有 RAG 知识库和 Agent，是现成的真实业务验证场景——迁移不过去就说明协议设计有问题。

建议将分阶段实现从：

```
Phase 0 → Phase 1 → Phase 2 → Phase 3 → Phase 4
```

改为：

```
Phase A (KnowledgeSource + Tools) → Phase B (Memory) → Phase C (自动召回) → Phase D (Runtime 升级)
```

让 Knowledge 核心能力尽快落地验证，runtime 升级作为最后一步替换过渡方案。
