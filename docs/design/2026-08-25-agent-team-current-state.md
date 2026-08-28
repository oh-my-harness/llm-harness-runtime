# Agent Team 当前状态

> 日期：2026-08-25
> 状态：现状梳理（基于代码 + 设计讨论）
> 关联：`origin/agent-society` 分支的白皮书和基础设施设计文档；架构评审见 `2026-08-27-agent-team-architecture-review.md`

---
## 1. 定位

`llm-harness-agent-team` 是三驾马车的第三驾：

| Crate | 拓扑 | 核心模型 |
|---|---|---|
| `llm-harness-subagents` | 层级式（main ↔ sub） | Spawner 显式 spawn/join，MessageBus 通信 |
| `llm-harness-workflow` | 流程编排（DAG） | WorkflowEngine 驱动 Step → Judge → Transition |
| `llm-harness-agent-team` | peer-to-peer 自治 | AgentRunner 自驱循环，SocialContext 共享状态 |

三者各自独立，不共享基础设施。agent-team 不依赖 runtime（subagents/workflow），直接依赖 agent/loop/types/strategy 等下层 crate。

原始设计来自 `agent-society` 分支（lxn，已废弃）的白皮书和基础设施设计文档。agent-team 是重新审视后的第二版实现，保留了核心思想（社交网络拓扑、有界通讯录、关系动力学、非阻塞委派），但重新设计了实现方式。

## 2. 核心原则

- **个体是主体，团队是平台。** 每个 agent 自治运转，没有中心调度器。AgentTeam 层提供共享状态和社交工具，不干预个体内部决策。
- **AgentHarness 是唯一原子单元。** `IndividualSpec` 是配置声明，框架物化为 `AgentHarness` 实例。
- **声明式开发者体验。** 开发者定义 spec + 组合 = team，不写通信/路由/调度代码。
- **所有信息源统一进 inbox。** 委派结果、超时通知、广播回复、operator 输入——全部走同一条 inbox 优先队列路径。

## 3. 架构

```
┌─ Developer API ─────────────────────────────────────┐
│  IndividualSpec × N → AgentTeam::new().build()      │
│    → submit() / receive() / shutdown()              │
├─ Team Layer (llm-harness-agent-team) ───────────────┤
│                                                     │
│  SocialContext（共享状态，Arc，廉价 Clone）           │
│    AgentRegistry / InboxStore / ContactStore /      │
│    RelationEngine                                   │
│                                                     │
│  Social Tools（注入每个 agent）                      │
│    search_contacts / send_message / broadcast       │
│    delegate_task / set_reminder / todo_write        │
│    memory_write / knowledge_search（T15 起安装）     │
│    spawn_agent 工具集（仅 allow_subagents(true)）    │
│                                                     │
│  Agent 自驱循环（每个 agent 一个 tokio task）         │
│    idle → wait_for_message → drain inbox →          │
│    harness.run() → [monitor task: drain → steer]    │
│    → idle                                           │
│                                                     │
├─ Building Blocks（现有 crate，不改）─────────────────┤
│  AgentHarness  Memory  Knowledge  Tools  MCP       │
│  Sandbox  Strategy  Types  Loop                    │
└─────────────────────────────────────────────────────┘
```

## 4. 核心组件

### 4.1 AgentId

UUIDv7 新类型，时间有序 + 唯一。两种来源：开发者指定（`IndividualSpec::new("architect", ...)`）或运行时生成（`AgentId::generate()`）。

### 4.2 IndividualSpec

开发者的配置声明。内部持有一个 `HarnessBuilder`，通过 `configure(|b| ...)` 闭包配置 builder（system_prompt、tools、max_tokens、auto_compact 等）。builder 方法是消费式的（`fn xxx(mut self) -> Self`），无法通过引用调用，因此不做 `Deref` 透传。

```rust
pub struct IndividualSpec {
    id: AgentId,
    expertise: String,           // 自然语言专长描述
    builder: HarnessBuilder,     // 完整 AgentHarness 配置（model 经 new() 传入）
    reflection_interval: usize,  // 经验沉淀间隔（0=禁用）
    allow_subagents: bool,       // 是否安装 sub-agent spawn 工具集（默认 false）
}
```

### 4.3 SocialContext

所有 agent 共享的状态。全部是 `Arc`，`Clone` 廉价。社交工具通过它读写状态。

```rust
#[derive(Clone)]
pub struct SocialContext {
    registry: Arc<AgentRegistry>,
    inboxes: Arc<InboxStore>,
    contacts: Arc<ContactStore>,
    relations: Arc<RelationEngine>,
    max_contacts: usize,
}
```

**方法分类：**
- Agent 注册：`register_agent` / `get_agent_info` / `update_status`
- 消息投递：`send_direct` / `broadcast` / `push_message` / `drain_inbox` / `has_messages` / `wait_for_message`
- 通讯录：`list_contacts` / `has_contact` / `add_contact` / `ensure_contact` / `touch_contact`
- 配置访问：`initial_strength` / `relation_engine`

### 4.4 InboxStore

Per-agent 优先队列，线程安全，支持异步等待。

```rust
pub struct InboxMessage {
    pub from: AgentId,
    pub to: AgentId,
    pub body: String,
    pub message_type: MessageType,
    pub priority: u32,              // 数值越大越优先
    pub task_id: Option<String>,    // 关联委派任务 ID
    pub broadcast_id: Option<String>,
}
```

**MessageType 枚举：** Direct / DelegationRequest / Broadcast / Timeout / SelfReminder / ReflectionTrigger

**优先级约定：**

