# Eliminate Spawn-Specific Hooks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove three spawn-specific fields/traits from `HarnessInner`, replacing them with two generic hooks (`AfterRunHook` + `OnAbortHook`) and a `SpawnPlugin` that owns its own completion queue.

**Architecture:** The types crate gains `AfterRunHook` + `OnAbortHook` traits. The agent crate adds these to `HarnessHooks`, deletes `hooks.rs` (3 spawn traits), removes 3 fields from `HarnessInner`, rewrites `abort()`/`continue_run()`/`run()`/`run_loop()`. The runtime crate gets a new `SpawnPlugin` (4 hook implementations), `MessageBus` loses its event channel (replaced by `event_sink` callback), and `delivery.rs` loses `AsyncSpawnHook`/`IdleWatcher`.

**Tech Stack:** Rust, tokio, `tokio_util::sync::CancellationToken`, `tokio::sync::Notify`, `std::sync::Mutex`, `futures::future::BoxFuture`.

**Spec:** `docs/design/2026-08-21-eliminate-spawn-hooks-design.md`

## Global Constraints

- Agent crate (`llm-harness-agent`) MUST NOT depend on the runtime crate (`llm-harness-runtime`).
- `AfterRunHook::after_run` returns `BoxFuture<'_, ()>` (async, all-execute semantics).
- `OnAbortHook::on_abort` is synchronous (`fn on_abort(&self)`) — no async, no blocking.
- `SpawnAfterRun` only spawns the wait task via `tokio::spawn`, never awaits it — `run_loop` must return `Ok(())` and restore Idle so the wait task's `continue_run` call can pass the Idle guard.
- `aborted: AtomicBool` is set by `SpawnOnAbort` (in `abort()`), swapped by `SpawnAfterRun` (in `after_run`). `OnAbortHook` runs before `AfterRunHook` because `abort()` → cancel self token → `drive_loop` returns `Aborted` → `run_loop` cleanup → `drop(_guard)` → `after_run()`.
- All commits: run `cargo fmt` before committing.
- Branch: `fix/issue-125-eliminate-spawn-hooks` (already created).

---

## File Structure

### types crate
- `crates/llm-harness-types/src/hooks.rs` — add `AfterRunHook` + `OnAbortHook` traits at end of file.

### loop crate
- `crates/llm-harness-loop/src/composite.rs` — add `CompositeAfterRunHook` at end of file.
- `crates/llm-harness-loop/src/lib.rs` — add `CompositeAfterRunHook` to re-export list.

### agent crate
- `crates/llm-harness-agent/src/harness/state.rs` — add 2 fields to `HarnessHooks` + `none()`; delete 3 fields from `HarnessInner`; delete `use super::{...}` import of 3 traits.
- `crates/llm-harness-agent/src/harness/hooks.rs` — **delete entire file**.
- `crates/llm-harness-agent/src/harness/mod.rs` — delete `mod hooks;` and `pub use hooks::{...}`.
- `crates/llm-harness-agent/src/harness/core.rs` — delete 3 setters; rewrite `abort()`, `continue_run()`, `run()`.
- `crates/llm-harness-agent/src/harness/loop_driver.rs` — rewrite `run_loop()`: delete `async_hook`/`watcher` clone + borrow/release/on_idle, add `after_run` call.
- `crates/llm-harness-agent/src/builder.rs` — add 2 lines to `hooks()` method; add `after_run_hook()` + `on_abort_hook()` convenience methods.
- `crates/llm-harness-agent/src/lib.rs` — delete `IdleWatcherHook`, `AsyncSpawnHookHandle`, `AbortCascadeHook` from re-exports.
- `crates/llm-harness-agent/src/harness/tests/lifecycle.rs` — rewrite abort/continue_run tests for new hooks.

### runtime crate
- `crates/llm-harness-runtime/src/spawn/plugin.rs` — **new file**: `SpawnPlugin` + 4 hook impls + `format_events()`.
- `crates/llm-harness-runtime/src/spawn/mod.rs` — add `pub mod plugin;`.
- `crates/llm-harness-runtime/src/spawn/delivery.rs` — delete `AsyncSpawnHook`, `IdleWatcher`, `async_spawn_pair`, `has_running_agents`; keep `AsyncSpawnHandle`, `format_success`, `format_failure`, `SubAgentMessageConverter`; delete unit tests for deleted types.
- `crates/llm-harness-runtime/src/spawn/message_bus.rs` — delete `event_tx`/`event_rx`/`EventRxGuard`/`take_event_rx`/`return_event_rx`/`try_resend_event`/`resend_event`/`message_bus_pair`/`AbortCascadeHook impl`; add `event_sink` field + `set_event_sink`; change `MessageBus::new()` signature; update `send()` + `track_with_oneshot()` + `track()`.
- `crates/llm-harness-runtime/src/workflow/engine/runner.rs` — rewrite `post_build_spawn_setup` to use `SpawnPlugin`.
- `crates/llm-harness-runtime/tests/abort_propagation.rs` — rewrite setup.
- `crates/llm-harness-runtime/tests/async_delivery_integration.rs` — rewrite setup.
- `crates/llm-harness-runtime/tests/spawn_communication_integration.rs` — rewrite setup.

### live-tests crate
- `crates/llm-harness-live-tests/examples/11_spawn_subagent.rs` — rewrite setup.
- `crates/llm-harness-live-tests/tests/runtime_layer/spawn.rs` — rewrite setup.

---

## Task 1: Add `AfterRunHook` + `OnAbortHook` traits to types crate

**Files:**
- Modify: `crates/llm-harness-types/src/hooks.rs` (append at end, before `#[cfg(test)]`)

**Interfaces:**
- Produces: `pub trait AfterRunHook: Send + Sync { fn after_run(&self) -> BoxFuture<'_, ()>; }`
- Produces: `pub trait OnAbortHook: Send + Sync { fn on_abort(&self); }`

- [ ] **Step 1: Add the two traits**

Insert before the `#[cfg(test)]` block (currently at line 384) in `crates/llm-harness-types/src/hooks.rs`:

```rust
// ── AfterRunHook ──────────────────────────────────────────────────────────────

/// run_loop 结束、harness 回到 Idle 后调用。
///
/// 可在此 spawn 等待 task（如等待子 agent 完成后调 `continue_run`）。
/// wait task 应捕获 `Weak<AgentHarness>` 而非 `Arc`，以打断引用环。
/// `BeforeRunHook` 负责取消 pending 的 wait task。
pub trait AfterRunHook: Send + Sync {
    /// 在 run_loop 结束后调用。
    fn after_run(&self) -> BoxFuture<'_, ()>;
}

// ── OnAbortHook ───────────────────────────────────────────────────────────────

/// harness.abort() 时调用。
///
/// 在 `AfterRunHook` 之前执行（`abort()` → cancel self token → `drive_loop`
/// 返回 → `run_loop` cleanup → `drop(guard)` → `after_run()`），让
/// `AfterRunHook` 能检查标志跳过 wait task。
/// 非阻塞：只置标志/cancel token，不 join sub-agent task。
/// 幂等：可重复调用。
pub trait OnAbortHook: Send + Sync {
    /// abort 时调用。
    fn on_abort(&self);
}
```

- [ ] **Step 2: Build the types crate**

