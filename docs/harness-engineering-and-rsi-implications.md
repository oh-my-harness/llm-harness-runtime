# Harness Engineering 与 RSI：对 runtime 演进的指导

> 基于 Lilian Weng 的 harness engineering & RSI 研究综述，结合 `llm-harness-runtime` 现状分析。
> 目标：(1) 为 runtime 机制层演进指明方向；(2) 为上层建筑的策略层指明正确使用 runtime 的方式。

---

## 1. 文章核心论点

### 1.1 Harness 的定位

Harness 是围绕 base model 的系统，负责 orchestrate execution、决定 model 如何 think/plan/call tools/act、perceive/manage context、store artifacts、evaluate results。在 RSI 路径中，**deployment system（harness）和模型原始智能同等重要**。

Harness 相对早期 agent 框架（`agent = LLM + memory + tools + planning + action`）的进化在于额外包含：workflow design（loop engineering）、evaluation、permission controls、persistent state management。本质从 prompt templates 转向 runtime / software system design。

### 1.2 优化对象演进链

文章给出 harness 优化对象的演进路径：

```
instruction prompts → structured context → workflow → harness code → optimizer code
```

模型越强，优化目标越复杂、越通用。从优化"说什么"到优化"系统怎么运行"再到优化"怎么优化系统"。

### 1.3 关键设计模式

文章反复出现以下模式：

- **File system as persistent memory**：不把所有状态塞 context，durable state 存文件。LLM 的文件读写能力受益于 pretraining。
- **Sub-agent and backend jobs**：并行探索多假设、委派隔离子任务。并行要 explicit 且 inspectable——输出存文件/日志/状态记录，而非 transient chat。
- **Context as evolving playbook**：context 不是不断变长的 prompt，而是被策展（curated）的结构化条目。ACE 框架用 Generator/Reflector/Curator 三角色维护 playbook，Curator 产出 delta（增量条目）而非重写全文，确定性逻辑合并。
- **Code as universal harness language**：harness 是代码。LLM 能优化执行 agent 的代码，就能访问比手写 prompt 大得多的设计空间。
- **Bounded editable surface**：harness 自我改进时，可编辑表面必须被显式界定（如 AlphaEvolve 的 `# EVOLVE-BLOCK`）。permission control 和 security 层必须在 evolution loop 之外。
- **Search-based harness optimization**：ADAS（archive-based evolutionary）、AFlow（MCTS tree search）、DGM（harness-code repo evolution）等方法把 workflow/harness design 建模为搜索问题。

### 1.4 RSI 的约束与瓶颈

- **Base model 必须足够强**：STOP 实验表明 GPT-4 能递归改进，GPT-3.5/Mixtral 反而退化。harness 改进能释放模型能力，但智能本身是核心。
- **Evaluator 必须在 loop 之外**：reward hacking 是结构性风险。evaluator + permission control 应在 evolution loop 之外，配 held-out tests + trace audits + human review。
- **Diversity collapse**：进化/RL 倾向利用已知高 reward 模式，需要机制防止种群塌缩。
- **Negative results 的价值**：harness 应让失败尝试易于保留，学习失败是裁剪搜索空间的最佳方式。
- **人类应沿栈上移**：不是被移除，而是在正确的时间、正确的抽象层级提供 oversight。

---

## 2. runtime 现状评估

### 2.1 能力矩阵（存在性）

| 文章维度 | runtime 对应 | 存在 |
|---|---|---|
| workflow design / loop engineering | `WorkflowEngine` — staged workflow + back-edge + retry | ✅ |
| evaluation | `StepTransitionJudge` trait + `StepResult.structured` | ✅ |
| permission controls | `BeforeToolCallHook`（业务层直接实现人工审批）+ `WorkflowGateHook` + `RuleBasedApprovalHook` | ✅ |
| persistent state management | `TaskStore` + `Checkpoint` + `WorkflowState` | ✅ |
| memory / context management | `WorkflowContext` (blackboard) + `Session` + compaction | ✅ |
| tools | `ToolRegistry` + `ToolSource` + MCP | ✅ |
| planning | `PlanWorkflowTool` + LLM step | ✅ |
| action | `StepExecutor` + `Sandbox` | ✅ |
| file system as persistent memory | `Sandbox` cwd + `shell` executor | ✅ |
| sub-agent spawn + parallel | `spawn_async` + `MessageBus` + `delivery` | ✅ |
| agent delegation tools | `SpawnAgentTool` + `MessageSubagentTool`/`MessageMainTool` + `AwaitSubagentReplyTool`/`AwaitMainMessageTool` + `QuerySubagentTool` + `AbortSubagentTool` | ⚠️ 缺 resume_agent |
| audit trail | `AuditSink` + SHA-256 hash chain | ✅ |
| cost / budget control | `CostAggregate` + `BudgetControlAdapter` | ✅ |
| crash recovery | `WorkflowEngine.restore()` + `TaskStore` | ✅ |
| event-driven pause/resume | `EventStream` + `WaitForExternalEventTool` | ✅ |
| mechanism/policy separation | trait 声明在 types，实现在 runtime/后端 crate，策略靠 trait 回调注入 | ✅ |

### 2.2 质量评估（"有但够不够好"）

存在 ≠ 够用。以下逐个评估关键机制的实际实现质量，指出"有但不够好"的地方。每条都有代码级依据。

#### Q1：step_history 摘要注入——截断式，非策展式

