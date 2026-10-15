# Mobile UI 重构 + 多会话/resume 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 MVP 调试壳重构为 Compose/M3 聊天应用：配置向导 + 持久化、气泡流式聊天、多会话管理与冷启动 resume。

**Architecture:** Rust 侧（llm-harness-mobile-core）新增 `MobileSessionManager` FFI，接 `JsonlSessionRepo`（app 私有目录）实现会话持久化与 `build_context` 历史回放，resume 时按 sandbox_id 查 gateway 活性决定重连或新建。Kotlin 侧删 XML 换 Compose，ViewModel + StateFlow 单向数据流，每会话独立事件泵，DataStore 存配置。

**Tech Stack:** Rust/uniffi（已有）、Kotlin + Jetpack Compose BOM + Material 3、Navigation Compose、Lifecycle ViewModel Compose、DataStore Preferences、Coroutines。

**Spec:** `docs/superpowers/specs/2026-09-29-mobile-ui-redesign-design.md`

## Global Constraints

- 语言：代码注释/文档/提交信息英文，与用户对话中文
- 不改 `llm-harness-agent`、`llm-harness-sandbox-remote`、`llm-harness-sandbox-gateway` 的公开语义；只消费
- `mobile-core` 错误面必须完整透传错误链（`format!("{error:#}")`，见 8b7ee6d 教训）
- 沙箱信任 profile 硬底线不因 UI 改动放松（deny 优先）
- Kotlin 侧"不吞错"：MobileError 原文进 SystemNotice，不翻译不美化
- Android 构建：`scripts/build-android.sh`（host 构建需 `SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk`，本机 SDK 27 链接问题）；APK 用 `JAVA_HOME="/Applications/Android Studio.app/Contents/jbr/Contents/Home" gradle assembleDebug`（在 `mobile/android/` 下）；真机安装走 `adb push` + 手动装（MIUI 无 SIM 限制）
- Rust 测试：`SDKROOT=… CARGO_HTTP_PROXY="" cargo test -p llm-harness-mobile-core`
- 每个任务 TDD：先写失败测试，再实现，再提交；提交信息 conventional commits
- jniLibs 已 gitignore（.so 走 build-android.sh 生成），提交不含 .so

---

### Task 1: Rust — MobileEvent 增加 ToolFinished.full_output

**Files:**
- Modify: `crates/llm-harness-mobile-core/src/events.rs`
- Test: `crates/llm-harness-mobile-core/tests/session.rs`

**Interfaces:**
- Consumes: `AgentEvent::ToolExecutionEnd { result: Result<ToolResult, ToolFailure> }`
- Produces: `MobileEvent::ToolFinished { name: String, summary: String, full_output: String }`（uniffi enum 变体加字段）

- [ ] **Step 1: 写失败测试**（`crates/llm-harness-mobile-core/tests/session.rs` 中已有 tool 事件测试处扩展）

```rust
#[test]
fn tool_finished_carries_full_output() {
    let text = "line one\nline two\nline three";
    let result = Ok(ToolResult::full(
        vec![DataBlock::text(text.to_string())],
        serde_json::json!({}),
        false,
    ));
    let mobile = map_agent_event(&AgentEvent::ToolExecutionEnd {
        tool_name: "bash".into(),
        result,
    })
    .expect("maps");
    match mobile {
        MobileEvent::ToolFinished { summary, full_output, .. } => {
            assert_eq!(summary, "line one");
            assert_eq!(full_output, text);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}
```

（对齐文件内现有 `AgentEvent::ToolExecutionEnd` 构造写法；字段名以 `llm-harness-types` 实际定义为准，先读再写。）

- [ ] **Step 2: 运行确认失败**

Run: `SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk CARGO_HTTP_PROXY="" cargo test -p llm-harness-mobile-core --test session`
Expected: FAIL（`ToolFinished` 无 `full_output` 字段 / 编译错）

- [ ] **Step 3: 修改 events.rs**

`MobileEvent::ToolFinished` 加 `full_output: String` 字段；`map_agent_event` 的 `Ok(tool_result)` 分支：`summary` 逻辑不变（首行 120 字符截断），`full_output` 取完整 text（不做 120 截断）。`Err(failure)` 分支 `full_output` 置空串。

- [ ] **Step 4: 运行测试通过**

Run: 同 Step 2
Expected: PASS（原有测试同步修正构造处）

- [ ] **Step 5: 提交**

