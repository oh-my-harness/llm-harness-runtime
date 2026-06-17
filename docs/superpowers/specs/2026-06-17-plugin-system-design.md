# Oh-My-Harness 插件系统设计

> 日期：2026-06-17 | v3（2026-06-17 code review 后修订）
> 范围：`llm-harness-types` / `llm-harness-loop` / `llm-harness-agent` / `llm-harness-runtime`
> 目标：让外部开发者以 Cargo 编译期依赖的方式编写可组合插件
> 约束：底层库，不做运行时动态加载 / UI 扩展 / 热重装

---

## 修订记录（Implementation Plan 前 Code Review 发现）

以下问题在写实现计划时通过对代码库的完整 review 发现，已反映在修订后的设计中：

1. **`HarnessHooks` 实际位置**：规范 v2 说它在 `llm-harness-types/src/hooks.rs`，实际在 `llm-harness-agent/src/harness.rs` 61 行。保持现状——`HarnessHooks` 聚合了 `AgentHarness` 使用的 11 个 hook trait，留在 agent crate 合理。`hooks.rs`（types crate）定义的是各 hook **trait** 和 context 类型。

2. **`auth` 字段重复**：`AgentHarness` 目前有独立的 `auth: Option<Arc<dyn AuthHook>>` 字段（第 432 行）和 `AgentHarnessOptions.auth`（第 309 行）。规范建议在 `HarnessHooks` 加 `auth: Vec<Arc<dyn AuthHook>>`，但未说明如何处理旧的独立 `auth` 字段。**决定**：迁移后移除 `AgentHarness.auth` 和 `AgentHarnessOptions.auth`，统一通过 `hooks.auth` 访问。

3. **`singleton()` / `none_vec()` 泛型辅助函数无法编译**：`fn singleton<T>(hook: Arc<dyn T>) -> Vec<Arc<dyn T>>` 在 Rust 中不合法——`Arc<dyn T>` 要求 `T: ?Sized`，且对每个具体 trait 需要不同的 monomorphization。**决定**：改用 `macro_rules! singleton_hook!` 和 `macro_rules! none_hooks!`。

4. **`Provider` 类型名不匹配**：规范使用 `Arc<dyn Provider>`，实际代码库中 `LlmClient` 是 `llm_adapter::provider::Provider` 的 re-export（`llm-harness-loop/src/lib.rs:31`），`AgentHarness` 的字段也写 `Arc<dyn LlmClient>`。**决定**：Builder 使用 `Arc<dyn LlmClient>` 命名（与现有代码一致）。

5. **`HarnessBuilder::build()` 缺少 client 创建步骤**：`AgentHarness::new_in_memory()` 和 `with_session()` 都接收 `client: Arc<dyn LlmClient>`。Builder 存的是 providers（pattern → client），必须在 `build()` 中先 `resolve_provider()` 拿到 client 再传给 `AgentHarness`。**决定**：`build()` 的签名改为 `build(self, env, session_or_none)`，内部调用 `resolve_provider()` 获取 client。

6. **`AgentHarnessOptions` 与 `HarnessBuilder` 字段重叠**：Builder 的 model / tools / hooks / skills / templates 字段在 `AgentHarnessOptions` 中全部存在。**决定**：保留 `AgentHarnessOptions` 作为内部类型，Builder 的 `build()` 内部构造 `AgentHarnessOptions` 然后调用现有的 `AgentHarness::with_session()` / `new_in_memory()`。不破坏现有公开 API。

7. **`LoopConfig` 的 hook 字段仍是 `Option<Arc<dyn XHook>>`**：`LoopConfig`（`llm-harness-loop/src/config.rs:84`）的 `transform_context`、`prepare_next_turn`、`should_stop` 等字段仍是 `Option<Arc<dyn Hook>>`。在 `build_loop_config()` 中需要将 `Vec<Arc<dyn Hook>>` wrap 成 Composite（实现对应 hook trait），然后传给 `LoopConfig`。LoopConfig 自身不需要改为 Vec——它只接收一个 Composite。