Run: `cargo build -p llm-harness-types`
Expected: PASS (no errors — new traits don't break anything).

- [ ] **Step 3: Commit**

```bash
cargo fmt
git add crates/llm-harness-types/src/hooks.rs
git commit -m "feat(types): add AfterRunHook + OnAbortHook traits (issue #125)"
```

---

## Task 2: Add `CompositeAfterRunHook` to loop crate

**Files:**
- Modify: `crates/llm-harness-loop/src/composite.rs` (append at end, before `#[cfg(test)]`)
- Modify: `crates/llm-harness-loop/src/lib.rs` (add to re-export list at line 263-268)

**Interfaces:**
- Consumes: `AfterRunHook` from Task 1.
- Produces: `pub struct CompositeAfterRunHook { hooks: Vec<Arc<dyn AfterRunHook>> }` with `new()` + impl `AfterRunHook`.

- [ ] **Step 1: Add the composite struct**

Insert before the `#[cfg(test)]` block (line 492) in `crates/llm-harness-loop/src/composite.rs`:

```rust
/// Chains multiple `AfterRunHook` implementations (all-execute semantics).
///
/// All hooks execute in registration order (observing only, no return value).
pub struct CompositeAfterRunHook {
    hooks: Vec<Arc<dyn AfterRunHook>>,
}

impl CompositeAfterRunHook {
    /// Create a composite from a list of hooks.
    pub fn new(hooks: Vec<Arc<dyn AfterRunHook>>) -> Self {
        Self { hooks }
    }
}

impl AfterRunHook for CompositeAfterRunHook {
    fn after_run(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            for hook in &self.hooks {
                hook.after_run().await;
            }
        })
    }
}
```

Also add `AfterRunHook` to the import list at the top of `composite.rs` (line 4-12). Add `AfterRunHook` to the existing import from `llm_harness_types`:

```rust
use llm_harness_types::{
    AfterProviderResponseCtx, AfterProviderResponseHook, AfterRunHook, AfterToolCallCtx, AfterToolCallDecision,
    AfterToolCallHook, AfterTurnCtx, AfterTurnHook, AgentContext, AgentError, BeforeCompactCtx,
    BeforeCompactDecision, BeforeCompactHook, BeforeProviderRequestCtx, BeforeProviderRequestHook,
    BeforeRunCtx, BeforeRunHook, BeforeRunResult, BeforeToolCallCtx, BeforeToolCallDecision,
    BeforeToolCallHook, BeforeTurnCtx, BeforeTurnHook, FinalAnswerValidationCtx,
    FinalAnswerValidationError, FinalAnswerValidator, PrepareNextTurnCtx, PrepareNextTurnHook,
    ShouldStopCtx, ShouldStopHook, TransformContextCtx, TransformContextHook,
};
```

- [ ] **Step 2: Add to re-exports**

In `crates/llm-harness-loop/src/lib.rs`, add `CompositeAfterRunHook` to the `pub use composite::{...}` block (line 263-268):

```rust
pub use composite::{
    CompositeAfterProviderResponseHook, CompositeAfterRunHook, CompositeAfterToolCallHook, CompositeAfterTurnHook,
    CompositeBeforeCompactHook, CompositeBeforeProviderRequestHook, CompositeBeforeRunHook,
    CompositeBeforeToolCallHook, CompositeBeforeTurnHook, CompositeFinalAnswerValidator,
    CompositePrepareNextTurnHook, CompositeShouldStopHook, CompositeTransformContextHook,
};
```

- [ ] **Step 3: Build the loop crate**

Run: `cargo build -p llm-harness-loop`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
cargo fmt
git add crates/llm-harness-loop/src/composite.rs crates/llm-harness-loop/src/lib.rs
git commit -m "feat(loop): add CompositeAfterRunHook (issue #125)"
```

---

## Task 3: Extend `HarnessHooks` + `HarnessInner` in agent crate

This task adds the two new hook fields to `HarnessHooks` and removes the three spawn-specific fields from `HarnessInner`. It also deletes `hooks.rs` and updates `mod.rs` + `lib.rs` exports. The crate will NOT compile after this task until Task 4 fixes the call sites — that's expected; we commit at the end of Task 4.

**Files:**
- Modify: `crates/llm-harness-agent/src/harness/state.rs`
- Delete: `crates/llm-harness-agent/src/harness/hooks.rs`
- Modify: `crates/llm-harness-agent/src/harness/mod.rs`
- Modify: `crates/llm-harness-agent/src/lib.rs`

**Interfaces:**
- Consumes: `AfterRunHook` + `OnAbortHook` from Task 1.
- Produces: `HarnessHooks` with `pub after_run: Vec<Arc<dyn AfterRunHook>>` and `pub on_abort: Vec<Arc<dyn OnAbortHook>>`.
- Removes: `IdleWatcherHook`, `AsyncSpawnHookHandle`, `AbortCascadeHook` traits.

- [ ] **Step 1: Add new fields to `HarnessHooks`**

In `crates/llm-harness-agent/src/harness/state.rs`:

1. Delete line 9: `use super::{AbortCascadeHook, AsyncSpawnHookHandle, IdleWatcherHook};`

2. In the `HarnessHooks` struct (line 65-90), add after `before_compact`:
```rust
    /// Called after run_loop ends and harness returns to Idle.
    pub after_run: Vec<Arc<dyn AfterRunHook>>,
    /// Called when harness.abort() is invoked, before AfterRunHook.
    pub on_abort: Vec<Arc<dyn OnAbortHook>>,
```

3. In `HarnessHooks::none()` (line 94-109), add after `before_compact: vec![],`:
```rust
            after_run: vec![],
            on_abort: vec![],
```

- [ ] **Step 2: Delete spawn-specific fields from `HarnessInner`**

In the same file, delete lines 135-143 (the three fields + their doc comments):
```rust
    pub(super) idle_watcher: Option<Arc<dyn IdleWatcherHook>>,
    pub(super) async_spawn_hook: Option<Arc<dyn AsyncSpawnHookHandle>>,
    pub(super) abort_cascade_hook: Option<Arc<dyn AbortCascadeHook>>,
```
Plus their doc comments (lines 135-143).

- [ ] **Step 3: Delete `hooks.rs`**

Delete the entire file `crates/llm-harness-agent/src/harness/hooks.rs`.

- [ ] **Step 4: Update `mod.rs`**

In `crates/llm-harness-agent/src/harness/mod.rs`:

1. Delete line 7: `mod hooks;`
2. Delete line 19: `pub use hooks::{AbortCascadeHook, AsyncSpawnHookHandle, IdleWatcherHook};`

- [ ] **Step 5: Update `lib.rs` re-exports**

In `crates/llm-harness-agent/src/lib.rs`, line 44-48, change:
```rust
pub use harness::{
    AbortCascadeHook, AgentHarness, AgentHarnessEvent, AgentHarnessOptions, AsyncSpawnHookHandle,
    CompactionStats, FollowUpHandle, HarnessHooks, HarnessState, HarnessToolCallResult,
    IdleWatcherHook, UsageLedger,
};
```
to:
```rust
pub use harness::{
    AgentHarness, AgentHarnessEvent, AgentHarnessOptions,
    CompactionStats, FollowUpHandle, HarnessHooks, HarnessState, HarnessToolCallResult,
    UsageLedger,
};
```

- [ ] **Step 6: Verify build fails at expected locations**

Run: `cargo build -p llm-harness-agent 2>&1 | head -30`
Expected: FAIL — errors in `core.rs` (setters, `abort`, `continue_run`, `run` referencing deleted fields/types) and `loop_driver.rs` (`run_loop` referencing `async_hook`/`watcher`). This confirms the deletion is correct; Task 4 fixes these.

Do NOT commit yet — proceed to Task 4.

---

## Task 4: Rewrite agent crate core methods (`abort`, `continue_run`, `run`, `run_loop`, setters)

**Files:**
- Modify: `crates/llm-harness-agent/src/harness/core.rs`
- Modify: `crates/llm-harness-agent/src/harness/loop_driver.rs`

**Interfaces:**
- Consumes: `HarnessHooks.after_run` + `HarnessHooks.on_abort` from Task 3, `CompositeAfterRunHook` from Task 2.
- Produces: `AgentHarness` with no spawn-specific setters, `abort()` using `OnAbortHook`, `continue_run()` calling `run_before_run_hook`, `run_loop()` calling `after_run` hooks.

- [ ] **Step 1: Delete the three setters from `core.rs`**

Delete lines 766-782 in `crates/llm-harness-agent/src/harness/core.rs`:
- `set_idle_watcher`
- `set_async_spawn_hook`
- `set_abort_cascade_hook`

- [ ] **Step 2: Rewrite `abort()` in `core.rs`**

Replace the current `abort()` method (lines 880-909) with:

```rust
    /// Cancel the current run and all in-flight sub-agents.
    ///
    /// `OnAbortHook`s run first (synchronously), setting flags and cancelling
    /// sub-agent tokens. Then the self abort token is cancelled, causing
    /// `drive_loop` to return and `run_loop` to clean up. `AfterRunHook`
    /// (called after `run_loop` cleanup) checks the abort flag to skip
    /// spawning a wait task.
    pub fn abort(&self) {
        for hook in &self.hooks.on_abort {
            hook.on_abort();
        }
        let mut inner = self.inner.lock().unwrap();
        if let Some(token) = inner.current_abort.take() {
            token.cancel();
        }
    }
```

- [ ] **Step 3: Rewrite `continue_run()` in `core.rs`**

Replace the current `continue_run()` method (lines 792-820) with:

```rust
    /// Drive another agent loop turn without injecting initial messages.
    /// Called by spawn plugin's wait task after a sub-agent completes.
    /// `BeforeRunHook` drains the completion queue and returns
    /// `additional_messages`, so the LLM sees sub-agent results on its
    /// first turn (no wasted LLM call).
    pub async fn continue_run(&self) -> Result<(), HarnessError> {
        {
            let inner = self.inner.lock().unwrap();
            if inner.state.phase != HarnessPhase::Idle {
                return Err(HarnessError::NotIdle(inner.state.phase));
            }
        }
        let run = Arc::new(RunContext::new(RunRequest::new(vec![])));
        let initial = run.initial_messages().to_vec();
        let initial = self.run_before_run_hook(&run, initial).await?;
        self.run_loop(run, initial).await
    }
```

- [ ] **Step 4: Rewrite `run()` in `core.rs`**

Replace the current `run()` method (lines 212-244) with:

```rust
    /// Start a new run from an explicit request and its typed extensions.
    pub async fn run(&self, request: RunRequest) -> Result<(), HarnessError> {
        let run = Arc::new(RunContext::new(request));
        let messages = run.initial_messages().to_vec();

        let queued = {
            let mut inner = self.inner.lock().unwrap();
            if inner.state.phase != HarnessPhase::Idle {
                return Err(HarnessError::NotIdle(inner.state.phase));
            }
            inner.state.error_message = None;
            std::mem::take(&mut inner.state.queued_next_turn)
        };

        self.try_auto_compact().await?;
        let initial: Vec<AgentMessage> = queued.into_iter().chain(messages).collect();
        let initial = self.run_before_run_hook(&run, initial).await?;
        self.run_loop(run, initial).await
    }
```

Key change: deleted `watcher` clone + `watcher.cancel()`. `BeforeRunHook` now handles wait task cancellation.

- [ ] **Step 5: Rewrite `run_loop()` in `loop_driver.rs`**

Replace the current `run_loop()` method (lines 252-319) with:

```rust
    pub(super) async fn run_loop(
        &self,
        run: Arc<RunContext>,
        initial: Vec<AgentMessage>,
    ) -> Result<(), HarnessError> {
        // 1. Setup: channels, abort token, system prompt, original tools.
        let (steer_rx, follow_up_rx, abort, system_prompt, original_tools) = {
            let mut inner = self.inner.lock().unwrap();
            inner.state.streaming_message = None;
            inner.state.pending_tool_calls.clear();
            let (steer_rx, follow_up_rx) = inner.reset_channels();
            let abort = inner.current_abort.take().unwrap_or_default();
            inner.current_abort = Some(abort.clone());
            let sp = inner.state.system_prompt.clone();
            let ot = inner.state.tools.clone();
            (steer_rx, follow_up_rx, abort, sp, ot)
        };
        let _guard = self.enter_phase(HarnessPhase::Turning)?;

        let result = self
            .drive_loop(run, initial, steer_rx, follow_up_rx, abort, system_prompt)
            .await;

        // Cleanup always runs, success or failure.
        {
            let mut inner = self.inner.lock().unwrap();
            inner.state.streaming_message = None;
            inner.state.pending_tool_calls.clear();
            inner.active_tool_names.clear();
            inner.current_abort = None;
            if let Err(ref e) = result {
                inner.state.error_message = Some(e.to_string());
                inner.state.pending_session_writes.clear();
                inner.state.tools_overwritten_mid_run = false;
            }
            if inner.state.tools_overwritten_mid_run {
                inner.state.tools_overwritten_mid_run = false;
            } else {
                inner.state.tools = original_tools;
            }
        }
        drop(_guard); // Restore Idle.

        // AfterRunHook: replaces watcher.on_idle().
        if !self.hooks.after_run.is_empty() {
            let composite = Arc::new(CompositeAfterRunHook::new(self.hooks.after_run.clone()));
            composite.after_run().await;
        }

        result
    }
```

Key changes: deleted `async_hook` clone + `borrow_completion_rx`/`release_completion_rx`, deleted `watcher` clone + `watcher.on_idle()`, added `CompositeAfterRunHook` call after `drop(_guard)`.

Also add the import at the top of `loop_driver.rs` (check if `CompositeAfterRunHook` is already imported; if not, add it). The file already imports from `llm_harness_loop` — add `CompositeAfterRunHook` to that import. Also need `AfterRunHook` — but since we're using `CompositeAfterRunHook` which implements `AfterRunHook`, and `AfterRunHook` is re-exported via `llm_harness_types::*`, the `use llm_harness_types::*;` import already covers it.

Check the existing imports in `loop_driver.rs` for `Composite*` types and add `CompositeAfterRunHook` there.

- [ ] **Step 6: Build the agent crate**

Run: `cargo build -p llm-harness-agent 2>&1 | head -50`
Expected: FAIL — errors in `tests/lifecycle.rs` (references to `AbortCascadeHook`, `IdleWatcherHook`, `set_abort_cascade_hook`, `set_idle_watcher`). These are fixed in Task 5. Core code should compile.

- [ ] **Step 7: Commit (state + core + loop_driver together)**

```bash
cargo fmt
git add crates/llm-harness-agent/src/harness/state.rs \
  crates/llm-harness-agent/src/harness/hooks.rs \
  crates/llm-harness-agent/src/harness/mod.rs \
  crates/llm-harness-agent/src/lib.rs \
  crates/llm-harness-agent/src/harness/core.rs \
  crates/llm-harness-agent/src/harness/loop_driver.rs
git commit -m "refactor(agent): replace spawn hooks with AfterRunHook/OnAbortHook (issue #125)

- Delete hooks.rs (IdleWatcherHook, AsyncSpawnHookHandle, AbortCascadeHook)
- Remove 3 fields from HarnessInner, add after_run/on_abort to HarnessHooks
- Rewrite abort() to use OnAbortHook, continue_run() to call before_run_hook
- Rewrite run_loop() to call AfterRunHook instead of watcher.on_idle()
- Delete set_idle_watcher/set_async_spawn_hook/set_abort_cascade_hook"
```

---

## Task 5: Rewrite lifecycle unit tests

**Files:**
- Modify: `crates/llm-harness-agent/src/harness/tests/lifecycle.rs`

**Interfaces:**
- Consumes: `OnAbortHook`, `AfterRunHook`, `BeforeRunHook` traits from types crate; `HarnessHooks` from Task 3.

- [ ] **Step 1: Replace `abort_invokes_cascade_hook` with `abort_invokes_on_abort_hook`**

Replace the test at lines 161-179 with:

```rust
#[tokio::test]
async fn abort_invokes_on_abort_hook() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingHook(Arc<AtomicUsize>);
    impl OnAbortHook for CountingHook {
        fn on_abort(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let counter = Arc::new(AtomicUsize::new(0));
    let client = Arc::new(MockLlmClient::new(vec![]));
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.hooks = HarnessHooks {
        on_abort: vec![Arc::new(CountingHook(counter.clone()))],
        ..HarnessHooks::none()
    };
    let h = AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, Arc::new(NoOpEnv), opts).await;
    h.abort();
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "abort should call on_abort hook once"
    );
}
```

- [ ] **Step 2: Replace `abort_invokes_cascade_hook_twice_after_reabort` with `abort_invokes_on_abort_hook_twice_after_reabort`**

Replace lines 180-213 with:

```rust
#[tokio::test]
async fn abort_invokes_on_abort_hook_twice_after_reabort() {
    // 回归测试：on_abort hooks 是生命周期持久的（Vec<Arc<dyn OnAbortHook>>
    // 遍历不 consume），re-prompt 后再次 abort() 仍触发。
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingHook(Arc<AtomicUsize>);
    impl OnAbortHook for CountingHook {
        fn on_abort(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let counter = Arc::new(AtomicUsize::new(0));
    let client = Arc::new(MockLlmClient::new(vec![]));
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.hooks = HarnessHooks {
        on_abort: vec![Arc::new(CountingHook(counter.clone()))],
        ..HarnessHooks::none()
    };
    let h = AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, Arc::new(NoOpEnv), opts).await;
    // 第一次 abort
    let token = CancellationToken::new();
    h.inner.lock().unwrap().current_abort = Some(token.clone());
    h.abort();
    assert_eq!(counter.load(Ordering::SeqCst), 1, "first abort");
    // re-prompt 后再次 abort
    let token2 = CancellationToken::new();
    h.inner.lock().unwrap().current_abort = Some(token2.clone());
    h.abort();
    assert_eq!(counter.load(Ordering::SeqCst), 2, "second abort");
}
```

- [ ] **Step 3: Replace `abort_cancels_idle_watcher` with `after_run_hook_called_after_run_loop`**

Replace lines 214-233 with:

```rust
#[tokio::test]
async fn after_run_hook_called_after_run_loop() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingHook(Arc<AtomicUsize>);
    impl AfterRunHook for CountingHook {
        fn after_run(&self) -> futures::future::BoxFuture<'_, ()> {
            let c = self.0.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
            })
        }
    }
    let counter = Arc::new(AtomicUsize::new(0));
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("hello")]));
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.hooks = HarnessHooks {
        after_run: vec![Arc::new(CountingHook(counter.clone()))],
        ..HarnessHooks::none()
    };
    let h = AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, Arc::new(NoOpEnv), opts).await;
    h.prompt("hi").await.unwrap();
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "after_run hook should be called once after run_loop"
    );
}
```

- [ ] **Step 4: Add `continue_run_calls_before_run_hook` test**

Add after the `continue_run_drives_loop_with_empty_initial` test:

```rust
#[tokio::test]
async fn continue_run_calls_before_run_hook() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingHook(Arc<AtomicUsize>);
    impl BeforeRunHook for CountingHook {
        fn before_run<'a>(
            &'a self,
            _ctx: llm_harness_types::BeforeRunCtx<'a>,
        ) -> futures::future::BoxFuture<'a, Result<llm_harness_types::BeforeRunResult, llm_harness_types::AgentError>> {
            let c = self.0.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(llm_harness_types::BeforeRunResult {
                    additional_messages: vec![],
                    system_prompt: None,
                })
            })
        }
    }
    let counter = Arc::new(AtomicUsize::new(0));
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("first"),
        MockResponse::text("second"),
    ]));
    let mut opts = AgentHarnessOptions::new("test-model");
    opts.hooks = HarnessHooks {
        before_run: vec![Arc::new(CountingHook(counter.clone()))],
        ..HarnessHooks::none()
    };
    let h = AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, Arc::new(NoOpEnv), opts).await;
    h.prompt("hello").await.unwrap(); // before_run called once
    h.continue_run().await.unwrap();   // before_run called again
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "before_run should be called for both prompt() and continue_run()"
    );
}
```

- [ ] **Step 5: Verify `abort_cancels_token`, `abort_emits_aborted_event`, `continue_run_returns_not_idle_when_turning`, `continue_run_drives_loop_with_empty_initial` need no changes**

These tests don't reference the deleted traits/setters. Verify by checking:
- `abort_cancels_token` (line 153-160): uses `h.abort()` + `inner.current_abort` — no spawn hooks. **No change needed.**
- `abort_emits_aborted_event` (line 234-308): uses `h.abort()` + BlockingTool — no spawn hooks. **No change needed.**
- `continue_run_returns_not_idle_when_turning` (line 310-319): uses `h.continue_run()` — no spawn hooks. **No change needed.**
- `continue_run_drives_loop_with_empty_initial` (line 321-345): uses `h.continue_run()` + `follow_up_message` — no spawn hooks. `make_harness` doesn't register `before_run_hook` or `readable_skills`, so `run_before_run_hook` early-returns. **No change needed.**

- [ ] **Step 6: Build + run agent crate tests**

Run: `cargo test -p llm-harness-agent --features test-utils -- lifecycle 2>&1`
Expected: PASS — all lifecycle tests pass.

- [ ] **Step 7: Commit**

```bash
cargo fmt
git add crates/llm-harness-agent/src/harness/tests/lifecycle.rs
git commit -m "test(agent): rewrite lifecycle tests for OnAbortHook/AfterRunHook (issue #125)"
```

---

## Task 6: Add builder convenience methods + `hooks()` wiring

**Files:**
- Modify: `crates/llm-harness-agent/src/builder.rs`

- [ ] **Step 1: Add two lines to `hooks()` method**

In `crates/llm-harness-agent/src/builder.rs`, after line 395 (`self.hooks.before_compact.extend(hooks.before_compact);`), add:

```rust
        self.hooks.after_run.extend(hooks.after_run);
        self.hooks.on_abort.extend(hooks.on_abort);