| 消息类型 | priority | 来源 |
|---|---|---|
| operator 任务投递 | 100 | `AgentTeam::submit()` |
| Timeout | 50 | `TimeoutManager::set_timeout()` |
| SelfReminder | 30 | `set_self_reminder()` / `set_reminder` 工具 |
| Direct（普通） | 0 | `send_direct()` / `send_message` 工具 |
| DelegationRequest | 1 | `delegate_task` 工具 |
| ReflectionTrigger | 10 | `push_message(ReflectionTrigger)`（反思触发） |

**并发模型：** **Per-agent `tokio::sync::Notify`**（与 InboxQueue 同存）：推消息只唤醒接收方自己的 waiter，其他 agent 的等待者不会被误唤醒。250ms 轮询兜底保留，覆盖 check-then-await 竞态窗口。

### 4.5 AgentRegistry

`HashMap<AgentId, AgentInfo>`，O(1) 查找 + 全队遍历（`list()`，供 broadcast 使用）。

```rust
pub enum AgentStatus { Idle, Busy, Stopped }

pub struct AgentInfo {
    pub id: AgentId,
    pub expertise: String,
    pub status: AgentStatus,
}
```

### 4.6 ContactStore 与 ContactIndex

每个 agent 维护有界联系人列表。`ContactStore` 是线程安全外壳，内部按 `AgentId` 分区，每个分区是一个 `ContactIndex`。

```rust
pub struct ContactRecord {
    pub peer_agent_id: AgentId,
    pub peer_expertise: String,        // peer 的专长描述（添加时从 registry 查）
    pub strength: f64,                  // 关系强度 [0.0, 1.0]
    pub last_interaction_tick: u64,     // 上次与该 peer 通信时 agent 自己的 tick 值
}
```

**Per-agent tick：** 每个 agent 有自己的 tick 计数器，记录该 agent 参与了多少次通信活动。tick 只在该 agent **发起通信时** +1（在 `apply_relation_event` 的 `advance_tick()`），不因其他 agent 的通信而推进。不活跃的 agent 不会因为别人的通信而衰减自己的关系强度。

**衰减公式：** 对 agent 的每个联系人 c：
```
tick_gap = agent.tick - c.last_interaction_tick
c.strength = (c.strength - decay_per_tick * tick_gap).clamp(0, 1)
```

### 4.7 RelationEngine

纯计算，无状态，无 LLM 调用。调用方传入当前 strength 和事件，返回更新后的 strength。

| 事件 | delta | 语义 | 生产触发点 |
|---|---|---|---|
| `SuccessfulCollaboration` | +0.15 | 协作成功，大幅增强 | 发起者 drain inbox 时取消带 peer 的 delegate 超时（回复及时到达）→ 从发起者向被委派者发事件 |
| `Communication` | +0.02 | 常规通信，小幅增强 | `send_direct` / `send_direct_with_task` / `delegate_task` 的 `touch_contact` |
| `FailedCollaboration` | -0.05 | 协作失败，小幅减弱 | delegate 超时真正 fire（`TimeoutManager` 内）→ 从发起者向被委派者发事件；broadcast/reminder 超时无 peer，不发声 |
| `NewContact` | →0.1 | 新联系人，重置为初始值 | 无直接触发点——建联走直接构造（`ensure_contact` 等），分支保留用于测试与语义完备 |
| 衰减 | -0.001/tick | 长时间无通信，按 tick 间隔线性衰减 | `apply_relation_event` 每次通信结算，经 `RelationEngine::apply_decay`（唯一衰减实现，含非有限值消毒） |

所有返回值 clamp 到 [0.0, 1.0]，非有限输入先消毒为 0.0 基底再应用 delta（`RelationEngine` 是最后一道防线）。每次通信的衰减结算后，低于 forget_threshold（0.01）的联系人被遗忘（移除）——刚交互的 peer 除外。遗忘与容量驱逐互补：遗忘管时间（关系淡化），驱逐管空间（通讯录满时淘汰最弱者，同强度按 LRU）。

> **保留理由：** 为未来超大规模 agent 协作准备。当前 3-5 个 agent 的团队不会触发衰减/遗忘的实际效果，但机制就位。

### 4.8 Social Tools

通过 `SocialToolPlugin`（impl `Plugin` trait）注入每个 `AgentHarness`。agent 自主决定何时调用。

#### search_contacts
返回完整通讯录（id、expertise、strength），由 LLM 做语义匹配。通讯录有界（`max_contacts` 默认 20），token 开销可忽略。列表按 strength 降序排序（最值得合作的在前），`limit` 截断后仍是确定性结果。不做文本搜索——名字叫 search 但实际是 list。

**入站自动建联：** 联系人平时只在发送方向建立，但 `push_message` 会对每条入站消息执行 `ensure_contact(to, from)`——被委派方收到请求后能回复（send_message 的联系人门槛）、也能通过 search_contacts 找到发起者。timer/self 系统消息不建联。

#### send_message

点对点发消息给通讯录内的联系人。先校验目标已注册（未注册返回错误），再检查 target 是否在通讯录中，不在则拒绝。内部调 `ctx.send_direct_with_task(agent_id, target_id, body, 0, task_id, broadcast_id)`，自动建立关系（`apply_relation_event(Communication)` + 推 Direct 消息）。

两个可选关联字段：
- `task_id`：回复委派时携带，发起者 runner 据此取消 delegate 超时
- `broadcast_id`：回复广播时携带，发起者 runner 据此取消 broadcast 超时，且 `format_inbox_messages` 把同一 `broadcast_id` 的回复聚合成志愿列表

`"operator"` 无特殊路由：operator 是注册到 registry 的普通 peer，`send_message("operator", ...)` 走与所有消息相同的 inbox 优先队列（priority=0），由 `team.receive()` 阻塞 drain operator 的 inbox 消费（`team.rs` `receive()`，`send_message.rs` `call_inner` 无 operator 分支）。

