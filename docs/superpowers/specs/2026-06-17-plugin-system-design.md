# Oh-My-Harness 插件系统设计

> 日期：2026-06-17
> 范围：`llm-harness-types` / `llm-harness-runtime`
> 目标：将 oh-my-harness 打造为可供外部开发者编写插件的底层框架

---

## 背景与目标

oh-my-harness 当前的 hook 和工具注册机制已经有良好的基础（13 个 hook trait、composite 组合器、ToolRegistry），但缺少让外部开发者以**声明式、可叠加**的方式组合这些能力的编排层。

目标是补齐这层编排，使外部开发者可以：

```rust
// 插件作者（只依赖 llm-harness-types）
impl Plugin for CompanyToolsPlugin {
    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) { ... }
    fn register_hooks(&self, hooks: &mut HarnessHooks) { ... }
}

// 框架使用者（依赖 llm-harness-runtime）
let agent = HarnessBuilder::new("claude-sonnet-4-6")
    .install(CompanyToolsPlugin::new(config))
    .provider("anthropic", Arc::new(AnthropicProvider::new(key)))
    .build(env);
```

---

## 一、Hook 多播化（P0）

### 问题

`HarnessHooks` 所有字段（含 `auth`）目前是 `Option<Arc<dyn XHook>>`，每个 hook 点只能挂一个实现，多个插件无法叠加同一个 hook。

### 方案

将所有字段改为 `Vec<Arc<dyn XHook>>`，`build_loop_config()` 自动 wrap 成对应的 Composite。

```rust
// llm-harness-types: hooks.rs 中的 HarnessHooks
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

### 各 Hook 的组合语义

| Hook | 语义 | 说明 |
|------|------|------|
| `BeforeRunHook` | all-execute | 纯观察，无返回值影响 |
| `BeforeTurnHook` | all-execute | 纯观察 |
| `AfterTurnHook` | all-execute | 纯观察 |
| `BeforeProviderRequestHook` | all-execute | 纯观察 |
| `AfterProviderResponseHook` | all-execute | 纯观察 |
| `BeforeToolCallHook` | first-non-allow 短路 | 第一个 Deny/Modify 胜出 |
| `AfterToolCallHook` | **链式** | 每个 hook 看到前一个 hook 修改后的结果 |
| `TransformContextHook` | 链式 | 每个 hook 拿到前一个变换后的 ctx |
| `PrepareNextTurnHook` | 链式（字段合并） | `NextTurnDirective` 各字段独立合并：每个字段取最后一个非 `None` 的值 |
| `ShouldStopHook` | any-true 短路 | 任一返回 true 则停止 |
| `BeforeCompactHook` | first-non-proceed 短路 | 第一个 Skip/Override 胜出 |
| `AuthHook` | last-wins | 最后注册的 auth 生效（通常只有一个） |

`composite.rs` 需补齐当前缺少的 7 个实现：`CompositeBeforeRunHook`、`CompositeBeforeTurnHook`、`CompositeTransformContextHook`、`CompositePrepareNextTurnHook`、`CompositeShouldStopHook`、`CompositeBeforeCompactHook`、`CompositeAuthHook`。

---

## 二、Plugin trait（P0）

### 位置

放在 `llm-harness-types`，使插件作者只需依赖类型 crate，无需引入 runtime 的全部实现依赖。

### 定义

```rust
// llm-harness-types: plugin.rs（新文件）
pub trait Plugin: Send + Sync {
    /// 插件名称，用于诊断和冲突检测。
    fn name(&self) -> &str;

    fn register_tools(&self, _tools: &mut Vec<Arc<dyn Tool>>) {}
    fn register_hooks(&self, _hooks: &mut HarnessHooks) {}
    fn register_skills(&self, _skills: &mut Vec<Skill>) {}
    fn register_templates(&self, _templates: &mut Vec<PromptTemplate>) {}

