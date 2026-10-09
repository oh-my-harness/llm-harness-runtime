# Mobile Work Assistant — 正式设计 spec

日期：2026-10-10
输入：`docs/superpowers/specs/2026-10-09-mobile-product-discussion.md`（讨论决议）
状态：待实施

---

## 1. 数据模型

### 1.1 机器（Machine）

新增 Kotlin 侧实体 + DataStore 持久化（`machines.preferences_pb`，JSON 序列化 list）：

```kotlin
data class Machine(
    val id: String,            // UUID
    val name: String,          // 显示名，如 "Mac Pro"
    val gatewayUrl: String,    // 空 = 纯对话机器（无 gateway）
    val gatewayToken: String,
    val trustProfile: String,  // restricted / standard / trusted-full
    val createdAtMs: Long,
)
```

- 机器配置**不含 LLM 字段**（LLM 全局配置解耦，已决议）。
- 纯对话机器 = `gatewayUrl` 为空的合法机器（"本机对话"），卡片仍显示，在线态恒为"本地"。
- 沙箱类型暂不进 UI：gateway 后端由服务端配置决定（Mac=seatbelt / Linux=bwrap）；"无沙箱"通过 `gatewayUrl` 空表达。gateway 新增 `os` 后端见 §4.1。
- 迁移：现有单配置（`AppConfig.gatewayUrl/token/trustProfile`）首启迁移为 machines 列表第一项；`AppConfig` 保留 LLM 字段与 fontScale，删除 gateway 三字段（一次性 DataStore 迁移）。

### 2. 任务（Task = 现有 session）

Rust 侧 `SessionState` 扩展：

```rust
pub struct SessionState {
    pub sandbox_id: String,          // 纯对话任务为空串
    pub trust_profile: String,
    pub last_active_at_ms: u64,
    pub machine_id: String,          // 新增；空串 = 纯对话
    pub title: String,               // 新增；自动生成
    pub status: String,              // 新增；running|waiting|completed|failed|aborted
    pub last_summary: String,        // 新增；最后一条消息摘要（列表展示）
}
```

- `SessionSummary`（uniffi）同步加 `machine_id / status / title / last_summary / unread_hint`。未读计数在 Kotlin 层维护（进程内存 + DataStore），不进 Rust。
- **标题自动生成**：首条用户消息发出后，用小模型（复用 `compaction_model` 配置思路）生成一句话标题；失败/超时回退首条消息前 30 字。
- **状态推导**：从 harness phase + 最后持久化条目推导（omp 同款）。映射：
  - harness Busy → `running`（若有 pending steer，列表显示 activity：最后一个 ToolStarted 的 name）
  - 等待用户输入（run 结束无错误）→ `waiting`
  - `RunFinished` → `completed`；`Failed` → `failed`；`RunAborted` → `aborted`
  - 持久化：每次 turn 结束（ToolFinished/RunFinished/Failed 落盘时）更新 `state.json`。
- **状态推导的实现位置**：mobile-core `on_event` 路径里顺手写 state.json（已有 write_state 机制），Kotlin 列表项由 `listSessions()` 直接拿到，无需新协议。

### 3. 任务生命周期与连接（懒连接）

```
创建任务 → 本地建目录（entries.jsonl + state.json）→ 不连接
进入会话页 → 只读本地 history（已有 replay 路径）→ 不连接
发送消息  →
    纯对话任务：直接 harness.prompt()（沙箱无、工具集不含 bash/read/write/edit/glob/grep）
    机器任务：  lazy connect（ensure_connected）→ 建沙箱 → send
后台任务：事件泵持续运行（pumps 已按 session 分桶），列表摘要/未读实时更新
```

- `MobileConfig` 拆分语义：`gateway_url` 空时 `MobileSession::new` 走 `LocalSession` 组装（`OsEnvSandbox`，工作目录 = 用户目录或任务目录），工具集同现有六件套但执行在手机本地沙箱外？**否**——纯对话任务不挂工具（`opts.tools = vec![]`），仅对话。装依赖/跑命令类任务必须绑机器。
- 多任务并行：每任务一个 `MobileSession`（现状已支持 per-session live/pump），同机多沙箱 gateway 已支持，无需协议变更。
- 失败策略：**手动重发**。send 失败 → `Failed` 事件 → UI 显示错误 + 输入框恢复原文本；不自动重试、不静默重建沙箱。

