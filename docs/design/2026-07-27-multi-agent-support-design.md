# 多 Agent 支持设计：runtime 层引入博弈回路

> 状态：提案（Proposal）
> 日期：2026-07-27
> 关联：`MISSION.md`（限制 6/7）、`ROADMAP.md`、`spawn/` 现有实现

---

## 一、动机：单 agent 的注意力收窄问题

单 agent 在复杂任务上有一个反复观察到的失效模式：**注意力过度集中于一个小点，在局部不断打补丁，导致整体架构退化**。

这不是"效率"问题（多 agent 几乎总是更费 token），也不是"数值纠错"问题（现有 `model_check_feedback` 已覆盖规格层面的 A–G 检查）。它是**方向纠偏**问题——agent 越深入越窄， sunk-cost 锚定在已采纳的假设上，没有外部视角把它拽回全局。

现有反馈机制全部是 step 级或数值级的：

| 机制 | 纠错层次 | 能否治"注意力收窄" |
|------|---------|-------------------|
| `model_check_feedback`（F/G 策略 + LLM FeedbackAdvisor） | 数值是否在 spec 内 | 否——只判"这步数值过没过" |
| `l4_checkpoint`（F-88 scaffold） | 确定性质量门 | 否——仍是规格层面 |
| `StepTransitionJudge::decide` → `To(step_id)` 返工 | step 级路由 | 否——返工因为"这步没过"，不是因为"整体设计跑偏" |
| `CompositeAfterTurnHook` 反思 | turn 级 | 部分——但反思者继承完整历史，被同一组假设锚定 |

**真空缺**：没有"停下来整体回看设计方向"的机制。judge 能返工到某 step，但触发条件是局部质量，不是架构方向。

---

## 二、第一性原理映射

按 `MISSION.md` 的六潜力 / 七限制框架：

| 本设计解除的限制 | 机制 |
|----------------|------|
| 限制 6：单次推理无迭代修正回路 | 独立 critic agent 提供外部纠错回路 |
| 限制 7：单实例无法自行分裂并行 | 多 agent 并行执行 + 汇聚 |

核心价值在**限制 6 的方向纠偏层**——这是现有机制未覆盖的层次。并行汇聚（限制 7）是附带收益，不是主要目标。

---

## 三、核心洞察

### 3.1 博弈 ≠ 建议

| | 建议模式 | 博弈模式 |
|---|---------|---------|
| 方向 | 单向（critic → doer） | 双向多轮 |
| 地位 | 不对等（审计员 vs 执行者） | 对等（分工不同的辩手） |
| doer 行为 | 可选采纳/忽略 | 必须回应（辩护或让步） |
| 碰撞 | 无 | 有——双方在对抗中可能改变立场 |
| 收敛性 | 可控（一次往返） | 不可预测（需显式收敛机制） |

关键区别：如果 doer 能随便忽略 supervisor 的意见，supervisor 就退化成审稿员，碰撞消失。博弈要求 doer **必须回应**质疑，supervisor 基于回应再质疑，直至收敛。

### 3.2 critic 的价值在独立上下文，不在"第二个脑子"

如果 critic 继承 doer 的完整对话历史，它被**同一组已采纳假设锚定**，确认偏误与 doer 相同——等于同一个脑子想两遍，增益接近零。

正确做法：critic 只看**产物快照**（当前代码结构 / 文件树 / 关键决策摘要）+ 评判 rubric，不看 doer 一步步怎么想的。这样它的注意力分配是独立的，能跳出 doer 的 sunk-cost 路径。

### 3.3 角色分工编码"立场"，不是"功能"

两个 agent 用相同 prompt 会 **herding**（从众收敛），LLM 默认倾向同意对方。角色分工 = 强制不同的立场偏向：

- ❌ 功能描述："你负责监督和提意见" → 模型输出空泛建议，doer 说"好的"，秒收敛
- ✅ 立场描述："找出方案里最薄弱的假设，用具体理由质疑，在找到真正缺陷前不要同意"

立场 prompt 同时是**反 sycophancy 杠杆**——LLM 默认太容易被说服，用立场掰向对抗。

### 3.4 角色分工 + 独立上下文，缺一不可

- 角色分工 → 视角不对称（治 herding）
- 不看过程 → 锚定独立（治 sunk-cost）

只做角色分工却让 critic 看 doer 完整思考过程 → 立场掰不动锚定。
只给独立上下文却不分角色 → 两个中立观察者，没有碰撞动力。

---

## 四、现状评估：已有的比想象的多

`spawn/` 不是空壳，是相当完整的主从通信基座：

| 组件 | 能力 |
|------|------|
| `HarnessSubAgentSpawner` | 每 sub-agent 独立 AgentHarness，provider 别名、env/session/plugin 工厂、`max_concurrent` 槽位控制 |
| `MessageBus` | agent 注册表（Running/Done/Aborted）+ 事件 channel + waiter（oneshot）+ `cancel_all` + idle timeout |
| 7 个 LLM 可见工具 | `SpawnAgent`（异步）、`MessageSubagent`/`MessageMain`/`AwaitSubagentReply`/`AwaitMainMessage`/`QuerySubagent`/`AbortSubagent` |
| `AsyncSpawnHook` | 每 turn 排空事件、注入 follow_up |
| `ProviderSelection::Alias` | sub-agent 可用不同模型（缓解同源偏见） |

**拓扑**：主从 hub-and-spoke，main 当协调者，sub 只跟 main 通信，sub↔sub 要中转。这是**正确的默认**——协调逻辑集中一处，避免 sub↔sub mesh 的发散和死锁。

