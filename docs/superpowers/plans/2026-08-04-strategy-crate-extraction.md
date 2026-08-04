# 策略 Crate 提取实现计划（阶段一）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 3 项用户可选注入的策略代码从机制层 crate 迁移到独立 `llm-harness-strategy` crate。

**Architecture:** 新建 `llm-harness-strategy` crate，只依赖 `llm-harness-types`。`BudgetExceededHook` trait 和 `ResourceProvider` trait 移到 types crate。`HarnessBuilder` 删除 `budget()` 方法，用户通过 `builder.hooks()` 直接注入 strategy crate 构造的 hook。

**Tech Stack:** Rust, workspace crates, tokio, futures

## Global Constraints

- strategy crate 只依赖 `llm-harness-types`，不依赖 runtime/agent/loop
- runtime 不依赖 strategy crate（依赖方向：types ← strategy，types ← ... ← runtime）
- `BudgetExceededHook` trait 移到 `llm-harness-types/src/cost.rs`
- `ResourceProvider`/`ResourceDescriptor`/`ResourceContent` 移到 `llm-harness-types/src/resources.rs`
- 迁移的单元测试随代码迁移到 strategy crate
- 每个 task 结束时 `cargo test` 验证无回归

---

## File Structure

| 文件 | 操作 | 职责 |
|------|------|------|
| `crates/llm-harness-strategy/Cargo.toml` | 创建 | crate 清单 |
| `crates/llm-harness-strategy/src/lib.rs` | 创建 | 模块声明 + re-export |
| `crates/llm-harness-strategy/src/budget.rs` | 创建 | `BudgetControlAdapter` |
| `crates/llm-harness-strategy/src/resource.rs` | 创建 | `ResourceInjector` |
| `crates/llm-harness-strategy/src/skills.rs` | 创建 | `format_skills_for_system_prompt` + escape helpers |
| `crates/llm-harness-types/src/cost.rs` | 修改 | 添加 `BudgetExceededHook` trait |
| `crates/llm-harness-types/src/resources.rs` | 修改 | 添加 `ResourceProvider`/`ResourceDescriptor`/`ResourceContent` |
| `crates/llm-harness-runtime/src/control/budget.rs` | 修改 | 删除迁移代码，re-export `BudgetExceededHook` |
| `crates/llm-harness-runtime/src/platform/resource.rs` | 修改 | 删除 `ResourceInjector`，re-export resource traits |
| `crates/llm-harness-runtime/src/builder.rs` | 修改 | 删除 `budget()` 方法、字段、import |
| `crates/llm-harness-agent/src/skills.rs` | 修改 | 删除迁移函数 |
| `crates/llm-harness-agent/src/lib.rs` | 修改 | 删除 `format_skills_for_system_prompt` re-export |
| `crates/llm-harness-runtime/src/control/cost_hook.rs` | 修改 | 测试 import 改为从 types 引用 |
| `crates/llm-harness-runtime/tests/budget_tests.rs` | 修改 | import 改为从 strategy + types 引用 |
| `Cargo.toml` (根) | 修改 | workspace members + dependencies |

---

### Task 1: 创建 strategy crate 骨架并注册到 workspace

**Files:**
- Create: `crates/llm-harness-strategy/Cargo.toml`
- Create: `crates/llm-harness-strategy/src/lib.rs`
- Modify: `Cargo.toml` (根)

**Interfaces:**
- Produces: `llm_harness_strategy` crate（空模块，可编译）

- [ ] **Step 1: 创建 `crates/llm-harness-strategy/Cargo.toml`**

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
llm-harness-loop = { workspace = true, features = ["test-utils"] }
```

- [ ] **Step 2: 创建 `crates/llm-harness-strategy/src/lib.rs`**

```rust
//! llm-harness-strategy — 用户可选注入的策略实现。
//!
//! 只依赖 `llm-harness-types` 的公开 API。trait 定义在 types，
//! 默认实现在此 crate。用户按需引入，runtime 不依赖此 crate。