#### broadcast

全队广播。内部调 `ctx.broadcast(agent_id, message)`，给除自己外的所有 agent 推 `Broadcast` 消息（priority=0），返回 `broadcast_id`。

**两阶段志愿协议：**

```
A 广播："谁能做 X？"
  → 所有 agent 收到广播消息（进 inbox，携带 broadcast_id）
  → 每个 agent 的 LLM 自主判断：
    → "我能做" → 用 send_message 回复 A（携带 broadcast_id），只说"我能做"
    → "不相关" → 忽略
  → A 的 runner drain inbox 时，format_inbox_messages 把同一 broadcast_id 的
    回复聚合成志愿列表展示给 A 的 LLM
  → A 的 LLM 从志愿列表中选一个 → delegate_task 正式委派
  → 不在通讯录则自动加入（send_direct 的 touch_contact 自动建联）
```

架构保证：broadcast_id 关联、志愿回复聚合展示、timeout 可取消（收到回复时取消）。语义判断（谁能做、选哪个）由 LLM 完成，代码不做预筛。

若配置了 `with_broadcast_timeout`：
1. 通过 `TimeoutManager::set_timeout` 安排可取消超时（以 `broadcast_id` 为键）
2. agent 收到回复（携带 broadcast_id）时自动取消超时；超时则收到通知；超时后迟到的回复被丢弃
3. agent 可在调用时传入 `timeout_secs` 覆盖默认值——志愿回复是简单判断，时长取决于 agent 响应速度和请求紧急程度，非任务复杂度

todo list 只记录"当前马上能做的事"，不记录"等待回复"——agent 不主动等待，做完能做的就回 idle，回复或超时到达时由 inbox 唤醒。

#### delegate_task

委派任务给指定 agent（非阻塞，立即返回任务 ID）。

当前实现：
1. `touch_contact(agent_id, target_id, Communication)` — 推进 tick + 建联 + 衰减 + 关系更新
2. `push_message(DelegationRequest, priority=1, task_id=Some("task-{agent_id}-{n}"))` — 推委派请求到目标 inbox
3. 自动 `set_timeout`（以 `task_id` 为键）；收到回复（携带 `task_id`）时自动取消；超时后迟到的回复被丢弃
4. agent 可在调用时传入 `timeout_secs` 覆盖默认值——不同任务复杂度不同，由 agent 自行判断
5. 返回 `"Task '{task_id}' delegated to {to}. A {n}s follow-up timer is set."`
`task_id`（形如 `task-alice-0`）由工具内部的 `AtomicU64` 计数器生成（`delegate_task.rs`）。委派给 operator 或自己会被拒绝。

#### spawn_agent (来自 llm-harness-subagents)

**仅 `allow_subagents(true)` 的成员安装** `llm-harness-subagents` 的 spawn 工具集（默认关闭——spawn 会倍增 LLM 流量，需显式授权；`team.rs` build() 的 opt-in 分支），用于派发子任务：

- `spawn_agent`：异步派发 sub-agent（fire-and-forget），立即返回 agent_id
- `message_subagent`：向 sub-agent 发消息
- `await_subagent_reply`：等待 sub-agent 回复或完成
- `query_subagent`：查询 sub-agent 状态
- `abort_subagent`：终止 sub-agent

**替代了原来的 clone_self/kill_clone 机制。** subagents crate 提供完整的子 agent 生命周期管理：spawn → run → 结果通过 SpawnPlugin 事件投递回父 agent → 自动清理。父 agent 可随时 abort。

**集成方式（`team.rs` build()）：** 每个 team agent 创建独立的 `MessageBus` + `HarnessSubAgentSpawner`（配置 `OsEnvFactory` + `JsonlSessionFactory`）+ `SpawnPlugin`，注册到 `HarnessBuilder`。构建后调 `set_harness_weak()` 连接 plugin 与 harness。`SubAgentMessageConverter` 将 sub-agent 事件转为 user message 注入 LLM 上下文。

#### set_reminder

设定延时自提醒。内部调 `TimeoutManager::set_self_reminder(agent_id, reminder, Duration, task_id)`，到期后推入 `SelfReminder` 消息（priority=30）。

#### todo_write

来自 strategy crate 的 `TodoWriteTool`，管理 agent 的 todolist。支持添加、更新状态、删除 item。工具返回值包含完整的 todolist 快照。

#### operator peer

operator 是注册到 registry 的普通 peer（无 runner，其 inbox 由 `team.receive()` 阻塞 drain），不通过 NotifyPlugin/NotificationChannel 机制。

- agent → 人类：`send_message("operator", text)` → 推入 operator 的普通 inbox（无特殊路由，priority=0）→ `team.receive()` 消费，`OperatorMessage` 携带发送者 id
- 人类 → agent：`team.submit(target, task)` → 以 operator 身份发 Direct 消息（priority=100，最高优先级）到目标 agent inbox

每个 agent 的通讯录在 build 时自动加入 operator（strength=1.0）。

### 4.9 AgentRunner — 自驱循环

每个 agent 跑在自己的 tokio task 里，没有中心调度器。

