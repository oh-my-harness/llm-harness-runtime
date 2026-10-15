# Mobile 聊天体验升级：Markdown 流式渲染 + 工具卡片 + 历史操作

日期：2026-10-08
状态：设计已确认（对话选型：对话体验优先；四痛点全选；技术路线按调研结论确定）
前置：2026-09-29-mobile-ui-redesign-design.md（聊天 UI 已落地：Compose M3 + AppViewModel 单向数据流）

## 概要

在已上线的聊天 UI 基础上做四项体验升级：
1. Markdown 全量渲染（表格/嵌套列表/代码高亮）替换自制 code-fence 分段
2. LLM 流式输出零闪烁渲染（增量解析）
3. 工具卡片"执行中"过程态（spinner + 耗时）
4. 历史操作（长按复制 + 会话内搜索）

## 技术选型（2026-10 调研结论）

| 候选 | 结论 | 依据 |
|---|---|---|
| mikepenz/multiplatform-markdown-renderer 0.45.0 | **采用** | 1.1k star 活跃维护；`rememberStreamingMarkdownState()` 只重解析不稳定尾部；GFM 表格/GitHub alerts 开箱；`-code` 模块代码高亮；Apache-2.0 |
| antgroup/FluidMarkdown | 排除 | Android 端基于 Markwon/TextView View 体系，与本项目纯 Compose 架构不合 |
| huarangmeng/Markdown 1.5.0 | 观望 | 功能最全（LaTeX/Mermaid）但 53 star 太新，生产风险高 |
| GetStream/stream-chat-android-ai | 仅参考 | 强绑定 Stream Chat 后端；组件设计思路可借鉴 |
| 自制渲染器 | 排除 | 表格/嵌套列表实现量等于维护 mini markdown 引擎 |

**硬约束**：mikepenz 0.45.0 以 Kotlin 2.4.10 编译（klib metadata），Kotlin 编译器只能读
ahead 一个 minor 版本 → 项目 Kotlin 必须先从 2.0.21 升级到 ≥2.3（直接 2.4.x）。

## 实施顺序（四个独立可交付步）

### 步骤 0：工具链升级（前置，独立提交）

- Kotlin `2.0.21` → `2.4.x`（AGP 8.7.3 兼容性验证；不兼容则 AGP 同步升）
- Compose BOM `2024.12.01` → 最新（≥2025.xx，需 Kotlin 2.4 配套 compose compiler）
- **回归标准**：`./gradlew assembleDebug` 成功 + 真机验证五项——连接、发消息流式回复、
  工具卡片出现、会话切换、Settings 保存
- 回滚点：升级单独提交，出问题 revert 一个 commit 即可

### 步骤 1：Markdown 渲染

- 依赖：`com.mikepenz:multiplatform-markdown-renderer-m3:0.45.0` + `-code`（代码高亮）
- `MessageBubble.kt`：删 `splitCodeFences`/`Segment`，改 `rememberStreamingMarkdownState` +
  `Markdown(markdownState)`；流式时 `retainState = true` 防闪烁
- 非流式历史消息（resume 回放）用同一组件（state 内部处理）
- **验证**：真机让 agent 回复含表格+代码块+列表的消息，dump 检查渲染

### 步骤 2：工具卡片过程态

- `MobileEvent.ToolStarted` 已存在（name）——AppViewModel 在 ToolStarted 时插入
  `ChatMessage.Tool(running = true)` 占位卡，ToolFinished 原位更新（已有 updateTool 机制）
- `ToolStatusCard.kt`：`running` 时显示 `CircularProgressIndicator(16dp)` + 已运行秒数
  （LaunchedEffect 计时）；完成态不变
- ChatMessage.Tool 加 `running: Boolean = false` 字段（默认值保证 resume 回放不破）

### 步骤 3：历史操作

- 长按复制：MessageBubble/User 气泡加 `combinedClickable(onLongClick = 复制到剪贴板 + Toast)`
- 会话内搜索：ChatScreen 顶部搜索图标 → 展开 TextField，输入时对 `uiState.messages[activeId]`
  过滤高亮（不重排，滚动到命中处可选；YAGNI：先只做过滤显示）

## 不做的事（YAGNI）

- LaTeX/Mermaid 渲染（huarangmeng 有但当前 agent 输出用不到）
- 图片渲染（sandbox 内无图片输出场景）
- 会话历史全局搜索（先做单会话内）
- 消息重排/编辑

## 风险

| 风险 | 缓解 |
|---|---|
| Kotlin 2.4 升级破坏 uniffi 生成代码 | uniffi 生成的是纯 Kotlin（无 Kotlin 特性依赖），风险低；assembleDebug 立刻暴露 |
| AGP 8.7.3 与 Kotlin 2.4 不兼容 | 同步升 AGP 至最新 stable（8.13.x）；gradle-wrapper 需 ≥8.9 |
| mikepenz 库与 Compose BOM 版本冲突 | 库依赖 Compose Multiplatform；Android 端走 androidx.compose 双载入，实测确认 |
| 流式渲染每 delta 重解析卡顿 | `rememberStreamingMarkdownState` 官方场景；真机实测，卡则 100ms 批量合帧 |