### 4. gateway `os` 后端（无沙箱直跑）

- `BackendConfig` 新增 `Os` 变体 → `GatewayBackend::Os` → 网关建沙箱时用 `OsEnvSandbox`（工作目录 = per-sandbox 目录，无 seatbelt/bwrap 策略）。
- 机器配置不改（后端类型是 gateway 服务端配置）；手机侧信任级别语义不变。
- 风险：无隔离下工具可直接写宿主任意路径。仅建议自用机器（trusted-full 等价场景）。

### 5. 输入与排队注入

runtime 已有完整机制，**纯接线**：

- agent 忙（phase != Idle）时用户发送 → 调 `harness.steer_message()`（现有 `steer` 通道，turn 内注入）；若在 Idle 前窗口（race），走 `next_turn_message()` 缓冲，下次 `prompt()` 自动带上。
- UI 上消息立即显示"排队中"标记（小时钟图标），steer 被消费后（下一轮 TextDelta 开始）转普通用户气泡。
- **打断**：发送键在 streaming 时变 stop（已有）；`abort()` 保留，打断后 steer 队列不清理（runtime 默认保留，`clear_steering_queue` 不调用）。
- 移除 UI 的 Steer 按钮（排队机制取代其语义）。

### 6. 消息渲染（结果验收）

- `ToolStatusCard`：完成后默认折叠（一行：命令 + summary），点击展开 full_output（等宽字体、可滚动、可复制）。
- `full_output` 以 `diff --git` 开头或含 `@@` 行 → +/- 行着色（绿/红背景 15% alpha）。
- agent 端约定（system prompt 追加）：测试/构建命令的结果，先给一行结论（N passed / N failed），再给失败详情，最后才完整日志。

### 7. 通知与后台

- 前台服务：任一任务 running 时启动 `MobileTaskService`（常驻通知"1 个任务运行中"），全部空闲自动停。熄屏后事件泵继续（服务持有 runtime）。
- 任务结束（RunFinished/Failed）且该任务**不在前台** → 系统通知（标题 = 任务名，内容 = 最后一条 assistant 消息摘要，点击直达会话）。
- 通知渠道：`task-completion`（低优先级，声音可关）。

### 8. 模型配置

- app 级多套 LLM 配置：`models.preferences_pb`，`List<LlmProfile(id, name, provider, apiKey, baseUrl, model)>` + `activeLlmId`。
- Chat 页顶栏模型名可点 → bottom sheet 切换；调 uniffi `setLlm(model, provider, baseUrl, apiKey, contextWindow?, maxTokens?)`
  （harness `switch_llm`：换 client + 换 model，记录带 provider 的 `ModelChange` 条目；Idle 即时 / 运行中排队到下一轮）。
  跨 provider 热切换已支持，无需新建会话。
- Task 创建时用当时 active 的 LLM；对话内切换即时生效（runtime 自动 ModelChange 条目）。

### 9. UI 状态机（Kotlin 层新增状态）

```kotlin
// AppUiState 扩展
data class TaskSummary(          // 第二层列表项
    val id: String,
    val machineId: String?,
    val title: String,
    val status: TaskStatus,      // Running/Waiting/Completed/Failed/Aborted
    val lastMessage: String,     // 摘要
    val lastActiveAtMs: Long,
    val unreadCount: Int,
)
data class MachineUiState(...)   // 第一层：id/name/online/runningTaskCount

// AppUiState 加 machines: List<Machine>、tasksByMachine: Map<String?, List<TaskSummary>>
```

- 未读计数：Kotlin `onEvent` 里非 active session 的 TextDelta/ToolFinished/RunFinished/Failed → `unread[sessionId]++`；进入会话页清零。
- 导航：`Machines → Tasks(machineId) → Chat` 三级 NavHost 路由。

## 10. 实施依赖顺序（关键路径）

1. **gateway `os` 后端**（小）
2. mobile-core：SessionState 扩展 + 无沙箱 session 组装 + `setModel` uniffi 导出
3. Kotlin 数据层：Machine 实体 + LLM 配置分家 + 迁移
4. 三层导航 + 任务列表（状态/未读/摘要/自动标题）
5. 懒连接 + 排队注入 UI
6. 工具卡片折叠 + diff 着色
7. 通知 + 前台服务