```
loop {
    // 1. Idle: 阻塞等待消息
    ctx.wait_for_message(&agent_id).await;

    // 2. Busy: drain + 构建 RunRequest
    ctx.update_status(&agent_id, Busy);
    let messages = ctx.drain_inbox(&agent_id);
    if messages.is_empty() { continue; }  // 竞态：被其他 drainer 消费

    let text = format_inbox_messages(&messages);
    let request = RunRequest::from_text(text);

    // 3. 启动 monitor task (run 期间 drain inbox → steer)
    let monitor = spawn_monitor_task(agent_id, harness, ctx);

    // 4. harness.run()
    harness.run(request).await;

    // 5. 停止 monitor
    monitor.abort();

    // 6. 反思触发（若配置）：增量读 transcript，达阈值推 ReflectionTrigger
    if let Some(trigger) = &mut reflection {
        let entries = harness.read_active_path().await.ok();
        if let Some(task_text) = trigger.record_run(entries.as_deref()) {
            ctx.push_message(InboxMessage {
                from: agent_id.clone(),
                to: agent_id.clone(),
                body: task_text,
                message_type: MessageType::ReflectionTrigger,
                priority: crate::inbox::priority::REFLECTION,
                task_id: None,
                broadcast_id: None,
            });
        }
    }

    // 7. 回到 idle
    if !ctx.has_messages(&agent_id) {
        ctx.update_status(&agent_id, Idle);
    }
}
```

**两条注入路径：**

1. **Idle → Wake：** agent idle 时阻塞在 `wait_for_message`。消息推入 inbox 触发 `notify_one()` → 唤醒 → drain inbox → `harness.run()`。
2. **Running → Steer：** `harness.run()` 期间，monitor task 持续 drain inbox → `harness.steer(text)`。Loop 在 `turn_index > 0` 时 drain steer channel，push 到 `ctx.messages`，LLM 在下一个 turn 看到新消息。steer 是非中断的——消息在 turn 边界注入，不打断当前 turn。

**等待模型：** agent 做完自己能做的部分后，LLM 自然停止（没有更多工具可调）→ `harness.run()` 结束 → 回到 idle → 阻塞在 `wait_for_message`。委派结果到达时推入 inbox → agent 被唤醒 → 新一轮 `harness.run()`。不需要 `await_task_result` 工具——idle/inbox 机制统一处理所有等待。

#### ReflectionTrigger

每完成 N 次 `harness.run()` 后，将自上次反思以来**增量**的 transcript 摘要（每段经 `format_transcript_capped` 截断至 20k 字符，`agent_runner.rs` `REFLECTION_TRANSCRIPT_MAX_CHARS`）作为反思任务经 `push_message` 推入 agent 自己的 inbox（`MessageType::ReflectionTrigger`，priority=10，见 `agent_runner.rs` 反思块——`push_message` 而非 `send_direct`：自发自收会把自己加为联系人并推进 tick）。agent 在下一轮 idle 时处理该任务，拥有 memory/knowledge 工具（`memory_write`/`knowledge_search`，由 build 统一安装，见 `team.rs` `build_team_plugins`）。

#### BeforeRunHook

每次 `harness.run()` 开始时注入 todolist 快照作为 additional user message。LLM 在第一个 turn 就能看到当前待办。

### 4.10 TimeoutManager

```rust
pub struct TimeoutManager { ctx: SocialContext }
```

每个调用 spawn 一个 tokio task：`sleep(after)` → `ctx.push_message(InboxMessage)`。

| 方法 | MessageType | Priority | 语义 |
|---|---|---|---|
| `set_timeout` | `Timeout` | 50 | 紧急：任务超时提醒 |
| `set_self_reminder` | `SelfReminder` | 30 | 重要但非紧急：自设待办 |

**迟到回复丢弃：** `set_timeout` 触发后，key（`task_id` 或 `broadcast_id`）被标记为 fired。`drain_inbox` 过滤掉 `task_id`/`broadcast_id` 已 fired 的消息——迟到回复是过期数据，不应打扰已做出决策的 agent。`Timeout` 和 `SelfReminder` 消息本身不受过滤（它们是通知，不是回复）。`set_self_reminder` 不触发 fired 标记（自提醒不等待回复）。

**超时与关系动力学：** `TimeoutRegistry` 的 pending 条目携带 `(AbortHandle, Option<peer>)`——peer 仅 delegate 超时有值（broadcast/reminder 无单一对端）。取消或 fire 时 peer 随结果返回：取消 delegate 超时 = 协作成功信号（`cancel_timeouts_for_messages` 据此发 `SuccessfulCollaboration`）；delegate 超时 fire = 协作失败信号（`TimeoutManager` 据此发 `FailedCollaboration`）。`cancel_timeout` 返回 `TimeoutCancel` 枚举（`Cancelled { peer }` / `NotFound`），区分"无挂起"与"无 peer 的成功取消"。

### 4.11 AgentTeam

开发者组合 IndividualSpec 的入口。

