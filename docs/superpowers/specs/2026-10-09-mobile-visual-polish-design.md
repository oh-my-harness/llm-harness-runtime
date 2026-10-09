# Mobile UI 视觉打磨：开发者工具风设计

日期：2026-10-09
状态：设计已确认（视觉方向：开发者工具风；配色：明暗双主题；用户拍板 OK）
前置：2026-10-08-mobile-chat-ux-upgrade-design.md（功能层已完成并真机验证）

## 问题

当前 UI 是"调试壳"质感，五个具体来源：
1. 顶栏是 TextButton 拼的假 AppBar
2. 字体无层级（全用 Material 默认，无字重/字号对比）
3. 间距无体系（2/4/8/10/12/16dp 硬编码散落，消息间距仅 4dp）
4. 气泡廉价（uniform 12dp 圆角色块，user 消息纯色块）
5. 输入栏裸 TextField 无容器感

## 设计原则

开发者工具风（对标 Termius / Working Copy / VS Code）：
- 深灰蓝底、单一青绿强调色、信息密度中高
- 代码/路径/commit hash/工具输出用等宽字体；聊天正文用无衬线
- 弱气泡：assistant 无背景直接排，user 右对齐窄气泡
- 明暗双主题（开发者风色板两套），不使用 Material You 动态取色

## 设计 Token（theme/ 三文件）

### Color.kt

暗色（主）：
- background `#0D1117`（GitHub Dark 同系）、surface `#161B22`、surfaceVariant `#21262D`
- primary（强调）`#4EC9B0` 青绿、secondary `#CE9178` 橙、error `#F85149`
- onBackground `#E6EDF3`、onSurfaceVariant `#8B949E`（meta 灰）

亮色：
- background `#FFFFFF`、surface `#F6F8FA`、surfaceVariant `#EAEEF2`
- primary `#0E7A6E`（青绿的深色可读版）、onBackground `#1F2328`

### Type.kt

- 字体族：正文 Roboto（默认）；**等宽 JetBrains Mono**（res/font 打包，Regular+Medium，~200KB）用于代码块/工具输出/commit hash/路径/顶栏标题
- 字阶（行高 1.5）：
  - `caption` 11sp（meta/时间戳/连接状态）
  - `body` 14sp（聊天正文、列表项）
  - `bodyLarge` 15sp（assistant 正文——agent 回复是主内容，略大）
  - `titleMedium` 16sp medium（顶栏标题、会话名）
  - `headlineSmall` 18sp semibold（页面大标题）

### Dimens.kt

- `spacingXs 4dp / spacingSm 8dp / spacingMd 12dp / spacingLg 16dp / spacingXl 24dp`
- `messageGap = 12dp`（现 4dp）
- 圆角：`bubble 8dp / card 10dp / input 16dp`

## 组件重设计

### 顶栏（ChatScreen/SessionsScreen/SettingsScreen 统一）

`CenterAlignedTopAppBar` 或普通 `TopAppBar`：标题等宽 `titleMedium`；右侧
`IconButton`（Sessions→列表图标、Settings→齿轮、ChatScreen 加 Search 图标）。
替代现有 TextButton 行。

### 气泡（MessageBubble + UserBubble）

- assistant：**无背景**，正文 `bodyLarge` 直排，宽度撑满，markdown 渲染不变
- user：右对齐，primary 15% alpha 底 + 8dp 圆角，`body` 字号
- SystemNotice：`caption` 灰阶居中（现在是左对齐三级色）

### 输入栏（InputBar）

- 外层 `Surface(shape = RoundedCornerShape(16.dp))` + surfaceVariant 底 + 1dp border
  （焦点时 primary）
- 内部基本 TextField 无自身装饰；右侧圆形 IconButton（发送=primary 底，流式中=stop）
- 整体距底 8dp 悬浮

### 工具卡片（ToolStatusCard）

- 等宽字体输出区；执行中 spinner/竖线用 primary 青绿
- 布局不变（已验证可用），只换字体/颜色 token

### 会话列表（SessionsScreen）

- 每项 meta 行：最后活跃时间 + 消息条数（`labelSmall` + onSurfaceVariant 灰）
- 删除/打开维持现状

## 不做

- 头像（单人 agent）
- 动画/转场系统（后续可加）
- FFI/Rust 改动
- 图片/表情

## 验证

每个组件改完：构建门（`gradle assembleDebug`）→ 真机安装 → uiautomator dump +
用户目测。最终跑全量回归：连接 → 发消息（markdown/工具卡/流式）→ 长按复制 →
搜索 → 会话切换。

## 风险

| 风险 | 缓解 |
|---|---|
| JetBrains Mono 字体打包（license OFL，可自由分发） | 放 `res/font/`，Compose `FontFamily` 引用 |
| TopAppBar 需要 `enableEdgeToEdge` 配合 | 只用普通 TopAppBar（不 edge-to-edge），规避 insets 复杂度 |
| assistant 无气泡后与 SystemNotice 层级混淆 | SystemNotice 用居中 + caption + 三级色，视觉可区分 |
