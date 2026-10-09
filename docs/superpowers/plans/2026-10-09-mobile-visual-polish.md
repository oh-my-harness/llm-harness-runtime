# Mobile 视觉打磨 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 手机 app 视觉从"调试壳"升级为开发者工具风：设计 token 体系（色板/字阶/间距）、真 TopAppBar、弱气泡、悬浮输入栏。

**Architecture:** 纯 Kotlin/Compose 层改动。新增 `theme/Color.kt`、`theme/Type.kt`、`theme/Dimens.kt`（token 源），重写 `Theme.kt` 消费 token；然后按屏幕逐个替换硬编码值。零 FFI/Rust 改动，零行为变更（纯视觉）。

**Tech Stack:** Compose M3（BOM 2026.08 已就位）、JetBrains Mono 字体打包（OFL 许可）、现有 AppViewModel 数据流不动。

**Spec:** `docs/superpowers/specs/2026-10-09-mobile-visual-polish-design.md`

## Global Constraints

- 构建门：`cd mobile/android && JAVA_HOME="/Applications/Android Studio.app/Contents/jbr/Contents/Home" gradle assembleDebug` 成功
- 真机：`export PATH="$HOME/Library/Android/sdk/platform-tools:$PATH" && export ANDROID_SERIAL=dc8a6246`，安装 `adb install -r mobile/android/app/build/outputs/apk/debug/app-debug.apk`（MIUI 弹确认框时提示用户点允许）
- 每任务独立提交（conventional commits + `--no-verify`）
- 零行为变更：所有 intent/状态流/FFI 调用不动，只动视觉
- 字体文件下载后核对 OFL 许可文件一并入库
- 颜色/字号/间距一律引用 token，禁止新硬编码

---

### Task 1: 设计 token 三文件 + Theme 重写 + JetBrains Mono 打包

**Files:**
- Create: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/theme/Color.kt`
- Create: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/theme/Type.kt`
- Create: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/theme/Dimens.kt`
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/theme/Theme.kt`
- Create: `mobile/android/app/src/main/res/font/jetbrains_mono_regular.ttf`、`jetbrains_mono_medium.ttf`（+ OFL.txt → `res/font/jetbrains_mono_ofl.txt`，注意资源名须小写）

**Interfaces:**
- Produces: `OhmColorScheme`（扩展 M3 `ColorScheme` 的语义 token：`codeBackground`、`codeText`、`metaText`）；`OhmTypography`（5 级字阶：`caption/body/bodyLarge/titleMedium/headlineSmall`，等宽样式 `code`）；`OhmSpacing`（object：xs/sm/md/lg/xl）、`OhmRadius`（bubble 8dp/card 10dp/input 16dp）。后续所有 Task 只从这些对象取值。

- [ ] **Step 1: 下载 JetBrains Mono**

Run:
```bash
curl -sL -o /tmp/jbm.zip "https://github.com/JetBrains/JetBrainsMono/releases/download/v2.304/JetBrainsMono-2.304.zip" && unzip -o -q /tmp/jbm.zip -d /tmp/jbm && ls /tmp/jbm/fonts/ttf/ | head -4
```
Expected: 看到 `JetBrainsMono-Regular.ttf`、`JetBrainsMono-Medium.ttf` 等。

- [ ] **Step 2: 拷贝字体进 res**

```bash
mkdir -p mobile/android/app/src/main/res/font
cp /tmp/jbm/fonts/ttf/JetBrainsMono-Regular.ttf mobile/android/app/src/main/res/font/jetbrains_mono_regular.ttf
cp /tmp/jbm/fonts/ttf/JetBrainsMono-Medium.ttf mobile/android/app/src/main/res/font/jetbrains_mono_medium.ttf
curl -sL -o mobile/android/app/src/main/res/font/jetbrains_mono_ofl.txt "https://raw.githubusercontent.com/JetBrains/JetBrainsMono/master/OFL.txt"
```
（资源文件名必须小写下划线）

- [ ] **Step 3: 写 Color.kt**