**实现**：`format_step_history_summary`（engine.rs:443）把 `step_history` 格式化为文本，每个 step 取 output 前 1000 字符 + structured 前 4000 字符，截断后拼进 prompt 前缀。

**不够好**：
- **无选择性**：所有历史 step 等权注入，没有"哪些对当前 step 重要"的筛选。长 workflow（如 AgenticPD 20 iterations × 4 stages）中 history 会很长，截断后信息丢失。
- **无结构化检索**：LLM 拿到的是平铺文本，不能按 step_id、stage、QoR 指标检索特定历史。文章的 ACE 模式要求 context 是"evolving playbook"（有 identifier 的条目化结构），不是平铺日志。
- **单向注入**：history → prompt 是单向的，LLM 不能请求"给我 step X 的完整 structured result"——只能拿到截断摘要。

**文章对照**：ACE 的 Curator 产出 "(identifier, description)" 结构化条目，用确定性逻辑合并；MCE 的 context function $F_s$ 包含 search、selection、filtering 动态算子。runtime 的 history 注入只是 "append + truncate"，没有 selection 或 filtering。

**判断**：机制存在但不够好。长周期任务中会成为瓶颈。需要从"截断式注入"进化为"策展式注入"——但策展策略是应用层，runtime 只需提供按需检索历史 step 的机制（如 `StepResult` 查询接口）。

#### Q2：WorkflowContext——无类型、无 schema、无大小限制

**实现**：`WorkflowContext` 是 `serde_json::Map<String, Value>`（model.rs:20-24），纯 KV 黑板，无 schema 约束、无大小限制、无类型检查。

**不够好**：
- **无大小控制**：`format_context_vars`（engine.rs:469）把所有 KV 格式化进 prompt，无截断、无大小限制。如果应用层往 context 塞大对象（如完整 QoR report），prompt 会膨胀。
- **无 schema**：key 是任意 String，value 是任意 JSON。没有字段定义、类型约束或迁移机制。多个 step 约定 key 名靠口头共识，容易拼错或语义漂移。
- **无结构化查询**：LLM 拿到的是 `key: value` 文本平铺，不能按路径查询嵌套 JSON。

**文章对照**：ACE 的 playbook 是 "(identifier, description)" 条目列表，有结构；MCE 的 context function 有 search/selection/filtering 算子。runtime 的 blackboard 是无结构的 JSON blob。

**判断**：blackboard 的简洁性是优点（mutable、跨 step 共享），但缺少最低限度的结构化能力。至少需要：KV 值大小限制 + prompt 注入时的截断策略。

#### Q3：Checkpoint——通用 JSON blob，无阶段语义

**实现**：`Checkpoint` 是 `{seq, saved_at, description, payload: Value}`（task.rs:44-53）。`TaskStore.save_checkpoint` / `list_checkpoints` 是 append-only。引擎不主动创建 checkpoint——由应用层（executor/judge）通过 `WorkflowEngine` 的 `save_checkpoint` 方法写入。

**不够好**：
- **无阶段标记**：checkpoint 不记录属于哪个 stage、哪个分支、父 checkpoint 是谁。AgenticPD 的优化树需要 "从 node #7 的 CTS 阶段分支"——当前 checkpoint 没有 stage/parent 元数据。
- **无选择性 restore**：`list_checkpoints` 返回全部，没有按 stage/seq/时间范围过滤。恢复某个 checkpoint 的语义完全靠应用层。
- **引擎不管理 checkpoint**：runtime 只提供存取，不维护 checkpoint 之间的关系（树结构、分支历史）。

**文章对照**：AgenticPD 的 checkpoint reuse 需要从优化树的中间节点分支；AFlow 的 MCTS 需要回溯到树中任意节点。这些都需要 checkpoint 之间的关系元数据。

**判断**：作为通用存储机制够用，但缺阶段语义和关系元数据。是否在 `Checkpoint` 里加 `stage` / `parent_seq` 字段是值得考虑的——但分支管理策略仍属应用层。

#### Q4：sub-agent 结果收集——只有文本，无结构化回传

**实现**：`spawn_inner`（spawner.rs:309-399）的 event_task 收集 sub-agent 事件：取最后一个 `FinalAnswer` 的文本，回退到 `TextDelta` 拼接。`SubAgentResult` 包含 `{final_answer: String, tool_calls_count: u32, session_id: String, cost: CostAggregate}`。结构化数据只能从 `final_answer` 文本里解析。

**不够好**：
- **无结构化回传**：sub-agent 不能向主 agent 返回 JSON 结构化结果。`SubmitStepResultTool` 只在 workflow step 内可用，sub-agent 没有等价物。如果 sub-agent 是 Stage Agent 跑完一轮 EDA，它只能把 QoR 指标写进文本回答，主 agent 要自己解析。
- **无中间进度**：主 agent 只能拿到最终结果，看不到 sub-agent 的中间步骤。除非 sub-agent 主动通过 `MessageMainTool` 发消息。
- **session 可恢复但需手动**：`session_id` 被保留，但要恢复 sub-agent 的 session 需应用层手动构建。

**文章对照**：文章的 "sub-agent and backend jobs" 模式强调输出要 "stored as files, logs, and status records"——inspectable。runtime 的 session 落盘（`JsonlSessionFactory`）提供了可检视性，但结构化结果回传缺失。

**判断**：sub-agent 应有类似 `SubmitStepResultTool` 的结构化回传机制。这是明确的质量改进点。

