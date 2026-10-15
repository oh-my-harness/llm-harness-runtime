# Mobile UI 重构 + 多会话/resume 设计

日期：2026-09-29
状态：方向已确认（本文档为详细设计）
前置：2026-09-25-mobile-agent-client-design.md（MVP + 真机实测）、UI 硬约束章节

## 概要

把 MVP 调试壳（单 Activity + 6 个平铺输入框 + 纯文本 transcript）重构为正式聊天应用：
Jetpack Compose + Material 3 全量重写 UI 层，配置向导 + 持久化，完整聊天体验（气泡、
流式渲染、工具状态卡、Markdown 代码块），多会话管理与冷启动 resume。

架构模式：ViewModel + StateFlow 单向数据流（Android 官方 UI 层指南同款）。
Rust core 扩展多会话 FFI；Kotlin UI 层删旧换新。

## 已确认的决策

| 决策点 | 结论 |
|---|---|
| 范围 | 聊天体验完整重构（不含语音/前台服务，预留架构位置） |
| 配置 | 首次向导 + DataStore 持久化，启动直达聊天页 |
| 消息呈现 | 气泡 + 流式 + 工具状态卡 + Markdown/代码块渲染 |
| 技术栈 | 删 XML 换 Compose；Material 3 动态取色；Rust core/uniffi 绑定机制不动 |
| 连接 | 自动重连 + 连接状态机 |
| 多会话 | 支持；会话历史本地持久化（jsonl）；单活跃多会话、后台续跑 |
| resume | 沙箱尝试重连，失效则新建并系统提示 |

## Rust 侧扩展（llm-harness-mobile-core）

### 现状

FFI 仅有 `connect(config) -> MobileSession`（内部 `AgentHarness::new_in_memory`），
`send/poll_event/shutdown`。会话持久化与恢复的底层能力已齐备但未接线：

- `JsonlSessionRepo`（`meta.json` + `entries.jsonl`，`SessionRepo::create/open/list/delete`）
- `Session::build_context()` → `BuiltContext { messages: Vec<AgentMessage>, ... }` 还原对话
- `AgentHarness::with_session(client, env, session, opts)` 接受既有 Session
- `RemoteSandbox::sandbox_id()` 可取；gateway `GET /v1/sandboxes/{id}` 可查活性

### FFI 新增面

```rust
#[uniffi::Object]
pub struct MobileSessionManager { /* JsonlSessionRepo + 配置 */ }

impl MobileSessionManager {
    pub fn new(data_dir: String) -> Arc<Self>;                  // app filesDir/sessions/
    pub async fn create_session(config, name) -> Result<Arc<MobileSession>, MobileError>;
    pub async fn list_sessions() -> Result<Vec<SessionSummary>, MobileError>;
    pub async fn open_session(config, session_id) -> Result<Arc<MobileSession>, MobileError>;
    pub async fn delete_session(session_id) -> Result<(), MobileError>;
}

#[uniffi::Record]
pub struct SessionSummary { id, name, created_at_ms, last_active_at_ms }
```

`open_session` resume 语义（按顺序）：

1. `repo.open(id)` → `build_context()` 还原 `Vec<AgentMessage>`，转历史消息（只读回放）
2. 查 `GET /v1/sandboxes/{saved_sandbox_id}`：
   - `200 running` → 以原 sandbox_id 重连 `RemoteSandbox`（文件/依赖全在）
   - 404/超时/网络错 → 新建沙箱（原 trust profile），注入一条 SystemNotice
     "原沙箱已失效，已新建沙箱（会话历史保留，沙箱内文件丢失）"
3. `AgentHarness::with_session(client, env, session, opts)` 组装
4. 返回的 `MobileSession` 事件泵只转发 resume 之后的新事件（历史走 build_context，不混入事件流）

`MobileSession` 扩展：`session_id()`、`sandbox_id()`；新建会话时把 sandbox_id
记录在 manager 侧的 sidecar 文件（`sessions/<id>/sandbox.json`），供 resume 查询。

存储布局（app 私有目录，卸载即清）：

```
filesDir/
  sessions/
    <session-id>/
      meta.json          (JsonlSessionRepo 自有)
      entries.jsonl      (JsonlSessionRepo 自有)
      sandbox.json       { sandbox_id, trust_profile }  ← manager sidecar
```

`create_session` 默认 SessionDurability=持久化；`build_context` 已处理 compaction
回放，直接复用。`last_active_at_ms` 由 manager 维护在 sidecar（`sandbox.json` 扩为
`state.json`：`{ sandbox_id, trust_profile, last_active_at_ms }`），每次
`open_session` 与事件到达时更新。

### 事件面不变

`MobileEvent` 五变体（TextDelta/ToolStarted/ToolFinished/RunFinished/Failed）不变，
新增 `ToolFinished.full_output`（完整输出，替代 120 字符截断只留 summary 的现状；
summary 保留为首行）。Kotlin 侧点击工具卡展开全量。

## Kotlin 侧结构

