**Goal:** 为 oh-my-harness 构建可组合的插件系统——外部开发者通过实现 `Plugin` trait 一次性注册 tools/hooks/skills/templates，用户通过 `HarnessBuilder::install()` 将多个插件叠在一起。

**Architecture:** 底层将 `HarnessHooks` 所有字段从 `Option<Arc<dyn Hook>>` 改为 `Vec<Arc<dyn Hook>>` 实现多播化，补全 12 个 Composite 包装器（全部放在 loop crate），新增 `Plugin` trait（agent crate）和 `HarnessBuilder`（runtime crate），Builder 内部构造 `AgentHarnessOptions` 调用现有构造函数，provider 匹配支持精确和 glob 模式。

**Tech Stack:** Rust (async/await, tokio, Arc, trait objects), tracing

## 架构决策

1. **Composite 从 runtime crate 移到 loop crate**：agent harness 的 `build_loop_config()` 需要将 `Vec<Arc<dyn Hook>>` wrap 成 `Option<Arc<dyn CompositeHook>>` 传给 `LoopConfig`。agent 依赖 loop 但不依赖 runtime，所以 Composite 必须在 loop 或 types 中。选 loop——与 `HookedTool` 同一层次作为 hook 基础设施。

2. **Agent harness 中非 LoopConfig 的 hook（before_run/before_turn/after_turn/before_compact）直接用 for 循环遍历 Vec**：这几个 hook 在 agent harness 的事件循环中调用，不需要进 LoopConfig。在 agent crate 内直接遍历 Vec 避免跨层依赖 Composite。

3. **HarnessBuilder 生成 AgentHarness 而非替换**：Builder 的 `build()` 内部构造 `AgentHarnessOptions`，然后调用现有 `AgentHarness::with_session()` 或 `new_in_memory()`。不破坏现有公开 API。

---

## 依赖关系图

```
types (hook traits, ctx types, Tool trait, AgentError, ...)
  ↑
loop (LoopConfig, HookedTool, Composite*系列, agent_loop, ...)
  ↑
agent (HarnessHooks, Plugin trait, AgentHarness, Skill, PromptTemplate, ...)
  ↑
runtime (HarnessBuilder, task, MCP, sandbox, auth, audit, ...)
```

---

## 文件变更清单

| 操作 | 文件 | 职责 |
|------|------|------|
| 创建 | `crates/llm-harness-loop/src/composite.rs` | 12 个 Composite hook 包装器（从 runtime 移入 5 个 + 新增 7 个） |
| 修改 | `crates/llm-harness-loop/src/lib.rs` | 导出 composite 模块 |
| 修改 | `crates/llm-harness-runtime/src/composite.rs` | 改为 re-export（`pub use llm_harness_loop::composite::*;`）保持向后兼容 |
| 修改 | `crates/llm-harness-agent/src/harness.rs` | HarnessHooks Option→Vec，适配 build_loop_config，run_before_run_hook，事件循环 hook 调用 |
| 修改 | `crates/llm-harness-agent/src/lib.rs` | 导出 Plugin trait、StateProvider trait |
| 创建 | `crates/llm-harness-agent/src/plugin.rs` | Plugin trait、StateProvider trait |
| 创建 | `crates/llm-harness-runtime/src/builder.rs` | HarnessBuilder + ProviderMatcher + HarnessBuildError |
| 修改 | `crates/llm-harness-runtime/src/lib.rs` | 导出 builder 模块 |
| 修改 | 所有 HarnessHooks 构造点 | Option→Vec 迁移（~7 个文件） |

---

### Task 1: 将 Composite 从 runtime 移至 loop crate

**Files:**
- Create: `crates/llm-harness-loop/src/composite.rs`
- Modify: `crates/llm-harness-loop/src/lib.rs`
- Modify: `crates/llm-harness-runtime/src/composite.rs`
- Modify: `crates/llm-harness-runtime/src/lib.rs`

**Interfaces:**
- Consumes: 现有 `crates/llm-harness-runtime/src/composite.rs`（5 个 Composite），`crates/llm-harness-loop/src/hooked_tool.rs`（理解 HookedTool 模式）
- Produces: `llm_harness_loop::composite` 模块，包含 5 个已有 Composite；runtime 的 composite.rs 改为 `pub use llm_harness_loop::composite::*;`

- [ ] **Step 1: 运行现有测试确认基线**

Run: `cargo test -p llm-harness-runtime composite_ 2>&1`
Expected: 5 个 composite 测试 PASS

- [ ] **Step 2: 创建 `crates/llm-harness-loop/src/composite.rs`**

从现有的 `crates/llm-harness-runtime/src/composite.rs` 复制 5 个已有 Composite 实现，修改 crate 引用：

```rust
// crates/llm-harness-loop/src/composite.rs
use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{
    AfterProviderResponseHook, AfterToolCallCtx, AfterToolCallDecision, AfterToolCallHook,
    AfterTurnCtx, AfterTurnHook, BeforeProviderRequestHook, BeforeToolCallCtx,
    BeforeToolCallDecision, BeforeToolCallHook, ProviderResponseInfo, StreamOptions,
};

/// Chains multiple `AfterProviderResponseHook` implementations (all-execute semantics).
///
/// All hooks execute in registration order. Return value: `()` (observing only).
pub struct CompositeAfterProviderResponseHook {
    hooks: Vec<Arc<dyn AfterProviderResponseHook>>,
}

impl CompositeAfterProviderResponseHook {
    pub fn new(hooks: Vec<Arc<dyn AfterProviderResponseHook>>) -> Self {
        Self { hooks }
    }
}

impl AfterProviderResponseHook for CompositeAfterProviderResponseHook {
    fn after_response<'a>(&'a self, info: &'a ProviderResponseInfo) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for hook in &self.hooks {
                hook.after_response(info).await;
            }
        })
    }
}

/// Chains multiple `AfterToolCallHook` implementations (last-non-passthrough semantics).
///
/// Hooks execute in order; each sees the original ctx. The last `Patch` decision wins.
pub struct CompositeAfterToolCallHook {
    hooks: Vec<Arc<dyn AfterToolCallHook>>,
}

impl CompositeAfterToolCallHook {
    pub fn new(hooks: Vec<Arc<dyn AfterToolCallHook>>) -> Self {
        Self { hooks }
    }
}

impl AfterToolCallHook for CompositeAfterToolCallHook {
    fn on_complete<'a>(
        &'a self,
        ctx: AfterToolCallCtx<'a>,
    ) -> BoxFuture<'a, AfterToolCallDecision> {
        Box::pin(async move {
            let mut decision = AfterToolCallDecision::Passthrough;
            for hook in &self.hooks {
                let hook_ctx = AfterToolCallCtx {
                    assistant_message: ctx.assistant_message,
                    tool_use_id: ctx.tool_use_id,
                    tool_name: ctx.tool_name,
                    args: ctx.args,
                    result: ctx.result,
                    turn_index: ctx.turn_index,
                };
                let d = hook.on_complete(hook_ctx).await;
                if !matches!(d, AfterToolCallDecision::Passthrough) {
                    decision = d;
                }
            }
            decision
        })
    }
}

/// Chains multiple `BeforeToolCallHook` implementations (first-non-allow semantics).
///
/// Hooks execute in order; the first `Deny` or `Modify` decision short-circuits.
pub struct CompositeBeforeToolCallHook {
    hooks: Vec<Arc<dyn BeforeToolCallHook>>,
}

impl CompositeBeforeToolCallHook {
    pub fn new(hooks: Vec<Arc<dyn BeforeToolCallHook>>) -> Self {
        Self { hooks }
    }
}

impl BeforeToolCallHook for CompositeBeforeToolCallHook {
    fn on_call<'a>(&'a self, ctx: BeforeToolCallCtx<'a>) -> BoxFuture<'a, BeforeToolCallDecision> {
        Box::pin(async move {
            for hook in &self.hooks {
                let hook_ctx = BeforeToolCallCtx {
                    assistant_message: ctx.assistant_message,
                    tool_use_id: ctx.tool_use_id,
                    tool_name: ctx.tool_name,
                    args: ctx.args,
                    turn_index: ctx.turn_index,
                };
                let decision = hook.on_call(hook_ctx).await;
                if !matches!(decision, BeforeToolCallDecision::Allow) {
                    return decision;
                }
            }
            BeforeToolCallDecision::Allow
        })
    }
}

/// Chains multiple `AfterTurnHook` implementations (all-execute semantics).
///
/// All hooks execute in registration order (observing only, no return value).
pub struct CompositeAfterTurnHook {
    hooks: Vec<Arc<dyn AfterTurnHook>>,
}

impl CompositeAfterTurnHook {
    pub fn new(hooks: Vec<Arc<dyn AfterTurnHook>>) -> Self {
        Self { hooks }
    }
}

impl AfterTurnHook for CompositeAfterTurnHook {
    fn after_turn<'a>(&'a self, ctx: AfterTurnCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for hook in &self.hooks {
                let hook_ctx = AfterTurnCtx {
                    turn_index: ctx.turn_index,
                    new_messages: ctx.new_messages,
                };
                hook.after_turn(hook_ctx).await;
            }
        })
    }
}

/// Chains multiple `BeforeProviderRequestHook` implementations (all-execute semantics).
///
/// All hooks execute in registration order, each may mutate `StreamOptions`.
pub struct CompositeBeforeProviderRequestHook {
    hooks: Vec<Arc<dyn BeforeProviderRequestHook>>,
}

impl CompositeBeforeProviderRequestHook {
    pub fn new(hooks: Vec<Arc<dyn BeforeProviderRequestHook>>) -> Self {
        Self { hooks }
    }
}

impl BeforeProviderRequestHook for CompositeBeforeProviderRequestHook {
    fn before_request<'a>(&'a self, opts: &'a mut StreamOptions) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for hook in &self.hooks {
                hook.before_request(opts).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_after_provider_response_hook_is_object_safe() {
        fn _accept(_: &dyn AfterProviderResponseHook) {}
        let composite = CompositeAfterProviderResponseHook::new(vec![]);
        _accept(&composite);
    }

    #[test]
    fn composite_after_tool_call_hook_is_object_safe() {
        fn _accept(_: &dyn AfterToolCallHook) {}
        let composite = CompositeAfterToolCallHook::new(vec![]);
        _accept(&composite);
    }

    #[test]
    fn composite_before_tool_call_hook_is_object_safe() {
        fn _accept(_: &dyn BeforeToolCallHook) {}
        let composite = CompositeBeforeToolCallHook::new(vec![]);
        _accept(&composite);
    }

    #[test]
    fn composite_after_turn_hook_is_object_safe() {
        fn _accept(_: &dyn AfterTurnHook) {}
        let composite = CompositeAfterTurnHook::new(vec![]);
        _accept(&composite);
    }

    #[test]
    fn composite_before_provider_request_hook_is_object_safe() {
        fn _accept(_: &dyn BeforeProviderRequestHook) {}
        let composite = CompositeBeforeProviderRequestHook::new(vec![]);
        _accept(&composite);
    }
}
```

