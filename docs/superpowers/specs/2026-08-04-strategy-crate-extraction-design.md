# 策略 crate 提取设计（阶段一）

## 背景

基于《AI Agents in Depth》分层梳理的架构决策：策略层代码放在独立 crate `llm-harness-strategy`（同仓库，不同 crate），只依赖机制层 public API。编译器强制边界——策略代码只能用 `Tool`/`BeforeToolCallHook`/`HarnessBuilder` 等公开类型。与现有 `llm-harness-runtime-tools`/`llm-harness-runtime-mcp` 模式一致（独立可选 crate）。

trait 定义留机制层（types），默认实现放策略层。边界原则：如果实现需要改 loop_fn/session/dispatch 内部逻辑 → 机制层；如果实现只需注册 hook 或实现 trait → 策略层。

**例外原则**：不机械执行机制/策略分离。runtime 自身必然集成的策略留机制层，用户可选注入的策略才迁移到 strategy crate。

## 目标

将 3 项用户可选注入的策略代码从机制层 crate 迁移到 `llm-harness-strategy` crate。

## 迁移项

### 1. `BudgetControlAdapter`

**当前位置**：`crates/llm-harness-runtime/src/control/budget.rs`

**迁移内容**：
- `BudgetControlAdapter`（`pub struct`，impl `ShouldStopHook`）——读共享 `Arc<Mutex<CostAggregate>>`，超预算时咨询 `BudgetExceededHook`
- 全部单元测试（budget.rs:106-212）

**`BudgetExceededHook` trait 移到 types**：同 `PricingProvider` 模式——trait 定义留机制层，策略 crate 和 runtime 都从 types 引用。

**依赖**：
- `CostAggregate` / `ShouldStopCtx` / `ShouldStopHook` / `BudgetExceededHook` — 全在 `llm-harness-types`，strategy crate 直接引用
- 无 runtime 内部类型依赖

**不迁移**：`CostAccumulatorHook`（`pub(crate)`，builder 必然注入）、`UsageLedger`（共享状态容器）

### 2. `ResourceInjector` + `ResourceProvider` trait

**当前位置**：`crates/llm-harness-runtime/src/platform/resource.rs`

**迁移内容**：
- `ResourceInjector`（`pub struct`）——往 `AgentContext.system_prompt` 追加资源内容
- `ResourceInjector::inject_all` / `inject` / `inject_one` 方法
- 全部单元测试（resource.rs:108-177）

**不迁移但移动**：`ResourceProvider` trait + `ResourceDescriptor` + `ResourceContent` 从 runtime 移到 `llm-harness-types`。理由：
- `ResourceProvider` 是外部资源提供者抽象（同 `PricingProvider` 已在 types）
- `runtime-mcp` crate 的 `McpResourceProvider` 实现此 trait，从 types 引用方向正确
- strategy crate 的 `ResourceInjector` 依赖此 trait，从 types 引用方向正确
- runtime 的 `platform/resource.rs` 保留 re-export（`pub use llm_harness_types::{ResourceProvider, ResourceDescriptor, ResourceContent};`）避免外部调用者 breaking

### 3. `format_skills_for_system_prompt` + 配套 escape 函数

**当前位置**：`crates/llm-harness-agent/src/skills.rs`

**迁移内容**：
- `format_skills_for_system_prompt(skills: &[Skill], has_skill_read_tool: bool) -> String`（`pub fn`）
- `escape_skill_markup(s: &str) -> String`（`fn`，被 `format_skills_for_system_prompt` 使用）
- `escape_xml_attr(s: &str) -> String`（`fn`，同上）
- 相关单元测试

**依赖**：
- `Skill` — 在 `llm-harness-types`，strategy crate 直接引用
- 无 agent/runtime 内部类型依赖

**agent crate 改造**：
- `lib.rs` 的 re-export `pub use ... format_skills_for_system_prompt ...` 改为从 strategy crate re-export，或直接删除（当前无生产调用）
- `skills.rs` 中删除迁移的函数和 escape helper