```
dev.ohmyharness.mobile/
  MainActivity.kt              ← Compose host + navController（~30 行）
  ui/
    AppViewModel.kt            ← 唯一 ViewModel；StateFlow<AppUiState>
    AppUiState.kt              ← 不可变状态模型（下节）
    nav/AppNav.kt              ← 路由: "chat" | "setup" | "settings" | "sessions"
    screen/
      ChatScreen.kt            ← 会话页（当前活跃会话）
      SetupScreen.kt           ← 首次配置向导（单页表单）
      SettingsScreen.kt        ← 设置页（复用 ConfigForm）
      SessionsScreen.kt        ← 会话列表（切换/新建/删除）
    component/
      MessageBubble.kt         ← 用户/AI 气泡
      ToolStatusCard.kt        ← 工具状态行，点击展开 full_output
      ConnectionBanner.kt      ← 顶部连接状态条（非 Connected 时显示）
      MarkdownBlock.kt         ← 代码块（等宽+横滚+复制）/粗体/列表
      ConfigForm.kt            ← 配置表单（setup/settings 共用）
  data/
    ConfigRepository.kt        ← DataStore Preferences
```

删除：`activity_main.xml`、旧 MainActivity 逻辑、`res/layout` 全部内容。

导航：

- 冷启动 → DataStore 读配置 → 无配置进 `setup`；有配置进 `chat`（会话列表页随之
  可用，历史会话通过 resume 恢复）并自动重连
- 会话列表入口：ChatScreen 顶部栏

## 状态模型与数据流

```kotlin
data class AppUiState(
    val configComplete: Boolean = false,
    val connection: ConnectionState = ConnectionStates.Idle,   // 活跃会话的连接状态
    val sessions: List<SessionSummary> = emptyList(),
    val activeSessionId: String? = null,
    val messages: Map<String, List<ChatMessage>> = emptyMap(), // 按会话累积
)

sealed interface ConnectionState {
    object Idle; object Connecting
    data class Connected(val trustProfile: String)
    data class Failed(val message: String)      // banner 红条 + 重试
}

sealed interface ChatMessage {
    data class User(val text: String) : ChatMessage
    data class Assistant(val text: String, val isStreaming: Boolean) : ChatMessage
    data class Tool(val name: String, val summary: String, val fullOutput: String?,
                    val isError: Boolean) : ChatMessage
    data class SystemNotice(val text: String) : ChatMessage
}
```

- ViewModel 持 `SessionManager` + `Map<sessionId, MobileSession>`；每个会话独立事件泵
  （100ms 轮询 `poll_event`，与现 MVP 相同），事件累积进 `messages[sessionId]`
- `activeSessionId` 决定 ChatScreen 渲染哪个会话；后台会话事件照常累积
- intent：`send(text)`、`connect()`、`retry()`、`switchSession(id)`、
  `newSession(name)`、`deleteSession(id)`、`saveConfig()`
- TextDelta 高频到达用 `withFrameNanos` 合帧；`LazyColumn` 自动滚底（用户上滑时暂停，
  出现"回到底部"按钮）

## 会话并发与生命周期

- 后台同时最多 1 个运行中会话语义上不限制 harness（切走任务续跑），UI 任意切换；
  Rust 侧无改动即可支持（每会话独立 harness + sandbox + 事件泵）
- 进程被杀 = 运行中会话全部丢失；冷启动 resume 靠 jsonl 恢复历史 + 沙箱重连策略
  （见 resume 语义）。Android 侧不做进程保活（前台服务是后续迭代）
- 旋转屏/配置变更：ViewModel 存活，不丢状态

## 错误处理

- 沿用 spec"不吞错"：`MobileError` 全链文案（`{error:#}`）进 SystemNotice，不翻译
- resume 时沙箱失效 → SystemNotice 提示 + 新沙箱继续（非致命）
- 连接失败 → ConnectionBanner.Failed + 重试按钮；输入框禁用
- 工具失败 → Tool 卡片红色左边框，summary 显示 `failure.model_message`

## 测试

| 层 | 方式 |
|---|---|
| ViewModel 状态机 | JVM 单测：伪 MobileSession/Manager（interface 抽象），验事件→uiState 映射、会话切换、合帧逻辑 |
| ConfigRepository | JVM 单测：读写、不完整配置判定 |
| resume 语义 | Rust 侧单测（mobile-core）：open_session 对不存在/失效 sandbox_id 的行为、build_context 回放映射 |
| 端到端 | 真机冒烟：配置→连接→发消息→流式渲染→切会话→杀进程→resume |

## 非目标（本迭代不做）

- 语音输入、前台服务/通知（UI 预留：输入栏语音按钮位、通知走系统通道）
- 会话改名、fork/branch UI、跨设备同步
- 消息 SQLite 持久化（jsonl 已覆盖）
- iOS、Compose 截图测试
- 多会话并行活跃的 gateway 侧改动（现有协议已支持多沙箱并存）

## 验收标准

1. 冷启动无配置 → 向导 → 保存自动连接 → 聊天页发消息，流式逐字 + 工具卡实时
2. 二次启动免配置直达聊天页并自动重连
3. 新建第二个会话，两会话独立对话；切换时后台会话任务续跑、事件不丢
4. 杀进程 → 冷启动 → 会话列表可恢复历史消息；沙箱活着则重连（文件在），失效则
   新建 + 系统提示
5. 工具执行/失败/空输出三种状态卡样式正确；错误消息原文可见
6. 动态取色 + 暗色模式正常；Markdown 代码块可复制