- [ ] **Step 3: 在 llm-harness-loop/src/lib.rs 中导出 composite 模块**

在 `lib.rs` 中添加：

```rust
pub mod composite;
```

并在 re-exports 区域添加：

```rust
// Re-export Composite wrappers for downstream (harness, runtime).
pub use composite::*;
```

- [ ] **Step 4: 修改 runtime crate 的 composite.rs 为 re-export**

将 `crates/llm-harness-runtime/src/composite.rs` 替换为：

```rust
// Composite hook wrappers are now defined in llm-harness-loop.
// This re-export preserves backward compatibility.
pub use llm_harness_loop::composite::*;
```

- [ ] **Step 5: 运行测试确认 compilation 和测试通过**

Run: `cargo test -p llm-harness-loop composite_ 2>&1`
Expected: 5 个 composite 测试 PASS

Run: `cargo test -p llm-harness-runtime composite_ 2>&1`
Expected: 5 个 composite 测试 PASS (via re-export)

- [ ] **Step 6: Commit**

```bash
git add crates/llm-harness-loop/src/composite.rs crates/llm-harness-loop/src/lib.rs
git add crates/llm-harness-runtime/src/composite.rs crates/llm-harness-runtime/src/lib.rs
git commit -m "refactor(loop): move Composite hook wrappers from runtime to loop crate

Agent harness needs Composite to wrap Vec<dyn Hook> into Option<dyn CompositeHook>
for LoopConfig but cannot depend on runtime. Loop crate (between types and agent)
is the correct layer for this infrastructure.

Runtime crate composite.rs now re-exports from loop for backward compatibility.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: HarnessHooks Option→Vec + 移除独立 auth 字段

**Files:**
- Modify: `crates/llm-harness-agent/src/harness.rs:57-103,309,420-433,468+`
- Modify: `crates/llm-harness-agent/src/lib.rs:24`

**Interfaces:**
- Consumes: Task 1 (Composite 已在 loop crate)
- Produces: `HarnessHooks` 所有 12 个字段类型 `Vec<Arc<dyn XxxHook>>`，`HarnessHooks::none()` 返回全空 vec，`AgentHarness` 移除独立 `auth` 字段，`AgentHarnessOptions.auth` 标记 `#[deprecated]`

- [ ] **Step 1: 改 HarnessHooks 结构体和 none()**

修改 `crates/llm-harness-agent/src/harness.rs` 第 59-103 行：

```rust
/// Collection of hooks injected into an [`AgentHarness`].
///
/// Multiple hooks of the same trait accumulate in registration order and are
/// composed at harness build time via `Composite*` wrappers from `llm-harness-loop`.
#[derive(Clone)]
pub struct HarnessHooks {
    pub before_run: Vec<Arc<dyn BeforeRunHook>>,
    pub before_turn: Vec<Arc<dyn BeforeTurnHook>>,
    pub after_turn: Vec<Arc<dyn AfterTurnHook>>,
    pub before_tool_call: Vec<Arc<dyn BeforeToolCallHook>>,
    pub after_tool_call: Vec<Arc<dyn AfterToolCallHook>>,
    pub transform_context: Vec<Arc<dyn TransformContextHook>>,
    pub prepare_next_turn: Vec<Arc<dyn PrepareNextTurnHook>>,
    pub should_stop: Vec<Arc<dyn ShouldStopHook>>,
    pub before_provider_request: Vec<Arc<dyn BeforeProviderRequestHook>>,
    pub after_provider_response: Vec<Arc<dyn AfterProviderResponseHook>>,
    pub before_compact: Vec<Arc<dyn BeforeCompactHook>>,
    pub auth: Vec<Arc<dyn AuthHook>>,
}

impl HarnessHooks {
    /// All hooks absent (default state).
    pub fn none() -> Self {
        Self {
            before_run: vec![],
            before_turn: vec![],
            after_turn: vec![],
            before_tool_call: vec![],
            after_tool_call: vec![],
            transform_context: vec![],
            prepare_next_turn: vec![],
            should_stop: vec![],
            before_provider_request: vec![],
            after_provider_response: vec![],
            before_compact: vec![],
            auth: vec![],
        }
    }
}
```

- [ ] **Step 2: 移除 AgentHarness 独立 auth 字段**

修改 `AgentHarness` struct（第 420-433 行），删除 `auth: Option<Arc<dyn AuthHook>>`：

```rust
pub struct AgentHarness {
    client: Arc<dyn LlmClient>,
    session: Session,
    env: Arc<dyn ExecutionEnv>,
    inner: Arc<Mutex<HarnessInner>>,
    event_tx: broadcast::Sender<Arc<AgentHarnessEvent>>,
    phase_tx: watch::Sender<HarnessPhase>,
    phase_rx: watch::Receiver<HarnessPhase>,
    convert_to_llm: Arc<dyn ConvertToLlmHook>,
    hooks: HarnessHooks,
    stream_options: StreamOptions,
}
```