```kotlin
package dev.ohmyharness.mobile.ui.theme

import androidx.compose.material3.ColorScheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.ui.graphics.Color

// Dark (primary developer palette)
private val DarkBackground = Color(0xFF0D1117)
private val DarkSurface = Color(0xFF161B22)
private val DarkSurfaceVariant = Color(0xFF21262D)
private val DarkPrimary = Color(0xFF4EC9B0)
private val DarkSecondary = Color(0xFFCE9178)
private val DarkError = Color(0xFFF85149)
private val DarkOnBackground = Color(0xFFE6EDF3)
private val DarkMeta = Color(0xFF8B949E)

// Light
private val LightBackground = Color(0xFFFFFFFF)
private val LightSurface = Color(0xFFF6F8FA)
private val LightSurfaceVariant = Color(0xFFEAEEF2)
private val LightPrimary = Color(0xFF0E7A6E)
private val LightSecondary = Color(0xFFB3552F)
private val LightError = Color(0xFFC73E3E)
private val LightOnBackground = Color(0xFF1F2328)
private val LightMeta = Color(0xFF656D76)

fun ohmDarkColorScheme(): ColorScheme = darkColorScheme(
    primary = DarkPrimary, onPrimary = Color(0xFF0D1117),
    secondary = DarkSecondary, error = DarkError,
    background = DarkBackground, onBackground = DarkOnBackground,
    surface = DarkSurface, onSurface = DarkOnBackground,
    surfaceVariant = DarkSurfaceVariant, onSurfaceVariant = DarkMeta,
    outline = DarkMeta,
)

fun ohmLightColorScheme(): ColorScheme = lightColorScheme(
    primary = LightPrimary, onPrimary = Color.White,
    secondary = LightSecondary, error = LightError,
    background = LightBackground, onBackground = LightOnBackground,
    surface = LightSurface, onSurface = LightOnBackground,
    surfaceVariant = LightSurfaceVariant, onSurfaceVariant = LightMeta,
    outline = LightMeta,
)
```

- [ ] **Step 4: 写 Type.kt**

```kotlin
package dev.ohmyharness.mobile.ui.theme

import androidx.compose.material3.Typography
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp
import dev.ohmyharness.mobile.R

val JetBrainsMono = FontFamily(
    Font(R.font.jetbrains_mono_regular, FontWeight.Normal),
    Font(R.font.jetbrains_mono_medium, FontWeight.Medium),
)

/** Code/path/commit/tool-output text; pair with `metaText` color. */
val CodeTextStyle = TextStyle(
    fontFamily = JetBrainsMono,
    fontSize = 13.sp,
    lineHeight = 19.sp, // 1.5
)

val OhmTypography = Typography(
    labelSmall = TextStyle(fontSize = 11.sp, lineHeight = 17.sp),        // meta
    bodySmall = TextStyle(fontSize = 12.sp, lineHeight = 18.sp),         // tool summary
    bodyMedium = TextStyle(fontSize = 14.sp, lineHeight = 21.sp),        // user text / list
    bodyLarge = TextStyle(fontSize = 15.sp, lineHeight = 23.sp),         // assistant prose
    titleMedium = TextStyle(fontSize = 16.sp, lineHeight = 22.sp, fontWeight = FontWeight.Medium),
    headlineSmall = TextStyle(fontSize = 18.sp, lineHeight = 25.sp, fontWeight = FontWeight.SemiBold),
)
```
（`import androidx.compose.ui.unit.sp` 别漏）

- [ ] **Step 5: 写 Dimens.kt**

```kotlin
package dev.ohmyharness.mobile.ui.theme

import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

object OhmSpacing {
    val xs = 4.dp; val sm = 8.dp; val md = 12.dp; val lg = 16.dp; val xl = 24.dp
    val messageGap = 12.dp
}

object OhmRadius {
    val bubble = 8.dp; val card = 10.dp; val input = 16.dp
}
```

- [ ] **Step 6: 重写 Theme.kt**

```kotlin
package dev.ohmyharness.mobile.ui.theme

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable

/**
 * Developer-tool theme: fixed ohm palettes (no Material You dynamic color),
 * light/dark follow the system.
 */
@Composable
fun OhmMobileTheme(content: @Composable () -> Unit) {
    val dark = isSystemInDarkTheme()
    MaterialTheme(
        colorScheme = if (dark) ohmDarkColorScheme() else ohmLightColorScheme(),
        typography = OhmTypography,
        content = content,
    )
}
```

- [ ] **Step 7: 构建门**

Run: 构建门（Global Constraints）
Expected: BUILD SUCCESSFUL（此时 UI 未变——token 尚未被消费）

- [ ] **Step 8: Commit**

