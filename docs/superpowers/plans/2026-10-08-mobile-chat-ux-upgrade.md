# Mobile 聊天体验升级 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 手机聊天 app 四项体验升级——Markdown 全量流式渲染、工具卡片过程态、长按复制、会话内搜索——以 Kotlin 2.4 工具链升级为前置。

**Architecture:** UI 层全部在 `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/`（Compose M3）。Markdown 渲染引入 mikepenz/multiplatform-markdown-renderer 0.45.0（`rememberStreamingMarkdownState` 处理流式）；工具卡片与历史操作沿用现有 AppViewModel 单向数据流模式，无新架构。

**Tech Stack:** Kotlin 2.4.x、Compose BOM 2025.xx、AGP 8.13.x、mikepenz markdown-renderer-m3/-code 0.45.0。

**Spec:** `docs/superpowers/specs/2026-10-08-mobile-chat-ux-upgrade-design.md`

## Global Constraints

- Kotlin 2.4.x；Compose BOM ≥2025.06；AGP 与 Kotlin 版本组合必须官方兼容
- 每个 Task 结束：`cd mobile/android && JAVA_HOME="/Applications/Android Studio.app/Contents/jbr/Contents/Home" gradle assembleDebug` 必须成功（下称「构建门」）
- Rust 侧（crates/llm-harness-mobile-core）本计划零改动；所有 FFI 事件已齐备
- 真机：K50 (22021211RC)，`ANDROID_SERIAL=dc8a6246`，adb 安装 `adb install -r mobile/android/app/build/outputs/apk/debug/app-debug.apk`
- 每个任务独立提交（conventional commits，`--no-verify`，`docs/superpowers/` 下文件 git add 需 `-f`）
- 环境已知坑：`java` 不在 PATH——gradle 一律带 `JAVA_HOME="/Applications/Android Studio.app/Contents/jbr/Contents/Home"` 前缀；Rust 构建需 `SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk CARGO_HTTP_PROXY=""`（本计划不涉及，仅网关重启时用）

---

### Task 1: Kotlin 2.0.21 → 2.4.x 工具链升级

**Files:**
- Modify: `mobile/android/build.gradle.kts`（插件版本）
- Modify: `mobile/android/app/build.gradle.kts`（Compose BOM、可能 compileSdk）
- Modify: `mobile/android/gradle/wrapper/gradle-wrapper.properties`（若 AGP 要求更高 Gradle）

**Interfaces:**
- Consumes: 无
- Produces: Kotlin 2.4.x 编译的 app-debug.apk，行为与升级前完全一致（后续所有 Task 的基线）

- [ ] **Step 1: 查 AGP-Kotlin 兼容矩阵并定版本**

Run: `web_search "Android Gradle Plugin 8.13 Kotlin 2.4 compatibility"` 与 `https://kotlinlang.org/docs/gradle-configure-project.html#apply-the-plugin` 兼容表
Expected: 确定 AGP 目标版本（Kotlin 2.4 需 AGP ≥8.7 即可维持 8.7.3；若 compose compiler 要求更高 Gradle，则升 AGP 并同步升 gradle-wrapper）

- [ ] **Step 2: 改插件版本**

`mobile/android/build.gradle.kts` 全文改为：

```kotlin
plugins {
    id("com.android.application") version "8.7.3" apply false
    id("org.jetbrains.kotlin.android") version "2.4.10" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.4.10" apply false
}
```

（若 Step 1 结论需升 AGP，则 `com.android.application` 改为目标版本并在 Step 4 同步 wrapper）

- [ ] **Step 3: 升 Compose BOM**

`mobile/android/app/build.gradle.kts` 第 46 行改为：

```kotlin
val composeBom = platform("androidx.compose:compose-bom:2025.09.00")
```

（用 2025 下半年任一 stable BOM；目标是被 mikepenz 0.45 依赖的 Compose 版本覆盖）

- [ ] **Step 4: 构建门**

Run: `cd mobile/android && JAVA_HOME="/Applications/Android Studio.app/Contents/jbr/Contents/Home" gradle assembleDebug`
Expected: `BUILD SUCCESSFUL`。若报 metadata 不兼容（仍引用 2.0 编译的传递依赖）或 compose compiler 版本不匹配，按报错补 `kotlin { jvmToolchain(...) }` 或 compose compiler 插件版本对齐。若 AGP 报需要更高 Gradle：改 `gradle-wrapper.properties` 的 `distributionUrl` 到 gradle-8.9+ 后重跑。