```bash
git add crates/llm-harness-mobile-core
git commit -m "feat(mobile-core): carry full tool output in ToolFinished event"
```

---

### Task 2: Rust — MobileSessionManager（create/list/delete + 持久化 Session）

**Files:**
- Create: `crates/llm-harness-mobile-core/src/manager.rs`
- Modify: `crates/llm-harness-mobile-core/src/lib.rs`（`mod manager;`、`MobileSession::new` 拆出共享装配函数）
- Test: `crates/llm-harness-mobile-core/tests/manager.rs`

**Interfaces:**
- Consumes: `JsonlSessionRepo::new(root_dir)`、`SessionRepo::{create, open, list, delete}`、`CreateSessionOptions`、`AgentHarness::with_session(client, env, session, opts)`
- Produces:
  - `pub struct MobileSessionManager`（uniffi::Object）:
    - `new(data_dir: String) -> Arc<Self>`
    - `create_session(config: MobileConfig, name: String) -> Result<Arc<MobileSession>, MobileError>`
    - `list_sessions() -> Result<Vec<SessionSummary>, MobileError>`
    - `delete_session(session_id: String) -> Result<(), MobileError>`
  - `pub struct SessionSummary { id: String, name: String, created_at_ms: u64, last_active_at_ms: u64 }`（uniffi::Record）
  - `MobileSession` 新增：`session_id() -> String`、`sandbox_id() -> String`
  - 内部共享装配：`assemble(config, session: Session) -> (harness, sandbox)`（把现 `MobileSession::new` 的 client/llm/env/opts 部分抽出复用）
  - sidecar：`<data_dir>/<session-id>/state.json` = `{"sandbox_id": "...", "trust_profile": "...", "last_active_at_ms": 0}`；`open_session` 与事件到达时更新 `last_active_at_ms`

- [ ] **Step 1: 写失败测试**（不连真实 gateway：用 `#[cfg(test)]` 下对 `SessionSummary` 排序/sidecar 读写的纯逻辑测试；跨网关行为留给 Task 4 e2e）

```rust
// tests/manager.rs
use llm_harness_mobile_core::manager::state::{read_state, write_state, SessionState};
use std::path::PathBuf;

#[test]
fn state_sidecar_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let path = PathBuf::from(dir.path()).join("s1");
    std::fs::create_dir_all(&path).unwrap();
    let state = SessionState {
        sandbox_id: "abc".into(),
        trust_profile: "trusted-lan".into(),
        last_active_at_ms: 42,
    };
    write_state(&path, &state).unwrap();
    let loaded = read_state(&path).unwrap();
    assert_eq!(loaded.sandbox_id, "abc");
    assert_eq!(loaded.last_active_at_ms, 42);
}

#[test]
fn read_state_missing_file_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = PathBuf::from(dir.path()).join("s2");
    std::fs::create_dir_all(&path).unwrap();
    let loaded = read_state(&path).unwrap();
    assert!(loaded.sandbox_id.is_empty());
    assert_eq!(loaded.last_active_at_ms, 0);
}
```

- [ ] **Step 2: 运行确认失败**

Run: `SDKROOT=… cargo test -p llm-harness-mobile-core --test manager`
Expected: FAIL（`manager` 模块不存在）

- [ ] **Step 3: 实现 manager.rs**

- `state` 子模块：`SessionState`（serde Serialize/Deserialize）、`write_state/read_state`（JSON 文件，读失败/缺失返回默认值）
- `MobileSessionManager`：`data_dir` 存 `PathBuf`；`create_session` 里 `JsonlSessionRepo::new(data_dir)` → `repo.create(CreateSessionOptions { name: Some(name), initial_model: Some(config.model.clone()), ..Default::default() })` → `Session::new(storage)`（注意 `SessionRepo::create` 返回 `Arc<dyn SessionStorage>`）→ `assemble` 构造 harness → 写 sidecar（sandbox_id 来自 `RemoteSandbox::sandbox_id()`）→ 组 `MobileSession`
- `list_sessions`：`repo.list(ListSessionOptions::default())` → 映射 `SessionMetadata`（`created_at.timestamp_millis()` 转 ms）+ 读 sidecar 的 `last_active_at_ms`；sidecar 缺失按 0
- `delete_session`：`repo.delete(id)` + 删 sidecar 目录
- `lib.rs`：`MobileSession::new` 重构为 `pub(crate) async fn new_with_session(config, session: Session) -> anyhow::Result<Arc<Self>>`（原 `new_in_memory` 调用点改为先 `Session::new(InMemorySessionStorage)`——若 `with_session` 语义要求，保留 `new_in_memory` 于内部测试）；`uniffi::setup_scaffolding!` 保持 lib.rs；`manager.rs` 导出类型加进 uniffi（同 crate 一个 scaffolding 即可，`#[derive(uniffi::Object)]` 已覆盖）
- Cargo.toml 无新依赖（serde_json 已有；tempfile 加 dev-dependency）