pub mod budget;
pub mod resource;
pub mod skills;
```

- [ ] **Step 3: 创建占位模块文件**

`crates/llm-harness-strategy/src/budget.rs`:
```rust
// 占位，Task 2 填充
```

`crates/llm-harness-strategy/src/resource.rs`:
```rust
// 占位，Task 3 填充
```

`crates/llm-harness-strategy/src/skills.rs`:
```rust
// 占位，Task 4 填充
```

- [ ] **Step 4: 注册到根 `Cargo.toml`**

在 `[workspace]` 的 `members` 数组中，`"crates/llm-harness-runtime-tools"` 行后添加：
```toml
"crates/llm-harness-strategy",
```

在 `[workspace.dependencies]` 中，`llm-harness-runtime-tools = ...` 行后添加：
```toml
llm-harness-strategy = { path = "crates/llm-harness-strategy" }
```

- [ ] **Step 5: 验证编译**

Run: `cargo check -p llm-harness-strategy`
Expected: PASS（空模块编译通过）

- [ ] **Step 6: Commit**

```bash
git add crates/llm-harness-strategy/ Cargo.toml
git commit -m "feat: scaffold llm-harness-strategy crate"
```

---

### Task 2: 迁移 `BudgetExceededHook` trait 到 types，迁移 `BudgetControlAdapter` 到 strategy

**Files:**
- Modify: `crates/llm-harness-types/src/cost.rs` (末尾追加 trait)
- Modify: `crates/llm-harness-strategy/src/budget.rs` (填充实现)
- Modify: `crates/llm-harness-runtime/src/control/budget.rs` (删除迁移代码)
- Modify: `crates/llm-harness-runtime/src/control/mod.rs` (保持 `pub mod budget;`)
- Modify: `crates/llm-harness-runtime/src/builder.rs` (删除 budget 相关)
- Modify: `crates/llm-harness-runtime/src/control/cost_hook.rs:152` (测试 import)
- Modify: `crates/llm-harness-runtime/tests/budget_tests.rs` (import)
- Modify: `crates/llm-harness-runtime/src/lib.rs` (文档注释)

**Interfaces:**
- Consumes: `CostAggregate` (types), `ShouldStopHook`/`ShouldStopCtx` (types)
- Produces: `llm_harness_strategy::budget::BudgetControlAdapter`, `llm_harness_types::BudgetExceededHook`

- [ ] **Step 1: 在 `crates/llm-harness-types/src/cost.rs` 末尾添加 `BudgetExceededHook` trait**

在文件末尾（`mod tests` 之前）追加：

```rust
use futures::future::BoxFuture;

/// Business-layer hook for budget override decisions.
/// Called when the per-task budget limit is exceeded.
pub trait BudgetExceededHook: Send + Sync {
    /// Called when budget is exceeded. Return `true` to continue, `false` to
    /// transition the task to `Failed`.
    ///
    /// `cost` is the session-wide accumulated cost snapshot.
    fn on_budget_exceeded<'a>(
        &'a self,
        cost: &'a CostAggregate,
        budget_limit: f64,
    ) -> BoxFuture<'a, bool>;
}
```

- [ ] **Step 2: 验证 types 编译**

Run: `cargo check -p llm-harness-types`
Expected: PASS

- [ ] **Step 3: 填充 `crates/llm-harness-strategy/src/budget.rs`**

```rust
//! Budget control: pure decider hook.
//!
//! `BudgetControlAdapter` implements only `ShouldStopHook`. It reads a shared
//! `Arc<Mutex<CostAggregate>>` (maintained by `CostAccumulatorHook`) and, when
//! `total_cost` exceeds `budget_limit`, consults the optional
//! `BudgetExceededHook` for a continue/stop decision.
//!
//! Accounting-profile violations and unknown-cost provider responses always
//! stop and cannot be overridden.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use llm_harness_types::{BudgetExceededHook, CostAggregate, ShouldStopCtx, ShouldStopHook};

/// Pure budget-exceeded decider.
///
/// Implements only `ShouldStopHook`: reads a shared `Arc<Mutex<CostAggregate>>`
/// (maintained by `CostAccumulatorHook`) and, when `total_cost` exceeds
/// `budget_limit`, consults the optional `BudgetExceededHook` for a
/// continue/stop decision.
///
/// - `exceeded_hook = None` → surveillance mode: cost is tracked but the loop
///   never stops on budget.
/// - `exceeded_hook = Some(h)` → `h.on_budget_exceeded` decides: `true` =
///   continue, `false` = stop.
pub struct BudgetControlAdapter {
    cost: Arc<Mutex<CostAggregate>>,
    budget_limit: f64,
    exceeded_hook: Option<Arc<dyn BudgetExceededHook>>,
}