```bash
git add mobile/android/app/src/main/res/font/ mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/theme/
git commit --no-verify -m "feat(mobile-ui): design tokens - ohm color/type/spacing, JetBrains Mono packaged"
```

---

### Task 2: 顶栏 TopAppBar 统一（四屏）

**Files:**
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/screen/ChatScreen.kt:76-99`
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/screen/SessionsScreen.kt:45-60,84-100`
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/screen/SettingsScreen.kt:26-27`
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/screen/SetupScreen.kt`（若有同款假顶栏）

**Interfaces:**
- Consumes: Task 1 的 `OhmTypography`（等宽标题）、`OhmSpacing`
- Produces: 各屏签名不变（onSessions/onSettings/onBack 参数不变）

- [ ] **Step 1: ChatScreen 顶栏**

替换 Row(TextButton…, connection label, TextButton…) 为：

```kotlin
TopAppBar(
    title = {
        Text(
            "oh-my-harness",
            style = OhmTypography.titleMedium.copy(fontFamily = JetBrainsMono),
        )
    },
    actions = {
        IconButton(onClick = { if (searchOpen) searchQuery = ""; searchOpen = !searchOpen }) {
            Icon(if (searchOpen) Icons.Filled.Close else Icons.Filled.Search, "Search")
        }
        IconButton(onClick = onSessions) {
            Icon(Icons.AutoMirrored.Filled.List, "Sessions")
        }
        IconButton(onClick = onSettings) {
            Icon(Icons.Filled.Settings, "Settings")
        }
    },
    colors = TopAppBarDefaults.topAppBarColors(
        containerColor = MaterialTheme.colorScheme.surface,
    ),
)
```

连接状态 label 移到 ConnectionBanner（已有组件）内展示，不占顶栏。

- [ ] **Step 2: SessionsScreen/SettingsScreen 顶栏同款**

`TextButton(onBack) { Text("Back") }` → `TopAppBar(title = { Text("Sessions"/"Settings") }, navigationIcon = { IconButton(onBack) { Icon(Icons.AutoMirrored.Filled.ArrowBack, "Back") } })`。

- [ ] **Step 3: 构建门 + 真机目测**

构建门 → 安装（用户点确认框）→ 启动。用户目测：顶栏像正式 app 了。

- [ ] **Step 4: Commit**

```bash
git add mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/screen/ mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/component/
git commit --no-verify -m "feat(mobile-ui): real TopAppBar on all screens with icon actions"
```

---

### Task 3: 弱气泡 + 间距体系（ChatScreen/MessageBubble）

**Files:**
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/screen/ChatScreen.kt`（UserBubble、消息间距）
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/component/MessageBubble.kt`

**Interfaces:**
- Consumes: `OhmSpacing.messageGap`、`OhmRadius.bubble`、`CodeTextStyle`
- Produces: MessageBubble/UserBubble 签名不变

- [ ] **Step 1: assistant 无气泡化**

MessageBubble.kt 的 Column：删 `.clip(...)` `.background(...)`，padding 换 `padding(vertical = OhmSpacing.sm)`；正文 style 用 `OhmTypography.bodyLarge`。长按复制保留（combinedClickable 移到外层 Column，去掉 clip 以外的背景）。

- [ ] **Step 2: user 气泡弱化**

ChatScreen.kt UserBubble：`.background(colorScheme.primary.copy(alpha = 0.15f))` + `RoundedCornerShape(OhmRadius.bubble)`，文字色改 `colorScheme.onBackground`，`body` 字号。

- [ ] **Step 3: 间距体系**

消息项 `padding(horizontal = OhmSpacing.sm, vertical = OhmSpacing.messageGap / 2)`（相邻项合计 messageGap）；所有硬编码 8.dp/16.dp 水平边距换成 `OhmSpacing.sm/lg`。

- [ ] **Step 4: 构建门 + 真机对比**

构建 → 安装 → 用户发一条消息，目测：assistant 无底色、user 窄气泡、间距呼吸感。

- [ ] **Step 5: Commit**

```bash
git add mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/
git commit --no-verify -m "feat(mobile-ui): weak bubbles + spacing system"
```

---

### Task 4: 悬浮输入栏