- [ ] **Step 4: 运行测试通过**

Run: 同 Step 2
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add crates/llm-harness-mobile-core
git commit -m "feat(mobile-core): session manager with jsonl persistence"
```

---

### Task 3: Rust — open_session（resume：历史回放 + 沙箱重连）

**Files:**
- Modify: `crates/llm-harness-mobile-core/src/manager.rs`
- Modify: `crates/llm-harness-mobile-core/src/lib.rs`（`MobileSession` 增加 `history: Vec<MobileHistoryMessage>` 与访问器）
- Test: `crates/llm-harness-mobile-core/tests/manager.rs`

**Interfaces:**
- Consumes: Task 2 的 manager/assemble/sidecar；`Harness::with_session`；`RemoteSandboxClient` 裸 HTTP GET 查活性（复用 client 的 `get_sandbox`，见下）
- Produces:
  - `manager.open_session(config: MobileConfig, session_id: String) -> Result<Arc<MobileSession>, MobileError>`
  - `MobileSession.history() -> Vec<MobileHistoryMessage>`
  - `pub struct MobileHistoryMessage { role: String /* user|assistant|tool */, text: String, is_error: bool }`（uniffi::Record）
  - resume 语义：sidecar 的 sandbox_id → `client.get_sandbox(id)`：`Ok(running)` → 用该 id 重连（`RemoteSandbox` 需支持"接管既有 sandbox"——见 Step 3 注）；`Err(Gateway{status:404})`/`Err(Timeout)`/其他 → 新建沙箱并置 `sandbox_recreated: true`
  - `MobileSession` 新增 `sandbox_recreated: bool` + getter；UI 据此注入 SystemNotice

- [ ] **Step 1: 写失败测试**（历史回放映射为纯函数，不依赖网关）

```rust
// tests/manager.rs 追加
use llm_harness_mobile_core::manager::history_to_mobile;
use llm_harness_types::{AgentMessage, UserMessage, ContentBlock};

#[test]
fn history_mapping_extracts_user_and_assistant_text() {
    let msgs = vec![
        AgentMessage::User(UserMessage { content: vec![ContentBlock::text("hi".into())] }),
        // assistant 变体按 llm-harness-types 实际字段构造
    ];
    let out = history_to_mobile(&msgs);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].role, "user");
    assert_eq!(out[0].text, "hi");
}
```

- [ ] **Step 2: 运行确认失败**

Run: `SDKROOT=… cargo test -p llm-harness-mobile-core --test manager`
Expected: FAIL（`history_to_mobile` 未定义）

- [ ] **Step 3: 实现**

- `history_to_mobile(&[AgentMessage]) -> Vec<MobileHistoryMessage>`：`User`→role "user"、拼 `ContentBlock::Text`；`Assistant`→"assistant"、拼 `DataBlock::Text`（`Progress`/`FinalAnswer` 均回放）；`ToolResult`→"tool"、文本 + `is_error` 按 exit/失败判定（读 `ToolResultMessage` 实际字段后定）；`BranchSummary`/`Custom`→跳过
- `open_session`：`repo.open(session_id)` → `Session::new(storage)` → `build_context()` 拿 `messages` → `history_to_mobile` → `assemble` 前：
  - 读 sidecar sandbox_id，非空则 `client.get_sandbox(&id)`：活 → 用 `RemoteSandbox` 需要的"按既有 id 重连"。`RemoteSandbox::connect` 只能新建；查 `sandbox-remote` 是否有 open-by-id 构造；**没有则** 直接新建沙箱并置 `sandbox_recreated=true`（保守正确：新沙箱等价于"失效新建"，只是少了活性判断收益——**计划默认走保守路径**，活性查询仅用于决定是否给用户提示"原沙箱仍在，但因实现限制已新建"？——否，这会让用户困惑。**定案：v1 一律新建沙箱 + SystemNotice**，`get_sandbox` 活性检查推迟。此决定缩小 Rust 面改动，行为可预测。）
- 因此本任务最终行为：`open_session` = 历史回放 + 新建沙箱 + `sandbox_recreated=true`；sidecar 仍记录新 sandbox_id
- `list_sessions` 中 sidecar 读取路径同步更名逻辑不变

- [ ] **Step 4: 运行测试通过**

Run: 同 Step 2
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add crates/llm-harness-mobile-core
git commit -m "feat(mobile-core): resume sessions with history replay + fresh sandbox"
```