impl BudgetControlAdapter {
    /// Create a new pure-decider adapter.
    ///
    /// `cost` is the shared accumulated cost state (same `Arc` held by
    /// `CostAccumulatorHook` and `AgentHarness`).
    /// `budget_limit` is in USD (e.g., `5.0` for $5.00).
    /// `exceeded_hook = None` means cost-surveillance mode only (never stops).
    pub fn new(
        cost: Arc<Mutex<CostAggregate>>,
        budget_limit: f64,
        exceeded_hook: Option<Arc<dyn BudgetExceededHook>>,
    ) -> Self {
        Self {
            cost,
            budget_limit,
            exceeded_hook,
        }
    }
}

impl ShouldStopHook for BudgetControlAdapter {
    fn should_stop<'a>(&'a self, _ctx: ShouldStopCtx<'a>) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let cost_snapshot = self.cost.lock().unwrap().clone();
            if cost_snapshot.billing_usage_violations != 0 {
                return true;
            }
            if cost_snapshot.usage_reports.provider_calls != 0
                && cost_snapshot
                    .usage_reports
                    .require_all_reported_valid()
                    .is_err()
            {
                return true;
            }
            if cost_snapshot.total_cost <= self.budget_limit {
                return false;
            }
            match &self.exceeded_hook {
                None => false,
                Some(hook) => {
                    !hook
                        .on_budget_exceeded(&cost_snapshot, self.budget_limit)
                        .await
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::{AssistantMessage, StopReason};

    struct NoExceededHook;
    impl BudgetExceededHook for NoExceededHook {
        fn on_budget_exceeded<'a>(
            &'a self,
            _cost: &'a CostAggregate,
            _budget_limit: f64,
        ) -> BoxFuture<'a, bool> {
            Box::pin(async move { false })
        }
    }

    fn test_run() -> &'static llm_harness_types::RunContext {
        static RUN: std::sync::OnceLock<llm_harness_types::RunContext> = std::sync::OnceLock::new();
        RUN.get_or_init(|| {
            llm_harness_types::RunContext::new(llm_harness_types::RunRequest::default())
        })
    }

    fn make_ctx<'a>(assistant: &'a AssistantMessage) -> ShouldStopCtx<'a> {
        ShouldStopCtx {
            run: test_run(),
            last_assistant: assistant,
            stop_reason: StopReason::EndTurn,
            turn_index: 0,
        }
    }

    fn make_assistant() -> AssistantMessage {
        let mut m = llm_harness_loop::test_utils::test_assistant_message(vec![]);
        m.stop_reason = Some(StopReason::EndTurn);
        m
    }

    #[tokio::test]
    async fn does_not_stop_when_under_limit() {
        let cost = Arc::new(Mutex::new(CostAggregate::default()));
        let hook = Arc::new(NoExceededHook) as Arc<dyn BudgetExceededHook>;
        let adapter = BudgetControlAdapter::new(cost.clone(), 5.0, Some(hook));

        {
            let mut c = cost.lock().unwrap();
            c.total_cost = 1.0;
        }

        let assistant = make_assistant();
        let ctx = make_ctx(&assistant);
        let stop = adapter.should_stop(ctx).await;
        assert!(!stop, "under budget → must not stop");
    }

    #[tokio::test]
    async fn surveillance_mode_never_stops_without_hook() {
        let cost = Arc::new(Mutex::new(CostAggregate::default()));
        let adapter = BudgetControlAdapter::new(cost.clone(), 5.0, None);

        {
            let mut c = cost.lock().unwrap();
            c.total_cost = 10.0;
        }

        let assistant = make_assistant();
        let ctx = make_ctx(&assistant);
        let stop = adapter.should_stop(ctx).await;
        assert!(!stop, "surveillance mode (no hook) must not stop");
    }

    #[tokio::test]
    async fn accounting_profile_violation_always_stops() {
        let cost = Arc::new(Mutex::new(CostAggregate {
            billing_usage_violations: 1,
            ..Default::default()
        }));
        let adapter = BudgetControlAdapter::new(cost, f64::MAX, None);

        let assistant = make_assistant();
        assert!(adapter.should_stop(make_ctx(&assistant)).await);
    }

    #[tokio::test]
    async fn stops_when_over_limit_and_hook_says_stop() {
        let cost = Arc::new(Mutex::new(CostAggregate::default()));
        let hook = Arc::new(NoExceededHook) as Arc<dyn BudgetExceededHook>;
        let adapter = BudgetControlAdapter::new(cost.clone(), 5.0, Some(hook));

        {
            let mut c = cost.lock().unwrap();
            c.total_cost = 10.0;
        }

        let assistant = make_assistant();
        let ctx = make_ctx(&assistant);
        let stop = adapter.should_stop(ctx).await;
        assert!(stop, "hook returned false (stop) → adapter must stop");
    }

    #[test]
    fn budget_control_adapter_is_should_stop_hook() {
        let cost = Arc::new(Mutex::new(CostAggregate::default()));
        let adapter = BudgetControlAdapter::new(cost, 5.0, None);
        let _: &dyn ShouldStopHook = &adapter;
    }
}
```

- [ ] **Step 4: 验证 strategy crate 编译和测试**

Run: `cargo test -p llm-harness-strategy`
Expected: PASS（5 个测试）

- [ ] **Step 5: 修改 `crates/llm-harness-runtime/src/control/budget.rs`——删除迁移代码，保留 re-export**

将整个文件替换为：

```rust
//! Budget control types re-exported from `llm-harness-types`.
//!
//! `BudgetControlAdapter` has moved to `llm-harness-strategy`.
//! `BudgetExceededHook` trait is defined in `llm-harness-types`.