**Files:**
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/screen/InputBar.kt`

**Interfaces:**
- Consumes: `OhmRadius.input`、`OhmSpacing`；签名不变（enabled/value/onValueChange/onSend/onStop/onSteer/streaming）
- Produces: 无 API 变化

- [ ] **Step 1: 重写布局**

```kotlin
Surface(
    shape = RoundedCornerShape(OhmRadius.input),
    color = MaterialTheme.colorScheme.surfaceVariant,
    border = BorderStroke(1.dp, MaterialTheme.colorScheme.outline.copy(alpha = 0.4f)),
    modifier = modifier.fillMaxWidth().padding(horizontal = OhmSpacing.sm, vertical = OhmSpacing.sm),
) {
    Row(verticalAlignment = Alignment.Bottom) {
        BasicTextField(
            value = value, onValueChange = onValueChange, enabled = enabled,
            modifier = Modifier.weight(1f).padding(OhmSpacing.md),
            textStyle = MaterialTheme.typography.bodyMedium.copy(color = MaterialTheme.colorScheme.onBackground),
            maxLines = 5,
            decorationBox = { inner ->
                if (value.isEmpty()) Text("message", color = MaterialTheme.colorScheme.onSurfaceVariant, style = MaterialTheme.typography.bodyMedium)
                inner()
            },
        )
        // 右侧圆形按钮：streaming 时 stop(红叉/方块)，value 非空可 steer 时 steer 图标，否则 send
        Surface(shape = CircleShape, color = if (canSend) colorScheme.primary else colorScheme.surface) {
            IconButton(onClick = { when { streaming -> onStop(); value.isNotBlank() -> onSend(value.trim()) } }, enabled = enabled) {
                Icon(if (streaming) Icons.Filled.Stop else Icons.AutoMirrored.Filled.Send, tint = if (canSend) colorScheme.onPrimary else colorScheme.onSurfaceVariant)
            }
        }
    }
}
```

- [ ] **Step 2: steer 保留**

streaming 且有文字时在输入条上方显示一行小 Steer 按钮（现有逻辑挪位），或长按发送键 steer——**选简单方案**：保持现有 Steer TextButton，放在输入条与键盘之间右侧，`caption` 字号。

- [ ] **Step 3: 构建门 + 真机**

构建 → 安装 → 目测：悬浮圆角条、按钮圆形强调色、流式中按钮变 stop。

- [ ] **Step 4: Commit**

```bash
git add mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/screen/InputBar.kt
git commit --no-verify -m "feat(mobile-ui): floating rounded input bar with circular action button"
```

---

### Task 5: 工具卡片/会话列表 token 化 + 全量回归

**Files:**
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/component/ToolStatusCard.kt`
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/screen/SessionsScreen.kt`
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/component/ConnectionBanner.kt`

**Interfaces:**
- Consumes: 全部 token；无新接口
- Produces: 无

- [ ] **Step 1: 工具卡片等宽化**

ToolStatusCard：fullOutput 区 `CodeTextStyle`；running 竖线/spinner 用 primary；圆角 `OhmRadius.card`；padding 用 token。

- [ ] **Step 2: 会话列表 meta 行**

SessionsScreen 列表项：标题 `titleMedium`，meta 行 `labelSmall` + onSurfaceVariant（最后活跃时间已有字段，无则只显示 id 前缀等宽）。顶栏按 Task 2 已改。

- [ ] **Step 3: ConnectionBanner**

连接状态文案 `caption` + meta 色；connected 用 primary。

- [ ] **Step 4: 构建门 + 全量真机回归**

构建 → 安装 → 用户配合：
1. 连接（connected 显示）
2. 发 markdown+git 消息（渲染/工具卡/流式）
3. 长按复制、搜索过滤
4. 会话切换、Settings 进出

- [ ] **Step 5: Commit**

```bash
git add mobile/android/app/src/main/java/dev/ohmyharness/mobile/ui/
git commit --no-verify -m "feat(mobile-ui): tokenize tool cards, sessions list, connection banner"
```

---

## Self-Review 记录

- Spec 覆盖：五痛点 ↔ Task 2（顶栏）/3（气泡+间距）/4（输入栏）/1+5（字体+token+工具卡+列表）——全覆盖
- 占位符：无 TBD；Task 4 代码块为完整可编译骨架（BasicTextField decorationBox 模式）
- 类型一致：`OhmTypography/OhmSpacing/OhmRadius/CodeTextStyle` 在 Task 1 定义，Task 2-5 同名消费；`OhmColorScheme` 未作为类型引入（用标准 ColorScheme 扩展语义色可后续加，避免过度设计）