---

## 背景与目标

llm-harness-runtime 当前的 hook 和工具注册机制已经有良好的基础（12 个 hook trait、composite 组合器、ToolRegistry），但缺少让外部开发者以**声明式、可叠加**的方式组合这些能力的编排层。

场景是：

- 公司 A 写了一个 `InternalAuditPlugin`（注入审计 hook + 成本监控工具）
- 公司 B 扩展了 A 的插件，加了 `CompliancePlugin`（加了一个 before_tool_call hook）
- 最终用户用 `HarnessBuilder` 把两个插件叠在一起

所有插件都是 **Rust crate 级别的编译期依赖**，不是动态加载的脚本。

```rust
// 插件作者
impl Plugin for CompanyToolsPlugin {
    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) { ... }
    fn register_hooks(&self, hooks: &mut HarnessHooks) { ... }
}

// 框架使用者
let agent = HarnessBuilder::new("claude-sonnet-4-6")
    .install(CompanyToolsPlugin::new(config))
    .install(CompliancePlugin::new(config))
    .build(env);
```

---

## 一、Hook 多播化（P0）

### 问题

`HarnessHooks` 所有字段（含 `auth`）目前是 `Option<Arc<dyn XHook>>`，每个 hook 点只能挂一个实现，多个插件无法叠加同一个 hook。

### 方案

将所有字段改为 `Vec<Arc<dyn XHook>>`，`build_loop_config()` 自动 wrap 成对应的 Composite。

```rust
// llm-harness-agent: src/harness.rs 中的 HarnessHooks
pub struct HarnessHooks {
    pub before_run:               Vec<Arc<dyn BeforeRunHook>>,
    pub before_turn:              Vec<Arc<dyn BeforeTurnHook>>,
    pub after_turn:               Vec<Arc<dyn AfterTurnHook>>,
    pub before_tool_call:         Vec<Arc<dyn BeforeToolCallHook>>,
    pub after_tool_call:          Vec<Arc<dyn AfterToolCallHook>>,
    pub transform_context:        Vec<Arc<dyn TransformContextHook>>,
    pub prepare_next_turn:        Vec<Arc<dyn PrepareNextTurnHook>>,
    pub should_stop:              Vec<Arc<dyn ShouldStopHook>>,
    pub before_provider_request:  Vec<Arc<dyn BeforeProviderRequestHook>>,
    pub after_provider_response:  Vec<Arc<dyn AfterProviderResponseHook>>,
    pub before_compact:           Vec<Arc<dyn BeforeCompactHook>>,
    pub auth:                     Vec<Arc<dyn AuthHook>>,
}
```

迁移后从 `AgentHarness` 和 `AgentHarnessOptions` 中移除独立的 `auth: Option<Arc<dyn AuthHook>>` 字段，统一走 `hooks.auth`。

### 兼容性

这是破坏性变更。现有调用方从：

```rust
HarnessHooks {
    before_tool_call: Some(Arc::new(my_hook)),
    ..HarnessHooks::none()
}
```

改为：

```rust
HarnessHooks {
    before_tool_call: vec![Arc::new(my_hook)],
    ..HarnessHooks::none()
}
```

提供两个宏降低迁移成本（泛型函数 `fn singleton<T>(hook: Arc<dyn T>)` 在 Rust 中无法编译，因为 `Arc<dyn T>` 的 monomorphization 对每个具体 trait 不同）：

```rust
/// 快速构造仅含一个 hook 的 vec（替换旧 `Some(x)` 模式）。
/// 用法：singleton_hook!(my_hook, BeforeToolCallHook) → Vec<Arc<dyn BeforeToolCallHook>>
macro_rules! singleton_hook {
    ($hook:expr, $trait:ty) => {{
        let arc: std::sync::Arc<dyn $trait> = std::sync::Arc::new($hook);
        vec![arc]
    }};
}

/// 构造空 vec（替换旧 None 模式）。
/// 用法：none_hooks!(BeforeToolCallHook) → Vec<Arc<dyn BeforeToolCallHook>>
macro_rules! none_hooks {
    ($trait:ty) => {
        vec![] as Vec<std::sync::Arc<dyn $trait>>
    };
}
```