pub use llm_harness_types::BudgetExceededHook;
```

- [ ] **Step 6: 修改 `crates/llm-harness-runtime/src/builder.rs`**

删除 import（第 34-37 行中的 budget 相关部分）：
- 将 `use crate::control::{budget::BudgetControlAdapter, budget::BudgetExceededHook, cost::UsageLedger, cost_hook::CostAccumulatorHook,};` 改为 `use crate::control::{cost::UsageLedger, cost_hook::CostAccumulatorHook,};`

删除 `budget` 字段（第 128-130 行）：
```rust
// 删除这三行：
/// 可选的预算上限 + 超限 hook。设置后，一个 `BudgetControlAdapter`
/// 会被注入 `should_stop`，与 `CostAccumulatorHook` 共享同一 cost Arc。
budget: Option<(f64, Option<Arc<dyn BudgetExceededHook>>)>,
```

删除 `budget` 字段初始化（第 188 行 `budget: None,`）。

删除 `budget()` 方法（第 320-336 行整个方法）。

删除 `build_options()` 中 budget 注入逻辑（第 630-638 行）：
```rust
// 删除这整段：
// 若配置了 budget，注入预算决策器。
if let Some((limit, exceeded_hook)) = &self.budget {
    let budget_adapter: Arc<dyn ShouldStopHook> = Arc::new(BudgetControlAdapter::new(
        cost.clone(),
        *limit,
        exceeded_hook.clone(),
    ));
    opts.hooks.should_stop.push(budget_adapter);
}
```

删除 `ShouldStopHook` import（如果不再使用——检查 `builder.rs:30` 的 `use llm_harness_types::{... ShouldStopHook ...}`，如果 builder 不再直接引用 `ShouldStopHook` 则删除）。

删除 `build_options_injects_accumulator_and_budget_hooks` 测试（第 1128-1157 行）和 `build_options_injects_accumulator_without_pricing` 测试中关于 `BudgetControlAdapter` 的断言（第 1167-1170 行）。

- [ ] **Step 7: 修改 `crates/llm-harness-runtime/src/control/cost_hook.rs` 测试 import**

第 152 行：
```rust
// 原：
use crate::control::budget::{BudgetControlAdapter, BudgetExceededHook};
// 改为：
use llm_harness_strategy::budget::BudgetControlAdapter;
use llm_harness_types::BudgetExceededHook;
```

在 `crates/llm-harness-runtime/Cargo.toml` 的 `[dev-dependencies]` 中添加：
```toml
llm-harness-strategy.workspace = true
```

- [ ] **Step 8: 修改 `crates/llm-harness-runtime/tests/budget_tests.rs`**

第 10 行：
```rust
// 原：
use llm_harness_runtime::control::budget::{BudgetControlAdapter, BudgetExceededHook};
// 改为：
use llm_harness_strategy::budget::BudgetControlAdapter;
use llm_harness_types::BudgetExceededHook;
```

- [ ] **Step 9: 修改 `crates/llm-harness-runtime/src/lib.rs` 文档注释**

第 11 行 `//! - Budget control (`BudgetExceededHook`, `BudgetControlAdapter`)` 改为：
```rust
//! - Budget control (`BudgetExceededHook` trait; `BudgetControlAdapter` in `llm-harness-strategy`)
```