> 注：Step 3 偏离 spec 的"尝试重连"——spec 前提是 `RemoteSandbox` 能按 id 重连，实际接口不支持。保守实现（一律新建）满足验收标准 4 的用户可见行为（历史保留 + 系统提示），只是放弃了"文件还在"的优化。执行者若发现 `sandbox-remote` 已有 open-by-id API，可按 spec 实现重连分支并补测试。

---

### Task 4: Rust — uniffi 绑定重生成 + manager e2e 冒烟

**Files:**
- Modify: `crates/llm-harness-mobile-core/src/lib.rs`（如 uniffi 需要显式导出清单）
- Test: `crates/llm-harness-mobile-core/tests/manager.rs`（补一条 live 冒烟，`#[ignore]` 标注）

**Interfaces:**
- Produces: Kotlin 侧 `uniffi/llm_harness_mobile_core/…kt` 含 `MobileSessionManager`、`SessionSummary`、`MobileHistoryMessage`、`ToolFinished.full_output`

- [ ] **Step 1: 补 live 冒烟测试**（gateway + SiliconFlow 环境变量驱动，本地默认跳过）

```rust
#[tokio::test]
#[ignore] // cargo run 时用 --ignored 执行；需要 GATEWAY_URL/GATEWAY_TOKEN/SF_API_KEY
async fn manager_create_and_resume_roundtrip() {
    // create_session → send → poll until RunFinished → drop session
    // open_session → history 非空 → sandbox_recreated == true
}
```

- [ ] **Step 2: 跑 build-android.sh 重生成 Kotlin 绑定**

Run: `SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk CARGO_HTTP_PROXY="" bash scripts/build-android.sh`
Expected: `mobile/android/app/src/main/java/uniffi/llm_harness_mobile_core/…kt` 含新类型；提交绑定 .kt（源代码，非 .so）

- [ ] **Step 3: Rust 全测试**

Run: `SDKROOT=… CARGO_HTTP_PROXY="" cargo test -p llm-harness-mobile-core`
Expected: PASS

- [ ] **Step 4: 提交**

```bash
git add crates/llm-harness-mobile-core mobile/android/app/src/main/java
git commit -m "feat(mobile-core): expose session manager over uniffi"
```

---

### Task 5: Kotlin — Gradle 接入 Compose + Theme

**Files:**
- Modify: `mobile/android/build.gradle.kts`（root plugins：加 `org.jetbrains.kotlin.plugin.compose` version `2.0.21`）
- Modify: `mobile/android/app/build.gradle.kts`（`buildFeatures { compose = true }`、compose-bom、material3、activity-compose、lifecycle-viewmodel-compose、navigation-compose、datastore-preferences 依赖）
- Create: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/theme/Theme.kt`

**Interfaces:**
- Produces: `OhmMobileTheme(content: @Composable () -> Unit)` 供所有 Screen 使用

- [ ] **Step 1: 升级依赖**（版本用 BOM 2024.12.01 或更新；kotlin compiler extension 走 compose plugin，Kotlin 2.0 无需 composeOptions）

```kotlin
// app/build.gradle.kts dependencies 追加
val composeBom = platform("androidx.compose:compose-bom:2024.12.01")
implementation(composeBom)
implementation("androidx.compose.ui:ui")
implementation("androidx.compose.material3:material3")
implementation("androidx.compose.ui:ui-tooling-preview")
debugImplementation("androidx.compose.ui:ui-tooling")
implementation("androidx.activity:activity-compose:1.9.3")
implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.7")
implementation("androidx.navigation:navigation-compose:2.8.5")
implementation("androidx.datastore:datastore-preferences:1.1.1")
```

root `build.gradle.kts` plugins 加：
```kotlin
id("org.jetbrains.kotlin.plugin.compose") version "2.0.21" apply false
```
app 模块 plugins 加 `id("org.jetbrains.kotlin.plugin.compose")`。

- [ ] **Step 2: Theme.kt**

```kotlin
@Composable
fun OhmMobileTheme(content: @Composable () -> Unit) {
    val context = LocalContext.current
    val dark = isSystemInDarkTheme()
    val scheme = when {
        Build.VERSION.SDK_INT >= Build.VERSION_CODES.S ->
            if (dark) dynamicDarkColorScheme(context) else dynamicLightColorScheme(context)
        dark -> darkColorScheme()
        else -> lightColorScheme()
    }
    MaterialTheme(colorScheme = scheme, content = content)
}
```

- [ ] **Step 3: 构建验证**

Run: `cd mobile/android && JAVA_HOME="/Applications/Android Studio.app/Contents/jbr/Contents/Home" gradle assembleDebug`
Expected: BUILD SUCCESSFUL（此时 UI 还是旧 XML，无引用冲突）

- [ ] **Step 4: 提交**

```bash
git add mobile/android
git commit -m "feat(mobile): add compose toolchain and material3 theme"
```

---

### Task 6: Kotlin — ConfigRepository（DataStore）

**Files:**
- Create: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/data/ConfigRepository.kt`
- Test: 手动验证（DataStore 需 Android 环境；逻辑极薄不建 JVM 假测试——不 mock）