```rust
pub struct AgentTeam {
    members: Vec<IndividualSpec>,
    config: TeamConfig,
    ctx: Option<SocialContext>,
    runners: Vec<JoinHandle<()>>,
    operator_id: AgentId,        // AgentId::new("operator")
    built: bool,
}

pub struct TeamConfig {
    pub max_contacts_per_agent: usize,   // 默认 20
    pub broadcast_timeout_secs: u64,     // 默认 60，agent 可通过 timeout_secs 覆盖
    pub delegate_timeout_secs: u64,      // 默认 300，agent 可通过 timeout_secs 覆盖
}

**API：**
- `new()` / `with_config(config)` — 创建 team
- `member(spec)` — 添加成员（builder 模式）
- `build(env, providers).await` — 物化所有 agent，启动自驱循环
- `submit(target, task)` — 向指定 agent 投递任务（operator → agent，priority=100）
- `receive() -> Result<impl Stream<Item = OperatorMessage>>` — 阻塞 drain agent 发往 operator 的消息（`OperatorMessage { from, body }`）
- `shutdown()` — 中止所有 runner

**build() 流程：**
1. 创建 `SocialContext`
2. 注册 operator 到 registry
3. 一次性构建全队共享的 memory/knowledge 插件（`build_team_plugins`），然后对每个 `IndividualSpec`：注册到 registry → 给通讯录加入 operator（strength=1.0）→ 创建 per-agent TodoStore → 创建 SocialToolPlugin（with_broadcast_timeout + with_delegate_timeout + with_operator）→ 创建 TeamBeforeRunHook → 配置 builder（providers + 插件 + hook；`allow_subagents(true)` 的成员额外装 spawn 工具集 + SpawnPlugin + SubAgentMessageConverter）→ `build(env)` → `set_harness_weak()`（仅 spawn 成员）→ 创建 AgentRunner（含 per-agent KnowledgeAccessContext）→ `spawn()`

## 5. 通信模型

| 模式 | 范围 | 语义 |
|---|---|---|
| 点对点 | 通讯录内的联系人 | "我知道该找谁" |
| 广播 | 全队所有 agent | "我不知道谁能做，谁能帮忙？" |
| operator 通信 | agent ↔ 人类 | 双向：`send_message("operator")` / `team.submit()` |
| 自提醒 | agent → 自己 | 延时待办 |
| 反思 | agent → 自己 | 经验沉淀 |

**委派流程（设计意图）：**

```
Agent A 调用 delegate_task(to=B, task=...) → 返回 task_id（不阻塞）
  → A 自动设 timeout（关联 task_id）
  → A 继续做自己能做的
  → A 做完了，需要 B 的结果才能继续
  → A 的 LLM 自然停止 → harness.run() 结束 → A 回到 idle
  → A 阻塞在 wait_for_message
  → ... B 完成后用 send_message 回复 A，带 task_id ...
  → A 被唤醒 → drain inbox（含 B 的结果）→ 取消 timeout（发 SuccessfulCollaboration）
  → 新一轮 harness.run()，A 看到结果继续处理

  若 timeout 先到：
  → A 收到 Timeout 消息 → drain inbox
  → 决定重试、换人、或放弃
  → 之后 B 才回复（携带 task_id）→ drain_inbox 直接丢弃（迟到回复是过期数据）
```

`task_id` 格式为 `task-{agent_id}-{n}`（如 `task-alice-0`，`delegate_task.rs` 内 `AtomicU64` 计数器生成）——非最早的裸 `task-N`。

**防重复响应：** 广播后每个 agent 的 LLM 自主判断"我能做"或"不相关"。不使用纯代码预筛——expertise 和任务描述都是自然语言，LLM 一轮就能判断。

## 6. Inbox 与 Todo List 的职责分工

所有 agent 结构完全对称——同样的 AgentRunner 自驱循环、同样的社交工具集、同样的 inbox/todo 机制。没有"协调者"或"领导者"角色。operator 是唯一的非对称 peer（无 runner，纯通信端点）。

### Inbox — 外部通信通道

**职责：接收所有来自外部的信息，驱动 agent 的唤醒。**

Inbox 是 **inter-run** 的：它决定 agent 什么时候开始一次新 run，以及这次 run 要处理什么。消息在 drain 后从队列移除（消费即消失）。

| 来源 | 消息类型 | priority | 谁触发 |
|---|---|---|---|
| operator 投递任务 | Direct | 100 | `team.submit()` |
| 其他 agent 发消息 | Direct | 0 | `send_message` 工具 |
| 委派请求 | DelegationRequest | 1 | `delegate_task` 工具 |
| 全队广播 | Broadcast | 0 | `broadcast` 工具 |
| 超时通知 | Timeout | 50 | `TimeoutManager::set_timeout` |
| 自提醒 | SelfReminder | 30 | `set_reminder` 工具 |
| 反思任务 | ReflectionTrigger | 10 | `push_message(ReflectionTrigger)`（反思触发） |

### Todo List — 内部任务管理

**职责：agent 在 run 内部追踪自己的进度，防止 LLM 在长任务中丢失线索。**

Todo list 是 **intra-run** 的：它帮助 LLM 在一次 run 内部组织工作，也跨 run 持久化（`TodoStore` 在 harness 外部，不随 session 重置）。条目持久存在，直到 LLM 主动更新或删除。

**设计原则：todo list 只记录"当前马上能做的事"，不记录"等待回复"或"等别人"。** agent 没有主动等待的设计——做完能做的就回 idle，回复或超时到达时由 inbox 唤醒。广播后不往 todo list 加"waiting for responses"条目；delegate 后不往 todo list 加"waiting for reply"条目。等待是 inbox 的职责（消息到达 → 唤醒），不是 todo list 的职责。

- Per-agent 的 `TodoStore`（`Vec<TodoItem>`），状态 Pending / InProgress / Completed / Abandoned
- 由 LLM 通过 `todo_write` 工具自主管理
- 每次 `harness.run()` 开始时，`TeamBeforeRunHook` 把 todo 快照注入为 additional user message
- **不控制 run 生命周期**——不使用 ShouldStopHook，todo 纯粹是上下文信息

### 两者的关系

```
                    ┌──────────────────────────────────┐
                    │          Agent 的一次 run          │
                    │                                  │
  Inbox ──drain──→  │  [user message: drain 结果]       │
                    │  [user message: todo 快照]        │
                    │                                  │
                    │  LLM 思考 → 调工具 → 思考 → ...    │
                    │    ├─ todo_write（更新进度）       │
                    │    ├─ delegate_task（委派出去）     │
                    │    ├─ send_message（回复别人）     │
                    │    └─ ...                        │
                    │                                  │
                    │  LLM 自然停止                     │
                    └──────────┬───────────────────────┘
                               │
                               │ run 结束
                               ▼
                    ┌──────────────────────────────────┐
                    │  回到 idle，阻塞 wait_for_message │
                    │  ...等待新消息进入 inbox...        │
                    └──────────────────────────────────┘