```

- [ ] **Step 2: Add convenience methods**

After the `before_run_hook` method (line 291-294), add:

```rust
    /// Register an `AfterRunHook` without wrapping it in a Plugin.
    ///
    /// Push semantics: appended to `hooks.after_run`.
    pub fn after_run_hook(mut self, hook: Arc<dyn AfterRunHook>) -> Self {
        self.hooks.after_run.push(hook);
        self
    }

    /// Register an `OnAbortHook` without wrapping it in a Plugin.
    ///
    /// Push semantics: appended to `hooks.on_abort`.
    pub fn on_abort_hook(mut self, hook: Arc<dyn OnAbortHook>) -> Self {
        self.hooks.on_abort.push(hook);
        self
    }
```

- [ ] **Step 3: Build agent crate**

Run: `cargo build -p llm-harness-agent`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
cargo fmt
git add crates/llm-harness-agent/src/builder.rs
git commit -m "feat(agent): add after_run_hook/on_abort_hook builder methods (issue #125)"
```

---

## Task 7: Rewrite `MessageBus` — replace event channel with `event_sink`

This is the largest single-file change. The `MessageBus` loses its `event_tx`/`event_rx` pair and gains an `event_sink` callback. `MessageBus::new()` changes signature.

**Files:**
- Modify: `crates/llm-harness-runtime/src/spawn/message_bus.rs`