**Interfaces:**
- Produces:
  - `data class AppConfig(gatewayUrl, gatewayToken, apiKey, model, llmBaseUrl, provider, allowInsecureHttp, trustProfile: String)`（字段与 Rust `MobileConfig` 一一对应）
  - `ConfigRepository(context)`:
    - `val config: Flow<AppConfig?>`
    - `suspend fun save(config: AppConfig)`
    - `suspend fun isComplete(): Boolean`（gatewayUrl/token/apiKey/model 非空）

- [ ] **Step 1: 实现**（Preferences DataStore，键名 `gateway_url` 等 snake_case；`provider` 默认 `"openai"`，`trust_profile` 默认 `"restricted"`）

- [ ] **Step 2: 构建验证**

Run: `gradle assembleDebug`（同 Task 5 命令）
Expected: BUILD SUCCESSFUL

- [ ] **Step 3: 提交**

```bash
git add mobile/android/app/src/main/java/dev/ohmyharness/mobile/data
git commit -m "feat(mobile): config persistence via datastore"
```

---

### Task 7: Kotlin — AppViewModel + 状态模型

**Files:**
- Create: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/AppUiState.kt`
- Create: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/AppViewModel.kt`

**Interfaces:**
- Consumes: Task 4 的 uniffi 类型（`MobileSessionManager`、`MobileSession`、`MobileEvent`、`SessionSummary`、`MobileHistoryMessage`）、Task 6 `ConfigRepository`
- Produces（Task 8/9 的 UI 消费）:
  - `AppUiState`（spec 定义：`configComplete/connection/sessions/activeSessionId/messages`）
  - `ConnectionState` sealed（`Idle/Connecting/Connected(trustProfile)/Failed(message)`）
  - `ChatMessage` sealed（`User/Assistant(text,isStreaming)/Tool(name,summary,fullOutput,isError)/SystemNotice(text)`）
  - `AppViewModel(factory)`: `val uiState: StateFlow<AppUiState>`；`fun connect()`, `fun send(text)`, `fun switchSession(id)`, `fun newSession(name)`, `fun deleteSession(id)`, `fun saveConfig(cfg)`
  - 事件泵：每 MobileSession 一个 coroutine，100ms `pollEvent()`；`TextDelta` 累积到最后一条 Assistant；`ToolStarted/Finished` 追加 Tool；`RunFinished` 置 `isStreaming=false`；`Failed` → SystemNotice
  - `ToolFinished.isError` 判定：summary 含 "Command exited with code" 或 Rust `Err` 分支（full_output 为空时）

- [ ] **Step 1: 实现状态模型与 ViewModel**（连接状态机、消息映射、多会话 Map 管理；MobileSession 生命周期由 ViewModel 持有，`onCleared` 逐个 `shutdown`）
- [ ] **Step 2: 构建验证**

Run: `gradle assembleDebug`
Expected: BUILD SUCCESSFUL

- [ ] **Step 3: 提交**

```bash
git add mobile/android/app/src/main/java
git commit -m "feat(mobile): app viewmodel with multi-session state"
```

---

### Task 8: Kotlin — 组件与四个屏幕 + 路由