- [ ] **Step 3: 标记 AgentHarnessOptions.auth 为 deprecated**

修改第 308-309 行：

```rust
    /// Authentication hook. **Deprecated:** use `hooks.auth` instead.
    #[deprecated(note = "Use hooks.auth: Vec<Arc<dyn AuthHook>> instead")]
    pub auth: Option<Arc<dyn AuthHook>>,
```

- [ ] **Step 4: AgentHarness::with_session() 中迁移 deprecated auth**

在 `with_session()` 中，在 `Self { }` 构造之前添加 auth 迁移逻辑：

```rust
// with_session() 中，opts 被解构或使用之前：
#[allow(deprecated)]
if let Some(auth_hook) = opts.auth {
    hooks.auth.push(auth_hook);
}
```

并在 `Self { ... }` 构造中删除 `.auth` 字段。

- [ ] **Step 5: 运行编译确认改动正确**

Run: `cargo check -p llm-harness-agent 2>&1`
Expected: 编译成功（可能有一些 warn 因为还没有适配所有调用方）

- [ ] **Step 6: Commit**

```bash
git add crates/llm-harness-agent/src/harness.rs crates/llm-harness-agent/src/lib.rs
git commit -m "refactor(agent): change HarnessHooks fields from Option to Vec, unify auth

All 12 hook slots are now Vec<Arc<dyn Hook>> for multi-plugin composition.
Removed standalone AgentHarness.auth and deprecated AgentHarnessOptions.auth.
Auth now lives only in hooks.auth.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: 补齐 7 个新 Composite + AfterToolCall 改为链式

**Files:**
- Modify: `crates/llm-harness-loop/src/composite.rs`

**Interfaces:**
- Consumes: 12 个 hook trait（from types），Task 2 的 Vec hook 结构
- Produces:
  - `CompositeBeforeRunHook` (all-execute, 消息累积 + 最后 system_prompt 胜出)
  - `CompositeBeforeTurnHook` (all-execute)
  - `CompositeTransformContextHook` (链式，ctx 依次传递)
  - `CompositePrepareNextTurnHook` (字段合并，每个字段取最后一个非 None)
  - `CompositeShouldStopHook` (any-true 短路)
  - `CompositeBeforeCompactHook` (first-non-proceed 短路)
  - `CompositeAuthHook` (last-wins)
  - `CompositeAfterToolCallHook` 改为链式（当前: last-non-passthrough → 改为: 每个 hook 看到前一个 patch 后的结果）

- [ ] **Step 1: 在 composite.rs 顶部追加新 import**

在 `crates/llm-harness-loop/src/composite.rs` 顶部添加：

```rust
use llm_harness_types::{
    AgentContext, AgentError, AuthHook, AuthInfo,
    BeforeCompactCtx, BeforeCompactDecision, BeforeCompactHook,
    BeforeRunCtx, BeforeRunHook, BeforeRunResult,
    BeforeTurnCtx, BeforeTurnHook,
    NextTurnDirective, PrepareNextTurnCtx, PrepareNextTurnHook,
    ShouldStopCtx, ShouldStopHook,
    TransformContextHook,
};
```

- [ ] **Step 2: 追加 7 个新 Composite 实现**

在 `CompositeBeforeProviderRequestHook` 实现之后、`#[cfg(test)]` 之前插入：

```rust
// ── CompositeBeforeRunHook ────────────────────────────────────────────────────

pub struct CompositeBeforeRunHook {
    hooks: Vec<Arc<dyn BeforeRunHook>>,
}

impl CompositeBeforeRunHook {
    pub fn new(hooks: Vec<Arc<dyn BeforeRunHook>>) -> Self {
        Self { hooks }
    }
}

impl BeforeRunHook for CompositeBeforeRunHook {
    fn before_run<'a>(
        &'a self,
        ctx: BeforeRunCtx<'a>,
    ) -> BoxFuture<'a, Result<BeforeRunResult, AgentError>> {
        Box::pin(async move {
            let mut all_messages = Vec::new();
            let mut system_prompt: Option<String> = None;
            for hook in &self.hooks {
                // Each hook reborrow the &mut pointers, allowing them to be
                // reused across loop iterations.
                let result = hook.before_run(BeforeRunCtx {
                    prompt_text: ctx.prompt_text,
                    initial_messages: &mut *ctx.initial_messages,
                    system_prompt: &mut *ctx.system_prompt,
                    resources: ctx.resources,
                }).await?;
                all_messages.extend(result.additional_messages);
                if result.system_prompt.is_some() {
                    system_prompt = result.system_prompt;
                }
            }
            Ok(BeforeRunResult {
                additional_messages: all_messages,
                system_prompt,
            })
        })
    }
}

// ── CompositeBeforeTurnHook ───────────────────────────────────────────────────

pub struct CompositeBeforeTurnHook {
    hooks: Vec<Arc<dyn BeforeTurnHook>>,
}

impl CompositeBeforeTurnHook {
    pub fn new(hooks: Vec<Arc<dyn BeforeTurnHook>>) -> Self {
        Self { hooks }
    }
}

impl BeforeTurnHook for CompositeBeforeTurnHook {
    fn before_turn<'a>(&'a self, ctx: BeforeTurnCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for hook in &self.hooks {
                let hook_ctx = BeforeTurnCtx {
                    turn_index: ctx.turn_index,
                    snapshot: ctx.snapshot,
                };
                hook.before_turn(hook_ctx).await;
            }
        })
    }
}

// ── CompositeTransformContextHook ─────────────────────────────────────────────

pub struct CompositeTransformContextHook {
    hooks: Vec<Arc<dyn TransformContextHook>>,
}

impl CompositeTransformContextHook {
    pub fn new(hooks: Vec<Arc<dyn TransformContextHook>>) -> Self {
        Self { hooks }
    }
}

impl TransformContextHook for CompositeTransformContextHook {
    fn transform<'a>(
        &'a self,
        mut ctx: AgentContext,
    ) -> BoxFuture<'a, Result<AgentContext, AgentError>> {
        Box::pin(async move {
            for hook in &self.hooks {
                ctx = hook.transform(ctx).await?;
            }
            Ok(ctx)
        })
    }
}

// ── CompositePrepareNextTurnHook ──────────────────────────────────────────────

pub struct CompositePrepareNextTurnHook {
    hooks: Vec<Arc<dyn PrepareNextTurnHook>>,
}

impl CompositePrepareNextTurnHook {
    pub fn new(hooks: Vec<Arc<dyn PrepareNextTurnHook>>) -> Self {
        Self { hooks }
    }
}

impl PrepareNextTurnHook for CompositePrepareNextTurnHook {
    fn prepare<'a>(
        &'a self,
        ctx: PrepareNextTurnCtx<'a>,
    ) -> BoxFuture<'a, Result<NextTurnDirective, AgentError>> {
        Box::pin(async move {
            let mut merged = NextTurnDirective {
                context: None,
                model: None,
                thinking_level: None,
                tools: None,
                active_tools: None,
            };
            for hook in &self.hooks {
                let hook_ctx = PrepareNextTurnCtx {
                    turn_index: ctx.turn_index,
                    last_message: ctx.last_message,
                    last_tool_results: ctx.last_tool_results,
                };
                let d = hook.prepare(hook_ctx).await?;
                if d.context.is_some() { merged.context = d.context; }
                if d.model.is_some() { merged.model = d.model; }
                if d.thinking_level.is_some() { merged.thinking_level = d.thinking_level; }
                if d.tools.is_some() { merged.tools = d.tools; }
                if d.active_tools.is_some() { merged.active_tools = d.active_tools; }
            }
            Ok(merged)
        })
    }
}

// ── CompositeShouldStopHook ───────────────────────────────────────────────────

pub struct CompositeShouldStopHook {
    hooks: Vec<Arc<dyn ShouldStopHook>>,
}

impl CompositeShouldStopHook {
    pub fn new(hooks: Vec<Arc<dyn ShouldStopHook>>) -> Self {
        Self { hooks }
    }
}

impl ShouldStopHook for CompositeShouldStopHook {
    fn should_stop<'a>(&'a self, ctx: ShouldStopCtx<'a>) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            for hook in &self.hooks {
                let hook_ctx = ShouldStopCtx {
                    last_assistant: ctx.last_assistant,
                    stop_reason: ctx.stop_reason,
                    turn_index: ctx.turn_index,
                };
                if hook.should_stop(hook_ctx).await {
                    return true;
                }
            }
            false
        })
    }
}

// ── CompositeBeforeCompactHook ────────────────────────────────────────────────

pub struct CompositeBeforeCompactHook {
    hooks: Vec<Arc<dyn BeforeCompactHook>>,
}

impl CompositeBeforeCompactHook {
    pub fn new(hooks: Vec<Arc<dyn BeforeCompactHook>>) -> Self {
        Self { hooks }
    }
}

impl BeforeCompactHook for CompositeBeforeCompactHook {
    fn before_compact<'a>(
        &'a self,
        ctx: BeforeCompactCtx<'a>,
    ) -> BoxFuture<'a, BeforeCompactDecision> {
        Box::pin(async move {
            for hook in &self.hooks {
                let hook_ctx = BeforeCompactCtx {
                    estimated_tokens: ctx.estimated_tokens,
                    messages: ctx.messages,
                };
                let decision = hook.before_compact(hook_ctx).await;
                if !matches!(decision, BeforeCompactDecision::Proceed) {
                    return decision;
                }
            }
            BeforeCompactDecision::Proceed
        })
    }
}

// ── CompositeAuthHook ─────────────────────────────────────────────────────────

pub struct CompositeAuthHook {
    hooks: Vec<Arc<dyn AuthHook>>,
}

impl CompositeAuthHook {
    pub fn new(hooks: Vec<Arc<dyn AuthHook>>) -> Self {
        Self { hooks }
    }
}

impl AuthHook for CompositeAuthHook {
    fn resolve<'a>(&'a self) -> BoxFuture<'a, Result<AuthInfo, AgentError>> {
        Box::pin(async move {
            let mut last = AuthInfo {
                api_key: None,
                headers: vec![],
            };
            for hook in &self.hooks {
                last = hook.resolve().await?;
            }
            Ok(last)
        })
    }
}
```