### 各 Hook 的组合语义

| Hook | 语义 | 说明 |
|------|------|------|
| `BeforeRunHook` | all-execute | 纯观察，无返回值影响 |
| `BeforeTurnHook` | all-execute | 纯观察 |
| `AfterTurnHook` | all-execute | 纯观察 |
| `BeforeProviderRequestHook` | all-execute | 纯观察 |
| `AfterProviderResponseHook` | all-execute | 纯观察 |
| `BeforeToolCallHook` | first-non-allow 短路 | 第一个 `Deny` / `Modify` 胜出 |
| `AfterToolCallHook` | **链式** | 每个 hook 看到前一个 hook 修改后的结果。安装顺序决定了变换流水线。 |
| `TransformContextHook` | 链式 | 每个 hook 拿到前一个变换后的 ctx。安装顺序决定了上下文变换顺序。 |
| `PrepareNextTurnHook` | 链式（字段合并） | `NextTurnDirective` 各字段独立合并：每个字段取最后一个非 `None` 的值。靠后的插件可以覆盖靠前的同名字段。 |
| `ShouldStopHook` | any-true 短路 | 任一返回 `true` 则停止 |
| `BeforeCompactHook` | first-non-proceed 短路 | 第一个 `Skip` / `Override` 胜出 |
| `AuthHook` | last-wins | 最后注册的 auth 生效（通常只有一个）。冲突时发出 `tracing::warn!`。 |

**⚠️ 对插件作者重要：** `AfterToolCallHook`、`TransformContextHook`、`PrepareNextTurnHook` 三种 hook 的行为**依赖安装顺序**。先安装的插件看到的是原始输入，后安装的插件看到的是前一个修改后的输出。如果两个插件都修改同一个字段，后安装的生效。

`composite.rs` 需补齐当前缺少的 7 个实现：`CompositeBeforeRunHook`、`CompositeBeforeTurnHook`、`CompositeTransformContextHook`、`CompositePrepareNextTurnHook`、`CompositeShouldStopHook`、`CompositeBeforeCompactHook`、`CompositeAuthHook`。

---

## 二、Plugin trait（P0）

### 位置

放在 **`llm-harness-agent`** 而非 `llm-harness-types`。

原因：`Plugin::register_skills` 和 `Plugin::register_templates` 引用了 `llm-harness-agent` 中的 `Skill` 和 `PromptTemplate` 类型，这两个类型不放到底层 types crate（skill 依赖了 agent 的会话模型，template 依赖了 harness 的生命周期）。

如果未来的插件真的只依赖工具和 hook（不需要 skills/templates），可以再加一个 `PluginLite` trait 放在 types crate。

### 定义

```rust
// llm-harness-agent: src/plugin.rs（新文件）
pub trait Plugin: Send + Sync {
    /// 插件名称，用于诊断和冲突检测。
    fn name(&self) -> &str;

    /// 注册工具。
    fn register_tools(&self, _tools: &mut Vec<Arc<dyn Tool>>) {}
    /// 注册 hook。
    fn register_hooks(&self, _hooks: &mut HarnessHooks) {}
    /// 注册 skills。
    fn register_skills(&self, _skills: &mut Vec<Skill>) {}
    /// 注册 prompt 模板。
    fn register_templates(&self, _templates: &mut Vec<PromptTemplate>) {}

    // P2：有状态插件可覆盖此方法以接入 StateProvider 机制
    fn state_provider(&self) -> Option<Arc<dyn StateProvider>> { None }
}

impl HarnessBuilder {
    /// 安装一个插件。
    ///
    /// ## 冲突策略
    /// - **工具 / 技能 / 模板**：同名以后安装的插件为准（后注册覆盖先注册），
    ///   冲突时发出 `tracing::warn!`。
    /// - **Hook**：按注册顺序累加（不冲突，前端插件追加到末尾）。
    /// - **AuthHook**：以后安装的为准，冲突时发出 `tracing::warn!`。
    /// - **StateProvider**：同 `entry_type` 以后安装的为准。
    ///
    /// ## 执行顺序
    /// 安装顺序决定了 hook 链的执行顺序。对于链式 hook（AfterToolCallHook 等），
    /// 先安装的插件先执行，后安装的插件看到的是前一个修改后的结果。
    pub fn install(mut self, plugin: &dyn Plugin) -> Self { ... }
}
```