- [ ] **Step 10: 验证 runtime 编译**

Run: `cargo check -p llm-harness-runtime`
Expected: PASS

- [ ] **Step 11: 验证 runtime 测试**

Run: `cargo test -p llm-harness-runtime`
Expected: PASS（cost_hook 测试和 budget_tests 通过）

- [ ] **Step 12: Commit**

```bash
git add -A
git commit -m "refactor: migrate BudgetControlAdapter to strategy crate, move BudgetExceededHook to types"
```

---

### Task 3: 迁移 `ResourceProvider` trait 到 types，迁移 `ResourceInjector` 到 strategy

**Files:**
- Modify: `crates/llm-harness-types/src/resources.rs`
- Modify: `crates/llm-harness-strategy/src/resource.rs`
- Modify: `crates/llm-harness-runtime/src/platform/resource.rs`
- Modify: `crates/llm-harness-runtime-mcp/src/lib.rs:27-28` (import 改为从 types)

**Interfaces:**
- Consumes: `AgentContext` (types)
- Produces: `llm_harness_strategy::resource::ResourceInjector`, `llm_harness_types::{ResourceProvider, ResourceDescriptor, ResourceContent}`

- [ ] **Step 1: 在 `crates/llm-harness-types/src/resources.rs` 添加 resource trait 和类型**

在现有 `AgentHarnessResources` struct 之前追加：

```rust
use anyhow::Result;
use futures::future::BoxFuture;

/// Description of a context resource.
#[derive(Debug, Clone)]
pub struct ResourceDescriptor {
    /// Resource URI (e.g., `file:///path/to/doc`, `mcp://server/resource`).
    pub uri: String,
    /// Display name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// MIME type.
    pub mime_type: String,
}

/// Content of a context resource.
#[derive(Debug, Clone)]
pub enum ResourceContent {
    /// UTF-8 text content.
    Text(String),
    /// Raw binary content.
    Blob(Vec<u8>),
}

/// Context resource provider (MCP resources, local files, etc.).
pub trait ResourceProvider: Send + Sync {
    /// List available resources.
    fn list(&self) -> BoxFuture<'_, Result<Vec<ResourceDescriptor>>>;

    /// Read a specific resource by URI.
    fn read<'a>(&'a self, uri: &'a str) -> BoxFuture<'a, Result<ResourceContent>>;
}
```

- [ ] **Step 2: 验证 types 编译**

Run: `cargo check -p llm-harness-types`
Expected: PASS

- [ ] **Step 3: 填充 `crates/llm-harness-strategy/src/resource.rs`**

```rust
//! Resource injector — appends resource content to the system prompt tail.

use std::sync::Arc;

use anyhow::Result;
use llm_harness_types::{
    AgentContext, ResourceContent, ResourceProvider,
};

/// Resource injector — appends resource content to the system prompt tail
/// with cache-control-compatible markers.
pub struct ResourceInjector {
    _private: (),
}

impl ResourceInjector {
    /// Create a new injector.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Inject all resources from all providers into the system prompt tail.
    pub async fn inject_all(
        &self,
        providers: &[Arc<dyn ResourceProvider>],
        context: &mut AgentContext,
    ) -> Result<()> {
        for provider in providers {
            let descriptors = provider.list().await?;
            for desc in descriptors {
                self.inject_one(provider.as_ref(), &desc.uri, context)
                    .await?;
            }
        }
        Ok(())
    }

    /// Inject a single resource by URI.
    pub async fn inject(
        &self,
        provider: &dyn ResourceProvider,
        uri: &str,
        context: &mut AgentContext,
    ) -> Result<()> {
        self.inject_one(provider, uri, context).await
    }

    async fn inject_one(
        &self,
        provider: &dyn ResourceProvider,
        uri: &str,
        context: &mut AgentContext,
    ) -> Result<()> {
        let content = provider.read(uri).await?;
        let text = match content {
            ResourceContent::Text(t) => t,
            ResourceContent::Blob(b) => format!("[binary resource, {} bytes]", b.len()),
        };
        let injected = format!("<resource uri=\"{uri}\">\n{text}\n</resource>");
        match &mut context.system_prompt {
            Some(prompt) => {
                prompt.push_str("\n\n");
                prompt.push_str(&injected);
            }
            None => {
                context.system_prompt = Some(injected);
            }
        }
        Ok(())
    }
}