```

- **Inbox** 回答"做什么"（外部输入），消息消费即消失
- **Todo list** 回答"做到哪了"（内部状态），跨 run 持久保留
- **无交叉引用**：broadcast 和 delegate_task 不往 todo list 写条目。等待回复是 inbox 的职责——超时消息或回复消息进入 inbox 后唤醒 agent 开始新 run，BeforeRunHook 注入的 todo 快照只反映 agent 自己的进度，不包含"等待"状态

## 7. 单 Agent 工作流程

以 architect 收到 operator 任务、委派给 coder 为例。所有 agent 对称，此例适用于任意 agent。

### 阶段一：Idle 等待

```
architect idle，阻塞在 wait_for_message
  通讯录: [operator(1.0), coder(0.12)]
  todo list: 空
  session: 空
```

### 阶段二：唤醒 + drain

```
operator 调 team.submit("architect", "实现一个 HTTP 服务器")
  → send_direct(operator, architect, "实现一个 HTTP 服务器", priority=100)
  → inbox push Direct 消息

architect 被 notify 唤醒
  → update_status(Busy)
  → drain_inbox → 1 条消息
  → format_inbox_messages:
    "You have new messages:
     [from: operator, type: Direct] 实现一个 HTTP 服务器"
  → RunRequest::from_text(上述文本)
```

### 阶段三：run 开始

```
TeamBeforeRunHook 触发:
  → todo 快照注入为 additional user message
  → "Your current todo list: (empty)"

spawn monitor task（run 期间持续 drain inbox → steer）

harness.run(request) 开始
  → LLM 第一轮看到: 任务 + 空 todo list
```

### 阶段四：LLM 执行（intra-run）

```
LLM 第 1 轮:
  "用户要我实现 HTTP 服务器。我是架构师，应该先设计再委派实现。"
  → todo_write: 添加 3 个 item
    [1] 设计 HTTP 服务器架构 (InProgress)
    [2] 委派 coder 实现 (Pending)
    [3] 审查 coder 的实现 (Pending)
  → 工具返回完整 todo 快照

LLM 第 2 轮:
  "先做架构设计。"
  → read_file / write_file
  → todo_write: item 1 → Completed, item 2 → InProgress

LLM 第 3 轮:
  "设计做完了，委派给 coder 实现。"
  → search_contacts → 返回 [operator, coder]
  → delegate_task(to=coder, task="根据设计文档实现 HTTP 服务器")
     → touch_contact(architect, coder, Communication)
     → push_message(DelegationRequest) 到 coder 的 inbox
     → 自动 set_timeout（以 task_id 为键）
  → LLM 自然停止

harness.run() 结束
```

### 阶段五：run 结束 + 回到 idle

```
monitor task abort

reflection trigger（若配置了 interval=5）:
  → record_run(transcript) → count=1，未达阈值，不触发反思

检查 inbox: 空（coder 还没回复）
  → update_status(Idle)
  → 回到 wait_for_message，阻塞等待

此时 architect 的状态:
  session: 3 轮对话历史
  todo list: [1 Completed, 2 InProgress, 3 Pending]  ← 跨 run 保留
  inbox: 空
```

### 阶段六：coder 处理 + 回复（并行发生）

```
coder 的 inbox 收到 DelegationRequest
coder 被唤醒 → drain → harness.run()
  → LLM 看到委派任务 + 自己的 todo list
  → 写代码 → 测试 → 完成
  → send_message(to=architect, body="HTTP 服务器实现完成，代码在 src/main.rs")
     → send_direct(coder, architect, 结果, priority=0)
     → inbox push Direct 消息到 architect 的 inbox（带 task_id 关联）
coder 回到 idle
```

### 阶段七：architect 被唤醒处理结果

```
architect 的 inbox 收到 coder 的回复
  → notify_one() → architect 唤醒
  → update_status(Busy)
  → drain_inbox → 1 条消息
  → format_inbox_messages:
    "You have new messages:
     [from: coder, type: Direct] HTTP 服务器实现完成，代码在 src/main.rs"
  → drain inbox 时自动取消 delegate_task 设的 timeout

TeamBeforeRunHook 注入 todo 快照:
  "Your current todo list:
   1. [Completed] 设计 HTTP 服务器架构
   2. [InProgress] 委派 coder 实现
   3. [Pending] 审查 coder 的实现"

harness.run() 开始
  → LLM 看到: coder 的结果 + todo list
  → "coder 完成了，我应该审查实现。"
  → todo_write: item 2 → Completed, item 3 → InProgress
  → read_file(src/main.rs) → 审查
  → send_message(to=operator, body="HTTP 服务器已完成并审查通过")
     → 推入 operator 的普通 inbox → team.receive() 消费
  → todo_write: item 3 → Completed
  → LLM 自然停止

harness.run() 结束 → inbox 空 → Idle
```

### steer 的角色

上面阶段四里，如果 architect 在 `harness.run()` 期间 coder 的回复就到了：

```
monitor task drain inbox → harness.steer(text)
→ LLM 在下一个 turn 边界看到:
  "You have new messages:
   [from: coder, type: Direct] HTTP 服务器实现完成..."