所有方法均有默认空实现，插件只需覆盖用到的方法。

### 初始化

插件不在 builder 阶段获得独立生命周期回调。需要初始化的插件在结构体构造时做：

```rust
pub struct MyPlugin {
    config: MyConfig,
    client: Arc<MyService>,
}

impl MyPlugin {
    pub fn new(config_path: &str) -> Result<Self, ConfigError> {
        let config = load_config(config_path)?;
        let client = MyService::connect(&config)?;
        Ok(Self { config, client: Arc::new(client) })
    }
}
```

`ConfigError` 是应用层的错误类型，在 `install()` 之前就处理。builder 不介入插件初始化。

### 冲突诊断

`HarnessBuilder::install` 在注册过程中对所有资源做冲突检测并日志：

```rust
pub fn install(mut self, plugin: &dyn Plugin) -> Self {
    let name = plugin.name();

    // 注册工具：检测同名冲突
    let tool_count_before = self.tools.len();
    plugin.register_tools(&mut self.tools);
    if self.tools.len() > tool_count_before {
        let new_tools = &self.tools[tool_count_before..];
        for new_tool in new_tools {
            if let Some(existing) = self.tools[..tool_count_before]
                .iter().find(|t| t.name() == new_tool.name())
            {
                tracing::warn!(
                    plugin = %name,
                    tool = %new_tool.name(),
                    "插件 '{name}' 的工具 '{}' 覆盖了之前注册的同名工具",
                    new_tool.name(),
                );
            }
        }
    }

    // 注册 hook：auth hook 冲突警告
    let auth_count_before = self.hooks.auth.len();
    plugin.register_hooks(&mut self.hooks);
    if self.hooks.auth.len() > auth_count_before {
        tracing::warn!(
            plugin = %name,
            "插件 '{name}' 注册了 AuthHook，将覆盖之前的认证配置",
        );
    }

    // 注册 skills：检测同名冲突
    let skills_before = self.skills.len();
    plugin.register_skills(&mut self.skills);
    if self.skills.len() > skills_before {
        for skill in &self.skills[skills_before..] {
            if self.skills[..skills_before].iter().any(|s| s.name() == skill.name()) {
                tracing::warn!(
                    plugin = %name,
                    skill = %skill.name(),
                    "插件 '{name}' 的 skill '{}' 覆盖了之前注册的同名 skill",
                    skill.name(),
                );
            }
        }
    }

    // 注册模板：检测同名冲突
    let templates_before = self.templates.len();
    plugin.register_templates(&mut self.templates);
    if self.templates.len() > templates_before {
        for tmpl in &self.templates[templates_before..] {
            if self.templates[..templates_before].iter().any(|t| t.name == tmpl.name) {
                tracing::warn!(
                    plugin = %name,
                    template = %tmpl.name,
                    "插件 '{name}' 的模板 '{}' 覆盖了之前注册的同名模板",
                    tmpl.name,
                );
            }
        }
    }

    self
}
```

**冲突检测总成本：约 40 行代码，没有新的依赖和工作量。**

---

## 三、HarnessBuilder（P0）

### 位置

放在 `llm-harness-runtime`，负责将 plugin 注册结果组装为 `AgentHarness`。

### 设计

Provider 匹配使用 `Vec<ProviderMatcher>` 直接做线性查找（provider 注册量通常 ≤ 5，HashMap 带来的复杂度不值得）：