    // P2：有状态插件可覆盖此方法以接入 StateProvider 机制
    fn state_provider(&self) -> Option<Arc<dyn StateProvider>> { None }
}
```

所有方法均有默认空实现，插件只需覆盖用到的方法。

---

## 三、HarnessBuilder（P0）

### 位置

放在 `llm-harness-runtime`，负责将 plugin 注册结果组装为 `AgentHarness`。

### 定义

```rust
// llm-harness-runtime: builder.rs（新文件）
pub struct HarnessBuilder {
    model: String,
    tools: Vec<Arc<dyn Tool>>,
    hooks: HarnessHooks,
    skills: Vec<Skill>,
    templates: Vec<PromptTemplate>,
    providers: HashMap<String, Arc<dyn Provider>>,
}

impl HarnessBuilder {
    pub fn new(model: impl Into<String>) -> Self { ... }

    /// 安装一个插件（增量注册工具、hook、技能、模板）。
    pub fn install(mut self, plugin: &dyn Plugin) -> Self {
        plugin.register_tools(&mut self.tools);
        plugin.register_hooks(&mut self.hooks);
        plugin.register_skills(&mut self.skills);
        plugin.register_templates(&mut self.templates);
        self
    }

    /// 注册一个 provider，按 model name 前缀路由。
    pub fn provider(mut self, name: &str, provider: Arc<dyn Provider>) -> Self {
        self.providers.insert(name.to_string(), provider);
        self
    }

    /// 构建 AgentHarness，自动将 Vec<hook> wrap 成 Composite。
    pub fn build(self, env: Arc<dyn ExecutionEnv>) -> AgentHarness { ... }
}
```

Provider 路由逻辑：`providers` 的 key 是 model name 前缀（如 `"claude"` 匹配所有以 `claude` 开头的 model ID）。`build()` 时按最长前缀优先匹配；未匹配到的 model 使用 fallback provider（若有）或报错。

---

## 四、StateProvider（P2）

允许有状态的插件在 session 分支切换时自动重建状态。

```rust
// llm-harness-types: plugin.rs
pub trait StateProvider: Send + Sync {
    /// 对应 append_custom_entry 的 custom_type 标记。
    fn entry_type(&self) -> &str;

    /// 从分支历史 entries 重建当前状态。
    fn rebuild(&self, entries: &[SessionEntry]);
}
```

`AgentHarness` 在 `session_start` / `session_tree` 生命周期点调用所有已注册 `StateProvider` 的 `rebuild()`。`Plugin` trait 可扩展一个可选方法 `fn state_provider(&self) -> Option<Arc<dyn StateProvider>> { None }`。

---

## 五、删除项

- **EventBus**：hook 系统 + `Arc<Mutex<T>>` 共享状态可覆盖所有已知需求，不引入字符串 channel 的动态分发开销。

---

## 六、预期工作量

| 任务 | 估算 |
|------|------|
| `HarnessHooks` 字段 `Option` → `Vec` | ~30 行 |
| 补齐 7 个 Composite 实现 | ~200 行 |
| `AfterToolCallHook` 改为链式 Composite | ~20 行（修改现有实现） |
| `Plugin` trait（llm-harness-types） | ~40 行 |
| `HarnessBuilder`（llm-harness-runtime） | ~150 行 |
| `StateProvider` trait + harness 接入 | ~100 行 |
| **合计** | **~540 行** |

---

## 七、文件变更索引

```
修改  llm-harness-core/crates/llm-harness-types/src/hooks.rs
        HarnessHooks 字段 Option → Vec

新增  llm-harness-core/crates/llm-harness-types/src/plugin.rs
        Plugin trait, StateProvider trait

修改  llm-harness-runtime/crates/llm-harness-runtime/src/composite.rs
        补齐 7 个 Composite + 修改 AfterToolCallHook 为链式

新增  llm-harness-runtime/crates/llm-harness-runtime/src/builder.rs
        HarnessBuilder
```