impl Default for ResourceInjector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;
    use llm_harness_types::{ResourceDescriptor, ResourceContent, ResourceProvider};

    struct StubProvider {
        resources: Vec<ResourceDescriptor>,
    }

    impl ResourceProvider for StubProvider {
        fn list(&self) -> BoxFuture<'_, Result<Vec<ResourceDescriptor>>> {
            let res = self.resources.clone();
            Box::pin(async move { Ok(res) })
        }

        fn read<'a>(&'a self, uri: &'a str) -> BoxFuture<'a, Result<ResourceContent>> {
            let text = format!("content of {uri}");
            Box::pin(async move { Ok(ResourceContent::Text(text)) })
        }
    }

    #[tokio::test]
    async fn inject_appends_to_existing_system_prompt() {
        let provider = StubProvider {
            resources: vec![ResourceDescriptor {
                uri: "test://doc".into(),
                name: "Test Doc".into(),
                description: "A test resource".into(),
                mime_type: "text/plain".into(),
            }],
        };
        let injector = ResourceInjector::new();
        let mut ctx = AgentContext {
            system_prompt: Some("You are an assistant.".into()),
            messages: vec![],
        };
        injector
            .inject_all(&[Arc::new(provider)], &mut ctx)
            .await
            .unwrap();
        let prompt = ctx.system_prompt.as_ref().unwrap();
        assert!(prompt.contains("You are an assistant."));
        assert!(prompt.contains("<resource uri=\"test://doc\">"));
        assert!(prompt.contains("content of test://doc"));
    }

    #[tokio::test]
    async fn inject_creates_system_prompt_when_none() {
        let provider = StubProvider {
            resources: vec![ResourceDescriptor {
                uri: "test://doc".into(),
                name: "Test Doc".into(),
                description: "A test resource".into(),
                mime_type: "text/plain".into(),
            }],
        };
        let injector = ResourceInjector::new();
        let mut ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };
        injector
            .inject_all(&[Arc::new(provider)], &mut ctx)
            .await
            .unwrap();
        assert!(ctx.system_prompt.is_some());
        assert!(ctx
            .system_prompt
            .as_ref()
            .unwrap()
            .contains("<resource uri=\"test://doc\">"));
    }
}
```

- [ ] **Step 4: 验证 strategy crate 编译和测试**

Run: `cargo test -p llm-harness-strategy`
Expected: PASS（budget + resource 测试）

- [ ] **Step 5: 修改 `crates/llm-harness-runtime/src/platform/resource.rs`——删除迁移代码，保留 re-export**

将整个文件替换为：

```rust
//! Resource types re-exported from `llm-harness-types`.
//!
//! `ResourceInjector` has moved to `llm-harness-strategy`.
//! `ResourceProvider`/`ResourceDescriptor`/`ResourceContent` are in `llm-harness-types`.

pub use llm_harness_types::{ResourceContent, ResourceDescriptor, ResourceProvider};
```

- [ ] **Step 6: 修改 `crates/llm-harness-runtime-mcp/src/lib.rs` import**

第 27-28 行：
```rust
// 原：
use llm_harness_runtime::platform::resource::{
    ResourceContent, ResourceDescriptor, ResourceProvider,
};
// 改为：
use llm_harness_types::{ResourceContent, ResourceDescriptor, ResourceProvider};
```

- [ ] **Step 7: 验证 runtime + runtime-mcp 编译**

Run: `cargo check -p llm-harness-runtime -p llm-harness-runtime-mcp`
Expected: PASS

- [ ] **Step 8: 验证 runtime 测试**

Run: `cargo test -p llm-harness-runtime -p llm-harness-runtime-mcp`
Expected: PASS

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "refactor: migrate ResourceInjector to strategy crate, move ResourceProvider to types"
```

---

### Task 4: 迁移 `format_skills_for_system_prompt` + escape helpers 到 strategy

**Files:**
- Modify: `crates/llm-harness-strategy/src/skills.rs`
- Modify: `crates/llm-harness-agent/src/skills.rs` (删除迁移函数)
- Modify: `crates/llm-harness-agent/src/lib.rs:41-45` (删除 re-export)