- [ ] **Step 5: 真机回归**

Run:
```bash
export PATH="$HOME/Library/Android/sdk/platform-tools:$PATH" && export ANDROID_SERIAL=dc8a6246
adb install -r mobile/android/app/build/outputs/apk/debug/app-debug.apk
adb shell am force-stop dev.ohmyharness.mobile && adb shell am start -n dev.ohmyharness.mobile/.MainActivity
```
等 15s 后 `adb exec-out uiautomator dump /dev/tty` 应含 `text="connected (trusted-full)"`。
用户手动发一条消息确认：流式回复出现、工具卡片出现、切会话正常。

- [ ] **Step 6: Commit**

```bash
git add mobile/android/build.gradle.kts mobile/android/app/build.gradle.kts
git add -f mobile/android/gradle/wrapper/gradle-wrapper.properties 2>/dev/null
git commit --no-verify -m "build(mobile): upgrade Kotlin 2.0.21 -> 2.4.10, Compose BOM 2025.09"
```

---

### Task 2: 引入 mikepenz Markdown 渲染

**Files:**
- Modify: `mobile/android/app/build.gradle.kts`（依赖）
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/component/MessageBubble.kt`（重写渲染）
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/component/MessageBubble.kt` 的引用方 `ChatScreen.kt`（若 Segment/CodeBlock 被外部引用则保留导出）

**Interfaces:**
- Consumes: `ChatMessage.Assistant(text, isStreaming, key)`（AppViewModel.kt 已有）
- Produces: `MessageBubble(message: ChatMessage.Assistant)` 签名不变；`CodeBlock` 若被别处引用保持可导入

- [ ] **Step 1: 加依赖**

`mobile/android/app/build.gradle.kts` dependencies 块追加：

```kotlin
implementation("com.mikepenz:multiplatform-markdown-renderer-m3:0.45.0")
implementation("com.mikepenz:multiplatform-markdown-renderer-code:0.45.0")
```

- [ ] **Step 2: 重写 MessageBubble**

`MessageBubble.kt` 全文替换为：

```kotlin
package dev.ohmyharness.mobile.ui.component

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.unit.dp
import com.mikepenz.markdown.m3.Markdown
import com.mikepenz.markdown.model.rememberMarkdownState
import dev.ohmyharness.mobile.ui.ChatMessage

/** Assistant markdown message. Streaming text re-renders via markdown state. */
@Composable
fun MessageBubble(message: ChatMessage.Assistant, modifier: Modifier = Modifier) {
    Column(
        modifier = modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(12.dp))
            .background(MaterialTheme.colorScheme.surface),
    ) {
        Markdown(
            markdownState = rememberMarkdownState(
                markdown = message.text,
                immediate = message.isStreaming,
                retainState = true,
            ),
        )
        if (message.isStreaming) {
            Text(text = "…", color = MaterialTheme.colorScheme.primary)
        }
    }
}
```

注：`immediate = message.isStreaming` 让流式增量同步解析（小片段成本低），历史整段消息走默认异步解析。若 `rememberMarkdownState` 参数名与 0.45.0 实际 API 不符（`immediate`/`retainState`），以编译报错为准查 `com.mikepenz.markdown.model` 包源码修正参数——API 面以 0.45.0 为准，README 章节已核对存在这两参数。

- [ ] **Step 3: 处理 splitCodeFences/Segment/CodeBlock 的外部引用**

Run: `grep -rn "splitCodeFences\|Segment\|CodeBlock" mobile/android/app/src/main/java --include='*.kt' | grep -v MessageBubble.kt`
Expected: 若无外部引用 → 连同旧实现一起删除（旧文件里已删）。若有引用 → 把引用方改为直接用 Markdown 组件。

- [ ] **Step 4: 构建门 + 真机渲染验证**

Run: 构建门（见 Global Constraints）
Expected: BUILD SUCCESSFUL。