**Files:**
- Create: `ui/component/{MessageBubble,ToolStatusCard,ConnectionBanner,MarkdownBlock,ConfigForm}.kt`
- Create: `ui/screen/{ChatScreen,SetupScreen,SettingsScreen,SessionsScreen}.kt`
- Create: `ui/nav/AppNav.kt`
- Modify: `MainActivity.kt`（重写为 Compose host）
- Delete: `res/layout/activity_main.xml`（及 res/layout 目录）
- Modify: `mobile/android/app/src/main/res/values/themes.xml`（若存在 AppCompat 主题引用，改为 Material3 DayNight NoActionBar；AndroidManifest `android:theme` 同步）

**Interfaces:**
- Consumes: Task 7 的 `AppUiState`/`ChatMessage`/`ConnectionState` + intent 函数
- Produces: 完整可运行 app；`AppNav` 路由 `"chat"|"setup"|"settings"|"sessions"`

- [ ] **Step 1: 组件**——MessageBubble（用户右/主题色、AI 左/surface、长按复制）；ToolStatusCard（图标+首行摘要，`isError` 红色左边框，点击展开 full_output 等宽块）；ConnectionBanner（Connecting 转圈 / Failed 红条+重试）；MarkdownBlock（代码块等宽+横滚+复制按钮，粗体/列表；首版不做完整 MD 解析器，正则切分代码块+行内粗体）；ConfigForm（字段校验：URL http/https 前缀、三项非空；token/apikey 明文切换；trust profile SegmentedButton；LLM base URL 折叠在"高级"）
- [ ] **Step 2: 屏幕组装**——ChatScreen（LazyColumn + 自动滚底 + 上滑暂停 + 回到底部按钮 + 底部输入栏：输入框/发送/停止切换/语音按钮 disabled 占位；顶部栏：会话列表入口 + trust profile 徽标）；SetupScreen/SettingsScreen（ConfigForm + 保存）；SessionsScreen（列表：名称+相对时间+左滑删除，新建按钮）
- [ ] **Step 3: 导航与启动流**——`AppNav`：`configComplete` false → setup，否则 chat；冷启动 ViewModel init 自动 `connect()`
- [ ] **Step 4: 删除旧布局**——`git rm -r mobile/android/app/src/main/res/layout`；`MainActivity` 全部 `findViewById` 逻辑删除；确认无 `R.layout`/`R.id` 残留引用
- [ ] **Step 5: 构建**

Run: `gradle assembleDebug`
Expected: BUILD SUCCESSFUL

- [ ] **Step 6: 提交**

```bash
git add -A mobile/android
git commit -m "feat(mobile): compose chat ui with sessions, settings, markdown"
```

---

### Task 9: 真机端到端验收

**Files:** 无新代码；缺陷修复随发现随改（各自 TDD 小任务）

- [ ] **Step 1: 构建 + 推送安装**：`bash scripts/build-android.sh` → `gradle assembleDebug` → `adb push mobile/android/app/build/outputs/apk/debug/app-debug.apk /sdcard/Download/` → 手机手动安装
- [ ] **Step 2: 验收标准逐条过**（spec 验收 1-6）：
  1. 清 app 数据 → 冷启动向导 → 保存自动连接 → 发消息看流式 + 工具卡
  2. 重启 app 免配置直达 + 自动重连
  3. 新建第二会话独立对话；切走再切回，期间后台会话若在跑任务，事件不丢
  4. 杀进程 → 冷启动 → SessionsScreen 恢复历史消息（只读回放）+ 新沙箱提示
  5. 工具三种状态样式；错误原文可见
  6. 动态取色/暗色/代码块复制
- [ ] **Step 3: 发现的问题记录回 spec 的实测发现节**
- [ ] **Step 4: 提交修复（如有）**

---

## Self-Review 记录

- **Spec coverage**：Rust FFI 扩展（Task 1-4）、Compose 迁移（Task 5-6）、状态与页面（Task 7-8）、resume/多会话（Task 2-4 + Task 9.3/9.4）、错误处理（Task 7 isError + SystemNotice，沿用 8b7ee6d）、测试（各任务内嵌 + Task 9）——spec 验收 6 条全部有任务覆盖
- **类型一致**：`SessionSummary`/`SessionState`/`MobileHistoryMessage` 各任务引用一致；`state.json` 三字段在 Task 2 定义、Task 3 复用
- **已声明偏离**：Task 3 resume 一律新建沙箱（`RemoteSandbox` 无 open-by-id），已在计划中显式标注
- **占位符扫描**：无 TBD/TODO；Task 1 Step 1 标注"字段名以 types 实际定义为准，先读再写"是有意的执行时校验点，非占位