**Interfaces:**
- Consumes: `Skill` (types)
- Produces: `llm_harness_strategy::skills::format_skills_for_system_prompt`

- [ ] **Step 1: 填充 `crates/llm-harness-strategy/src/skills.rs`**

```rust
//! Skill metadata formatting for system prompt injection.

use llm_harness_types::Skill;

/// Escape tags that could forge skill markup boundaries.
///
/// Handles both bare tags (`<skill>`) and attributed open tags
/// (`<skill name="...">`) by matching the tag prefix.
fn escape_skill_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if s[i..].starts_with("</skill>") {
            out.push_str("</skill\\>");
            i += 8;
        } else if s[i..].starts_with("</available-skills>") {
            out.push_str("</available-skills\\>");
            i += 19;
        } else if s[i..].starts_with("<skill>") {
            out.push_str("<skill\\>");
            i += 7;
        } else if s[i..].starts_with("<skill ") {
            out.push_str("<skill\\ ");
            i += 7;
        } else if s[i..].starts_with("<available-skills>") {
            out.push_str("<available-skills\\>");
            i += 18;
        } else if s[i..].starts_with("<available-skills ") {
            out.push_str("<available-skills\\ ");
            i += 18;
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Escape a string for safe use inside an XML attribute value (`"..."`).
fn escape_xml_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Build the skills section injected into the system prompt.
///
/// Only includes skills with `disable_model_invocation = false`.
/// When `has_skill_read_tool` is true, appends a hint telling the model
/// to use the `skill_read` tool for full content.
pub fn format_skills_for_system_prompt(skills: &[Skill], has_skill_read_tool: bool) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();

    if visible.is_empty() {
        return String::new();
    }

    let mut buf = String::from("<available-skills>\n");
    for skill in visible {
        let desc = escape_skill_markup(&skill.description);
        let loc = escape_xml_attr(&skill.source.display().to_string());
        buf.push_str(&format!(
            "  <skill name=\"{}\" location=\"{}\">{}</skill>\n",
            skill.name, loc, desc
        ));
    }
    buf.push_str("</available-skills>\n");
    if has_skill_read_tool {
        buf.push_str(
            "Use the `skill_read` tool to read a skill's full content or sub-files \
             (e.g. references, examples, scripts). Pass `skill_name` and optional `path` \
             (relative to the skill directory).",
        );
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_skill(name: &str, desc: &str, disable: bool) -> Skill {
        Skill {
            name: name.into(),
            label: None,
            description: desc.into(),
            source: PathBuf::from(format!("/skills/{name}/SKILL.md")),
            base_dir: PathBuf::from(format!("/skills/{name}")),
            disable_model_invocation: disable,
        }
    }

    #[test]
    fn format_skills_excludes_disabled() {
        let skills = vec![
            make_skill("public-skill", "Do stuff", false),
            make_skill("private-skill", "Hidden", true),
        ];
        let prompt = format_skills_for_system_prompt(&skills, true);
        assert!(prompt.contains("public-skill"));
        assert!(!prompt.contains("private-skill"));
    }

    #[test]
    fn format_skills_for_system_prompt_escapes_closing_tags() {
        let skills = vec![Skill {
            name: "test".into(),
            label: None,
            description: "break </skill> <skill> </available-skills> <available-skills>".into(),
            source: PathBuf::from("/tmp/SKILL.md"),
            base_dir: PathBuf::from("/tmp"),
            disable_model_invocation: false,
        }];
        let out = format_skills_for_system_prompt(&skills, true);
        assert!(out.contains("</skill\\>"));
        assert!(out.contains("<skill\\>"));
        assert!(out.contains("</available-skills\\>"));
        assert!(out.contains("<available-skills\\>"));
        let raw_skill_count = out.matches("</skill>").count();
        assert_eq!(
            raw_skill_count, 1,
            "only the wrapper tag should be unescaped"
        );
        let raw_open_count = out.matches("<skill>").count();
        assert_eq!(
            raw_open_count, 0,
            "no unescaped <skill> should remain (wrapper uses <skill name=...>)"
        );
    }

    #[test]
    fn escape_skill_markup_escapes_attributed_open_tag() {
        let inp = r#"<skill name="evil">injected</skill>"#;
        let out = escape_skill_markup(inp);
        assert!(
            !out.contains(r#"<skill name="#),
            "attributed open tag should be escaped, got: {out}"
        );
        assert!(out.contains(r#"<skill\ name="#));
        assert!(out.contains("</skill\\>"));
    }

    #[test]
    fn escape_skill_markup_escapes_attributed_available_skills_tag() {
        let inp = r#"<available-skills level="all">content</available-skills>"#;
        let out = escape_skill_markup(inp);
        assert!(
            !out.contains(r#"<available-skills level="#),
            "attributed open tag should be escaped, got: {out}"
        );
        assert!(out.contains(r#"<available-skills\ level="#));
    }

    #[test]
    fn format_skills_for_system_prompt_escapes_source_path_in_attr() {
        let skills = vec![Skill {
            name: "test".into(),
            label: None,
            description: "safe".into(),
            source: PathBuf::from("/tmp/\"<evil>/SKILL.md"),
            base_dir: PathBuf::from("/tmp"),
            disable_model_invocation: false,
        }];
        let out = format_skills_for_system_prompt(&skills, false);
        assert!(out.contains("&quot;"), "double quote should be escaped");
        assert!(out.contains("&lt;"), "less-than should be escaped");
        assert!(out.contains("&gt;"), "greater-than should be escaped");
        let loc_start = out.find("location=\"").unwrap() + "location=\"".len();
        let loc_end = loc_start + out[loc_start..].find("\">").unwrap();
        let loc_attr = &out[loc_start..loc_end];
        assert!(
            !loc_attr.contains('"'),
            "no raw unescaped quote inside location attribute"
        );
    }
}
```