**Interfaces:**
- Produces: `MessageBus::new() -> Arc<Self>` (was `-> (Arc<Self>, mpsc::Receiver<SubAgentEvent>)`)
- Produces: `MessageBus::set_event_sink(&self, sink: Arc<dyn Fn(SubAgentEvent) + Send + Sync>)`
- Removes: `EventRxGuard`, `take_event_rx`, `return_event_rx`, `try_resend_event`, `resend_event`, `message_bus_pair`, `AbortCascadeHook for MessageBus` impl.

- [ ] **Step 1: Replace struct fields**

In `crates/llm-harness-runtime/src/spawn/message_bus.rs`, replace the `MessageBus` struct (lines 124-135):

```rust
pub struct MessageBus {
    event_sink: Mutex<Option<Arc<dyn Fn(SubAgentEvent) + Send + Sync>>>,
    in_flight: Arc<AtomicUsize>,
    agents: Mutex<HashMap<String, AgentEntry>>,
    waiters: Mutex<HashMap<String, Vec<Waiter>>>,
    subagent_aborts: Mutex<HashMap<String, CancellationToken>>,
    idle_timeout_fired: AtomicBool,
}
```

- [ ] **Step 2: Replace constructor + delete `new_for_test` + `message_bus_pair`**

Replace `MessageBus::new()` (lines 138-151), `new_for_test()` (lines 156-169), and `message_bus_pair()` (lines 656-667) with:

```rust
impl MessageBus {
    /// Create a new MessageBus with no event sink. Main agent events are
    /// silently dropped until `set_event_sink` is called (by SpawnPlugin).
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            event_sink: Mutex::new(None),
            in_flight: Arc::new(AtomicUsize::new(0)),
            agents: Mutex::new(HashMap::new()),
            waiters: Mutex::new(HashMap::new()),
            subagent_aborts: Mutex::new(HashMap::new()),
            idle_timeout_fired: AtomicBool::new(false),
        })
    }
```

- [ ] **Step 3: Add `set_event_sink` method**

Add after the constructor:

```rust
    /// Set the event sink callback. Called by `SpawnPlugin::new` to route
    /// main-agent events (sub-agent completions, sub→main messages) into
    /// the plugin's internal queue.
    pub fn set_event_sink(&self, sink: Arc<dyn Fn(SubAgentEvent) + Send + Sync>) {
        *self.event_sink.lock() = Some(sink);
    }

    /// Push an event to the event sink (if set). Called by track task
    /// and `send()` for main-agent delivery.
    fn push_event(&self, event: SubAgentEvent) {
        let sink = self.event_sink.lock();
        if let Some(sink) = sink.as_ref() {
            sink(event);
        }
    }
```

- [ ] **Step 4: Update `send()` main-agent routing**

In `send()` (lines 222-309), replace the main-agent block (lines 293-301):

```rust
        // 3. Main agent: deliver via event sink.
        if is_main {
            self.push_event(SubAgentEvent::Message {
                agent_id: from.to_string(),
                body: body.to_string(),
            });
            return SendReceipt::Injected;
        }
```

- [ ] **Step 5: Update `track()` — replace `event_tx.send` with `push_event`**

In `track()` (the `#[cfg(test)]` version, lines 472-555), replace line 547:
```rust
            let _ = event_tx.send(event).await;
```
with:
```rust
            registry.push_event(event);
```
And delete the `event_tx` clone at line 475 (`let event_tx = self.event_tx.clone();`).

- [ ] **Step 6: Update `track_with_oneshot()` — replace `event_tx.send` with `push_event`**

In `track_with_oneshot()` (lines 560-650), replace line 642:
```rust
            let _ = event_tx.send(event).await;
```
with:
```rust
            registry.push_event(event);
```
And delete the `event_tx` clone at line 566 (`let event_tx = self.event_tx.clone();`).

- [ ] **Step 7: Delete `EventRxGuard`, `take_event_rx`, `return_event_rx`, `try_resend_event`, `resend_event`, `AbortCascadeHook impl`, `message_bus_pair`**

Delete:
- Lines 99-121 (`EventRxGuard` struct + impl + Drop)
- Lines 412-423 (`take_event_rx` + `return_event_rx`)
- Lines 435-445 (`try_resend_event` + `resend_event`)
- Lines 653-673 (`message_bus_pair` function + `AbortCascadeHook for MessageBus` impl)

Also delete `pub(crate) const EVENT_CHANNEL_CAPACITY` (line 16) — no longer needed.

Also remove `mpsc` from imports if no longer used. Check: `mpsc` is imported at line 11. It may still be used elsewhere — check for remaining `mpsc::` references. If unused, remove from imports.

- [ ] **Step 8: Update unit tests in message_bus.rs**

In the `#[cfg(test)] mod tests` block:

1. Replace `MessageBus::new()` calls that expect a tuple return:
   - Line 993: `let (bus, _rx) = MessageBus::new();` → `let bus = MessageBus::new();`
   - Line 700: `let (bus, _event_rx) = MessageBus::new_for_test();` → `let bus = MessageBus::new();` (also need to change `bus` from value to `Arc`)

2. Delete `message_bus_pair_stores_event_rx_internally` test (lines 974-988) — tests deleted functionality.

3. Replace `test_bus()` in delivery.rs tests (line 442-444): `message_bus_pair()` → `MessageBus::new()`.

4. Any test that uses `take_event_rx` / `EventRxGuard` / `resend_event` needs rewriting or deletion. Specifically in delivery.rs tests:
   - `event_rx_guard_borrows_and_returns_on_drop` — delete (tests deleted type)
   - `event_rx_guard_try_recv_gets_message` — delete
   - `async_spawn_pair_creates_hook_bound_to_bus` — delete
   - `track_completes_and_sends_event` — rewrite to use `event_sink` instead of `take_event_rx`
   - `after_turn_*` tests — these test `AsyncSpawnHook` which is deleted in Task 8; defer to Task 8
   - `idle_watcher_*` tests — these test `IdleWatcher` which is deleted in Task 8; defer to Task 8
   - `cancel_all_cancels_tracked_tokens` — uses `AbortCascadeHook` import + `cancel_all_subagents()`; change to `bus.cancel_all()` directly

For `track_completes_and_sends_event`, rewrite:

```rust
    #[tokio::test]
    async fn track_completes_and_sends_event() {
        let bus = MessageBus::new();
        let events = Arc::new(Mutex::new(vec![]));
        let events_clone = events.clone();
        bus.set_event_sink(Arc::new(move |event| {
            events_clone.lock().push(event);
        }));
        let handle = quick_handle("done");
        let async_handle = bus.track(handle, "agent-1".into());
        assert_eq!(async_handle, "agent-1");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let evs = events.lock();
        assert_eq!(evs.len(), 1, "one event should be pushed");
        match &evs[0] {
            SubAgentEvent::Done { agent_id, result } => {
                assert_eq!(agent_id, "agent-1");
                let r = result.as_ref().unwrap();
                assert_eq!(r.final_answer, "done");
            }
            _ => panic!("expected Done event"),
        }
    }
```