→ architect 不需要回到 idle，在同一个 run 内就看到结果
```

steer 和 idle 唤醒的分工：
- **steer**：agent 正在 run 中，消息在 turn 边界注入，不中断当前 turn
- **idle 唤醒**：agent 不在 run 中，消息触发新一轮 run


## 8. 模块文件结构

```
src/
├── lib.rs                  模块声明和公共导出
├── agent_id.rs             AgentId 新类型
├── error.rs                TeamError 枚举
├── individual_spec.rs      IndividualSpec 配置声明
├── social_context.rs       SocialContext + AgentRegistry + AgentInfo + AgentStatus
├── inbox.rs                InboxStore + InboxQueue + InboxMessage + MessageType
├── contact.rs              ContactStore + ContactIndex + ContactRecord
├── relation.rs             RelationEngine + RelationConfig + RelationEvent
├── agent_runner.rs         AgentRunner + ReflectionTrigger + monitor task
├── hooks.rs                TeamBeforeRunHook
├── timeout_manager.rs      TimeoutManager
├── team.rs                 AgentTeam + TeamConfig
└── tools/
    ├── mod.rs              SocialToolPlugin + parse_timeout_secs
    ├── search_contacts.rs  SearchContactsTool
    ├── send_message.rs     SendMessageTool
    ├── broadcast.rs        BroadcastTool
    ├── delegate_task.rs    DelegateTaskTool
    └── set_reminder.rs     SetReminderTool
```

## 10. 已确认的设计决策

| # | 决策点 | 结论 | 理由 |
|---|---|---|---|
| 1 | 关系引擎 | 保留 | 为未来超大规模 agent 协作准备 |
| 2 | 三驾马车关系 | subagents / workflow / agent-team 各自独立 | 拓扑模型不同，不强制统一基础设施。agent-society（lxn）已废弃 |
| 3 | await_task_result | 不需要 | agent 做完后自然进入 idle，由统一 inbox 唤醒。不需要在 harness.run() 期间挂起执行流 |
| 4 | 委派结果投递 | delegate_task 自动设 timeout + 收到回复时取消 | 所有信息源统一进 inbox，timeout 驱动 agent 回来检查 |
| 5 | search_contacts | 返回按 strength 降序排序的完整通讯录 | 通讯录有界，排序让 limit 截断确定性强弱有序，LLM 自己判断匹配 |
| 6 | LLM 集成测试 | 先补全设计，后做测试 | 优先把缺失机制补齐 |

## 11. 已修复的代码缺失

以下缺口在 2026-08-25 已全部修复：

`delegate_task` 现在在委派后自动 `set_timeout`（默认 300s，由 `TeamConfig.delegate_timeout_secs` 配置，agent 可通过 `timeout_secs` 参数覆盖），以 `task_id` 为键、以被委派者为 peer。`TimeoutRegistry`（存储在 `SocialContext` 中）追踪待取消的 `AbortHandle`。发起者 drain inbox 时，`cancel_timeouts_for_messages` 取消匹配的 timeout，并对被委派者发 `SuccessfulCollaboration` 事件（回复及时 = 协作成功）；超时真正 fire 时发 `FailedCollaboration`（见 4.7 / 4.10）。

### 11.2 DelegationResult 消息类型未使用（P1）— 已修复

`MessageType::DelegationResult` 已从 enum 和 `message_type_label` 中删除。委派回复复用 `Direct + task_id`。

### 11.3 broadcast 的 timeout 不可取消（P1）— 已修复

`ctx.broadcast()` 现在返回 `broadcast_id`，broadcast tool 以其为键注册 timeout。与 delegate_task 共用同一套 `TimeoutRegistry` 取消机制。

### 11.4 clippy 警告（P2）— 已修复

`team.rs` 中 `+ before-run hook` 的 `+` 被 clippy 解析为列表标记，改为 `plus`。

### 11.5 monitor 竞态丢消息（P1）— 已修复

monitor task 从 inbox drain 消息到 `harness.steer` 之间无 await 点；若消息恰好在 run 的最后一个 turn 边界后到达，steer 会塞进已被弃用的 channel（`steer()` 忽略 Err），下次 run `reset_channels()` 换掉 channel 时消息两层同时丢失。现在 monitor 改用 `steer_message`，失败时把消息原样放回社交收件箱（`SocialContext::return_messages`），下一轮 run 自然处理。

### 11.6 reflection 污染联系人表（P2）— 已修复

reflection 原来用 `send_direct(self, self)` 自发自收：agent 把自己加为联系人（search_contacts 出现自己），且每次反思推进 tick 加速真实联系人衰减；`MessageType::ReflectionTrigger` 是死代码。现在用 `push_message` + `ReflectionTrigger` 类型，`push_message` 对 from==to 的自消息跳过建联。

### 11.7 run 失败吞消息 / panic 卡 Busy（P2）— 已修复

`harness.run()` 失败只打日志，本轮已 drain 的消息静默丢失；runner task panic 则永久停在 Busy 状态。现在：run 失败或 panic 时把已 drain 的消息放回收件箱（下轮重试），`catch_unwind` 防护 panic 不再杀死 runner task。

## 12. 与原始设计的偏差

原始设计来自 `agent-society` 分支的白皮书，实现过程中有几处调整：

| 设计描述 | 实际代码 |
|---|---|
| delegate_task 结果自动投递到发起者 | 依赖 LLM 用 `send_message`（带 `task_id`）回复；delegate_task 自动设置可取消超时 |
| `MessageType::DelegationResult` 用于委派结果 | 已删除，复用 `Direct + task_id` |
| `MessageType::BroadcastResponse` 用于广播回复 | 已删除，复用 `Direct + broadcast_id`；志愿回复由 `format_inbox_messages` 聚合展示 |
| broadcast timeout 后 agent 用 todo_write 标记 Completed | 代码支持但未验证 LLM 是否这样做 |
| `InboxStore::wait_for_message` 用全局 Notify + 10ms 轮询 | 已改为 per-agent Notify + 250ms 轮询兜底（`inbox.rs`） |
| operator peer 路由（send_message 识别 "operator"） | 无特殊路由：operator 是普通 inbox peer，`receive()` drain 其 inbox（`team.rs`） |

## 13. 开发者体验

```rust
use llm_harness_agent_team::{IndividualSpec, AgentTeam};