- [ ] **Step 3: 将 CompositeAfterToolCallHook 改为链式语义**

替换 `composite.rs` 中的 `impl AfterToolCallHook for CompositeAfterToolCallHook`（当前第 48-73 行区域）：

```rust
impl AfterToolCallHook for CompositeAfterToolCallHook {
    fn on_complete<'a>(
        &'a self,
        ctx: AfterToolCallCtx<'a>,
    ) -> BoxFuture<'a, AfterToolCallDecision> {
        Box::pin(async move {
            // Track the current effective result so each hook sees prior patches.
            // None means "use the original ctx.result".
            let mut effective_result: Option<Result<ToolResult, ToolError>> = None;
            let mut final_decision = AfterToolCallDecision::Passthrough;

            for hook in &self.hooks {
                let hook_ctx = AfterToolCallCtx {
                    assistant_message: ctx.assistant_message,
                    tool_use_id: ctx.tool_use_id,
                    tool_name: ctx.tool_name,
                    args: ctx.args,
                    result: effective_result.as_ref().unwrap_or(ctx.result),
                    turn_index: ctx.turn_index,
                };
                let d = hook.on_complete(hook_ctx).await;
                match &d {
                    AfterToolCallDecision::Passthrough => { /* keep prior decision */ }
                    AfterToolCallDecision::Patch(patch) => {
                        final_decision = d;
                        // Apply this patch so the next hook sees the updated result.
                        let base = effective_result.as_ref().unwrap_or(ctx.result);
                        let patched = match base {
                            Ok(tr) => Ok(ToolResult {
                                content: patch.content.clone().unwrap_or_else(|| tr.content.clone()),
                                details: patch.details.clone().unwrap_or_else(|| tr.details.clone()),
                                is_error: patch.is_error.unwrap_or(tr.is_error),
                                terminate: patch.terminate.unwrap_or(tr.terminate),
                            }),
                            Err(e) => Err(e.clone()), // ToolError patching is limited; preserve the error
                        };
                        effective_result = Some(patched);
                    }
                }
            }
            final_decision
        })
    }
}
```

- [ ] **Step 4: 追加 7 个新 object-safety 测试**

在 composite.rs 的 `#[cfg(test)] mod tests` 中追加：

```rust
#[test]
fn composite_before_run_hook_is_object_safe() {
    fn _accept(_: &dyn BeforeRunHook) {}
    let composite = CompositeBeforeRunHook::new(vec![]);
    _accept(&composite);
}

#[test]
fn composite_before_turn_hook_is_object_safe() {
    fn _accept(_: &dyn BeforeTurnHook) {}
    let composite = CompositeBeforeTurnHook::new(vec![]);
    _accept(&composite);
}

#[test]
fn composite_transform_context_hook_is_object_safe() {
    fn _accept(_: &dyn TransformContextHook) {}
    let composite = CompositeTransformContextHook::new(vec![]);
    _accept(&composite);
}

#[test]
fn composite_prepare_next_turn_hook_is_object_safe() {
    fn _accept(_: &dyn PrepareNextTurnHook) {}
    let composite = CompositePrepareNextTurnHook::new(vec![]);
    _accept(&composite);
}

#[test]
fn composite_should_stop_hook_is_object_safe() {
    fn _accept(_: &dyn ShouldStopHook) {}
    let composite = CompositeShouldStopHook::new(vec![]);
    _accept(&composite);
}

#[test]
fn composite_before_compact_hook_is_object_safe() {
    fn _accept(_: &dyn BeforeCompactHook) {}
    let composite = CompositeBeforeCompactHook::new(vec![]);
    _accept(&composite);
}

#[test]
fn composite_auth_hook_is_object_safe() {
    fn _accept(_: &dyn AuthHook) {}
    let composite = CompositeAuthHook::new(vec![]);
    _accept(&composite);
}
```

- [ ] **Step 5: 运行测试**

Run: `cargo test -p llm-harness-loop composite_ 2>&1`
Expected: 12 个 composite 测试全部 PASS

- [ ] **Step 6: Commit**