Run: 安装启动（同 Task 1 Step 5），用户发：`用 markdown 写一段话，包含一个 3x3 表格、一个 kotlin 代码块、一个嵌套列表`
Expected: dump 屏幕中表格/代码块以样式化组件渲染（无裸 `|---|` 文本、无裸 ``` 符号）。

- [ ] **Step 5: Commit**

```bash
git add mobile/android/app/build.gradle.kts mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/component/MessageBubble.kt
git commit --no-verify -m "feat(mobile-ui): full markdown rendering via mikepenz renderer"
```

---

### Task 3: 流式零闪烁

**Files:**
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/component/MessageBubble.kt`
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/AppViewModel.kt:240-265`（appendDelta 批量合帧，若 Task 2 真机观察卡顿才需要）

**Interfaces:**
- Consumes: `MobileEvent.TextDelta`（FFI 已有，100ms 轮询泵已存在）
- Produces: 流式过程视觉平滑；无 API 变更

- [ ] **Step 1: 真机压力流式测试**

用户发：`写一篇 800 字的文章，分五个段落，慢慢输出`
观察：文字增长是否卡顿/闪烁/整段重排。

- [ ] **Step 2（条件执行）: 若卡顿，在 appendDelta 加合帧**

AppViewModel.kt 的 `appendDelta`（约 244 行）改为 150ms 窗口聚合：

```kotlin
private var pendingDelta = StringBuilder()
private var deltaFlushJob: Job? = null

private fun appendDelta(sessionId: String, delta: String) {
    pendingDelta.append(delta)
    if (deltaFlushJob?.isActive == true) return
    deltaFlushJob = viewModelScope.launch {
        delay(150)
        val text = pendingDelta.toString()
        pendingDelta.clear()
        _uiState.update { state ->
            val list = state.messages[sessionId].orEmpty().toMutableList()
            val last = list.lastOrNull()
            if (last is ChatMessage.Assistant && last.isStreaming) {
                list[list.lastIndex] = last.copy(text = last.text + text)
            } else {
                list.add(ChatMessage.Assistant(text = text, isStreaming = true, key = nextKey()))
            }
            state.copy(messages = state.messages + (sessionId to list))
        }
    }
}
```

注意：`RunFinished`/`ToolStarted` 时先 flush `pendingDelta`（在对应 handler 开头调 `deltaFlushJob?.join()` 语义的 flush），避免丢尾巴。实现为：

```kotlin
private suspend fun flushPendingDelta(sessionId: String) {
    deltaFlushJob?.join()
}
```

并在 `onEvent` 的 `RunFinished`/`ToolStarted` 分支前 `flushPendingDelta(sessionId)`。

- [ ] **Step 3: 构建门 + 真机复测**

同上压力测试，Expected: 无可感知卡顿、光标尾巴稳定。

- [ ] **Step 4: Commit（有改动才提交）**

```bash
git add mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/AppViewModel.kt mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/component/MessageBubble.kt
git commit --no-verify -m "perf(mobile-ui): coalesce text deltas into 150ms frames"
```

---

### Task 4: 工具卡片执行中过程态

**Files:**
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/AppViewModel.kt`（ChatMessage.Tool + onEvent ToolStarted/ToolFinished）
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/component/ToolStatusCard.kt`
- 参考: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/AppUiState.kt`（ChatMessage 定义位置）

**Interfaces:**
- Consumes: `MobileEvent.ToolStarted { name }`、`MobileEvent.ToolFinished { name, summary, full_output }`（events.rs 已有）
- Produces: `ChatMessage.Tool(name, summary, fullOutput, isError, running, key)`——`running` 带默认值 `false`，resume 回放路径（`history()` 无 running 概念）不受影响

- [ ] **Step 1: 扩展 ChatMessage.Tool**

在 ChatMessage 定义处（AppUiState.kt 或 AppViewModel.kt，先 `grep -rn "data class Tool"` 定位）：

```kotlin
data class Tool(
    val name: String,
    val summary: String,
    val fullOutput: String?,
    val isError: Boolean,
    val running: Boolean = false,
    val key: Long,
)
```

（字段名以现定义为准，只加 `running`；`isError` 若已有默认值保持）

- [ ] **Step 2: ToolStarted 插占位卡 / ToolFinished 原位更新**

AppViewModel.kt 的 `onEvent`：ToolStarted 分支用已有 `updateTool`/`transformTool` 机制（约 279 行 `transform: (ChatMessage.Tool) -> ChatMessage.Tool`）追加或改写：

```kotlin
is MobileEvent.ToolStarted -> {
    appendMessage(sessionId, ChatMessage.Tool(
        name = event.name, summary = "running…", fullOutput = null,
        isError = false, running = true, key = nextKey(),
    ))
}
is MobileEvent.ToolFinished -> {
    updateTool(sessionId, event.name) { tool ->
        if (tool.running) tool.copy(
            summary = event.summary, fullOutput = event.fullOutput.takeIf { it.isNotBlank() },
            isError = event.summary.contains("error", ignoreCase = true), running = false,
        ) else tool
    }
}
```