```rust
// llm-harness-runtime: src/builder.rs（新文件）
pub struct HarnessBuilder {
    model: String,
    tools: Vec<Arc<dyn Tool>>,
    hooks: HarnessHooks,
    skills: Vec<Skill>,
    templates: Vec<PromptTemplate>,
    providers: Vec<ProviderMatcher>,
}

struct ProviderMatcher {
    /// 匹配模式："deepseek-chat"（精确）或 "claude-*"（glob）。
    pattern: String,
    /// 是否是 glob 模式（末尾 `*`）。
    is_glob: bool,
    /// 去除末尾 `*` 后的前缀（glob 模式下有效）。
    prefix: String,
    client: Arc<dyn LlmClient>,
}

impl ProviderMatcher {
    fn new(pattern: impl Into<String>, provider: Arc<dyn Provider>) -> Self {
        let pattern = pattern.into();
        let is_glob = pattern.ends_with('*');
        let prefix = if is_glob { pattern[..pattern.len()-1].to_string() } else { String::new() };
        Self { pattern, is_glob, prefix, provider }
    }

    fn matches(&self, model: &str) -> bool {
        if self.is_glob {
            model.starts_with(&self.prefix)
        } else {
            model == self.pattern
        }
    }
}

impl HarnessBuilder {
    pub fn new(model: impl Into<String>) -> Self { ... }

    /// 安装一个插件（参见 Plugin trait 上的冲突策略文档）。
    pub fn install(mut self, plugin: &dyn Plugin) -> Self { ... }

    /// 注册 provider，按 model name 匹配。
    /// pattern 支持两种格式：
    ///   - `"deepseek-chat"` → 精确匹配
    ///   - `"claude-*"` → glob 通配（仅末尾 `*`）
    pub fn provider(mut self, pattern: &str, provider: Arc<dyn Provider>) -> Self {
        self.providers.push(ProviderMatcher::new(pattern, provider));
        self
    }

    /// 构建 AgentHarness，自动将 Vec<hook> wrap 成 Composite。
    pub fn build(self, env: Arc<dyn ExecutionEnv>) -> Result<AgentHarness, HarnessBuildError> { ... }

    /// 根据 model name 查找匹配的 provider。
    /// 精确匹配优先于通配匹配；未匹配返回 HarnessBuildError::NoMatchingProvider。
    fn resolve_provider(&self, model: &str) -> Result<Arc<dyn Provider>, HarnessBuildError> {
        // 先尝试精确匹配
        for m in &self.providers {
            if !m.is_glob && m.matches(model) {
                return Ok(m.provider.clone());
            }
        }
        // 再尝试通配匹配（优先匹配更长的前缀）
        let mut candidates: Vec<&ProviderMatcher> = self.providers.iter()
            .filter(|m| m.is_glob && m.matches(model))
            .collect();
        candidates.sort_by_key(|m| std::cmp::Reverse(m.prefix.len()));
        if let Some(best) = candidates.into_iter().next() {
            return Ok(best.provider.clone());
        }
        Err(HarnessBuildError::NoMatchingProvider { model: model.to_string() })
    }
}
```

为什么用 `Vec<ProviderMatcher>` 而非 `HashMap<String, Vec<ProviderMatcher>>`：原先设计的 `HashMap<String, Vec<ProviderMatcher>>` 中 key 是 pattern 字符串，matcher 里又存了一份，数据冗余。线性查找在 provider 数量 ≤ 5 时完全够用。

未匹配的 model 直接报错，不设隐式 fallback。

### `HarnessBuildError`

```rust
#[derive(Debug, thiserror::Error)]
pub enum HarnessBuildError {
    #[error("没有匹配 model '{model}' 的 provider")]
    NoMatchingProvider { model: String },

    #[error("未注册任何 provider；请调用 .provider() 方法")]
    NoProvider,
}
```

---

## 四、StateProvider（P2）

允许有状态的插件在 session 分支切换时自动重建状态。

```rust
// llm-harness-agent: src/plugin.rs
pub trait StateProvider: Send + Sync {
    /// 对应 append_custom_entry 的 custom_type 标记。
    fn entry_type(&self) -> &str;

    /// 从分支历史 entries 重建当前状态。
    fn rebuild(&self, entries: &[SessionEntry]);
}
```