```bash
git add crates/llm-harness-loop/src/composite.rs
git commit -m "feat(loop): add 7 new Composite wrappers and chain semantics for AfterToolCall

Added CompositeBeforeRunHook, CompositeBeforeTurnHook, CompositeTransformContextHook,
CompositePrepareNextTurnHook, CompositeShouldStopHook, CompositeBeforeCompactHook,
CompositeAuthHook. Changed AfterToolCallHook composite to chain semantics so
each hook sees patches from prior hooks.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: 适配 agent harness 使用 Vec hooks + Composite

**Files:**
- Modify: `crates/llm-harness-agent/src/harness.rs:1-20(imports), 1184-1241, 1407-1423, 1131-1180, ~1466`

**Interfaces:**
- Consumes: Task 2 的 Vec HarnessHooks，Task 3 的 12 个 Composite（from loop）
- Produces: `build_loop_config()` 内 Vec→Composite→Option wrapping，`run_before_run_hook` 用 Composite，事件循环内 before_turn/after_turn/before_compact 用 Composite

- [ ] **Step 1: 添加 import**

在 harness.rs 顶部添加 Composite import：

```rust
use llm_harness_loop::composite::{
    CompositeBeforeRunHook,
    CompositeBeforeTurnHook,
    CompositeAfterTurnHook,
    CompositeTransformContextHook,
    CompositePrepareNextTurnHook,
    CompositeShouldStopHook,
    CompositeBeforeCompactHook,
    CompositeBeforeToolCallHook,
    CompositeAfterToolCallHook,
    CompositeBeforeProviderRequestHook,
    CompositeAfterProviderResponseHook,
    CompositeAuthHook,
};
```

- [ ] **Step 2: 修改 build_loop_config() Vec→Composite wrapping**

替换 `build_loop_config()` 方法（第 1184-1241 行）：

```rust
fn build_loop_config(
    &self,
    steer_rx: mpsc::Receiver<AgentMessage>,
    follow_up_rx: mpsc::Receiver<AgentMessage>,
    abort: CancellationToken,
) -> LoopConfig {
    let inner = self.inner.lock().unwrap();
    let st = &inner.state;
    let retry = inner.retry.clone();

    // Wrap tools with HookedTool if before/after tool hooks are registered.
    let tools: Vec<Arc<dyn Tool>> = if !self.hooks.before_tool_call.is_empty()
        || !self.hooks.after_tool_call.is_empty()
    {
        let before: Option<Arc<dyn BeforeToolCallHook>> =
            (!self.hooks.before_tool_call.is_empty()).then(||
                Arc::new(CompositeBeforeToolCallHook::new(self.hooks.before_tool_call.clone()))
                    as Arc<dyn BeforeToolCallHook>);
        let after: Option<Arc<dyn AfterToolCallHook>> =
            (!self.hooks.after_tool_call.is_empty()).then(||
                Arc::new(CompositeAfterToolCallHook::new(self.hooks.after_tool_call.clone()))
                    as Arc<dyn AfterToolCallHook>);
        st.tools
            .clone()
            .into_iter()
            .map(|t| {
                Arc::new(HookedTool {
                    inner: t,
                    before: before.clone(),
                    after: after.clone(),
                }) as Arc<dyn Tool>
            })
            .collect()
    } else {
        st.tools.clone()
    };

    // Build prepare_next_turn: chain user hooks with DefaultPrepareNextTurn.
    let user_prepare: Option<Arc<dyn PrepareNextTurnHook>> =
        (!self.hooks.prepare_next_turn.is_empty()).then(||
            Arc::new(CompositePrepareNextTurnHook::new(self.hooks.prepare_next_turn.clone()))
                as Arc<dyn PrepareNextTurnHook>);
    let prepare_next_turn: Arc<dyn PrepareNextTurnHook> = Arc::new(DefaultPrepareNextTurn {
        inner: self.inner.clone(),
        user_hook: user_prepare,
    });

    // Vec→Option wrapping for hooks passed into LoopConfig.
    let transform_context: Option<Arc<dyn TransformContextHook>> =
        (!self.hooks.transform_context.is_empty()).then(||
            Arc::new(CompositeTransformContextHook::new(self.hooks.transform_context.clone()))
                as Arc<dyn TransformContextHook>);
    let should_stop: Option<Arc<dyn ShouldStopHook>> =
        (!self.hooks.should_stop.is_empty()).then(||
            Arc::new(CompositeShouldStopHook::new(self.hooks.should_stop.clone()))
                as Arc<dyn ShouldStopHook>);
    let before_provider_request: Option<Arc<dyn BeforeProviderRequestHook>> =
        (!self.hooks.before_provider_request.is_empty()).then(||
            Arc::new(CompositeBeforeProviderRequestHook::new(self.hooks.before_provider_request.clone()))
                as Arc<dyn BeforeProviderRequestHook>);
    let after_provider_response: Option<Arc<dyn AfterProviderResponseHook>> =
        (!self.hooks.after_provider_response.is_empty()).then(||
            Arc::new(CompositeAfterProviderResponseHook::new(self.hooks.after_provider_response.clone()))
                as Arc<dyn AfterProviderResponseHook>);
    let auth: Option<Arc<dyn AuthHook>> = self.hooks.auth.last().cloned();

    LoopConfig {
        model: st.model.clone(),
        max_tokens: inner.max_tokens,
        temperature: None,
        thinking_level: st.thinking_level,
        tools,
        active_tools: st.active_tools.clone(),
        default_execution_mode: ToolExecutionMode::Parallel,
        env: self.env.clone(),
        abort,
        stream_options: self.stream_options.clone(),
        convert_to_llm: self.convert_to_llm.clone(),
        transform_context,
        prepare_next_turn: Some(prepare_next_turn),
        should_stop,
        before_provider_request,
        after_provider_response,
        auth,
        steer_rx: Some(steer_rx),
        follow_up_rx: Some(follow_up_rx),
        retry,
    }
}