#### Q5：compaction——session 级有，workflow context 级无

**实现**：`AgentHarness` 有成熟的 compaction 机制（`compaction.rs`）：token 预算估算 → cut point 决策 → LLM 生成结构化摘要 → session entry 替换。支持独立 compaction model（便宜模型做摘要）、`BeforeCompactHook` 覆盖、文件操作追踪。

**不够好**：
- **compaction 只管 session**：压缩的是 `Agent` 的对话历史。`WorkflowContext` 的 KV 黑板不参与 compaction——它随 `WorkflowState` 全量持久化，无压缩。
- **跨 step 无 context 压缩**：workflow 跑 20 iterations，每步往 `WorkflowContext` 累积数据，这些数据全量注入每步 prompt，无压缩。session compaction 只管单步内的对话历史。

**文章对照**：ACE 的 "context items are refined and deduplicated periodically"；文章把 "Context and memory lifecycle" 列为七大瓶颈之一。compaction 机制需要从 session 级扩展到 workflow context 级。

**判断**：session 级 compaction 质量很好。缺 workflow context 级的等价物。但 context 压缩策略是应用层——runtime 需要提供的是"机制"（如 context 条目的 TTL / 优先级 / 淘汰 hook），不是"策略"（什么时候压缩什么）。

#### Q6：AuditSink——事件记录有，但查询/分析无

**实现**：`AuditEntry` 有 timestamp、trace_id、session_id、task_id、principal、event_type、payload、decision、cost_delta、hash chain。`AuditSink` trait 只有 `record` + `flush`。

**不够好**：
- **只写不查**：`AuditSink` 没有 query 接口。要分析失败模式（Self-Harness 的 weakness mining），需要从 audit log 里检索特定 event_type、特定时间范围、特定 session 的记录。当前只能读全量 JSONL 文件自行解析。
- **无索引**：`AuditEntry` 有 trace_id 和 session_id，但 `AuditSink` 不提供按这些字段查询。
- **与 step_history 割裂**：`StepRecord` 在 `WorkflowState` 里，`AuditEntry` 在 `AuditSink` 里。两者的关联靠 task_id / session_id 间接对应，没有显式 join。

**文章对照**：Self-Harness 的 weakness mining 需要聚类失败为 patterns；文章的 "trace audits" 需要 queryable 的审计数据。runtime 有审计数据但没有查询分析机制。

**判断**：作为审计存储够用，但缺少查询接口。是否在 runtime 层提供 query 是一个边界判断——可以只定义 `AuditSink::query` trait method，实现留给后端 crate。

#### Q7：budget 控制——只管 cost，不管 iteration / time / compute

**实现**：`BudgetControlAdapter` 读 `CostAggregate` 的 `total_cost`，超限时调 `BudgetExceededHook`。`max_steps`（默认 100）和 `max_retries`（默认 5）是独立的硬限制。

**不够好**：
- **budget 只看钱**：只有 USD cost 维度。没有 iteration count budget（AgenticPD 的 20 iterations）、wall-clock time budget、compute resource budget（CPU hours）。
- **三种限制互相割裂**：`max_steps`（步数）、`max_retries`（单步重试）、`budget`（cost）是三个独立机制，没有统一的"预算"抽象。应用层要自己组合。

**文章对照**：AgenticPD 的 "20 iteration budget"、AFlow 的 "budget of B rounds"、AlphaEvolve 的 "maximum iteration count"——这些不是 cost budget，是 iteration budget。

**判断**：需要扩展 budget 概念到多维（cost + iterations + time），但具体哪些维度、超限后怎么办是策略层。runtime 提供多维 budget 的检查机制。

#### Q8：Sandbox——有隔离，无资源计费

**实现**：`Sandbox` trait 有 `start/reset/shutdown`，`SandboxConfig` 有 `fs_allowlist/denylist`、`net_allowlist`、`ResourceLimits`（CPU/memory/disk/timeout）。`shell` executor 在 sandbox 内执行命令。

**不够好**：
- **无实际资源计量**：`ResourceLimits` 声明了限制，但 sandbox 后端是否真正 enforce 取决于实现（`sandbox-os` / `bwrap` / `seatbelt`）。runtime 层不聚合 sandbox 的实际资源消耗。
- **无多 sandbox 管理**：如果并行跑多个 sub-agent 各用一个 sandbox，没有统一的资源池管理和计费。

**文章对照**：文章的 "process manager: launch jobs, inspect logs, cancel failed runs" 隐含对多个 sandbox 后端任务的管理。

**判断**：sandbox 隔离机制本身够用。多 sandbox 资源管理可能是上层建筑的职责，不一定是 runtime。但如果是 runtime 提供 `spawn_async`，它应该感知 sub-agent 的 sandbox 资源消耗。

#### Q9：EventStream——pause/resume 有，但恢复需应用层手动重建

**实现**：`WaitForExternalEventTool` 调用 `EventStream::next().await` 阻塞等待。等待时通过 `TaskStore.save_event_wait` 落盘 descriptor。崩溃恢复时业务层读 `load_event_wait` 重建 EventStream，调 `resume()`。

**不够好**：
- **恢复不是自动的**：runtime 明确声明 "runtime 不自动化恢复"。崩溃后需要业务层知道怎么从 descriptor 重建 EventStream。这对简单场景够用，但对复杂场景（多 event source、条件等待）是负担。
- **无超时**：`EventStream::next` 无超时参数。如果要 "等 5 分钟没事件就继续"，应用层要自己包装。