`AgentHarness` 在 `session_start` / `session_tree` 生命周期点调用所有已注册 `StateProvider` 的 `rebuild()`。`Plugin` trait 可扩展一个可选方法 `fn state_provider(&self) -> Option<Arc<dyn StateProvider>> { None }`。

例：一个"笔记插件"通过 `SessionEntry::Custom` 保存笔记内容，branch 切换时读取历史 custom entries 恢复当前笔记。

---

## 五、测试策略（P0 考虑，P1 实现）

插件作者需要一个轻量测试 harness，能在不启动完整 runtime 的情况下验证注册输出：

```rust
// 期望：在 test-utils 或 llm-harness-agent 的测试模块中
let mut builder = TestHarnessBuilder::new("test-model");
builder.install(&MyPlugin::default());

let tools = builder.tools();
assert_eq!(tools.len(), 2);
assert!(tools.iter().any(|t| t.name() == "my_tool"));

let hooks = builder.hooks();
assert_eq!(hooks.before_tool_call.len(), 1);
```

`TestHarnessBuilder` 是 `HarnessBuilder` 的非消费式变体（`&self` 而非 `self`），只收集注册结果，不调用 `build()`。P0 在 builder 上加一组 `pub fn inspect_*()` 方法，让测试能直接读注册状态。

---

## 六、未纳入范围（v0.3+）

| 功能 | 原因 |
|------|------|
| 运行时动态加载（WASM / dlopen） | 底层库不需要；用户通过 Cargo 管理依赖 |
| 插件间 EventBus | hook 系统 + `Arc<Mutex<T>>` 共享状态已覆盖 |
| 插件版本兼容性检查 | P1/P2，需定义扩展的 API 版本标记 |
| 插件依赖解析（A 必须在 B 之前安装） | 目前安装顺序就是执行顺序，未来可加 `fn before(&self) -> Vec<String>` |
| `PluginLite` trait（放在 types crate） | 插件只注册工具/hook 时的轻量选项，P1 |
| 插件独立生命周期回调（init/shutdown） | 初始化在构造时完成，shutdown 在 `Drop` 处理 |

---

## 七、预期工作量

| 任务 | 估算 |
|------|------|
| `HarnessHooks` 字段 `Option` → `Vec` | ~30 行 + 修改所有调用方 ~200 行 |
| 补齐 7 个 Composite 实现 | ~200 行 |
| `AfterToolCallHook` 改为链式 Composite | ~20 行（修改现有实现） |
| `Plugin` trait（llm-harness-agent）+ 冲突检测日志 | ~80 行 |
| `HarnessBuilder`（llm-harness-runtime）+ ProviderMatcher | ~240 行 |
| `StateProvider` trait + harness 接入 | ~100 行 |
| 测试辅助方法/TestHarnessBuilder | ~80 行 |
| **合计** | **~950 行** |

P0 可做项：冲突检测日志（~40 行）是最高性价比的投入。

---

## 八、文件变更索引

```
修改  crates/llm-harness-agent/src/harness.rs
        HarnessHooks 字段 Option → Vec，移除 AgentHarness 独立 auth 字段，
        对 AgentHarnessOptions.auth 标记 deprecated，build_loop_config() 
        中新增 Vec→Composite wrapping 逻辑

修改  crates/llm-harness-agent/src/lib.rs
        导出 Plugin trait、StateProvider trait

修改  crates/llm-harness-agent/src/harness.rs 及相关模块
        适配 Vec<dyn XHook> + build_loop_config wrap 逻辑

修改  crates/llm-harness-runtime/src/composite.rs
        补齐 7 个 Composite + 修改 AfterToolCallHook 为链式

新增  crates/llm-harness-runtime/src/builder.rs
        HarnessBuilder + ProviderMatcher + HarnessBuildError

修改  所有构造 HarnessHooks 的调用方
        Option → Vec 迁移
```