- [ ] **Step 2: 验证 strategy crate 编译和测试**

Run: `cargo test -p llm-harness-strategy`
Expected: PASS（budget + resource + skills 测试）

- [ ] **Step 3: 从 `crates/llm-harness-agent/src/skills.rs` 删除迁移的函数**

删除以下函数（保留文件中其他所有内容）：
- `escape_skill_markup`（第 365-398 行）
- `escape_xml_attr`（第 401-406 行）
- `format_skills_for_system_prompt`（第 408-441 行）
- 相关测试：`format_skills_excludes_disabled`、`format_skills_for_system_prompt_escapes_closing_tags`、`escape_skill_markup_escapes_attributed_open_tag`、`escape_skill_markup_escapes_attributed_available_skills_tag`、`format_skills_for_system_prompt_escapes_source_path_in_attr`

- [ ] **Step 4: 修改 `crates/llm-harness-agent/src/lib.rs` re-export**

第 41-45 行，从 re-export 列表中删除 `format_skills_for_system_prompt`：
```rust
// 原：
pub use skills::{
    PromptTemplate, Skill, SkillDiagnostic, SkillReadTool, SourcedSkill, format_skill_invocation,
    format_skills_for_system_prompt, invoke_template, load_prompt_templates, load_skills,
    parse_command_args, parse_frontmatter, parse_skill_file, resolve_skill_resource,
};
// 改为：
pub use skills::{
    PromptTemplate, Skill, SkillDiagnostic, SkillReadTool, SourcedSkill, format_skill_invocation,
    invoke_template, load_prompt_templates, load_skills,
    parse_command_args, parse_frontmatter, parse_skill_file, resolve_skill_resource,
};
```

- [ ] **Step 5: 验证 agent crate 编译**

Run: `cargo check -p llm-harness-agent`
Expected: PASS

- [ ] **Step 6: 验证全 workspace 编译**

Run: `cargo check --workspace`
Expected: PASS

- [ ] **Step 7: 验证全 workspace 测试**

Run: `cargo test --workspace`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "refactor: migrate format_skills_for_system_prompt to strategy crate"
```

---

### Task 5: 更新 layer-analysis 文档中的进度标记

**Files:**
- Modify: `docs/ai-agent-book-layer-analysis.md`

- [ ] **Step 1: 在阶段一标题后添加完成标记**

在 `> #### 阶段一：提取已有策略代码到策略 crate` 行后添加：
```
>
> > ✅ 已完成：3 项迁移（BudgetControlAdapter、ResourceInjector、format_skills_for_system_prompt）
```

- [ ] **Step 2: Commit**

```bash
git add -f docs/ai-agent-book-layer-analysis.md
git commit -m "docs: mark stage 1 migration complete"
```