**文章对照**：文章的 "proactive requests to users for clarity" 和 "event-driven pause/resume" 需要可靠的恢复机制。

**判断**：机制设计合理（descriptor 落盘 + 业务层重建），符合机制/策略分离。但 EventStream trait 可以加 `next_with_timeout` 方法。恢复自动化可以由上层建筑封装，不一定要改 runtime。

### 2.3 已确认的技术 gap（缺失）

以下 gap 基于代码级验证，非推测。与 2.2 的质量评估互补：2.2 是"有但不够好"，2.3 是"完全没有"。

#### Gap-1：Retry 不保留 session——无法实现 step 内 iterative refinement

**事实**：`run_llm_step`（engine.rs:1268）每次创建全新 `Session::new(storage)`。`Transition::Retry` 触发 `run_step` 重跑，重建 harness + 新 session。LLM 只拿到 `step_history` 文本摘要 + `WorkflowContext` KV 前缀，看不到上一轮 retry 的对话内容。

**影响**：ACE 的 Reflector iterative refinement、ADAS 的 self-refine（生成反馈→基于反馈改进）、任何需要"在同一 step 内多轮改进"的模式，都无法在当前 retry 机制下保持对话连续性。LLM 每轮从零开始，只能靠 prompt 里注入的摘要理解上一轮。

**文章依据**：ACE 的 Reflector 有 "Iterative Refinement" 自循环；ADAS 的 proposer 有两轮 self-refine。这些都需要 session 级连续性。

#### Gap-2：WorkflowContext 无 provenance tracking——无法做 Chain-of-Evidence

**事实**：`WorkflowContext` 是 `serde_json::Map<String, Value>` 的 mutable KV blackboard。一个 key 的值可被任意 step 覆盖，不记录写入来源（哪个 step、哪个 tool call、基于什么输入）。

**影响**：ScientistOne 的 "每个 claim 必须追溯到 evidence source"、Chain-of-Evidence checks 无法直接实现。需要靠 `AuditSink` 事后重建，或应用层在 `StepResult.structured` 里手动标注来源。

**文章依据**：ScientistOne 把 verifiability 作为中心设计约束。

#### Gap-3：无完整 harness 配置序列化——无法进入搜索空间

**事实**：`Workflow` 可序列化（serde），`PlanWorkflowTool` 能落盘 workflow 定义。但完整的 runtime 实例配置（workflow + tool 注册 + hook 配置 + prompt 模板 + skill 定义 + budget 策略）没有统一的序列化格式。

**影响**：Meta-Harness、ADAS、AFlow、DGM 等方法的前提是 "harness config as serializable artifact"——harness 要能被导出为文件、被外层系统读写、被重新加载执行。当前只有 workflow 图结构可序列化，其余配置散落在代码里。

**文章依据**："once harness design becomes an executable search space, a strong coding agent can exploit the same design space human engineers use."

#### Gap-4：无并行 fan-out 原语——无法在 workflow engine 内原生并行探索

**事实**：`WorkflowEngine` 是单线遍历（`run_loop` 串行执行 step）。Issue #37（并行 fan-out / fan-in）未实现。`spawn_async` 可派发 sub-agent，但不在 workflow engine 的图模型内。

**影响**：AFlow 的 MCTS 同时展开多个候选、Autodata 的强弱 solver 同时跑、AgenticPD 的多分支并行探索——这些需要并行 fan-out。当前只能用 `spawn_async` + `MessageBus` 在 workflow 外变通，Judge 的协调逻辑要应用层手写。

**文章依据**：AFlow MCTS 的 Expand 可同时展开多节点；Autodata 的 Strong/Weak Solver 并行执行。

#### Gap-5：无 "editable surface" 标记机制——无法界定 harness 自我改进的边界

**事实**：`WorkflowGateHook` 按 step 收窄工具集（工具级收窄），但没有"代码级可编辑表面"标记。AlphaEvolve 的 `# EVOLVE-BLOCK-START/END`、Self-Harness 的 "bounded proposal context: editable surfaces" 没有对应物。

**影响**：如果上层要做 harness 自我改进（Meta-Harness / DGM 式），需要一种机制声明"runtime 配置的哪些部分可被 proposer 修改"。当前没有这层抽象。

**文章依据**："The editable surface needs to be properly designed and the permission control and security layers need to live outside this loop."

#### Gap-6：无 context lifecycle 管理——playbook 无过期/淘汰/去重机制

**事实**：`WorkflowContext` 的 KV 只增不删（除非应用层显式删除），没有 TTL、优先级淘汰、自动去重。compaction 机制压缩 session 历史，但不管理 `WorkflowContext` 的结构化条目。

**影响**：ACE 的 "context items are refined and deduplicated periodically"、MCE 的 skill 数据库跟踪历史——这些需要 context 条目的生命周期管理。长周期任务中 `WorkflowContext` 会膨胀。

**文章依据**："Context and memory lifecycle" 列为七大瓶颈之一；ACE 的 Curator 定期 refine + dedup。

#### Gap-7：无失败模式聚合——step_history 记录但不分析

**事实**：`step_history` 是 `Vec<StepRecord>` 的审计链，记录每步的 step_id、result、transition。`AuditSink` 记录 hash chain。但没有"聚类失败为 failure patterns"的原语。