Add `use parking_lot::Mutex;` (or `std::sync::Mutex`) to the test imports if not present.

For `cancel_all_cancels_tracked_tokens`:
```rust
    #[tokio::test]
    async fn cancel_all_cancels_tracked_tokens() {
        let bus = MessageBus::new();
        let t1 = CancellationToken::new();
        let t2 = CancellationToken::new();
        bus.register_subagent_abort("a", t1.clone());
        bus.register_subagent_abort("b", t2.clone());
        bus.cancel_all();
        assert!(t1.is_cancelled(), "token a should be cancelled");
        assert!(t2.is_cancelled(), "token b should be cancelled");
    }
```

- [ ] **Step 9: Build runtime crate (expect failures — delivery.rs still references deleted types)**

Run: `cargo build -p llm-harness-runtime 2>&1 | head -50`
Expected: FAIL — errors in `delivery.rs` (references to `EventRxGuard`, `event_tx`, `IdleWatcher`, `AsyncSpawnHook`). These are fixed in Task 8. The `message_bus.rs` file itself should compile.

Do NOT commit yet — proceed to Task 8.

---

## Task 8: Create `SpawnPlugin` + rewrite `delivery.rs`

**Files:**
- Create: `crates/llm-harness-runtime/src/spawn/plugin.rs`
- Modify: `crates/llm-harness-runtime/src/spawn/mod.rs`
- Modify: `crates/llm-harness-runtime/src/spawn/delivery.rs`

**Interfaces:**
- Consumes: `AfterRunHook`, `OnAbortHook`, `BeforeRunHook`, `AfterTurnHook` from types crate; `Plugin` from agent crate; `MessageBus` from Task 7.
- Produces: `SpawnPlugin` with `new(bus: Arc<MessageBus>) -> Arc<Self>`, `set_harness_weak(&self, weak: Weak<AgentHarness>)`, `with_idle_timeout(self, timeout: Duration) -> Self`, impl `Plugin`.

- [ ] **Step 1: Create `plugin.rs`**

Create `crates/llm-harness-runtime/src/spawn/plugin.rs` with the following content:

```rust
//! SpawnPlugin: replaces AsyncSpawnHook + IdleWatcher + AbortCascadeHook.
//!
//! Holds its own completion queue (VecDeque + Notify), eliminating the shared
//! event channel. Routes sub-agent events through 4 generic hooks.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use llm_harness_agent::{AgentHarness, Plugin, HarnessHooks};
use llm_harness_types::{
    AfterRunHook, AfterTurnCtx, AfterTurnHook, AgentError, AgentMessage, BeforeRunCtx,
    BeforeRunHook, BeforeRunResult, CustomMessage, OnAbortHook,
};
use crate::spawn::delivery::{format_failure, format_success};
use crate::spawn::message_bus::{MessageBus, SubAgentEvent};
use crate::spawn::spawn_types::SubAgentResult;

pub struct SpawnPlugin {
    bus: Arc<MessageBus>,
    queue: Arc<Mutex<VecDeque<SubAgentEvent>>>,
    notify: Arc<Notify>,
    harness_weak: Arc<Mutex<Option<Weak<AgentHarness>>>>,
    wait_task_token: Arc<Mutex<Option<CancellationToken>>>,
    aborted: Arc<std::sync::atomic::AtomicBool>,
    idle_timeout: Duration,
}

impl SpawnPlugin {
    pub fn new(bus: Arc<MessageBus>) -> Arc<Self> {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let notify = Arc::new(Notify::new());
        let queue_clone = queue.clone();
        let notify_clone = notify.clone();
        bus.set_event_sink(Arc::new(move |event| {
            queue_clone.lock().unwrap().push_back(event);
            notify_clone.notify_one();
        }));
        Arc::new(Self {
            bus,
            queue,
            notify,
            harness_weak: Arc::new(Mutex::new(None)),
            wait_task_token: Arc::new(Mutex::new(None)),
            aborted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            idle_timeout: Duration::from_secs(60),
        })
    }

    pub fn with_idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    pub fn set_harness_weak(&self, weak: Weak<AgentHarness>) {
        *self.harness_weak.lock().unwrap() = Some(weak);
    }

    fn wake_harness(&self) {
        if let Some(weak) = self.harness_weak.lock().unwrap().as_ref() {
            if let Some(h) = weak.upgrade() {
                let h = h.clone();
                tokio::spawn(async move {
                    let _ = h.continue_run().await;
                });
            }
        }
    }
}

impl Plugin for SpawnPlugin {
    fn name(&self) -> &str {
        "spawn"
    }

    fn register_hooks(&self, hooks: &mut HarnessHooks) {
        hooks.before_run.push(Arc::new(SpawnBeforeRun {
            queue: self.queue.clone(),
            wait_task_token: self.wait_task_token.clone(),
            aborted: self.aborted.clone(),
        }));
        hooks.after_turn.push(Arc::new(SpawnAfterTurn {
            queue: self.queue.clone(),
            bus: self.bus.clone(),
            harness_weak: self.harness_weak.clone(),
        }));
        hooks.after_run.push(Arc::new(SpawnAfterRun {
            harness_weak: self.harness_weak.clone(),
            queue: self.queue.clone(),
            notify: self.notify.clone(),
            wait_task_token: self.wait_task_token.clone(),
            aborted: self.aborted.clone(),
            bus: self.bus.clone(),
            idle_timeout: self.idle_timeout,
        }));
        hooks.on_abort.push(Arc::new(SpawnOnAbort {
            bus: self.bus.clone(),
            aborted: self.aborted.clone(),
            wait_task_token: self.wait_task_token.clone(),
        }));
    }
}

// ── SpawnBeforeRun ───────────────────────────────────────────────────────────

struct SpawnBeforeRun {
    queue: Arc<Mutex<VecDeque<SubAgentEvent>>>,
    wait_task_token: Arc<Mutex<Option<CancellationToken>>>,
    aborted: Arc<std::sync::atomic::AtomicBool>,
}

impl BeforeRunHook for SpawnBeforeRun {
    fn before_run<'a>(
        &'a self,
        _ctx: BeforeRunCtx<'a>,
    ) -> BoxFuture<'a, Result<BeforeRunResult, AgentError>> {
        Box::pin(async move {
            // Clear stale aborted flag (from Idle-state abort that left it set).
            self.aborted.store(false, std::sync::atomic::Ordering::Relaxed);
            // Cancel pending wait task (if any).
            if let Some(token) = self.wait_task_token.lock().unwrap().take() {
                token.cancel();
            }
            let events: Vec<SubAgentEvent> = {
                let mut q = self.queue.lock().unwrap();
                q.drain(..).collect()
            };
            let additional = format_events(events);
            Ok(BeforeRunResult {
                additional_messages: additional,
                system_prompt: None,
            })
        })
    }
}

// ── SpawnAfterTurn ───────────────────────────────────────────────────────────

struct SpawnAfterTurn {
    queue: Arc<Mutex<VecDeque<SubAgentEvent>>>,
    bus: Arc<MessageBus>,
    harness_weak: Arc<Mutex<Option<Weak<AgentHarness>>>>,
}

impl AfterTurnHook for SpawnAfterTurn {
    fn after_turn<'a>(&'a self, _ctx: AfterTurnCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let msg = drain_and_format(&self.queue).or_else(|| status_hint(&self.bus));
            if let Some(msg) = msg {
                if let Some(weak) = self.harness_weak.lock().unwrap().as_ref() {
                    if let Some(h) = weak.upgrade() {
                        if let Err(e) = h.follow_up_message(msg) {
                            tracing::warn!("follow_up message dropped: {:?}", e);
                        }
                    }
                }
            }
        })
    }
}

// ── SpawnAfterRun ────────────────────────────────────────────────────────────

struct SpawnAfterRun {
    harness_weak: Arc<Mutex<Option<Weak<AgentHarness>>>>,
    queue: Arc<Mutex<VecDeque<SubAgentEvent>>>,
    notify: Arc<Notify>,
    wait_task_token: Arc<Mutex<Option<CancellationToken>>>,
    aborted: Arc<std::sync::atomic::AtomicBool>,
    bus: Arc<MessageBus>,
    idle_timeout: Duration,
}

impl AfterRunHook for SpawnAfterRun {
    fn after_run(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if self
                .aborted
                .swap(false, std::sync::atomic::Ordering::Relaxed)
            {
                return;
            }
            {
                let q = self.queue.lock().unwrap();
                if !q.is_empty() {
                    drop(q);
                    self.wake_harness();
                    return;
                }
            }
            if self.bus.in_flight_count() == 0 {
                return;
            }
            // Cancel any previous wait task before spawning a new one,
            // preventing orphaned tasks that compete for the same Notify.
            if let Some(old) = self.wait_task_token.lock().unwrap().take() {
                old.cancel();
            }
            let token = CancellationToken::new();
            *self.wait_task_token.lock().unwrap() = Some(token.clone());
            let harness_weak = self.harness_weak.clone();
            let notify = self.notify.clone();
            let bus = self.bus.clone();
            let timeout = self.idle_timeout;
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = notify.notified() => {
                            if bus.in_flight_count() == 0 {
                                break;
                            }
                            if let Some(weak) = harness_weak.lock().unwrap().as_ref() {
                                if let Some(h) = weak.upgrade() {
                                    let _ = h.continue_run().await;
                                }
                            }
                            if bus.in_flight_count() == 0 {
                                break;
                            }
                        }
                        _ = tokio::time::sleep(timeout) => {
                            if bus.in_flight_count() == 0 {
                                break;
                            }
                            bus.set_idle_timeout_fired();
                            if let Some(weak) = harness_weak.lock().unwrap().as_ref() {
                                if let Some(h) = weak.upgrade() {
                                    let _ = h.continue_run().await;
                                }
                            }
                            if bus.in_flight_count() == 0 {
                                break;
                            }
                        }
                        _ = token.cancelled() => {
                            break;
                        }
                    }
                }
            });
        })
    }
}

impl SpawnAfterRun {
    fn wake_harness(&self) {
        if let Some(weak) = self.harness_weak.lock().unwrap().as_ref() {
            if let Some(h) = weak.upgrade() {
                let h = h.clone();
                tokio::spawn(async move {
                    let _ = h.continue_run().await;
                });
            }
        }
    }
}

// ── SpawnOnAbort ─────────────────────────────────────────────────────────────

struct SpawnOnAbort {
    bus: Arc<MessageBus>,
    aborted: Arc<std::sync::atomic::AtomicBool>,
    wait_task_token: Arc<Mutex<Option<CancellationToken>>>,
}

impl OnAbortHook for SpawnOnAbort {
    fn on_abort(&self) {
        self.aborted
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(token) = self.wait_task_token.lock().unwrap().take() {
            token.cancel();
        }
        self.bus.cancel_all();
    }
}

// ── Formatting helpers ───────────────────────────────────────────────────────

fn format_events(events: Vec<SubAgentEvent>) -> Vec<AgentMessage> {
    if events.is_empty() {
        return vec![];
    }
    let mut results: Vec<String> = Vec::new();
    for event in events {
        match event {
            SubAgentEvent::Done { agent_id, result } => match result {
                Ok(r) => results.push(format_success(&agent_id, &r)),
                Err(AgentError::Aborted) => {}
                Err(e) => results.push(format_failure(&agent_id, &e)),
            },
            SubAgentEvent::Message { agent_id, body } => {
                results.push(format!("[from:{agent_id}] {body}"));
            }
        }
    }
    if results.is_empty() {
        return vec![];
    }
    let packed = results.join("\n\n---\n\n");
    vec![AgentMessage::Custom(CustomMessage {
        r#type: "sub_agent_message".into(),
        data: serde_json::json!({"messages": packed}),
        timestamp: chrono::Utc::now(),
    })]
}

fn drain_and_format(queue: &Arc<Mutex<VecDeque<SubAgentEvent>>>) -> Option<AgentMessage> {
    let events: Vec<SubAgentEvent> = {
        let mut q = queue.lock().unwrap();
        q.drain(..).collect()
    };
    let messages = format_events(events);
    if messages.is_empty() {
        None
    } else {
        Some(messages.into_iter().next().unwrap())
    }
}

fn status_hint(bus: &Arc<MessageBus>) -> Option<AgentMessage> {
    if !bus.consume_idle_timeout_fired() {
        return None;
    }
    let in_flight = bus.in_flight_count();
    if in_flight == 0 {
        return None;
    }
    let snapshots = bus.query_status(None);
    let running: Vec<&str> = snapshots
        .iter()
        .filter(|s| matches!(s.status, crate::spawn::message_bus::AgentStatus::Running))
        .map(|s| s.agent_id.as_str())
        .collect();
    if running.is_empty() {
        return None;
    }
    let hint = format!(
        "{n} sub-agent(s) still running: {ids}. \
         Use query_subagent to check status or await_subagent_reply to wait for results.",
        n = running.len(),
        ids = running.join(", ")
    );
    Some(AgentMessage::Custom(CustomMessage {
        r#type: "sub_agent_message".into(),
        data: serde_json::json!({"messages": hint}),
        timestamp: chrono::Utc::now(),
    }))
}
```