## crate 结构

### Cargo.toml

```toml
[package]
name = "llm-harness-strategy"
version = "0.1.0"
edition = "2021"

[dependencies]
llm-harness-types.workspace = true
futures.workspace = true
anyhow.workspace = true
serde_json.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

### 依赖图

```
types ← strategy
types ← loop ← agent ← runtime
```

strategy crate 只依赖 `llm-harness-types`，与 `runtime-tools`/`runtime-mcp` 同级。runtime 不依赖 strategy——builder 通过可选 feature 或用户手动注入使用 strategy crate 的类型。

### 模块结构

```
crates/llm-harness-strategy/
├── Cargo.toml
└── src/
    ├── lib.rs              # pub mod budget; pub mod resource; pub mod skills;
    ├── budget.rs           # BudgetControlAdapter
    ├── resource.rs         # ResourceInjector
    └── skills.rs           # format_skills_for_system_prompt + escape helpers
```

## workspace 注册

根 `Cargo.toml`：
- `[workspace] members` 数组添加 `"crates/llm-harness-strategy"`
- `[workspace.dependencies]` 添加 `llm-harness-strategy = { path = "crates/llm-harness-strategy" }`

## builder 改造

采用纯 hook 注入方案：builder 不内置 budget 逻辑。

- `BudgetExceededHook` trait 移到 `llm-harness-types`
- `BudgetControlAdapter` 迁移到 strategy crate
- builder 删除 `budget()` 方法和 `budget` 字段
- builder 删除 `import crate::control::budget::{BudgetControlAdapter, BudgetExceededHook}`
- `build_options()` 中删除 budget 注入逻辑（631-638 行）
- 用户直接从 strategy crate 构造 `BudgetControlAdapter`，通过 `builder.hooks()` 注入

```rust
// 用户侧
use llm_harness_strategy::budget::BudgetControlAdapter;

let budget_hook = BudgetControlAdapter::new(cost.clone(), 5.0, None);
let harness = HarnessBuilder::new("model")
    .hooks(HarnessHooks {
        should_stop: vec![Arc::new(budget_hook)],
        ..HarnessHooks::none()
    })
    .build(env).await?;
```

runtime 不依赖 strategy crate，依赖方向正确。strategy 是可选依赖，用户不引入则无 budget 功能。与 `runtime-tools`/`runtime-mcp` 的可选依赖模式一致。

## runtime 内部清理

### control/budget.rs
迁移后此文件只保留 `BudgetExceededHook` 的 re-export（`pub use llm_harness_types::BudgetExceededHook;`）或完全删除（如果 runtime 内部无其他引用）。`control/mod.rs` 中 `pub mod budget;` 保留或删除取决于是否有外部引用。

### control/mod.rs
`pub mod budget;` — 检查是否有外部 crate 通过 `llm_harness_runtime::control::budget` 路径引用。`cost_hook.rs` 测试中 `use crate::control::budget::{BudgetControlAdapter, BudgetExceededHook};` 需改为从 strategy crate / types 引用。

### platform/resource.rs
迁移 `ResourceInjector` 后，保留 `ResourceProvider`/`ResourceDescriptor`/`ResourceContent` 的 re-export（从 types）。`platform/mod.rs` 不变。

### rules/ 模块
不迁移。`workflow/gate.rs` 对 rules 的依赖不变。

## 测试策略

- 迁移的单元测试随代码一起迁移到 strategy crate 的对应模块
- `runtime/tests/budget_tests.rs` 迁移到 `strategy/tests/budget_tests.rs` 或留在 runtime 作为集成测试（改为从 strategy crate import 类型）
- `runtime/src/control/cost_hook.rs` 中的 `BudgetControlAdapter` 测试引用改为从 strategy crate import
- `cargo test -p llm-harness-strategy` 验证迁移后的测试通过
- `cargo test -p llm-harness-runtime` 验证 runtime 测试无回归（budget 集成测试需调整 import）