**影响**：Self-Harness 的 weakness mining（聚类失败为 verifier-grounded failure patterns）需要应用层自行实现。runtime 提供数据但不提供分析。

**文章依据**：Self-Harness 的第一阶段就是 weakness mining。
## 3. runtime 演进方向（机制层）

遵循"机制与策略分离"原则，以下方向只增强 runtime 的**机制**能力，不预设具体**策略**。

### 3.1 优先级 P0：Session 续接（解决 Gap-1）

**目标**：让 `Transition::Retry` 或 `Transition::To(self)` 时可选保留 session 上下文。

**方案**：在 `Step::Llm` 定义中增加 `session_mode: SessionMode` 字段：
- `Fresh`（默认，当前行为）：每次创建新 session。
- `Persistent`：retry 时复用上一轮 session，LLM 看到完整对话历史。

**边界**：只提供机制（session 复用 or 不复用），是否复用由 workflow 定义者（策略层）决定。

**影响范围**：`workflow/model.rs`（Step 枚举扩展）、`workflow/engine.rs`（`run_llm_step` 按 session_mode 选择 session 构造方式）。

### 3.2 优先级 P1：Harness 配置序列化（解决 Gap-3）

**目标**：一个完整的 runtime 实例配置可导出为文件系统目录，也可从目录恢复。

**方案**：定义 `HarnessConfig` 序列化格式，包含：
- workflow 定义（已有 serde）
- tool 注册清单（tool name + source 标识）
- hook 配置清单（hook type + 参数）
- prompt 模板
- skill 引用
- budget / cost 配置

**边界**：runtime 提供 `export_config()` / `import_config()` 机制。具体哪些字段可序列化、序列化格式是策略层决定。tool/hook 的实际实现代码不在序列化范围（只存标识 + 参数）。

**影响范围**：新增 `config.rs` 模块；`HarnessBuilder` 增加 `from_config()` 入口。

### 3.3 优先级 P1：Context provenance（解决 Gap-2）

**目标**：`WorkflowContext` 的每次写入记录来源。

**方案**：扩展 `WorkflowContext` 的 value 包装为 `ContextEntry { value: Value, written_by: StepId, written_at: DateTime, prev_value: Option<Value> }`，或提供可选的 provenance tracking mode。

**边界**：runtime 提供写入追踪机制。是否启用、如何用 provenance 做验证，是策略层决定。

**影响范围**：`workflow/model.rs`（WorkflowContext 扩展）；`workflow/engine.rs`（写入时注入 step_id）。

### 3.4 优先级 P2：并行 fan-out / fan-in（解决 Gap-4）

**目标**：workflow 图模型原生支持一个 step 同时展开多个并行分支，结果汇总后继续。

**方案**：实现 Issue #37。参考 LangGraph Send API：一个 step 可声明 fan-out 到 N 个并行实例（不同参数），fan-in step 等待所有并行实例完成后汇总。

**边界**：runtime 提供并行执行 + 汇总机制。并行多少个、参数怎么分、结果怎么合并，是策略层（Judge / Executor）决定。

**影响范围**：`workflow/model.rs`（Step/Edge 扩展并行语义）；`workflow/engine.rs`（run_loop 支持并行 step 执行 + 汇聚）。

### 3.5 优先级 P2：Context lifecycle 管理（解决 Gap-6）

**目标**：`WorkflowContext` 的条目支持 TTL、优先级淘汰、去重。

**方案**：在 `WorkflowContext` 增加 optional metadata（created_at, last_read_at, priority）。提供 `Step::Executor` 内置 executor `context_gc` 做确定性清理。

**边界**：runtime 提供 lifecycle 元数据 + 清理 executor。什么时候清理、清理什么，是策略层决定。

### 3.6 优先级 P3：Editable surface 标记（解决 Gap-5）

**目标**：runtime 配置的各部分可被标记为 "editable" 或 "frozen"，供外层 proposer 识别可编辑边界。

**方案**：`HarnessConfig` 的每个字段增加 `editable: bool` 标记。

**边界**：runtime 提供标记机制。标记哪些字段可编辑，是部署策略决定。


### 3.7 质量改进（解决 Q1-Q9）

以下方向针对 2.2 节"有但不够好"的问题，是现有机制的增强，不是新机制。

#### 3.7.1 优先级 P1：step_history 按需检索（解决 Q1）

**目标**：LLM step 能按 step_id / stage / 指标检索历史 step 的完整 `StepResult`，而非只能拿到截断摘要。

**方案**：提供一个内置 tool（如 `query_history`），让 LLM step agent 可查询 `step_history` 中特定 step 的完整 `structured` 结果。或提供 `StepCtx` 的扩展接口让 Judge 可访问完整历史。

**边界**：runtime 提供检索机制。查询什么、怎么用检索结果，是策略层。

#### 3.7.2 优先级 P1：sub-agent 结构化回传（解决 Q4）

**目标**：sub-agent 能向主 agent 返回 JSON 结构化结果，不只是文本。

**方案**：为 sub-agent 提供类似 `SubmitStepResultTool` 的 `submit_result` tool。`SubAgentResult` 增加 `structured: Option<Value>` 字段。

**边界**：runtime 提供回传机制。结构化结果的 schema 由应用层定义。

#### 3.7.3 优先级 P1：WorkflowContext 大小限制（解决 Q2）

**目标**：`WorkflowContext` 的 KV 注入 prompt 时有大小限制，防膨胀。