Note: `format_success` and `format_failure` are currently private functions in `delivery.rs`. They need to be made `pub(crate)` so `plugin.rs` can use them. This is done in Step 3 below.

- [ ] **Step 2: Add `pub mod plugin;` to `mod.rs`**

In `crates/llm-harness-runtime/src/spawn/mod.rs`, add:

```rust
pub mod plugin;
```

- [ ] **Step 3: Make `format_success`/`format_failure` pub(crate) in `delivery.rs`**

In `crates/llm-harness-runtime/src/spawn/delivery.rs`, change the visibility of:
- `fn format_success` (line 106) → `pub(crate) fn format_success`
- `fn format_failure` (line 117) → `pub(crate) fn format_failure`

- [ ] **Step 4: Delete `AsyncSpawnHook`, `IdleWatcher`, `async_spawn_pair`, `has_running_agents` from `delivery.rs`**

Delete:
- Lines 47-53: `async_spawn_pair()` function
- Lines 55-216: `AsyncSpawnHook` struct + all impls (inherent + `AfterTurnHook` + `AsyncSpawnHookHandle`)
- Lines 218-383: `IdleWatcher` struct + all impls (`IdleWatcherHook`)
- Lines 277-282: `has_running_agents()` function
- Lines 385-405: `AsyncSpawnHookHandle for AsyncSpawnHook` impl block

Keep:
- Lines 1-45: `AsyncSpawnHandle` struct + impl (the `new`/`agent_id`/`abort`/`abort_token` methods)
- Lines 105-123: `format_success()` + `format_failure()` (now `pub(crate)`)
- Lines 407-432: `SubAgentMessageConverter`
- Lines 434-788: `#[cfg(test)] mod tests` — but most tests reference deleted types. Delete tests that reference `AsyncSpawnHook`, `IdleWatcher`, `async_spawn_pair`, `message_bus_pair`, `take_event_rx`, `resend_event`, `EventRxGuard`, `AbortCascadeHook`. Keep `async_spawn_handle_abort_cancels_sub_agent`, `in_flight_count_increments_then_decrements`, `track_completes_and_sends_event` (updated in Task 7), `cancel_all_cancels_tracked_tokens` (updated in Task 7).

Actually, the tests are in `delivery.rs` not `message_bus.rs`. Let me re-check... The tests at lines 434-788 are in `delivery.rs`. They reference `message_bus_pair()`, `async_spawn_pair()`, `IdleWatcher`, etc. Delete all tests that reference deleted types. Keep `async_spawn_handle_abort_cancels_sub_agent` (line 530-545) and `in_flight_count_increments_then_decrements` (line 480-498) — these only use `MessageBus` + `track`.