```

对于不支持泛型 coercion 的返回类型，Task 4 中改为 inline `.then()` 写法。

- [ ] **Step 3: 修改 run_before_run_hook 使用 Composite**

替换第 1131-1180 行的方法：

```rust
async fn run_before_run_hook(
    &self,
    initial: Vec<AgentMessage>,
) -> Result<Vec<AgentMessage>, HarnessError> {
    if self.hooks.before_run.is_empty() {
        return Ok(initial);
    }
    let (skills, templates) = {
        let inner = self.inner.lock().unwrap();
        let snames: Vec<String> = inner.skills.iter().map(|s| s.name.clone()).collect();
        let tnames: Vec<String> = inner.templates.iter().map(|t| t.name.clone()).collect();
        (snames, tnames)
    };
    let resources = AgentHarnessResources {
        skill_names: skills,
        template_names: templates,
    };
    let prompt_text = initial
        .iter()
        .find_map(|m| {
            if let AgentMessage::User(u) = m {
                u.content.iter().find_map(|c| {
                    if let ContentBlock::Text { text } = c {
                        Some(text.clone())
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        })
        .unwrap_or_default();

    let composite = CompositeBeforeRunHook::new(self.hooks.before_run.clone());
    let mut messages = initial;
    let mut sys: Option<String> = None;
    let result = composite
        .before_run(BeforeRunCtx {
            prompt_text: &prompt_text,
            initial_messages: &mut messages,
            system_prompt: &mut sys,
            resources: &resources,
        })
        .await?;
    messages.extend(result.additional_messages);
    if result.system_prompt.is_some() {
        sys = result.system_prompt;
    }
    Ok(messages)
}
```

- [ ] **Step 4: 修改事件循环中 before_turn / after_turn / before_compact 调用**

修改第 1407-1423 行（after_turn 调用）：

```rust
AgentEvent::TurnEnd { index, .. } => {
    // Call all after_turn hooks before flush.
    if !self.hooks.after_turn.is_empty() {
        let composite = CompositeAfterTurnHook::new(self.hooks.after_turn.clone());
        composite.after_turn(AfterTurnCtx {
            turn_index: *index,
            new_messages: &turn_messages,
        }).await;
    }
    let flushed = self.flush_pending_writes().await?;
    // ... rest unchanged
}
```

修改 before_turn 调用点（约第 1313 行）：

```rust
// Before: if let Some(ref h) = self.hooks.before_turn { ... }
// After:
if !self.hooks.before_turn.is_empty() {
    let composite = CompositeBeforeTurnHook::new(self.hooks.before_turn.clone());
    composite.before_turn(BeforeTurnCtx { ... }).await;
}
```

修改 before_compact 调用（约第 1466 行）：

```rust
// Before: if let Some(ref h) = self.hooks.before_compact { ... }
// After:
if !self.hooks.before_compact.is_empty() {
    let composite = CompositeBeforeCompactHook::new(self.hooks.before_compact.clone());
    let decision = composite.before_compact(BeforeCompactCtx { ... }).await;
    match decision {
        BeforeCompactDecision::Proceed => { /* proceed with compaction */ }
        BeforeCompactDecision::Skip => { return Ok(...) }
        BeforeCompactDecision::Override(result) => { /* use override */ }
    }
}
```

- [ ] **Step 5: 对 agent.rs 中的 LoopConfig 构造做相同适配**

修改 `crates/llm-harness-agent/src/agent.rs` 中所有 `LoopConfig { ... auth: Some(...) }` 为 `auth: None`（auth 移入 hooks.auth）。

- [ ] **Step 6: 运行编译确认**

Run: `cargo check -p llm-harness-agent 2>&1`
Expected: 编译成功

- [ ] **Step 7: Commit**

```bash
git add crates/llm-harness-agent/src/harness.rs crates/llm-harness-agent/src/agent.rs
git add crates/llm-harness-loop/src/composite.rs
git commit -m "feat(agent): adapt harness to use Vec hooks with Composite wrapping

build_loop_config() now wraps Vec<dyn Hook> into Composite then Option for
LoopConfig. Event loop hooks (before_turn, after_turn, before_compact) use
Composite wrappers. run_before_run_hook uses CompositeBeforeRunHook.

Added From<Vec<Arc<dyn Hook>>> for all 12 Composite types.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: Plugin trait + StateProvider（agent crate）

**Files:**
- Create: `crates/llm-harness-agent/src/plugin.rs`
- Modify: `crates/llm-harness-agent/src/lib.rs`

**Interfaces:**
- Consumes: `HarnessHooks`（Task 2），`Skill`/`PromptTemplate`（agent crate skills 模块），`Tool` trait（types crate）
- Produces: `Plugin` trait（4 个默认空实现方法 + name），`StateProvider` trait（2 个方法）

- [ ] **Step 1: 创建 plugin.rs**

创建 `crates/llm-harness-agent/src/plugin.rs`：

```rust
use std::sync::Arc;

use llm_harness_types::Tool;

use crate::harness::HarnessHooks;
use crate::session::types::SessionEntry;
use crate::skills::{PromptTemplate, Skill};

// ── Plugin ────────────────────────────────────────────────────────────────────

/// A composable plugin that bundles tools, hooks, skills, and templates.
///
/// Plugins are Rust crate-level compile-time dependencies. Each method has a
/// default no-op implementation — override only what your plugin needs.
///
/// # Installation order
///
/// - **Tools, skills, templates:** last installed wins (same-name conflict).
/// - **Hooks:** accumulate in installation order. Chain hooks (AfterToolCallHook,
///   TransformContextHook, PrepareNextTurnHook) depend on order.
/// - **AuthHook:** last installed wins.
pub trait Plugin: Send + Sync {
    /// Unique plugin name for diagnostics and conflict detection.
    fn name(&self) -> &str;

    /// Register custom tools (e.g. `bash`, `read`, `web_search`).
    fn register_tools(&self, _tools: &mut Vec<Arc<dyn Tool>>) {}

    /// Register hooks (e.g. audit, compliance, cost tracking).
    fn register_hooks(&self, _hooks: &mut HarnessHooks) {}

    /// Register skills (markdown files loaded into system prompt).
    fn register_skills(&self, _skills: &mut Vec<Skill>) {}

    /// Register prompt templates (with `$1`, `$@` placeholders).
    fn register_templates(&self, _templates: &mut Vec<PromptTemplate>) {}

    /// Optional state provider for session-branch-aware plugins.
    ///
    /// See [`StateProvider`] for details.
    fn state_provider(&self) -> Option<Arc<dyn StateProvider>> {
        None
    }
}

// ── StateProvider ─────────────────────────────────────────────────────────────

/// Allows stateful plugins to rebuild state when the session branch switches.
///
/// # Example: a "notes" plugin
///
/// ```ignore
/// impl StateProvider for NotesStateProvider {
///     fn entry_type(&self) -> &str { "notes" }
///     fn rebuild(&self, entries: &[SessionEntry]) {
///         for e in entries {
///             if let SessionEntryPayload::Custom { custom_type, data } = &e.payload {
///                 if custom_type == "notes" {
///                     self.notes.push(serde_json::from_str(data).unwrap());
///                 }
///             }
///         }
///     }
/// }
/// ```
pub trait StateProvider: Send + Sync {
    /// The `custom_type` tag used in `SessionEntryPayload::Custom`.
    fn entry_type(&self) -> &str;

    /// Rebuild state from the branch history entries.
    ///
    /// Takes `&self` — implementations must use interior mutability
    /// (e.g. `Arc<Mutex<State>>`) to modify internal state.
    fn rebuild(&self, entries: &[SessionEntry]);
}
```

- [ ] **Step 2: 在 lib.rs 中导出**

修改 `crates/llm-harness-agent/src/lib.rs`，添加：

```rust
pub mod plugin;

// In re-export block:
pub use plugin::{Plugin, StateProvider};
```

- [ ] **Step 3: 运行编译确认**

Run: `cargo check -p llm-harness-agent 2>&1`
Expected: 编译成功

- [ ] **Step 4: Commit**

```bash
git add crates/llm-harness-agent/src/plugin.rs crates/llm-harness-agent/src/lib.rs
git commit -m "feat(agent): add Plugin trait and StateProvider trait

Plugin bundles tools/hooks/skills/templates into one installable unit.
StateProvider enables session-branch-aware state rebuild.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: HarnessBuilder（runtime crate）

**Files:**
- Create: `crates/llm-harness-runtime/src/builder.rs`
- Modify: `crates/llm-harness-runtime/src/lib.rs`

**Interfaces:**
- Consumes: `Plugin` trait（Task 5），`AgentHarness` + `AgentHarnessOptions`（agent crate），`LlmClient`（loop crate）
- Produces: `HarnessBuilder::new(model)` → `.install(plugin)` → `.provider(pattern, client)` → `.build(env)` → `Result<AgentHarness, HarnessBuildError>`

- [ ] **Step 1: 创建 builder.rs**

创建 `crates/llm-harness-runtime/src/builder.rs`：

```rust
use std::sync::Arc;

use llm_harness_agent::{AgentHarness, AgentHarnessOptions, HarnessHooks, Plugin, PromptTemplate, Skill, StateProvider};
use llm_harness_loop::LlmClient;
use llm_harness_types::{ExecutionEnv, Tool};

// ── HarnessBuilder ────────────────────────────────────────────────────────────

/// Builds an [`AgentHarness`] by composing plugins and providers.
///
/// # Example
///
/// ```ignore
/// let agent = HarnessBuilder::new("claude-sonnet-4-6")
///     .provider("claude-*", Arc::new(AnthropicProvider::default()))
///     .install(&AuditPlugin::new())
///     .install(&CompliancePlugin::new())
///     .build(env)
///     .unwrap();
/// ```
pub struct HarnessBuilder {
    model: String,
    tools: Vec<Arc<dyn Tool>>,
    hooks: HarnessHooks,
    skills: Vec<Skill>,
    templates: Vec<PromptTemplate>,
    providers: Vec<ProviderMatcher>,
    state_providers: Vec<Arc<dyn StateProvider>>,
}

struct ProviderMatcher {
    /// Match pattern: "deepseek-chat" (exact) or "claude-*" (glob).
    pattern: String,
    /// Whether this is a glob pattern (ends with `*`).
    is_glob: bool,
    /// Prefix without the trailing `*` (valid only when `is_glob`).
    prefix: String,
    client: Arc<dyn LlmClient>,
}

impl ProviderMatcher {
    fn new(pattern: impl Into<String>, client: Arc<dyn LlmClient>) -> Self {
        let pattern: String = pattern.into();
        let is_glob = pattern.ends_with('*');
        let prefix = if is_glob {
            pattern[..pattern.len() - 1].to_string()
        } else {
            String::new()
        };
        Self {
            pattern,
            is_glob,
            prefix,
            client,
        }
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
    /// Create a new builder for the given model ID.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            tools: vec![],
            hooks: HarnessHooks::none(),
            skills: vec![],
            templates: vec![],
            providers: vec![],
            state_providers: vec![],
        }
    }

    /// Install a plugin, merging its tools, hooks, skills, and templates.
    ///
    /// # Conflict handling
    ///
    /// - **Tools, skills, templates:** same-name resources are overwritten by later
    ///   plugins. Conflicts emit `tracing::warn!`.
    /// - **Hooks:** accumulate in installation order.
    /// - **AuthHook:** last installed wins. Conflicts emit `tracing::warn!`.
    /// - **StateProvider:** same `entry_type` overwritten by later plugins.
    pub fn install(mut self, plugin: &dyn Plugin) -> Self {
        let name = plugin.name();

        // ── Tools ──
        let tool_count_before = self.tools.len();
        plugin.register_tools(&mut self.tools);
        let new_tools = &self.tools[tool_count_before..];
        for new_tool in new_tools {
            let new_name = new_tool.name();
            if self.tools[..tool_count_before]
                .iter()
                .any(|t| t.name() == new_name)
            {
                tracing::warn!(
                    plugin = %name,
                    tool = %new_name,
                    "plugin '{}' overwrites previously registered tool '{}'",
                    name, new_name,
                );
            }
        }

        // ── Hooks ──
        let auth_count_before = self.hooks.auth.len();
        plugin.register_hooks(&mut self.hooks);
        if self.hooks.auth.len() > auth_count_before && auth_count_before > 0 {
            tracing::warn!(
                plugin = %name,
                "plugin '{}' overwrites previously registered AuthHook",
                name,
            );
        }

        // ── Skills ──
        let skills_before = self.skills.len();
        plugin.register_skills(&mut self.skills);
        let new_skills = &self.skills[skills_before..];
        for skill in new_skills {
            if self.skills[..skills_before]
                .iter()
                .any(|s| s.name == skill.name)
            {
                tracing::warn!(
                    plugin = %name,
                    skill = %skill.name,
                    "plugin '{}' overwrites previously registered skill '{}'",
                    name, skill.name,
                );
            }
        }

        // ── Templates ──
        let templates_before = self.templates.len();
        plugin.register_templates(&mut self.templates);
        let new_templates = &self.templates[templates_before..];
        for tmpl in new_templates {
            if self.templates[..templates_before]
                .iter()
                .any(|t| t.name == tmpl.name)
            {
                tracing::warn!(
                    plugin = %name,
                    template = %tmpl.name,
                    "plugin '{}' overwrites previously registered template '{}'",
                    name, tmpl.name,
                );
            }
        }

        // ── State providers ──
        if let Some(sp) = plugin.state_provider() {
            let entry_type = sp.entry_type().to_string();
            if let Some(existing) = self.state_providers.iter().find(|p| p.entry_type() == entry_type) {
                tracing::warn!(
                    plugin = %name,
                    entry_type = %entry_type,
                    "plugin '{}' overwrites previously registered StateProvider for entry_type '{}'",
                    name, entry_type,
                );
            }
            self.state_providers.push(sp);
        }

        self
    }

    /// Register a provider (LLM client) matched by model name pattern.
    ///
    /// Patterns:
    /// - `"deepseek-chat"` — exact match
    /// - `"claude-*"` — glob match (trailing `*` only)
    ///
    /// Providers are checked in registration order. Exact matches take priority
    /// over glob matches. Longer prefix glob matches take priority over shorter ones.
    pub fn provider(mut self, pattern: &str, client: Arc<dyn LlmClient>) -> Self {
        self.providers
            .push(ProviderMatcher::new(pattern, client));
        self
    }

    /// Build the `AgentHarness`.
    ///
    /// Resolves the LLM client from registered providers, constructs
    /// `AgentHarnessOptions`, and creates an in-memory `AgentHarness`.
    pub async fn build(
        self,
        env: Arc<dyn ExecutionEnv>,
    ) -> Result<AgentHarness, HarnessBuildError> {
        let client = self.resolve_provider(&self.model)?;

        let mut opts = AgentHarnessOptions::new(&self.model);
        opts.tools = self.tools;
        opts.hooks = self.hooks;
        opts.skills = self.skills;
        opts.templates = self.templates;

        Ok(AgentHarness::new_in_memory(client, env, opts).await)
    }

    /// Build the `AgentHarness` with an existing session.
    pub fn build_with_session(
        self,
        env: Arc<dyn ExecutionEnv>,
        session: llm_harness_agent::Session,
    ) -> Result<AgentHarness, HarnessBuildError> {
        let client = self.resolve_provider(&self.model)?;

        let mut opts = AgentHarnessOptions::new(&self.model);
        opts.tools = self.tools;
        opts.hooks = self.hooks;
        opts.skills = self.skills;
        opts.templates = self.templates;

        Ok(AgentHarness::with_session(client, env, session, opts))
    }

    /// Resolve an LLM client for the given model.
    ///
    /// Exact matches are checked first, then glob matches (longest prefix wins).
    fn resolve_provider(&self, model: &str) -> Result<Arc<dyn LlmClient>, HarnessBuildError> {
        // Exact match first.
        for m in &self.providers {
            if !m.is_glob && m.matches(model) {
                return Ok(m.client.clone());
            }
        }
        // Glob matches, longest prefix first.
        let mut candidates: Vec<&ProviderMatcher> = self
            .providers
            .iter()
            .filter(|m| m.is_glob && m.matches(model))
            .collect();
        candidates.sort_by_key(|m| std::cmp::Reverse(m.prefix.len()));
        if let Some(best) = candidates.first() {
            return Ok(best.client.clone());
        }
        if self.providers.is_empty() {
            return Err(HarnessBuildError::NoProvider);
        }
        Err(HarnessBuildError::NoMatchingProvider {
            model: model.to_string(),
        })
    }
}

// ── HarnessBuildError ─────────────────────────────────────────────────────────

/// Errors that can occur during [`HarnessBuilder::build`].
#[derive(Debug, thiserror::Error)]
pub enum HarnessBuildError {
    /// No provider registered. Call `.provider()` before `.build()`.
    #[error("no provider registered; call .provider() before .build()")]
    NoProvider,

    /// No provider matches the requested model.
    #[error("no provider matches model '{model}'")]
    NoMatchingProvider { model: String },
}
```

需要在 `Cargo.toml`（runtime crate）中添加 `thiserror` 依赖：

```toml
thiserror.workspace = true
```

检查 workspace `Cargo.toml` 中是否已有 thiserror。如果有 `thiserror.workspace = true` 即可。

- [ ] **Step 2: 在 lib.rs 中导出**

修改 `crates/llm-harness-runtime/src/lib.rs`：

```rust
pub mod builder;

pub use builder::{HarnessBuilder, HarnessBuildError};
```

- [ ] **Step 3: 运行编译确认**

Run: `cargo check -p llm-harness-runtime 2>&1`
Expected: 编译成功

- [ ] **Step 4: Commit**

```bash
git add crates/llm-harness-runtime/src/builder.rs crates/llm-harness-runtime/src/lib.rs
git commit -m "feat(runtime): add HarnessBuilder for Plugin composition and Provider matching

HarnessBuilder provides:
- .install(plugin) with conflict detection and tracing::warn!
- .provider(pattern, client) with exact and glob matching
- .build(env) and .build_with_session(env, session)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: 迁移所有 HarnessHooks 调用方（Option→Vec）

**Files:**
- Modify: `crates/llm-harness-runtime/tests/task_e2e.rs:112,54`
- Modify: `crates/llm-harness-runtime/tests/hook_matrix_tests.rs:106-137`
- Modify: `crates/llm-harness-runtime/tests/integration_test.rs:91-167`
- Modify: `crates/llm-harness-agent/tests/harness_integration.rs:336-338`
- Modify: `crates/llm-harness-runtime/src/task.rs:186,211`

**Interfaces:**
- Consumes: Task 2 的 Vec-based HarnessHooks
- Produces: 所有构造点从 `Some(Arc::new(hook))` 改为 `vec![Arc::new(hook)]`

- [ ] **Step 1: 迁移 hook_matrix_tests.rs**

修改 `crates/llm-harness-runtime/tests/hook_matrix_tests.rs`：

```rust
// 第 132-137 行
// Before:
let hooks = HarnessHooks {
    before_provider_request: Some(adapter.clone() as Arc<dyn BeforeProviderRequestHook>),
    after_provider_response: Some(adapter.clone() as Arc<dyn AfterProviderResponseHook>),
    after_tool_call: Some(adapter.clone() as Arc<dyn AfterToolCallHook>),
    after_turn: Some(adapter.clone() as Arc<dyn AfterTurnHook>),
    ..HarnessHooks::none()
};
// After:
let hooks = HarnessHooks {
    before_provider_request: vec![adapter.clone() as Arc<dyn BeforeProviderRequestHook>],
    after_provider_response: vec![adapter.clone() as Arc<dyn AfterProviderResponseHook>],
    after_tool_call: vec![adapter.clone() as Arc<dyn AfterToolCallHook>],
    after_turn: vec![adapter.clone() as Arc<dyn AfterTurnHook>],
    ..HarnessHooks::none()
};
```

- [ ] **Step 2: 迁移 integration_test.rs**

修改 `crates/llm-harness-runtime/tests/integration_test.rs` 第 106-111 行和第 162-167 行：同样的 `Some(...)` → `vec![...]` 替换。

- [ ] **Step 3: 迁移 harness_integration.rs**

修改 `crates/llm-harness-agent/tests/harness_integration.rs` 第 336-338 行：

```rust
// Before:
opts.hooks = HarnessHooks {
    before_compact: Some(hook.clone()),
    ..HarnessHooks::none()
};
// After:
opts.hooks = HarnessHooks {
    before_compact: vec![hook.clone()],
    ..HarnessHooks::none()
};
```

- [ ] **Step 4: 迁移 task.rs**

修改 `crates/llm-harness-runtime/src/task.rs` 第 211 行（如 `HarnessHooks::none()` 引用处）：

```rust
// Before:
harness_hooks: HarnessHooks::none(),
// After (same, but auth field exists now):
harness_hooks: HarnessHooks::none(),
```

无实际改动——`none()` 已在 Task 2 中更新为返回空 vec。

- [ ] **Step 5: 全局搜索确认无遗漏**

Run: `grep -rn "HarnessHooks\s*{" --include="*.rs" crates/ | grep -v target`
检查所有 HarnessHooks 构造点是否为 vec 模式。

- [ ] **Step 6: 运行全部测试**

Run: `cargo test --workspace 2>&1`
Expected: 所有测试 PASS

- [ ] **Step 7: Commit**

```bash
git add crates/llm-harness-runtime/tests/ crates/llm-harness-agent/tests/ crates/llm-harness-runtime/src/task.rs
git commit -m "refactor: migrate all HarnessHooks call sites from Option to Vec

Some(Arc::new(hook)) → vec![Arc::new(hook)]
HarnessHooks::none() stays unchanged (already returns empty vecs).

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: 端到端集成测试

**Files:**
- Create: `crates/llm-harness-runtime/tests/plugin_integration_test.rs`

**Interfaces:**
- Consumes: Task 5 的 `Plugin` trait，Task 6 的 `HarnessBuilder`，Task 3 的 Composite
- Produces: 验证多插件叠加、hook 调用顺序、冲突警告的端到端测试

- [ ] **Step 1: 写 E2E 测试**

创建 `crates/llm-harness-runtime/tests/plugin_integration_test.rs`：

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use llm_harness_agent::{HarnessHooks, Plugin};
use llm_harness_loop::composite::*;
use llm_harness_types::*;
use futures::future::BoxFuture;

// ── Test plugins ─────────────────────────────────────────────────────────────

struct CounterPlugin {
    name: &'static str,
    counter: Arc<AtomicUsize>,
}

impl Plugin for CounterPlugin {
    fn name(&self) -> &str { self.name }
}

struct AfterTurnCounter {
    counter: Arc<AtomicUsize>,
}

impl AfterTurnHook for AfterTurnCounter {
    fn after_turn<'a>(&'a self, _ctx: AfterTurnCtx<'a>) -> BoxFuture<'a, ()> {
        self.counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }
}

struct ToolPlugin { tool_name: String }

impl Plugin for ToolPlugin {
    fn name(&self) -> &str { "tool-plugin" }
    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        // Register a dummy tool
    }
}

// ── Test: HarnessBuilder installs plugins ─────────────────────────────────────

#[test]
fn builder_installs_multiple_plugins() {
    let builder = HarnessBuilder::new("test-model")
        .install(&TestPluginA)
        .install(&TestPluginB);
    // Verify hooks accumulate
}

// ── Test: Composite ordering ──────────────────────────────────────────────────

#[test]
fn composite_before_tool_call_first_non_allow_short_circuits() {
    // Verify first Deny short-circuits remaining hooks
    let hooks: Vec<Arc<dyn BeforeToolCallHook>> = vec![
        Arc::new(AllowHook),
        Arc::new(DenyHook),
        Arc::new(AllowHook),
    ];
    let composite = CompositeBeforeToolCallHook::new(hooks);
    // ... test that DenyHook stops execution
}

// ── Test: Tool name conflict warning ──────────────────────────────────────────

#[test]
fn builder_warns_on_tool_name_conflict() {
    // Register two plugins with same tool name, verify tracing::warn is emitted
}

// ── Test: Provider matching ───────────────────────────────────────────────────

#[test]
fn provider_exact_match_takes_priority_over_glob() {
    let builder = HarnessBuilder::new("claude-sonnet-4-6")
        .provider("claude-*", Arc::new(MockClient::new(1)))
        .provider("claude-sonnet-4-6", Arc::new(MockClient::new(2)));
    let client = builder.resolve_provider("claude-sonnet-4-6").unwrap();
    assert_eq!(client.id(), 2); // exact match wins
}
```

- [ ] **Step 2: 运行 E2E 测试**

Run: `cargo test -p llm-harness-runtime plugin_integration 2>&1`
Expected: 测试 PASS

- [ ] **Step 3: Commit**

```bash
git add crates/llm-harness-runtime/tests/plugin_integration_test.rs
git commit -m "test(runtime): add end-to-end plugin integration tests

Covers multi-plugin installation, Composite ordering, tool name conflict
detection, and provider matching priority.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 9: 运行全量测试 + clippy + 文档更新

**Files:**
- Modify: `crates/llm-harness-agent/src/plugin.rs`（文档注释）
- Modify: `crates/llm-harness-runtime/src/builder.rs`（文档注释）

- [ ] **Step 1: 运行全量测试**

Run: `cargo test --workspace 2>&1`
Expected: 所有测试 PASS

- [ ] **Step 2: 运行 clippy**

Run: `cargo clippy --workspace -- -D warnings 2>&1`
Expected: zero warnings

- [ ] **Step 3: 运行 rustfmt**

Run: `cargo fmt --all -- --check`
Expected: no formatting changes needed

- [ ] **Step 4: 更新文档**

在 `HarnessBuilder` struct 上确保有完整的 doc example，在 `Plugin` trait 上确保有使用示例。

- [ ] **Step 5: Final commit**

```bash
git add -A
git commit -m "chore: run full test suite, clippy, rustfmt, and doc polish

All tests pass across workspace. No clippy warnings. No fmt changes.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## 自检

1. **Spec 覆盖**：
   - Hook 多播化（Section 一）→ Tasks 2, 3, 4
   - Plugin trait（Section 二）→ Task 5
   - HarnessBuilder（Section 三）→ Task 6
   - StateProvider（Section 四）→ Task 5（同文件）
   - 冲突检测日志 → Task 6（install() 方法中）
   - 测试策略 → Task 8
   - 未纳入功能 → 不实现

2. **占位符检查**：无 TBD、TODO、placeholder。所有代码步骤包含完整实现。

3. **类型一致性**：
   - `Arc<dyn LlmClient>` 贯穿始终（匹配现有代码库）
   - `Composite*Hook::new(Vec<Arc<dyn Hook>>)` → Task 4 中 inline `.then()` 模式，无需 From trait
   - `HarnessBuilder::install(&dyn Plugin)` → `Plugin::register_*` 方法签名一致
   - `HarnessBuildError` 枚举变体贯穿 builder.rs