**方案**：`format_context_vars` 增加总大小上限 + 单值截断。超过上限时保留按 key 排序的前 N 个，或标记截断。

**边界**：runtime 提供大小控制机制。限制多少、截断策略，是配置参数。

#### 3.7.4 优先级 P2：Checkpoint 阶段元数据（解决 Q3）

**目标**：`Checkpoint` 可携带阶段标记和父 checkpoint 引用。

**方案**：`Checkpoint` 增加 optional 字段 `stage: Option<String>` 和 `parent_seq: Option<usize>`。

**边界**：runtime 提供元数据字段。怎么填充、怎么用于分支管理，是应用层。

#### 3.7.5 优先级 P2：AuditSink 查询接口（解决 Q6）

**目标**：`AuditSink` 支持按 event_type / time range / session_id 查询。

**方案**：`AuditSink` trait 增加 `query` method（默认实现返回空或 Err）。后端 crate 按需实现。

**边界**：runtime 定义查询 trait。查询实现和索引策略由后端 crate 决定。

#### 3.7.6 优先级 P2：多维 budget（解决 Q7）

**目标**：budget 控制从单一 cost 维度扩展到 cost + iterations + time。

**方案**：`BudgetControlAdapter` 扩展为读多维预算（`BudgetLimit { cost: Option<f64>, iterations: Option<u32>, wall_clock: Option<Duration> }`）。`ShouldStopHook` 检查所有维度。

**边界**：runtime 提供多维检查机制。哪些维度、超限怎么办，由 `BudgetExceededHook`（策略层）决定。

#### 3.7.7 优先级 P2：workflow context compaction hook（解决 Q5）

**目标**：`WorkflowContext` 支持注册 compaction hook，在 context 过大时触发压缩。

**方案**：新增 `BeforeContextCompactHook` trait，在 `format_context_vars` 前检查 context 大小，超阈值时调 hook 让应用层做策展（删除/合并/摘要）。

**边界**：runtime 提供 hook 触发点 + 大小检查。压缩策略（删什么、怎么摘要）是应用层。

#### 3.7.8 优先级 P3：EventStream 超时（解决 Q9）

**目标**：`EventStream::next` 支持超时。

**方案**：`EventStream` trait 增加 `next_with_timeout(duration: Duration)` 方法，默认实现委托给 `next` + `tokio::time::timeout`。

**边界**：runtime 提供超时机制。超时后做什么，是策略层。
### 3.8 不做的事

以下明确**不在 runtime 职责内**，应由上层建筑实现：

| 不做 | 理由 |
|---|---|
| Meta-Harness / ADAS / AFlow 外层搜索循环 | 这是"优化 harness 的系统"，不是 harness 本身。runtime 是被优化的对象，不是优化器。 |
| Skill 数据库 + crossover agent | MCE 的 outer loop。runtime 提供 skill 注册/执行机制，不提供 skill 演化策略。 |
| 失败模式聚合 / weakness mining | Self-Harness 的第一阶段。runtime 提供 `step_history` + `AuditSink` 数据，分析逻辑是策略。 |
| Pareto frontier 选择 | 多目标评估策略。runtime 提供评估数据，选择算法是策略。 |
| Novelty 检查 / embedding 相似度 | 搜索策略。runtime 提供历史数据，判断新颖性是策略。 |
| 模型权重更新 | SIA 的 joint optimization。完全在 runtime 之外（训练系统）。 |
| 领域评估逻辑 | QoR 指标计算、post-route signoff 结算等。runtime 提供 `StepExecutor` + `StepTransitionJudge` trait，具体评估由应用层实现。 |

---

## 4. 上层建筑使用 runtime 的正确方式（策略层指导）

### 4.1 Context Engineering：用 runtime 原语实现 ACE

ACE 的 Generator/Reflector/Curator 三角色映射到 runtime workflow：

```
Step: generator    (Llm, 读 context.playbook)
    ↓
Step: reflector    (Llm, session_mode=Persistent, Transition::Retry 自循环)
    ↓
Step: curator      (Llm, 产出 delta items)
    ↓
Step: apply_delta  (Executor: json_transform, 确定性合并 delta → playbook)
    ↓
Step: cleanup      (Executor: context_gc, 定期 refine + dedup)
    ↓
Transition::To(generator)  ← back-edge
```

**关键原则**：
- Curator 产出 **delta 条目**（`StepResult.structured`），不重写全文。
- 用 **确定性 executor**（`json_transform`）合并 delta，不让 LLM 重写全局——防 context collapse 和 brevity bias。
- Reflector 用 **session_mode=Persistent**（待 Gap-1 修复后）保持 iterative refinement 的对话连续性。当前可用 `WorkflowContext` 传递上一轮输出作为变通。
- 定期 cleanup step 做 refine + dedup。

### 4.2 File System as Persistent Memory

**原则**：context 只留当前需要的，全量状态存文件。

**runtime 用法**：
- `WorkflowContext` 存轻量 KV（指针、引用、摘要），不存全量数据。
- Agent 通过 `Sandbox` 的 bash 工具读写文件。
- 优化树、实验日志、轨迹历史写文件，Judge 只读压缩后的 profile 进 context。
- `Checkpoint` 的 `payload` 存文件路径引用，不存全量 artifact。

### 4.3 Sub-agent 并行探索

**原则**：并行要 explicit 且 inspectable——输出存文件，不只在 transient chat。