Also update imports at top of `delivery.rs`: remove `AsyncSpawnHookHandle`, `FollowUpHandle`, `IdleWatcherHook` from the `use llm_harness_agent::` line. Remove `AfterTurnCtx`, `AfterTurnHook` if no longer used (they're used by `AsyncSpawnHook` which is deleted). Remove `EventRxGuard` from the `use crate::spawn::message_bus::` line. Keep `AgentStatus` if used by `has_running_agents` — but `has_running_agents` is deleted, so check if `AgentStatus` is used elsewhere.

The final `delivery.rs` imports should be:
```rust
use std::sync::Arc;
use llm_harness_types::{AgentError, AgentMessage, CustomMessage};
use tokio_util::sync::CancellationToken;
use crate::spawn::message_bus::{MessageBus, SubAgentEvent};
use crate::spawn::spawn_types::{SubAgentHandle, SubAgentResult};
```

Remove `futures::future::BoxFuture`, `std::future::Future`, `std::pin::Pin`, `std::time::Duration`, `parking_lot::Mutex` — all were used by deleted types. Check each before removing.

- [ ] **Step 5: Update `test_bus()` in delivery.rs tests**

Change:
```rust
    fn test_bus() -> Arc<MessageBus> {
        message_bus_pair()
    }
```
to:
```rust
    fn test_bus() -> Arc<MessageBus> {
        MessageBus::new()
    }
```

- [ ] **Step 6: Build runtime crate**

Run: `cargo build -p llm-harness-runtime 2>&1 | head -50`
Expected: FAIL — errors in `runner.rs` (references to `async_spawn_pair`, `IdleWatcher`, `message_bus_pair`, `set_*` methods). Fixed in Task 9. Also errors in integration tests. The `delivery.rs` + `plugin.rs` + `message_bus.rs` should compile.

- [ ] **Step 7: Commit (message_bus + plugin + delivery together)**

```bash
cargo fmt
git add crates/llm-harness-runtime/src/spawn/message_bus.rs \
  crates/llm-harness-runtime/src/spawn/plugin.rs \
  crates/llm-harness-runtime/src/spawn/mod.rs \
  crates/llm-harness-runtime/src/spawn/delivery.rs
git commit -m "refactor(runtime): SpawnPlugin + event_sink replaces event channel (issue #125)

- New plugin.rs: SpawnPlugin + SpawnBeforeRun/AfterTurn/AfterRun/OnAbort
- MessageBus: delete event_tx/event_rx/EventRxGuard, add event_sink callback
- delivery.rs: delete AsyncSpawnHook/IdleWatcher/async_spawn_pair, keep
  AsyncSpawnHandle/format_success/format_failure/SubAgentMessageConverter"
```

---

## Task 9: Rewrite `runner.rs` spawn setup

**Files:**
- Modify: `crates/llm-harness-runtime/src/workflow/engine/runner.rs`

- [ ] **Step 1: Rewrite `post_build_spawn_setup`**

In `crates/llm-harness-runtime/src/workflow/engine/runner.rs`, replace lines 1229-1283 (the `post_build_spawn_setup` type + if block):

```rust
        let post_build_spawn_setup: Option<(
            Arc<crate::spawn::plugin::SpawnPlugin>,
            Arc<crate::spawn::message_bus::MessageBus>,
        )> = if allowed_tools.iter().any(|t| t == SPAWN_AGENT_TOOL_NAME) {
            let bus = crate::spawn::message_bus::MessageBus::new();
            let plugin = crate::spawn::plugin::SpawnPlugin::new(bus.clone());
            let spawner = Arc::new(self.build_step_spawner(bus.clone(), step.id(), attempt_seq));
            bus.register(
                crate::spawn::message_bus::MAIN_AGENT_ID,
                crate::spawn::message_bus::AgentEntry {
                    status: crate::spawn::message_bus::AgentStatus::Running,
                    harness: None,
                    is_main: true,
                    role: None,
                    description: None,
                },
            );
            injected_tool_names.insert(SPAWN_AGENT_TOOL_NAME.to_string());
            injected_tool_names.insert(llm_harness_types::MESSAGE_SUBAGENT_TOOL_NAME.to_string());
            injected_tool_names
                .insert(llm_harness_types::AWAIT_SUBAGENT_REPLY_TOOL_NAME.to_string());
            injected_tool_names.insert(llm_harness_types::ABORT_SUBAGENT_TOOL_NAME.to_string());
            builder = builder.tool(Arc::new(crate::spawn::tools::SpawnAgentTool::new(spawner)));
            builder = builder.tool(Arc::new(crate::spawn::tools::MessageSubagentTool::new(
                bus.clone(),
                crate::spawn::message_bus::MAIN_AGENT_ID,
            )));
            builder = builder.tool(Arc::new(crate::spawn::tools::AwaitSubagentReplyTool::new(
                bus.clone(),
                crate::spawn::message_bus::MAIN_AGENT_ID,
            )));
            builder = builder.tool(Arc::new(crate::spawn::tools::QuerySubagentTool::new(
                bus.clone(),
            )));
            builder = builder.tool(Arc::new(crate::spawn::tools::AbortSubagentTool::new(
                bus.clone(),
            )));
            builder = builder.convert_to_llm(Some(Arc::new(
                llm_harness_loop::convert::DefaultConvertToLlm::new().with_custom_converter(
                    Arc::new(crate::spawn::delivery::SubAgentMessageConverter),
                ),
            )));
            builder = builder.install(plugin.as_ref());
            Some((plugin, bus))
        } else {
            None
        };
```

Key changes:
- `message_bus_pair()` → `MessageBus::new()`
- `async_spawn_pair(bus)` → `SpawnPlugin::new(bus)`
- `builder.install(plugin.as_ref())` registers all 4 hooks via `Plugin::register_hooks`
- Removed the `after_turn.push(async_hook)` from hook_state (line 1314-1316) — hooks are now registered via `install`

- [ ] **Step 2: Delete the `after_turn.push(async_hook)` from hook_state**

Delete lines 1314-1316:
```rust
        if let Some((ref async_hook, _)) = post_build_spawn_setup {
            hook_state.after_turn.push(async_hook.clone());
        }
```

- [ ] **Step 3: Rewrite post-build wiring (lines 1340-1355)**

Replace:
```rust
        if let Some((async_hook, bus)) = post_build_spawn_setup {
            let watcher = crate::spawn::delivery::IdleWatcher::new(bus.clone(), {
                let h = Arc::downgrade(&harness);
                Arc::new(move || {
                    let h = h.clone();
                    Box::pin(async move {
                        if let Some(h) = h.upgrade() {
                            let _ = h.continue_run().await;
                        }
                    })
                })
            });
            harness.set_idle_watcher(Arc::new(watcher));
            harness.set_async_spawn_hook(async_hook);
            harness.set_abort_cascade_hook(bus);
        }
```
with:
```rust
        if let Some((plugin, _bus)) = post_build_spawn_setup {
            plugin.set_harness_weak(Arc::downgrade(&harness));
        }
```

- [ ] **Step 3b: Make `StructuredPromptHook` idempotent**

`continue_run` now calls `before_run_hook`, which means `StructuredPromptHook` (registered as a `BeforeRunHook` in runner.rs:1321) will be called on every `continue_run`. Without idempotency, the structured output segment is appended to `system_prompt` on every sub-agent completion wake, causing unbounded growth.

In `crates/llm-harness-runtime/src/workflow/engine/runner.rs`, replace the `StructuredPromptHook::before_run` impl (lines 85-102):

```rust
impl llm_harness_types::BeforeRunHook for StructuredPromptHook {
    fn before_run<'a>(
        &'a self,
        ctx: llm_harness_types::BeforeRunCtx<'a>,
    ) -> futures::future::BoxFuture<
        'a,
        std::result::Result<llm_harness_types::BeforeRunResult, llm_harness_types::AgentError>,
    > {
        Box::pin(async move {
            let existing = ctx.system_prompt.take().unwrap_or_default();
            // Idempotent: only append if segment is not already present.
            // This prevents unbounded growth when continue_run calls
            // before_run_hook repeatedly (each sub-agent completion wake).
            if !existing.contains(self.segment) {
                *ctx.system_prompt = Some(format!("{existing}{seg}", seg = self.segment));
            } else {
                *ctx.system_prompt = Some(existing);
            }
            Ok(llm_harness_types::BeforeRunResult {
                additional_messages: vec![],
                system_prompt: None,
            })
        })
    }
}
```

- [ ] **Step 4: Build runtime crate**

Run: `cargo build -p llm-harness-runtime 2>&1 | head -50`
Expected: FAIL — errors only in integration tests (`abort_propagation.rs`, `async_delivery_integration.rs`, `spawn_communication_integration.rs`). The runtime library itself should compile.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add crates/llm-harness-runtime/src/workflow/engine/runner.rs
git commit -m "refactor(runtime): runner uses SpawnPlugin + set_harness_weak (issue #125)"
```

---

## Task 10: Rewrite integration tests

**Files:**
- Modify: `crates/llm-harness-runtime/tests/abort_propagation.rs`
- Modify: `crates/llm-harness-runtime/tests/async_delivery_integration.rs`
- Modify: `crates/llm-harness-runtime/tests/spawn_communication_integration.rs`

All three files share the same pattern: replace `message_bus_pair()` + `async_spawn_pair(bus)` + 3 setters + `IdleWatcher::new(...)` with `MessageBus::new()` + `SpawnPlugin::new(bus)` + `builder.install(plugin)` + `plugin.set_harness_weak()`.

- [ ] **Step 1: Rewrite `abort_propagation.rs`**

The pattern appears 3 times (one per test function). For each occurrence:

**Before** (lines ~151-195, ~289-330, ~449-490):
```rust
    let registry = message_bus_pair();
    let async_hook = async_spawn_pair(registry.clone());
    // ... spawner setup ...
    let (harness, sandbox_tmp) = make_harness_with(...);
    let harness = Arc::new(harness);
    let watcher = IdleWatcher::new(registry.clone(), { ... });
    harness.set_idle_watcher(Arc::new(watcher));
    harness.set_async_spawn_hook(async_hook.clone());
    harness.set_abort_cascade_hook(registry.clone());
```

**After**:
```rust
    let registry = MessageBus::new();
    let spawn_plugin = SpawnPlugin::new(registry.clone());
    // ... spawner setup (unchanged) ...
    let (harness, sandbox_tmp) = make_harness_with(entry, |b| {
        b.tool(Arc::new(SpawnAgentTool::new(spawner_clone.clone())))
            // ... other tools ...
            .install(spawn_plugin.as_ref())
            .convert_to_llm(Some(Arc::new(
                DefaultConvertToLlm::new()
                    .with_custom_converter(Arc::new(SubAgentMessageConverter)),
            )))
    }).await;
    let harness = Arc::new(harness);
    spawn_plugin.set_harness_weak(Arc::downgrade(&harness));
```

Wait — `abort_propagation.rs` uses `make_harness_with` differently. Let me re-check its harness builder. Looking at lines 150-195, it uses `HarnessBuilder::new(...)` directly, not `make_harness_with`. The builder already pushes the async_hook via `.after_turn_hook(async_hook)`. We need to replace that with `.install(spawn_plugin.as_ref())`.

Actually, looking more carefully at the test file, each test has its own inline builder. The pattern is:
1. `let registry = message_bus_pair();`
2. `let async_hook = async_spawn_pair(registry.clone());`
3. Builder with `.after_turn_hook(async_hook.clone())`
4. Post-build: `set_idle_watcher`, `set_async_spawn_hook`, `set_abort_cascade_hook`

Replace with:
1. `let registry = MessageBus::new();`
2. `let spawn_plugin = SpawnPlugin::new(registry.clone());`
3. Builder with `.install(spawn_plugin.as_ref())` (instead of `.after_turn_hook(...)`)
4. Post-build: `spawn_plugin.set_harness_weak(Arc::downgrade(&harness))`

Update imports:
- Remove: `use llm_harness_runtime::spawn::delivery::async_spawn_pair;`
- Add: `use llm_harness_runtime::spawn::plugin::SpawnPlugin;`
- Change: `use llm_harness_runtime::spawn::message_bus::message_bus_pair;` → `use llm_harness_runtime::spawn::message_bus::MessageBus;`
- Remove: `use llm_harness_runtime::spawn::delivery::IdleWatcher;` (if imported)
- Keep: `use llm_harness_runtime::spawn::delivery::SubAgentMessageConverter;` (still needed for converter)

- [ ] **Step 2: Rewrite `async_delivery_integration.rs`**

Same pattern as Step 1, applied to 3 test functions. The imports also need updating similarly.

- [ ] **Step 3: Rewrite `spawn_communication_integration.rs`**

This file has both MessageBus-level tests and harness-level tests.

For MessageBus-level tests: `message_bus_pair()` → `MessageBus::new()`, remove `take_event_rx` usage, use `set_event_sink` for assertions instead.

For harness-level tests: same pattern as Steps 1-2.

Update imports (lines 24-29):
- Remove: `message_bus_pair` from message_bus import
- Add: `SpawnPlugin` import
- Remove: `IdleWatcher`, `async_spawn_pair` from delivery import if present
- The import at line 24-29 currently is:
  ```rust
  use llm_harness_runtime::spawn::message_bus::{
      AgentEntry, AgentStatus, MessageBus, SendReceipt, SubAgentEvent, message_bus_pair,
  };
  ```
  Change to:
  ```rust
  use llm_harness_runtime::spawn::message_bus::{
      AgentEntry, AgentStatus, MessageBus, SendReceipt, SubAgentEvent,
  };
  use llm_harness_runtime::spawn::plugin::SpawnPlugin;
  ```

For the `IdleWatcherHook` import (line 21):
```rust
use llm_harness_agent::{AgentHarness, IdleWatcherHook, Plugin};
```
Change to:
```rust
use llm_harness_agent::{AgentHarness, Plugin};
```

- [ ] **Step 4: Build + run integration tests**

Run: `cargo test -p llm-harness-runtime 2>&1 | tail -30`
Expected: PASS — all integration tests pass.

If tests fail, check:
- `call_count == 2` instead of `>= 3`: `event_sink` not injected, `Notify` not firing, `continue_run` not called, or `Weak` upgrade failing
- `in_flight_count` not reaching 0: `OnAbortHook` not calling `cancel_all()`
- `call_count == 3` in `idle_watcher_abort`: `aborted` flag not checked in `AfterRunHook`, or `OnAbortHook` not running before `AfterRunHook`

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add crates/llm-harness-runtime/tests/abort_propagation.rs \
  crates/llm-harness-runtime/tests/async_delivery_integration.rs \
  crates/llm-harness-runtime/tests/spawn_communication_integration.rs
git commit -m "test(runtime): rewrite integration tests for SpawnPlugin (issue #125)"
```

---

## Task 11: Rewrite live test + example

**Files:**
- Modify: `crates/llm-harness-live-tests/examples/11_spawn_subagent.rs`
- Modify: `crates/llm-harness-live-tests/tests/runtime_layer/spawn.rs`

Both files share the same `make_spawn_harness` pattern.

- [ ] **Step 1: Rewrite `11_spawn_subagent.rs`**

In `make_spawn_harness`:

1. Replace `message_bus_pair()` with `MessageBus::new()`
2. Replace `async_spawn_pair(bus.clone())` with `SpawnPlugin::new(bus.clone())`
3. In the builder closure, replace `.after_turn_hook(async_hook_for_builder)` with `.install(spawn_plugin.as_ref())`
4. Delete the `IdleWatcher::new(...)` block + 3 setters
5. Add `spawn_plugin.set_harness_weak(Arc::downgrade(&harness));`

Update imports:
- Remove: `use llm_harness_runtime::spawn::delivery::{IdleWatcher, SubAgentMessageConverter, async_spawn_pair};`
- Add: `use llm_harness_runtime::spawn::delivery::SubAgentMessageConverter;`
- Add: `use llm_harness_runtime::spawn::plugin::SpawnPlugin;`
- Change: `use llm_harness_runtime::spawn::message_bus::{MAIN_AGENT_ID, message_bus_pair};` → `use llm_harness_runtime::spawn::message_bus::{MAIN_AGENT_ID, MessageBus};`

- [ ] **Step 2: Rewrite `spawn.rs` live test**

Same changes as Step 1, applied to `make_spawn_harness` in `tests/runtime_layer/spawn.rs`.

- [ ] **Step 3: Build live-tests crate**

Run: `cargo build -p llm-harness-live-tests 2>&1 | head -30`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
cargo fmt
git add crates/llm-harness-live-tests/examples/11_spawn_subagent.rs \
  crates/llm-harness-live-tests/tests/runtime_layer/spawn.rs
git commit -m "refactor(live-tests): use SpawnPlugin in spawn tests/example (issue #125)"
```

---

## Task 12: Full workspace build + test

This is the final verification gate. All code changes are complete; this task verifies the full workspace compiles and all tests pass.

- [ ] **Step 1: Full workspace build**

Run: `cargo build --workspace 2>&1 | tee /tmp/build.log`
Expected: PASS (exit code 0, no errors).

If errors remain, fix them before proceeding. Common issues:
- Missing imports for `AfterRunHook`/`OnAbortHook`/`CompositeAfterRunHook`
- References to deleted types in files not covered by previous tasks

- [ ] **Step 2: Run types crate tests**

Run: `cargo test -p llm-harness-types 2>&1 | tail -5`
Expected: PASS.

- [ ] **Step 3: Run agent crate tests**

Run: `cargo test -p llm-harness-agent --features test-utils 2>&1 | tail -20`
Expected: PASS — all lifecycle tests pass, including the new `abort_invokes_on_abort_hook`, `abort_invokes_on_abort_hook_twice_after_reabort`, `after_run_hook_called_after_run_loop`, `continue_run_calls_before_run_hook`.

- [ ] **Step 4: Run runtime crate tests (CORE)**

Run: `cargo test -p llm-harness-runtime 2>&1 | tail -30`
Expected: PASS — all integration tests pass:
- `abort_propagation::abort_cancels_inflight_subagent`
- `abort_propagation::abort_does_not_affect_future_spawns`
- `abort_propagation::idle_watcher_abort`
- `async_delivery_integration::async_delivery_running_period_sub_completes_during_loop`
- `async_delivery_integration::async_delivery_idle_period_watcher_wakes_main`
- `async_delivery_integration::continue_run_idle_path_no_wasted_llm_call`
- `spawn_communication_integration::*`

This is the most critical verification. If all pass, behavior equivalence is confirmed.

- [ ] **Step 5: Run live tests (if API key available)**

Run: `cargo run -p llm-harness-live-tests --example 11_spawn_subagent 2>&1 | tail -20`
Expected: Program exits normally, output includes sub-agent results.
Skip if no API key configured (check for `.env` file).

- [ ] **Step 6: Final commit if any fixes were needed**

If Steps 1-5 required any fixes:
```bash
cargo fmt
git add -A
git commit -m "fix: resolve compilation/test issues from spawn hook refactor (issue #125)"
```

If no fixes were needed, no commit — the work is already committed across Tasks 1-11.

---

## Self-Review Notes

### Spec coverage check
- §3.1 (types crate hook traits) → Task 1 ✓
- §3.2 (HarnessHooks extension) → Task 3 ✓
- §3.3 (HarnessInner field deletion) → Task 3 ✓
- §3.4 (delete hooks.rs) → Task 3 ✓
- §3.5 (delete 3 setters) → Task 4 ✓
- §3.6 (run_loop改造) → Task 4 ✓
- §3.7 (abort()改造) → Task 4 ✓
- §3.8 (continue_run简化) → Task 4 ✓
- §3.9 (run()路径统一) → Task 4 ✓
- §3.10 (CompositeAfterRunHook) → Task 2 ✓
- §3.11 (SpawnPlugin) → Task 8 ✓
- §3.12 (四个hook实现) → Task 8 ✓
- §3.13 (MessageBus改造) → Task 7 ✓
- §3.14 (delivery.rs改造) → Task 8 ✓
- §3.15 (spawner.rs — 无直接修改) → no task needed ✓
- §3.16 (runner.rs改造) → Task 9 ✓
- §3.17 (builder.rs改造) → Task 6 ✓
- §九 验证计划 → Task 12 + Tasks 5/10 ✓

### Type consistency check
- `AfterRunHook::after_run` returns `BoxFuture<'_, ()>` — consistent across types crate, composite, plugin ✓
- `OnAbortHook::on_abort` is sync `fn on_abort(&self)` — consistent ✓
- `SpawnPlugin::new(bus: Arc<MessageBus>) -> Arc<Self>` — used in runner.rs, tests, example ✓
- `SpawnPlugin::set_harness_weak(&self, weak: Weak<AgentHarness>)` — used in runner.rs, tests, example ✓
- `MessageBus::new() -> Arc<Self>` — replaces `message_bus_pair()` everywhere ✓
- `MessageBus::set_event_sink(&self, sink: Arc<dyn Fn(SubAgentEvent) + Send + Sync>)` — called by `SpawnPlugin::new` ✓
- `format_success`/`format_failure` made `pub(crate)` in delivery.rs, used by plugin.rs ✓