（`event.summary`/`fullOutput` 属性名以 uniffi 生成 Kotlin 为准，查 `uniffi/llm_harness_mobile_core/llm_harness_mobile_core.kt` 中 `MobileEvent.ToolFinished`；isError 判定若 FFI 有显式标志优先用之——查 `map_event` 的 summary 惯例 `("tool", summary, text)`，无标志则维持现有 isError 逻辑）

- [ ] **Step 3: ToolStatusCard 渲染过程态**

ToolStatusCard.kt 的 Column 内、标题 Text 前：

```kotlin
if (tool.running) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        CircularProgressIndicator(modifier = Modifier.size(14.dp), strokeWidth = 2.dp)
        Spacer(Modifier.width(6.dp))
        ElapsedText(startedAtMs = tool.key)  // key 即 nextKey 时间戳，可当起始时刻
    }
}
```

`ElapsedText`（同文件内新增）：

```kotlin
@Composable
private fun ElapsedText(startedAtMs: Long) {
    var now by remember { mutableLongStateOf(startedAtMs) }
    LaunchedEffect(Unit) {
        while (true) { delay(1000); now = System.currentTimeMillis() }
    }
    Text(
        text = "${(now - startedAtMs).coerceAtLeast(0) / 1000}s",
        style = MaterialTheme.typography.labelSmall,
    )
}
```

若 `nextKey()` 不是时间戳（查实现），改为在 Tool 数据里加 `startedAtMs` 字段于 Step 1 一并处理。

- [ ] **Step 4: 构建门 + 真机验证**

用户发：`git status 一下`（触发 bash 工具）
Expected: 消息流先出现「🔧 bash: running… + 秒数 + spinner」卡片，命令完成后原位变为正常结果卡片，无残留 running 卡。

- [ ] **Step 5: Commit**

```bash
git add mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/
git commit --no-verify -m "feat(mobile-ui): live tool execution cards with spinner and elapsed time"
```

---

### Task 5: 长按复制 + 会话内搜索

**Files:**
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/component/MessageBubble.kt`（assistant 长按）
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/screen/ChatScreen.kt`（user 气泡长按 + 搜索栏）

**Interfaces:**
- Consumes: `uiState.messages[activeSessionId]`（AppUiState 已有）
- Produces: 纯 UI 行为，无状态结构变更

- [ ] **Step 1: 长按复制**

MessageBubble.kt 的 MessageBubble Column modifier 加：

```kotlin
.combinedClickable(
    onLongClick = {
        clipboardManager.setText(AnnotatedString(message.text))  // LocalClipboardManager.current
    },
)
```

user 气泡（ChatScreen.kt 内）同样处理。复制成功给 Toast：

```kotlin
Toast.makeText(context, "Copied", Toast.LENGTH_SHORT).show()
```

- [ ] **Step 2: 会话内搜索栏**

ChatScreen.kt 顶部 TopBar 加搜索 IconButton；展开时在消息列表上方显示 `OutlinedTextField`：

```kotlin
var query by remember { mutableStateOf("") }
val filtered = if (query.isBlank()) messages else messages.filter {
    it is ChatMessage.Assistant && it.text.contains(query, ignoreCase = true)
        || it is ChatMessage.User && it.text.contains(query, ignoreCase = true)
        || it is ChatMessage.Tool && (it.summary + (it.fullOutput ?: "")).contains(query, ignoreCase = true)
}
// LazyColumn 的 items 改用 filtered
```

（YAGNI：只做过滤显示，不做滚动定位/高亮标色）

- [ ] **Step 3: 构建门 + 真机验证**

用户长按一条 assistant 消息 → 系统提示已复制；粘贴到输入框确认内容正确。搜索框输入关键词 → 列表实时过滤。

- [ ] **Step 4: Commit**

```bash
git add mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/
git commit --no-verify -m "feat(mobile-ui): long-press copy and in-session message search"
```

---

## Self-Review 记录

- Spec 覆盖：spec 四项升级 ↔ Task 1（前置工具链）/2+3（Markdown+流式）/4（工具卡片）/5（历史操作）——全覆盖
- 占位符扫描：无 TBD；Task 2/4 中「以编译报错为准查 API」是对外部库 API 名的显式验证步骤而非占位——已给出 README 已核对的参数名与回退方案
- 类型一致性：`ChatMessage.Tool.running`（Task 4 定义，ToolStatusCard 消费）；`rememberMarkdownState(markdown, immediate, retainState)`（Task 2/3 同一签名）