**runtime 用法**：
- `spawn_async` 派发多个 sub-agent 并行探索不同参数。
- 每个 sub-agent 的 sandbox cwd 独立，结果写文件。
- 主 agent 通过 `MessageSubagentTool` 收集结果，或读 sub-agent 的输出文件。
- `MessageBus` 取消失败分支（`AbortSubagentTool`）。
- sub-agent 的 session 通过 `JsonlSessionFactory` 落盘，可事后 inspect。

### 4.4 Harness 自我改进（Meta-Harness / DGM 式）

**原则**：runtime 是被优化的对象，外层系统是优化器。

**架构**：
```
外层 Meta-Harness Loop（不在 runtime 职责内）
  ├── proposer agent（coding agent，用 runtime 跑）
  │     ├── 读文件系统 D（历史 harness 配置 + scores + traces）
  │     ├── 交叉/变异 → 新 HarnessConfig
  │     └── interface validation（用 runtime 的 validate_workflow）
  ├── evaluate（用 runtime 跑任务集）
  │     ├── WorkflowEngine::new(config) → engine.run()
  │     └── 收集 metrics（WorkflowEvent + AuditSink + CostAggregate）
  └── Pareto frontier 选择（应用层）
```

**runtime 需提供**（待 Gap-3 修复后）：
- `HarnessConfig` 序列化/反序列化。
- `validate_workflow` + 可扩展的完整配置校验。
- 标准化评估报告格式（step_history + cost + audit 的统一导出）。

**安全边界**：
- Permission control（`BeforeToolCallHook` 业务层直接实现）在 evolution loop 之外。
- Editable surface 标记（待 Gap-5 修复后）界定可修改范围。
- 新 harness 必须通过 regression test（held-in + held-out）才 accept。

### 4.5 失败保留与学习

**原则**：harness 应让失败尝试易于保留。

**runtime 用法**：
- `step_history` 记录所有 step（含 Failed/Retry），不丢弃。
- `AuditSink` 的 hash chain 保证失败记录不可篡改。
- 应用层在 `WorkflowContext` 里维护 `failure_log`，记录失败原因 + causal analysis。
- 外层搜索系统读 `step_history` + audit 数据做 weakness mining。

### 4.6 人类 oversight

**原则**：人类在正确的时间、正确的抽象层级介入。

**runtime 用法**：
- `BeforeToolCallHook`（业务层直接实现）在关键 tool call 前要求人工审批。
- `EventStream` + `WaitForExternalEventTool` 在关键决策点 pause workflow，等人工输入后 resume。
- `WorkflowEvent` 广播让人类实时监听进度。
- `BudgetControlAdapter` 在预算耗尽时停止，交人类决策。

---

## 5. 设计原则强化

基于文章论据，以下原则应在 runtime 后续开发中坚持：

### 5.1 机制与策略分离（最高优先级）

runtime 提供机制（workflow 遍历、hook 组合、spawn、persist、sandbox、audit），不提供策略（怎么路由、什么时候分支、怎么评估、怎么搜索）。策略通过 trait 回调注入（`StepTransitionJudge`、`StepExecutor`、`BudgetExceededHook`）。人工审批由业务层直接实现 `BeforeToolCallHook`，无需独立 trait。

**检验标准**：runtime 一行不改即可服务不同 agent（EDA / coding / research / data synthesis）。

### 5.2 刻意简单和通用

文章反复强调 harness 应 "deliberately simple and generic"，"fewer heuristic rules and more general mechanisms"。runtime 不预判模型能力边界，不预判领域需求。

**检验标准**：资深工程师会觉得这过于复杂吗？如果是，简化。

### 5.3 File system as first-class memory

context 不是垃圾桶。全量状态存文件，context 只留当前需要的。runtime 的 `WorkflowContext` 应保持轻量，`Sandbox` 文件系统是主要持久记忆。

### 5.4 可序列化、可比较、可恢复

harness 配置必须可被导出为文件、可被重新加载执行、可被外层系统比较。这是 harness 进入搜索空间的前提。

### 5.5 安全层在 evolution loop 之外

permission control、evaluator、human review 不在 harness 自我改进的循环内。editable surface 必须被显式界定。

### 5.6 OS 式封装

harness 像 OS——封装复杂逻辑，保持接口简单。trait 是系统调用，runtime 是内核，后端 crate 是驱动，应用层 agent 是用户程序。

---

## 6. 建议的开发路线图

路线图分两类工作：(G) gap 修补——补缺失机制；(Q) 质量改进——增强已有机制。

**依赖关系**：
- Phase 2 的 `workflow context compaction hook`（Q5）依赖 Phase 1 的 `WorkflowContext 大小限制`（Q2）——需要先有大小检测机制才能触发 compaction。
- Phase 2 的 `Context lifecycle 元数据`（Gap-6）和 Phase 1 的 `WorkflowContext 大小限制`（Q2）是互补关系——大小限制防膨胀，lifecycle 管理做主动清理。
- Phase 3 的 `AuditSink 查询接口`（Q6）间接解决 Gap-7（失败模式聚合）：runtime 提供查询机制，外层系统做聚类分析。
- Phase 3 的 `并行 fan-out`（Gap-4）理想情况下依赖 `State reducer`（Issue #40）做分支合并，但 State reducer 在 Phase 4。初期可用应用层在 fan-in step 手动合并。
- Phase 4 的 `时间旅行`（Issue #39）依赖 Phase 2 的 `Checkpoint 阶段元数据`（Q3）——需要 stage/parent 关系才能定位分支点。
### Phase 1：修补基础 gap + 关键质量改进（短期）