**结论**：不是"要不要引入多 agent"，是"已有的 spawn 基座要不要往上长出博弈/汇聚能力"。

---

## 五、缺口分析

现有基座能拼出博弈，但缺四个博弈特有的机制：

### 5.1 收敛机制（真空缺，最关键）

博弈不像建议那样一次往返结束，必须回答"什么时候停"。现有 `max_concurrent` 管并发槽位，但没有：
- 辩论轮数上限
- 共识检测（双方都无新论点）
- 僵局裁决（裁判拍板）

不建则**死循环或假收敛**（一方放弃但嘴硬同意）。

### 5.2 主张结构化载体（真空缺）

自由文本对喷会发散——各说各的，没有共享的"被辩论对象"。需要结构化的主张-理由-反驳链作为碰撞载体，否则碰撞不出结论只碰出噪音。

### 5.3 里程碑触发点（真空缺）

架构级 review 是重操作，不能每步触发。需要：
- 每 N 个 step 完成后
- 累积变更超阈值时
- doer 主动请求 review 时

`workflow/gate.rs` 是天然的挂载点。

### 5.4 跨 agent 预算聚合（真空缺）

`control/budget.rs` 现在是 per-harness。多 agent 下 N 个 sub 同时烧预算，需要聚合控制，否则预算爆掉。

---

## 六、设计原则

1. **Hub-and-spoke**：main 当裁判/协调者（不参与辩论，只管收敛/裁决），subs 当辩手。保持现有拓扑，不建 sub↔sub mesh。
2. **立场不对称**：doer 偏"推进/辩护"，supervisor 偏"质疑/找薄弱点"。prompt 编码立场，不编码功能。
3. **独立上下文**：supervisor 用 `ContextMode::Inject(产物快照)`，不继承 doer 完整历史。
4. **收敛优先于深度**：有收敛机制才谈博弈。先建收敛器，再建辩论深度。
5. **验证驱动**：Phase 0 手动实测，证明有边际增益后再建机制。

---

## 七、层级归属（遵循"先定层级"）

| 内容 | 归属层 | 理由 |
|------|--------|------|
| 收敛器（轮数上限/共识检测/僵局裁决） | runtime | 通用，多个 agent 都用，与 `spawn/` 同层 |
| 主张结构化载体 | runtime | 通用原语 |
| 跨 agent 预算聚合 | runtime | `control/` 已有预算设施，扩展为聚合 |
| trace span parent↔sub 关联 | runtime（trace-otel） | 可观测性，通用 |
| 里程碑触发门 | runtime（`workflow/gate.rs`） | 通用 workflow 门控 |
| 角色 prompt / 博弈协议 / rubric | agent/组合层 | 领域相关，不能塞 runtime |
| 何时 fanout N 路 / 投票方案 | agent/组合层 | 策略，非原语 |

遵循现有切分：WorkflowEngine（通用） vs workflow_adapter（领域）。博弈原语进 runtime，博弈策略进组合层。

---

## 八、分阶段方案

### Phase 0：验证驱动（零工程，先做）

挑一个**记得单 agent 跑偏打补丁**的真实任务，手动用现有 spawn 起两个 sub-agent：
- doer 给方案
- supervisor 带独立理由反驳（只喂产物快照，不喂思考过程）
- 用现有消息工具往返 3–5 轮

观察三个问题：
1. 收敛了吗？
2. 收敛到的解比单 agent 好吗？
3. 是真碰撞，还是一方秒倒 / 死扛？

**通过标准**：能指出单 agent 当初的跑偏点。不通过则先想清楚为什么——输入给错了还是 rubric 缺了。

### Phase 1：runtime 补原语（Phase 0 通过后）

- `ConvergenceController`：轮数上限 + 共识检测 + 僵局裁决（裁判 = main）
- `Proposition` 结构化载体：主张-理由-反驳链
- 里程碑门：接入 `workflow/gate.rs`
- 跨 agent 聚合 `BudgetControl`
- trace-otel parent↔sub span 关联

### Phase 2：组合层模板（非 runtime）

- 可复用博弈 workflow 模板（generator-verifier / 多角色辩论）
- 放 agent/composition 层，带收敛终止
- 保持 hub-and-spoke

---

## 九、风险

1. **LLM sycophancy / 固执非对称**：模型可能太容易被说服（秒收敛到错答案），也可能太固执（永不收敛）。收敛行为不可预测——这是 multi-agent debate 在研究上证据最混合的原因。建议模式没这个问题。
2. **"碰撞"≠"更对"**：碰撞能产生双方单独想不到的解，也能产生双方单独都不会犯的错（互相带偏）。增益是经验性的、任务相关的。
3. **可靠性可能倒挂**：agent 越多 = 协调失败模式越多（发散、死锁、一个坏 agent 污染汇聚）。限制 2（护栏）和限制 3（可观测）在多 agent 下变难。
4. **架构审视无客观标准**：generator-verifier 在"代码能跑/测试过"上有客观验证，证据强；纯架构审视证据弱。critic 没有可操作 rubric 时，要么空泛，要么陷细节变成第二个打补丁的。**rubric 设计 > critic 本身**。

---

## 十、与 MISSION 的对齐

- 每个机制能回答"释放了哪个潜力 / 解除了哪个限制"：收敛器解限制 6（纠错回路），spawn 并行解限制 7。✓
- 业务是验证而非目的：Phase 0 用真实任务验证增益，不为建机制而建。✓
- 防偏航：不为"多 agent 热门"而加功能，先证明边际增益。✓