let architect = IndividualSpec::new("architect", "架构设计", "claude-3-5-sonnet")
    .configure(|b| {
        b.system_prompt(Some("你是架构师，擅长拆解需求和设计方案".to_string()))
    })
    .reflection_interval(5)
    .auto_compact(true);

let coder = IndividualSpec::new("coder", "代码实现", "claude-3-5-sonnet")
    .configure(|b| {
        b.system_prompt(Some("你是实现者，擅长写代码".to_string()))
            .max_tokens(16384)
    });

let mut team = AgentTeam::new()
    .member(architect)
    .member(coder);

team.build(env, providers).await?;

team.submit("architect", "实现一个 HTTP 服务器").await?;
let result = team.receive().next().await.unwrap();
```
开发者不需要知道通讯录、关系强度、广播、委派、sub-agent spawn——这些全是 AgentTeam 内部基础设施。开发者只管：定义个体 + 组合 = 团队。

## 14. AgentPersona — 可迁移的持久人格（未来设计，暂不实现）

**状态：** 有价值的设计想法，已确认方向，排在 P1（LLM 集成测试）之后实现。这里仅记录设计意图与分期，不进入当前实现范围。

### 动机

Agent 不应是"一次性实例"。当前生命周期：

```
IndividualSpec → build → run → 进程结束即消失
```

目标：Agent 变成可迁移的持久实体：

```
IndividualSpec → build → run → export → Persona
                                            ↓
                                 import → build → run → ...
                                            ↓
                        (在其他 team / 进程 / 系统中以同一人格启动)
```

### Agent 进入系统的两种方式（对称入口）

```
方式一：声明式（静态）
  开发者写 IndividualSpec::new("coder", "代码实现", "gpt-4o")
    .system_prompt(...) .tools(...) .reflection_interval(...)
  → build() → agent 从零开始积累人格

方式二：运行时涌现（动态）
  agent A 调 spawn_agent() → 产生 fresh sub-agent B
  → B 通过做任务逐渐形成人格、记忆、知识库
  → 导出 B → 在其他系统插入 B（以已成型的人格启动）
```

### Agent 人格 = 什么

```
┌─ AgentPersona ──────────────────────────────────┐
│  1. 声明式配置（IndividualSpec 静态部分）          │
│     - id / expertise / model / system_prompt     │
│     - tools 配置 / reflection_interval           │
│  2. Session 历史（对话 transcript）               │
│     - JsonlSessionRepo 可持久化                   │
│  3. 记忆（memory_write 积累的事实 + 反思 lessons） │
│     - MemoryStore 中的条目                        │
│  4. 知识库（knowledge_search 积累的文档）           │
│     - KnowledgeRegistry 中的 sources（存引用）     │
└──────────────────────────────────────────────────┘
```

### 已确认的现有基础设施

| 能力 | 已有 | 位置 |
|---|---|---|
| 运行中 spawn 新 agent | ✅ `spawn_agent` (llm-harness-subagents) | `team.rs` build() |
| agent 写持久记忆 | ✅ `memory_write` / `memory_forget` | `llm-harness-memory` |
| agent 搜索知识 | ✅ `knowledge_search` / `knowledge_read` | `llm-harness-knowledge` |
| session 持久化到磁盘 | ✅ `JsonlSessionRepo` | `llm-harness-agent/src/session` |
| 从磁盘恢复 session | ✅ `SessionRepo::open()` + `build_with_session()` | `builder.rs` |
| 反思沉淀经验 | ✅ `ReflectionTrigger` → `memory_write` | `agent_runner.rs` |
| 记忆防注入 | ✅ `MemoryDefenseHook` | `llm-harness-strategy` |

### 待解决的关键设计点

1. **Memory 怎么导出** — `MemoryStore` trait 只有 `upsert`/`delete`，无 `list_all()`。读取走 `KnowledgeSource`（`knowledge_search`）。候选：给 trait 加 `dump()`；或 agent-team 层维护 shadow copy（每次 `memory_write` 同步记录）；或只存 read source id 导入时重挂。
2. **Session 怎么导出** — 每 agent 当前用 `InMemorySessionRepo`（不持久化），需要 team 层改用 `JsonlSessionRepo`。导出=复制 session 目录+记录 id；导入=`open` + `build_with_session()`。
3. **Knowledge 怎么导出** — source 可能是文件目录/向量库/外部 API，导出只存引用（source_id + 配置），不内联内容。导入时重建 source 并注册。
4. **clone 如何注入记忆/知识工具** — 当前 clone 无 memory/knowledge 工具。需要 clone 时注入 MemoryPlugin + KnowledgePlugin，knowledge 继承 parent 的 source（read-only），memory 独立。
5. **导出载体** — 目录（persona.json + session/ + memory.jsonl）或 tar/zip。
6. **导出时机** — 手动 `team.export_agent(id)` / 完成任务后自动 checkpoint / agent 自触发。

### 分期

| 阶段 | 内容 | 依赖 |
|---|---|---|
| P1 | LLM 集成测试（验证自驱循环、委派、steer） | 设计定稿 |
| P2-a | team 层改用 `JsonlSessionRepo`（session 持久化） | 无新 API |
| P2-b | clone 时注入 MemoryPlugin + KnowledgePlugin | P2-a |
| P2-c | `AgentPersona` export/import API | P2-a + P2-b |
| P3 | 自动 checkpoint + persona 跨系统迁移 | P2-c |

**此设计明确排在 P1 之后，当前不实现，仅记录。**