| 任务 | 类型 | 解决 | 影响 |
|---|---|---|---|
| Session 续接（`SessionMode::Persistent`） | G | Gap-1 | 解锁 iterative refinement 类 workflow |
| sub-agent 结构化回传（`submit_result` tool） | Q | Q4 | sub-agent 可返回 JSON，不再靠文本解析 |
| WorkflowContext 大小限制 + prompt 注入截断 | Q | Q2 | 防止 context 膨胀导致 prompt 爆炸 |
| step_history 按需检索（`query_history` tool） | Q | Q1 | LLM 可查完整历史，不再只有截断摘要 |

### Phase 2：进入搜索空间 + context 治理（中期）

| 任务 | 类型 | 解决 | 影响 |
|---|---|---|---|
| `HarnessConfig` 序列化格式 + export/import | G | Gap-3 | 解锁 Meta-Harness / ADAS / DGM 类外层搜索 |
| Context provenance tracking | G | Gap-2 | 解锁 Chain-of-Evidence 类验证 |
| workflow context compaction hook | Q | Q5 | context 过大时触发应用层策展 |
| Context lifecycle 元数据 + `context_gc` executor | G | Gap-6 | 解锁长周期任务的 context 管理 |
| Checkpoint 阶段元数据（`stage` / `parent_seq`） | Q | Q3 | 支持优化树的分支关系追踪 |
| 多维 budget（cost + iterations + time） | Q | Q7 | 统一的预算控制，不再只看钱 |
| Editable surface 标记 | G | Gap-5 | 界定 harness 自我改进的安全边界 |
| 标准化评估报告导出 | — | — | 让外层系统可比较不同 harness 实例 |

### Phase 3：并行与高级编排（中长期）

| 任务 | 类型 | 解决 | 影响 |
|---|---|---|---|
| 并行 fan-out / fan-in（Issue #37） | G | Gap-4 | 解锁 MCTS、多 solver 并行、多分支探索 |
| AuditSink 查询接口 | Q | Q6 | 解锁 failure pattern 分析、trace audit |
| EventStream 超时 | Q | Q9 | 等事件不再无限阻塞 |
| 子图嵌套（Issue #38） | — | — | 解锁 stage 内子 workflow |
| `resume_agent` 工具 | — | — | 补全 agent delegation 工具集 |

### Phase 4：远期（探索性）

| 任务 | 类型 | 方向 |
|---|---|---|
| 时间旅行 / checkpoint history（Issue #39） | G | 支持 AgenticPD 式从任意中间节点分支 |
| State reducer（Issue #40） | G | 并行分支 state 合并 |
| 运行时动态 workflow 修改 | G | 支持 MCE 式 skill 在运行时被替换 |
| Sandbox 资源计量与多实例管理 | Q | Q8 | 多 sub-agent sandbox 的资源聚合 |
---

## 附录：文章方法与 runtime 的映射速查

| 文章方法 | 核心机制 | runtime 支撑 | 外层系统职责 |
|---|---|---|---|
| ACE (Zhang et al. 2025) | Generator/Reflector/Curator + evolving playbook | workflow + back-edge + executor + context | 三角色的 prompt 设计 + delta 合并逻辑 |
| MCE (Ye et al. 2026) | bi-level optimization: skill evolution + context optimization | skill 注册/执行 + workflow + TaskStore | skill 数据库 + crossover agent + 双层优化策略 |
| Meta-Harness (Lee et al. 2026) | harness 优化 harness，Pareto frontier | HarnessConfig 序列化(待) + validate_workflow + audit | proposer agent + 评估 + Pareto 选择 |
| ADAS (Hu et al. 2025) | meta-agent search，archive-based | Workflow 序列化 + PlanWorkflowTool | meta-agent + archive 管理 + novelty 检查 |
| AFlow (Zhang et al. 2025) | MCTS 搜索 workflow 图 | Workflow 图模型 + engine.run() + TaskStore | MCTS 选择/展开/回传策略 |
| DGM (Zhang et al. 2025) | 进化 harness-code repo | Sandbox(bash+editor) + HarnessConfig(待) | 进化循环 + parent 选择 + 评估 |
| Self-Harness (Zhang et al. 2026) | weakness mining + bounded proposal + validation | step_history + AuditSink + `BeforeToolCallHook`（业务层实现审批） | failure 聚类 + proposal + regression test |
| STOP (Zelikman et al. 2023) | 递归改进 improver 函数 | runtime 是被改进的 $I$ | 递归循环 + meta-utility 评估 |
| AlphaEvolve (Novikov et al. 2025) | evolutionary search + EVOLVE-BLOCK | Sandbox + file system | 进化循环 + parent sampling + novelty rejection |
| Autodata (Kulikov et al. 2026) | challenger + weak/strong solver + verifier | spawn_async + MessageBus + multi-provider | 角色协调 + reward 逻辑 + training data 输出 |
| AI Scientist (Lu et al. 2026) | ideation → experimentation → write-up pipeline | Workflow(staged) + back-edge + shell executor | 领域 prompt + 阶段间逻辑 |
| AgenticPD | stage-aware optimization tree + checkpoint reuse | Workflow + Sandbox + spawn + context | 优化树管理 + Judge 导航 + QoR 评估 |
