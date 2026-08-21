# 消除 HarnessInner 中的 spawn 专属 hook

> 状态：提案（Proposal）
> 日期：2026-08-21
> 关联：[Issue #125](https://github.com/oh-my-harness/llm-harness-runtime/issues/125)、`spawn/` 现有实现、`agent-society-2` 分支 `AgentRunner` 参考实现

---

## 一、动机

`HarnessInner`（`crates/llm-harness-agent/src/harness/state.rs:114-157`）持有三个 spawn 专属字段：

```rust
pub(super) idle_watcher: Option<Arc<dyn IdleWatcherHook>>,
pub(super) async_spawn_hook: Option<Arc<dyn AsyncSpawnHookHandle>>,
pub(super) abort_cascade_hook: Option<Arc<dyn AbortCascadeHook>>,
```

对应的三个 trait（`crates/llm-harness-agent/src/harness/hooks.rs`）共 9 个方法，仅服务于 spawn 场景。不用 spawn 的 agent（大多数场景）这三个字段永远是 `None`。agent crate 被 spawn 逻辑污染。

### 根因：共享 completion channel

当前 spawn 用一个共享的 `mpsc::Receiver<SubAgentEvent>`（`MessageBus::event_rx`），两个消费者争抢：

- **主 agent loop 运行时**：`AsyncSpawnHookHandle::borrow_completion_rx()` 借出，`AfterTurnHook`（`AsyncSpawnHook::after_turn`）drain
- **主 agent idle 时**：`IdleWatcherHook::on_idle` 阻塞在同一个 rx 上等子 agent 完成

同一时刻只能一方持有 rx，催生了整套 borrow/release 协调。`AsyncSpawnHookHandle` 的四个方法（`borrow_completion_rx` / `release_completion_rx` / `drain_completions` / `set_follow_up_handle`）全部为了管理这个共享 channel 的生命周期。`IdleWatcher` 的 re-send hack、cancel 竞态修复（F-2/F-5/F-6/F-8）也由此而生。

### 两个缺失的通用 hook 点

- **run_loop 结束回到 Idle 后**没有回调 → `IdleWatcherHook::on_idle` 由此而生
- **abort() 时**没有回调 → `AbortCascadeHook` 由此而生

现有 hook 系统（`BeforeRunHook` / `AfterTurnHook` / `BeforeTurnHook` / `ShouldStopHook` 等）覆盖不了这两个时机。

---

## 二、方案概述

### 2.1 新增两个通用 hook

```rust
/// run_loop 结束、harness 回到 Idle 后调用。
/// 可在此 spawn 等待 task（如等待子 agent 完成）。
pub trait AfterRunHook: Send + Sync {
    fn after_run(&self) -> BoxFuture<'_, ()>;
}

/// harness.abort() 时调用。在 AfterRunHook 之前执行，
/// 让 AfterRunHook 能检查标志跳过 wait task。
pub trait OnAbortHook: Send + Sync {
    fn on_abort(&self);
}
```

`HarnessHooks` 增加 `after_run: Vec<Arc<dyn AfterRunHook>>` 和 `on_abort: Vec<Arc<dyn OnAbortHook>>`。

### 2.2 spawn plugin 持有自己的 completion queue

消除共享 channel。子 agent 完成 → 结果 push 进 plugin 自有的 `VecDeque<SubAgentEvent> + Notify`，两条消费路径不抢同一个 rx：

- **run 期间**：`AfterTurnHook` drain queue → 格式化 → 通过 `FollowUpHandle` 注入 follow_up（与当前行为一致）
- **idle 时**：`AfterRunHook` 检查 queue 非空或有 in-flight sub-agent → spawn wait task 阻塞在 `Notify` 上 → 收到通知后调 `harness.continue_run()`；`BeforeRunHook` 负责取消 pending 的 wait task

### 2.3 "鸡生蛋"问题的解决

`AfterRunHook` 的 wait task 完成后需调 `harness.continue_run()`，这要求 `Weak<AgentHarness>`。但 `register_hooks` 在 harness 构建前调用，`Weak<AgentHarness>` 不存在。

**这是固有约束，不是设计缺陷。** 解决方案：spawn plugin 持有 `Arc<Mutex<Option<Weak<AgentHarness>>>>` slot，构建后通过单个 setter 填充。

为何不用 `FollowUpHandle`（前一个方案的推荐）：`FollowUpHandle` 封装 `Weak<Mutex<HarnessInner>>`，只能注入 follow_up 消息。但 Idle 阶段 `follow_up_tx.try_send()` 虽然成功，消息会在下一次 `run_loop` 的 `reset_channels()` 中被静默丢弃（`follow_up.rs` 文档明确标注"仅在 run 进行中有效"）。wait task 需要的是"唤醒 harness 启动新一轮 run"，只有 `continue_run()` 能做到，而 `continue_run()` 是 `AgentHarness` 的方法，需要 `Weak<AgentHarness>`。

当前 3 个 setter（`set_idle_watcher` / `set_async_spawn_hook` / `set_abort_cascade_hook`）合并为 1 个 `set_harness_weak`。复杂度从"3 个 trait × 9 个方法 + 3 个 setter + 共享 channel 借还协议"降为"2 个通用 trait × 2 个方法 + 1 个 setter + 自有队列"。

---

## 三、详细设计

### 3.1 types crate：新增 hook trait

文件：`crates/llm-harness-types/src/hooks.rs`

```rust
// ── AfterRunHook ──────────────────────────────────────────────────────────────

/// run_loop 结束、harness 回到 Idle 后调用。
///
/// 可在此 spawn 等待 task（如等待子 agent 完成后调 `continue_run`）。
/// wait task 应捕获 `Weak<AgentHarness>` 而非 `Arc`，以打断引用环。
/// `BeforeRunHook` 负责取消 pending 的 wait task。
pub trait AfterRunHook: Send + Sync {
    fn after_run(&self) -> BoxFuture<'_, ()>;
}

// ── OnAbortHook ───────────────────────────────────────────────────────────────

/// harness.abort() 时调用。
///
/// 在 `AfterRunHook` 之前执行，让 `AfterRunHook` 能检查标志跳过 wait task。
/// 非阻塞：只置标志/cancel token，不 join sub-agent task。
/// 幂等：可重复调用。
pub trait OnAbortHook: Send + Sync {
    fn on_abort(&self);
}
```

### 3.2 agent crate：HarnessHooks 扩展

文件：`crates/llm-harness-agent/src/harness/state.rs`

`HarnessHooks` 增加两个字段：

```rust
pub struct HarnessHooks {
    // ... 现有字段 ...
    /// Called after run_loop ends and harness returns to Idle.
    pub after_run: Vec<Arc<dyn AfterRunHook>>,
    /// Called when harness.abort() is invoked, before AfterRunHook.
    pub on_abort: Vec<Arc<dyn OnAbortHook>>,
}
```

`HarnessHooks::none()` 初始化两个空 vec。

### 3.3 agent crate：HarnessInner 删除三个字段

文件：`crates/llm-harness-agent/src/harness/state.rs`

删除：
```rust
pub(super) idle_watcher: Option<Arc<dyn IdleWatcherHook>>,
pub(super) async_spawn_hook: Option<Arc<dyn AsyncSpawnHookHandle>>,
pub(super) abort_cascade_hook: Option<Arc<dyn AbortCascadeHook>>,
```

### 3.4 agent crate：删除三个 spawn 专属 trait

文件：`crates/llm-harness-agent/src/harness/hooks.rs`

整文件删除（`IdleWatcherHook`、`AsyncSpawnHookHandle`、`AbortCascadeHook` 三个 trait）。

`mod.rs` 中删除 `pub use hooks::{AbortCascadeHook, AsyncSpawnHookHandle, IdleWatcherHook};`。

### 3.5 agent crate：删除三个 setter

文件：`crates/llm-harness-agent/src/harness/core.rs`

删除：
- `set_idle_watcher`（行 766-769）
- `set_async_spawn_hook`（行 771-777）
- `set_abort_cascade_hook`（行 779-782）

### 3.6 agent crate：run_loop 改造

文件：`crates/llm-harness-agent/src/harness/loop_driver.rs`

当前 `run_loop`（行 252-319）在 cleanup 后调用 `watcher.on_idle()`。改为调用 `after_run` hooks：

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
    // 删除 async_hook clone + borrow_completion_rx

    let _guard = self.enter_phase(HarnessPhase::Turning)?;

    let result = self
        .drive_loop(run, initial, steer_rx, follow_up_rx, abort, system_prompt)
        .await;

    // Cleanup (不变)
    {
        let mut inner = self.inner.lock().unwrap();
        // ... 现有 cleanup 逻辑 ...
    }
    // 删除 release_completion_rx

    drop(_guard); // 恢复 Idle

    // AfterRunHook：替代原 watcher.on_idle()
    if !self.hooks.after_run.is_empty() {
        let composite = Arc::new(CompositeAfterRunHook::new(self.hooks.after_run.clone()));
        composite.after_run().await;
    }

    result
}
```

关键变化：
- 删除 `async_hook` clone + `borrow_completion_rx()` / `release_completion_rx()`
- 删除 `watcher` clone + `watcher.on_idle()`
- 新增 `after_run` hooks 调用（在 `PhaseGuard` drop 恢复 Idle 之后）

### 3.7 agent crate：abort() 改造

文件：`crates/llm-harness-agent/src/harness/core.rs`

当前 `abort()`（行 886-909）先 cancel watcher，再 cancel cascade hook，最后 cancel self token。改为：

```rust
pub fn abort(&self) {
    // 1. OnAbortHook（替代 cascade hook + watcher cancel）
    for hook in &self.hooks.on_abort {
        hook.on_abort();
    }
    // 2. cancel self token
    let mut inner = self.inner.lock().unwrap();
    if let Some(token) = inner.current_abort.take() {
        token.cancel();
    }
}
```

关键变化：
- 删除 `cascade_hook` clone + `watcher` clone
- `OnAbortHook` 在 `abort()` 中同步调用（非 async，与当前 `cancel_all_subagents` 一致）
- 不再有"先 cancel watcher 再 cancel sub-agent"的顺序依赖——`OnAbortHook` 设置标志，后续 `AfterRunHook` 检查标志跳过 wait task

### 3.8 agent crate：continue_run 简化

文件：`crates/llm-harness-agent/src/harness/core.rs`

当前 `continue_run`（行 792-820）预排空 completion channel。移除排空逻辑（由 `BeforeRunHook` 处理）：

```rust
pub async fn continue_run(&self) -> Result<(), HarnessError> {
    {
        let inner = self.inner.lock().unwrap();
        if inner.state.phase != HarnessPhase::Idle {
            return Err(HarnessError::NotIdle(inner.state.phase));
        }
    }
    let run = Arc::new(RunContext::new(RunRequest::new(vec![])));
    let initial = run.initial_messages().to_vec();
    // before_run hook 会 drain queue 并返回 additional_messages
    let initial = self.run_before_run_hook(&run, initial).await?;
    self.run_loop(run, initial).await
}
```

关键变化：
- 删除 `async_spawn_hook` clone + `borrow/drain/release`
- 新增 `run_before_run_hook` 调用（当前 `continue_run` 不调 `before_run_hook`，是 `run()` 路径独有的）
- `BeforeRunHook` drain queue 返回 `additional_messages`，两条路径（`run()` 和 `continue_run()`）统一

### 3.9 agent crate：run() 路径统一

文件：`crates/llm-harness-agent/src/harness/core.rs`

当前 `run()`（行 212-244）已调用 `run_before_run_hook`。删除 watcher cancel 逻辑：

```rust
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
    // 删除 watcher clone + cancel

    self.try_auto_compact().await?;
    let initial: Vec<AgentMessage> = queued.into_iter().chain(messages).collect();
    let initial = self.run_before_run_hook(&run, initial).await?;
    self.run_loop(run, initial).await
}
```

关键变化：删除 `watcher` clone + `watcher.cancel()`。`BeforeRunHook` 负责取消 wait task。

### 3.10 agent crate：Composite hook 包装器

文件：`crates/llm-harness-loop`（或 agent crate 内，取决于 `Composite*` 放置约定）

新增 `CompositeAfterRunHook`（all-execute，与 `CompositeAfterTurnHook` 一致）。

`OnAbortHook` 不需要 composite wrapper——`abort()` 中直接遍历 `self.hooks.on_abort` 逐个调用（同步，无返回值）。

### 3.11 runtime crate：SpawnPlugin

文件：`crates/llm-harness-runtime/src/spawn/plugin.rs`（新建）

```rust
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use llm_harness_agent::{AgentHarness, Plugin, HarnessHooks};
use llm_harness_types::{
    AfterRunHook, AfterTurnHook, BeforeRunHook, BeforeRunResult, OnAbortHook,
    AgentError, AgentMessage, BeforeRunCtx,
};
use crate::spawn::message_bus::{MessageBus, SubAgentEvent};
use crate::spawn::delivery::{format_success, format_failure};

/// Spawn plugin：替代 AsyncSpawnHook + IdleWatcher + AbortCascadeHook。
///
/// 持有自己的 completion queue（VecDeque + Notify），消除共享 channel。
/// 通过 4 个通用 hook（BeforeRun / AfterTurn / AfterRun / OnAbort）注入行为。
pub struct SpawnPlugin {
    bus: Arc<MessageBus>,
    /// 子 agent 完成事件队列。track task 和 MessageBus::send push 进来。
    queue: Arc<Mutex<VecDeque<SubAgentEvent>>>,
    /// queue 非空时通知 wait task。
    notify: Arc<Notify>,
    /// 构建后填充。wait task 通过它调 continue_run。
    harness_weak: Arc<Mutex<Option<Weak<AgentHarness>>>>,
    /// wait task 的 cancel token。BeforeRunHook 取消它。
    wait_task_token: Arc<Mutex<Option<CancellationToken>>>,
    /// abort 标志。OnAbortHook 设置，AfterRunHook 检查。
    aborted: Arc<std::sync::atomic::AtomicBool>,
    /// idle 超时秒数。
    idle_timeout: Duration,
}

impl SpawnPlugin {
    pub fn new(bus: Arc<MessageBus>) -> Arc<Self> {
        Arc::new(Self {
            bus,
            queue: Arc::new(Mutex::new(VecDeque::new())),
            notify: Arc::new(Notify::new()),
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

    /// 构建后调用：填充 Weak<AgentHarness>。
    pub fn set_harness_weak(&self, weak: Weak<AgentHarness>) {
        *self.harness_weak.lock().unwrap() = Some(weak);
    }
}

impl Plugin for SpawnPlugin {
    fn name(&self) -> &str { "spawn" }

    fn register_hooks(&self, hooks: &mut HarnessHooks) {
        hooks.before_run.push(Arc::new(SpawnBeforeRun {
            queue: self.queue.clone(),
            wait_task_token: self.wait_task_token.clone(),
        }));
        hooks.after_turn.push(Arc::new(SpawnAfterTurn {
            queue: self.queue.clone(),
            bus: self.bus.clone(),
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
```

### 3.12 runtime crate：四个 hook 实现

文件：`crates/llm-harness-runtime/src/spawn/plugin.rs`

#### SpawnBeforeRun

```rust
/// run 开始前：取消 pending wait task + drain queue 返回 additional_messages。
struct SpawnBeforeRun {
    queue: Arc<Mutex<VecDeque<SubAgentEvent>>>,
    wait_task_token: Arc<Mutex<Option<CancellationToken>>>,
}

impl BeforeRunHook for SpawnBeforeRun {
    fn before_run<'a>(&'a self, _ctx: BeforeRunCtx<'a>)
        -> BoxFuture<'a, Result<BeforeRunResult, AgentError>>
    {
        Box::pin(async move {
            // 1. 取消 pending wait task（如果有）
            if let Some(token) = self.wait_task_token.lock().unwrap().take() {
                token.cancel();
            }
            // 2. Drain queue → format → additional_messages
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
```

#### SpawnAfterTurn

```rust
/// turn 结束后：drain queue，格式化，通过 FollowUpHandle 注入 follow_up。
/// （与当前 AsyncSpawnHook::after_turn 行为一致）
struct SpawnAfterTurn {
    queue: Arc<Mutex<VecDeque<SubAgentEvent>>>,
    bus: Arc<MessageBus>,
}

impl AfterTurnHook for SpawnAfterTurn {
    fn after_turn<'a>(&'a self, _ctx: AfterTurnCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let msg = drain_and_format(&self.queue)
                .or_else(|| status_hint(&self.bus));
            if let Some(msg) = msg {
                // FollowUpHandle 从 harness_weak 派生
                // （需要 harness_weak 引用，或由 SpawnPlugin 预注入）
                // ... 注入 follow_up ...
            }
        })
    }
}
```

**设计问题**：`SpawnAfterTurn` 需要访问 `FollowUpHandle` 来注入 follow_up。当前 `AsyncSpawnHook` 通过 `set_follow_up_handle` 在构建后获得。新设计中，`SpawnAfterTurn` 可以从 `harness_weak` 派生 `FollowUpHandle`（`AgentHarness::follow_up_handle()` 返回 `FollowUpHandle`，它封装 `Weak<Mutex<HarnessInner>>`，可从 `Weak<AgentHarness>` 获取）。

但 `AfterTurnHook` 是同步 trait 方法返回 `BoxFuture`，在 hook 内部无法直接从 `Weak<AgentHarness>` 调 `follow_up_handle()`（需要 `&AgentHarness`）。解决方案：`SpawnAfterTurn` 也持有 `harness_weak: Arc<Mutex<Option<Weak<AgentHarness>>>>`，在 `after_turn` 中 `upgrade` 后调 `follow_up_handle()`。

```rust
struct SpawnAfterTurn {
    queue: Arc<Mutex<VecDeque<SubAgentEvent>>>,
    bus: Arc<MessageBus>,
    harness_weak: Arc<Mutex<Option<Weak<AgentHarness>>>>,
}

impl AfterTurnHook for SpawnAfterTurn {
    fn after_turn<'a>(&'a self, _ctx: AfterTurnCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let msg = drain_and_format(&self.queue)
                .or_else(|| status_hint(&self.bus));
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
```

注意：这里用 `h.follow_up_message(msg)` 直接发送，不需要 `FollowUpHandle` 中间层。`follow_up_message` 是 `AgentHarness` 的 public 方法。

#### SpawnAfterRun

```rust
/// run_loop 结束回到 Idle 后：检查 queue/in-flight，按需 spawn wait task。
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
            // 1. 检查 abort 标志
            if self.aborted.swap(false, std::sync::atomic::Ordering::Relaxed) {
                return; // abort 了，不 spawn wait task
            }
            // 2. queue 已有事件 → 直接 continue_run（BeforeRunHook 会 drain）
            {
                let q = self.queue.lock().unwrap();
                if !q.is_empty() {
                    drop(q);
                    self.wake_harness();
                    return;
                }
            }
            // 3. 无 in-flight sub-agent → 不需要等
            if self.bus.in_flight_count() == 0 {
                return;
            }
            // 4. 有 in-flight → spawn wait task
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
                            // 有事件到达，唤醒 harness
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
                            // 超时：设置标志，唤醒 harness 让 after_turn 注入状态提示
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
```

#### SpawnOnAbort

```rust
/// abort() 时：cancel 所有子 agent + 设置标志让 AfterRunHook 跳过 wait task。
struct SpawnOnAbort {
    bus: Arc<MessageBus>,
    aborted: Arc<std::sync::atomic::AtomicBool>,
    wait_task_token: Arc<Mutex<Option<CancellationToken>>>,
}

impl OnAbortHook for SpawnOnAbort {
    fn on_abort(&self) {
        // 1. 设置 abort 标志
        self.aborted.store(true, std::sync::atomic::Ordering::Relaxed);
        // 2. 取消 pending wait task
        if let Some(token) = self.wait_task_token.lock().unwrap().take() {
            token.cancel();
        }
        // 3. cancel 所有子 agent
        self.bus.cancel_all();
    }
}
```

### 3.13 runtime crate：MessageBus 改造

文件：`crates/llm-harness-runtime/src/spawn/message_bus.rs`

**删除**：
- `event_tx: mpsc::Sender<SubAgentEvent>` 字段
- `event_rx: Mutex<Option<mpsc::Receiver<SubAgentEvent>>>` 字段
- `EventRxGuard` 结构体及 impl
- `take_event_rx` / `return_event_rx` 方法
- `try_resend_event` / `resend_event` 方法
- `message_bus_pair()` 函数
- `AbortCascadeHook for MessageBus` impl

**保留**：
- `in_flight: Arc<AtomicUsize>` — 仍需追踪 in-flight count
- `agents: Mutex<HashMap<String, AgentEntry>>` — agent registry 不变
- `waiters: Mutex<HashMap<String, Vec<Waiter>>>` — waiter 表不变（AwaitSubagentReplyTool 用）
- `subagent_aborts: Mutex<HashMap<String, CancellationToken>>` — abort 注册不变
- `idle_timeout_fired: AtomicBool` — 保留，spawn plugin 的 wait task 设置，after_turn 消费
- `cancel_all()` 方法 — 保留，`SpawnOnAbort` 调用
- `send()` 方法 — 修改：主 agent 的 `SubAgentEvent::Message` 不再走 `event_tx`，改为通过 callback 推入 spawn plugin 的 queue

**send() 改造**：

当前 `send()` 对主 agent 走 `event_tx.send(SubAgentEvent::Message{...})`。新设计中，主 agent 的事件需要推入 spawn plugin 的 queue。

方案：`MessageBus` 增加一个可选的 `event_sink` callback：

```rust
pub struct MessageBus {
    // ... 保留字段 ...
    /// 主 agent 事件投递回调。由 SpawnPlugin 在构造时注入。
    /// None 时主 agent 事件被丢弃（无 spawn plugin 场景）。
    event_sink: Mutex<Option<Arc<dyn Fn(SubAgentEvent) + Send + Sync>>>,
}
```

`MessageBus::send()` 对主 agent 路由改为：
```rust
if is_main {
    let sink = self.event_sink.lock().unwrap();
    if let Some(sink) = sink.as_ref() {
        sink(SubAgentEvent::Message {
            agent_id: from.to_string(),
            body: body.to_string(),
        });
    }
    return SendReceipt::Injected;
}
```

同样，`track_with_oneshot` 中的 `event_tx.send(event)` 改为通过 `event_sink`：

```rust
// track task 完成时：
let event = SubAgentEvent::Done { agent_id, result };
// 先尝试 waiter 匹配（不变），失败后走 event_sink
let sink = registry.event_sink.lock().unwrap();
if let Some(sink) = sink.as_ref() {
    sink(event);
}
```

`SpawnPlugin` 在 `new()` 时注入 sink：
```rust
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
        // ...
    }
}
```

### 3.14 runtime crate：delivery.rs 改造

文件：`crates/llm-harness-runtime/src/spawn/delivery.rs`

**删除**：
- `AsyncSpawnHook` 结构体及所有 impl（`AfterTurnHook`、`AsyncSpawnHookHandle`）
- `IdleWatcher` 结构体及所有 impl（`IdleWatcherHook`）
- `async_spawn_pair()` 函数
- `has_running_agents()` 函数

**保留**：
- `AsyncSpawnHandle` 结构体（不持 join 能力，仍需返回给调用方）
- `format_success()` / `format_failure()` 函数（移到 plugin.rs 或保留在 delivery.rs）
- `SubAgentMessageConverter`（不变）

`format_events()` 辅助函数（plugin.rs 内）：
```rust
fn format_events(events: Vec<SubAgentEvent>) -> Vec<AgentMessage> {
    if events.is_empty() {
        return vec![];
    }
    let mut results: Vec<String> = Vec::new();
    for event in events {
        match event {
            SubAgentEvent::Done { agent_id, result } => match result {
                Ok(r) => results.push(format_success(&agent_id, &r)),
                Err(AgentError::Aborted) => {} // 丢弃
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
```

### 3.15 runtime crate：spawner.rs 改造

文件：`crates/llm-harness-runtime/src/spawn/spawner.rs`

`spawn_async` 中 track task 完成后不再走 `event_tx`，走 `event_sink`（已由 MessageBus 改造处理）。无直接修改。

### 3.16 runtime crate：runner.rs 改造

文件：`crates/llm-harness-runtime/src/workflow/engine/runner.rs`

当前 `post_build_spawn_setup`（行 1229-1283）创建 `AsyncSpawnHook` + `MessageBus`，build 后调 3 个 setter。改为：

```rust
let spawn_setup: Option<(
    Arc<SpawnPlugin>,
    Arc<MessageBus>,
)> = if allowed_tools.iter().any(|t| t == SPAWN_AGENT_TOOL_NAME) {
    let bus = Arc::new(MessageBus::new());
    let plugin = SpawnPlugin::new(bus.clone());
    let spawner = Arc::new(self.build_step_spawner(bus.clone(), step.id(), attempt_seq));
    bus.register(MAIN_AGENT_ID, AgentEntry { /* ... */ });
    // 注册 tools（不变）
    // ...
    builder = builder.install(plugin.as_ref());
    Some((plugin, bus))
} else {
    None
};

// ... build ...

if let Some((plugin, _bus)) = spawn_setup {
    plugin.set_harness_weak(Arc::downgrade(&harness));
}
```

关键变化：
- `message_bus_pair()` → `MessageBus::new()`（不再返回 rx）
- `async_spawn_pair(bus)` → `SpawnPlugin::new(bus)`
- 3 个 setter → 1 个 `set_harness_weak`
- `after_turn` hook 不再单独 push（由 `Plugin::register_hooks` 统一注册）

### 3.17 builder.rs 改造

文件：`crates/llm-harness-agent/src/builder.rs`

`hooks()` 方法增加两行：
```rust
self.hooks.after_run.extend(hooks.after_run);
self.hooks.on_abort.extend(hooks.on_abort);
```

可选：增加 `after_run_hook()` 和 `on_abort_hook()` 便利方法（与其他 hook 一致）。

---

## 四、专属 hook 替代映射

| 专属 hook 方法 | 调用时机 | 替代方案 |
|---|---|---|
| `IdleWatcherHook::on_idle` | run_loop 结束进 Idle | `AfterRunHook` |
| `IdleWatcherHook::cancel` | prompt() 开始前 | `BeforeRunHook`（取消 wait task token） |
| `AsyncSpawnHookHandle::borrow_completion_rx` | run_loop 开始 | 不需要（无共享 channel） |
| `AsyncSpawnHookHandle::release_completion_rx` | run_loop 结束 | 不需要（无共享 channel） |
| `AsyncSpawnHookHandle::drain_completions` | continue_run 预排空 | `BeforeRunHook` 返回 `additional_messages` |
| `AsyncSpawnHookHandle::set_follow_up_handle` | setup 时 | 不需要（plugin 从 `Weak<AgentHarness>` 派生） |
| `AbortCascadeHook::cancel_all_subagents` | abort() 时 | `OnAbortHook` |

---

## 五、abort 竞态分析

### 当前设计

abort 顺序：先 cancel watcher（让它停收 completion），再 cancel 子 agent，再 cancel self token。顺序很重要——若先 cancel 子 agent，它们的 aborted completion 会被 watcher 接收并触发 continue_run 唤醒已 abort 的主 agent。

### 新设计

```
abort():
  1. OnAbortHook → 设置 aborted 标志 + 取消 wait task token + cancel 所有子 agent
  2. cancel self token
```

不需要"先 cancel watcher"的顺序：
- `OnAbortHook` 取消 wait task token → wait task 在 `select!` 的 `cancelled` 分支 break
- 子 agent 被 cancel 后产生 aborted completion → push 进 queue
- `AfterRunHook` 检查 `aborted` 标志 → 跳过 spawn wait task
- aborted completion 留在 queue 里，下次 `BeforeRunHook` drain 时 `Err(Aborted)` 被丢弃

**竞态安全**：`OnAbortHook` 在 `abort()` 中同步执行（持 `self.hooks.on_abort` 引用），在 `AfterRunHook` 之前完成。`aborted` 标志的 `store` 在 `OnAbortHook` 中，`swap` 在 `AfterRunHook` 中——`AfterRunHook` 在 `run_loop` 的 `PhaseGuard` drop 之后调用，而 `abort()` 的 `cancel self token` 会导致 `drive_loop` 的 `agent_loop` 返回 `AgentError::Aborted` → `drive_loop` break → `run_loop` cleanup → `AfterRunHook`。时序确定。

---

## 六、Idle 超时迁移

### 当前设计

`IdleWatcher` 在 `on_idle` 中 spawn task，`select!` 等待 `guard.recv()` 或 `sleep(idle_timeout)`。超时后设置 `bus.set_idle_timeout_fired()`，然后调 `continue_run()`。`after_turn` 中 `status_hint()` 检查 `consume_idle_timeout_fired()`，注入状态提示。

### 新设计

`SpawnAfterRun` 的 wait task 用 `tokio::time::sleep(timeout)` 替代。超时后：
1. 设置 `bus.set_idle_timeout_fired()`
2. 调 `harness.continue_run()`
3. `continue_run()` → `BeforeRunHook` drain queue（可能空）→ `run_loop` → `after_turn` 中 `SpawnAfterTurn` 检查 `consume_idle_timeout_fired()` → 注入状态提示

行为与当前一致，只是 wait task 从 IdleWatcher 移到 `AfterRunHook`。

---

## 七、修改范围

### types crate
- `crates/llm-harness-types/src/hooks.rs`：新增 `AfterRunHook`、`OnAbortHook` trait

### agent crate
- `crates/llm-harness-agent/src/harness/state.rs`：`HarnessHooks` 加 2 字段，`HarnessInner` 删 3 字段
- `crates/llm-harness-agent/src/harness/hooks.rs`：**整文件删除**
- `crates/llm-harness-agent/src/harness/mod.rs`：删 `pub use hooks::...`，删 `mod hooks`
- `crates/llm-harness-agent/src/harness/core.rs`：删 3 个 setter，改 `abort()`、`continue_run()`、`run()`
- `crates/llm-harness-agent/src/harness/loop_driver.rs`：改 `run_loop()`，删 borrow/release/on_idle
- `crates/llm-harness-agent/src/builder.rs`：`hooks()` 加 2 行 extend
- 新增 `CompositeAfterRunHook`（放在 `llm-harness-loop` 或 agent crate 内）

### runtime crate
- `crates/llm-harness-runtime/src/spawn/plugin.rs`：**新建**（SpawnPlugin + 4 个 hook 实现）
- `crates/llm-harness-runtime/src/spawn/delivery.rs`：删 `AsyncSpawnHook`、`IdleWatcher`、`async_spawn_pair`、`has_running_agents`；保留 `AsyncSpawnHandle`、`format_*`、`SubAgentMessageConverter`
- `crates/llm-harness-runtime/src/spawn/message_bus.rs`：删 event channel 相关（`event_tx`/`event_rx`/`EventRxGuard`/`take_event_rx`/`return_event_rx`/`try_resend_event`/`resend_event`/`message_bus_pair`/`AbortCascadeHook impl`）；新增 `event_sink` 字段 + `set_event_sink` 方法；改 `send()` 和 `track_with_oneshot()` 的事件投递路径
- `crates/llm-harness-runtime/src/spawn/mod.rs`：加 `pub mod plugin`
- `crates/llm-harness-runtime/src/workflow/engine/runner.rs`：改 `post_build_spawn_setup` 为 `SpawnPlugin` + `set_harness_weak`

### 测试 / 示例
- `crates/llm-harness-runtime/tests/abort_propagation.rs`：改 3 setter → `SpawnPlugin` + `set_harness_weak`
- `crates/llm-harness-runtime/tests/async_delivery_integration.rs`：同上
- `crates/llm-harness-live-tests/examples/11_spawn_subagent.rs`：同上
- `crates/llm-harness-live-tests/tests/runtime_layer/spawn.rs`：同上
- `crates/llm-harness-agent/src/harness/tests/lifecycle.rs`：改 `set_abort_cascade_hook` / `set_idle_watcher` 测试为新 hook 测试

---

## 八、风险与缓解

### 8.1 `AfterRunHook` 中 wait task 的生命周期

wait task 持 `Weak<AgentHarness>`，harness drop 后 `upgrade()` 失败，task 自然退出。`wait_task_token` 在 `SpawnBeforeRun` 和 `SpawnOnAbort` 中被 cancel，确保不会泄漏。

### 8.2 `event_sink` callback 的线程安全

`event_sink` 是 `Arc<dyn Fn(SubAgentEvent) + Send + Sync>`，通过 `Mutex` 保护。track task 和 `MessageBus::send` 都通过 `event_sink` 投递事件，callback 内部只做 `queue.push_back` + `notify_one`（`Mutex` + `Notify` 都是线程安全的）。

### 8.3 `continue_run` 新增 `before_run_hook` 调用的行为变化

当前 `continue_run` 不调 `before_run_hook`。新设计调了。这是**有意的行为变化**：`BeforeRunHook` 负责 drain queue，`continue_run` 需要它。需确认现有的 `BeforeRunHook` 实现在 `continue_run` 路径调用时不会产生副作用。

**skills 元数据注入**：`run_before_run_hook`（loop_driver.rs:54-124）内联注入 skills 元数据。`continue_run` 场景下 `initial` 为空 vec，`prompt_text` 为空字符串。skills 元数据注入基于 `has_skill_readiness` 标志，不依赖 `prompt_text`。**安全**——但会在 `continue_run` 路径额外注入一次 skills 元数据。如果 `SpawnBeforeRun` 和 skills 注入共存，需确认顺序（`SpawnBeforeRun` 的 `additional_messages` 在 skills 元数据之后追加，不影响）。

**`StructuredPromptHook` system_prompt 累积（已修复）**：`StructuredPromptHook`（runner.rs:85-102）在 `before_run` 中执行 `*ctx.system_prompt = Some(format!("{existing}{seg}"))`，追加结构化输出约束。`run_before_run_hook` 将变异后的 `system_prompt` 持久化回 `inner.state.system_prompt`（loop_driver.rs:119-121）。当前只有 `run()` 调 `before_run_hook`，所以每次 `prompt()` 累积一次。新设计 `continue_run` 也调 `before_run_hook`，每次子 agent 完成唤醒都会再次追加，导致 system_prompt 无限增长。

**修复**：`StructuredPromptHook` 幂等化——追加前检查 segment 是否已存在：

```rust
impl BeforeRunHook for StructuredPromptHook {
    fn before_run<'a>(&'a self, ctx: BeforeRunCtx<'a>) -> BoxFuture<'a, Result<BeforeRunResult, AgentError>> {
        Box::pin(async move {
            let existing = ctx.system_prompt.take().unwrap_or_default();
            if !existing.contains(self.segment) {
                *ctx.system_prompt = Some(format!("{existing}{seg}", seg = self.segment));
            } else {
                *ctx.system_prompt = Some(existing);
            }
            Ok(BeforeRunResult { additional_messages: vec![], system_prompt: None })
        })
    }
}
```

### 8.4 `AfterRunHook` 是 async 的

`AfterRunHook::after_run` 返回 `BoxFuture`。`run_loop` 中 `composite.after_run().await` 会 await。wait task 在 `after_run` 内部 `tokio::spawn` 后立即返回（不阻塞 `run_loop`）。但如果 `after_run` 中需要同步处理（如直接 `continue_run().await`），会阻塞 `run_loop` 的返回。

**设计决策**：`SpawnAfterRun` 只 spawn wait task，不 await 它。直接 `continue_run` 的场景（queue 已有事件）也只 spawn 不 await——通过 `wake_harness()` 方法（spawn 一个分离 task 调 `continue_run`）。这样 `run_loop` 能正常返回 Ok，harness 回到 Idle，wait task 后续调 `continue_run` 时 Idle 守卫能通过。

`wake_harness()` 定义在 `SpawnPlugin` 和 `SpawnAfterRun` 上：

```rust
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
```

### 8.5 `SubAgentMessageConverter` 保留

`SubAgentMessageConverter` 将 `CustomMessage{type:"sub_agent_message"}` 转为 user message。新设计仍注入相同格式的 `CustomMessage`，converter 不变。runner.rs 中 `convert_to_llm` 注册不变。

### 8.6 陈旧 `aborted` 标志跨运行泄漏（已修复）

`SpawnOnAbort::on_abort` 执行 `aborted.store(true)`。`SpawnAfterRun::after_run` 执行 `aborted.swap(false)` 来检查并清除。但如果在 Idle 状态下调用 `abort()`（`current_abort` 为 None，cancel 是空操作），`aborted` 保持为 true。下一次 `prompt()` → `run_loop` → `after_run` → `aborted.swap(false)` 返回 true → 跳过 wait task → 子 agent 结果丢失。

**修复**：`SpawnBeforeRun::before_run` 开头清除标志：

```rust
impl BeforeRunHook for SpawnBeforeRun {
    fn before_run<'a>(&'a self, _ctx: BeforeRunCtx<'a>) -> BoxFuture<'a, Result<BeforeRunResult, AgentError>> {
        Box::pin(async move {
            // 清除可能陈旧的 aborted 标志（Idle 下误调 abort 遗留）
            self.aborted.store(false, std::sync::atomic::Ordering::Relaxed);
            // ... 取消 wait task + drain queue ...
        })
    }
}
```

`SpawnBeforeRun` 需新增 `aborted: Arc<AtomicBool>` 字段。

### 8.7 重复 `AfterRunHook` 导致孤立 wait tasks（已修复）

wait task 调 `h.continue_run().await` → `run_loop` → `after_run` → `SpawnAfterRun::after_run`。如果 `in_flight_count() > 0`，会 spawn 新的 wait task，覆盖 `wait_task_token` slot，旧 token 未被 cancel。导致两个 wait tasks 同时等待同一个 `Notify`，产生重复唤醒 / `NotIdle` 竞态。

**修复**：spawn 新 wait task 前取消旧 token：

```rust
// 在 *self.wait_task_token.lock().unwrap() = Some(token.clone()) 之前：
if let Some(old) = self.wait_task_token.lock().unwrap().take() {
    old.cancel();
}
```

### 8.8 `has_running_agents` 与 `in_flight_count` 的行为差异

当前代码（`delivery.rs:334, 354`）同时使用 `has_running_agents()` 和 `in_flight_count() == 0`。track task 中存在时间窗口：`status=Done` 但 `in_flight` 尚未 `fetch_sub`（更新发生在 fetch_sub 之前）。仅用 `in_flight_count` 的 wait task 可能比当前代码多等一个极短窗口。影响很小——wait task 在 `notify.notified()` 上等待，track task 完成后会 `notify_one`。无需修复，但记录此行为变更。

### 8.9 `FollowUpHandle` 弃用说明

新设计中 `SpawnAfterTurn` 直接通过 `h.follow_up_message(msg)` 发送，不使用 `FollowUpHandle` 中间层。原因：`FollowUpHandle` 封装 `Weak<Mutex<HarnessInner>>`，而 spawn plugin 已持有 `Weak<AgentHarness>`（通过 `harness_weak` slot），`AgentHarness::follow_up_message()` 是 public 方法，直接调用更简单。`FollowUpHandle` 作为 public API 保留（其他场景可能使用），spawn 场景不再需要。

### 8.10 `CompositeAfterRunHook` 位置

放在 `llm-harness-loop` crate 的 `composite.rs` 中，与其他 `Composite*` 一致。

### 8.11 `MessageBus::new()` 签名变更

当前：`pub fn new() -> (Arc<Self>, mpsc::Receiver<SubAgentEvent>)`
新：`pub fn new() -> Arc<Self>`

不再返回 receiver——event channel 已删除，事件通过 `event_sink` callback 投递。`message_bus_pair()` 函数删除。
## 九、验证计划

验证分三层：**编译期**（类型系统强制）、**单元测试**（agent crate 内部行为）、**集成测试**（runtime crate 端到端）。每层有明确的通过条件和失败信号。

### 9.1 编译期验证（第一道门）

重构是"删除 + 新增"型，编译器是最快的反馈。

| 验证项 | 方法 | 失败信号 |
|--------|------|---------|
| 3 个 trait 已删除 | `cargo build -p llm-harness-agent` | 编译错误：找不到 `IdleWatcherHook` / `AsyncSpawnHookHandle` / `AbortCascadeHook` |
| 3 个字段已删除 | 同上 | 编译错误：`HarnessInner` 无 `idle_watcher` / `async_spawn_hook` / `abort_cascade_hook` 字段 |
| 3 个 setter 已删除 | 同上 | 编译错误：`AgentHarness` 无 `set_idle_watcher` / `set_async_spawn_hook` / `set_abort_cascade_hook` 方法 |
| 新 hook trait 已注册 | `cargo build -p llm-harness-types` | 编译错误：`AfterRunHook` / `OnAbortHook` 未定义 |
| `HarnessHooks` 新字段 | `cargo build -p llm-harness-agent` | 编译错误：`HarnessHooks` 无 `after_run` / `on_abort` 字段 |
| agent crate 零 spawn 依赖 | `cargo build -p llm-harness-agent` | 编译错误：agent crate 引用了 runtime crate 的 spawn 模块 |
| runtime crate 编译 | `cargo build -p llm-harness-runtime` | 编译错误：`MessageBus` 缺 `event_tx` / `EventRxGuard` 等（调用方未更新） |
| 全 workspace 编译 | `cargo build --workspace` | 任何编译错误 |

**命令**：
```bash
cargo build --workspace 2>&1 | tee /tmp/build.log
```
通过条件：退出码 0，无错误。

### 9.2 单元测试（agent crate 内部行为）

文件：`crates/llm-harness-agent/src/harness/tests/lifecycle.rs`

#### 9.2.1 `abort_invokes_on_abort_hook`（替代 `abort_invokes_cascade_hook`）

**验证目标**：`OnAbortHook` 在 `abort()` 时被调用。

场景：注册一个计数 `OnAbortHook`，调 `abort()`，断言计数 == 1。

通过条件：`counter == 1`。
失败信号：`counter == 0`（hook 未注册或 `abort()` 未遍历 `on_abort`）。

#### 9.2.2 `abort_invokes_on_abort_hook_twice_after_reabort`（替代原 `abort_invokes_cascade_hook_twice_after_reabort`）

**验证目标**：`OnAbortHook` 是生命周期持久的（`Vec<Arc<dyn OnAbortHook>>` 不被 `take` 消费），re-prompt 后再次 `abort()` 仍触发。

场景：注册计数 hook → 设置 abort token → `abort()` → 断言计数 == 1 → 再设置 abort token → `abort()` → 断言计数 == 2。

通过条件：两次 `abort()` 后 `counter == 2`。
失败信号：第二次 `counter` 仍为 1（hook 被 consume 或 Vec 被 clear）。

**这是回归测试**：当前代码注释明确标注"take() 会消耗 hook 导致第二次 abort 静默跳过"。新设计用 `Vec` 遍历而非 `Option::take()`，但仍需验证。

#### 9.2.3 `after_run_hook_called_after_run_loop`（新增）

**验证目标**：`AfterRunHook` 在 `run_loop` 结束、harness 回到 Idle 后被调用。

场景：注册一个计数 `AfterRunHook` → `prompt("hello")` → 断言计数 == 1。

通过条件：`counter == 1`。
失败信号：`counter == 0`（hook 未被调用，或调用时机错误——如在 `run_loop` 内部而非结束后）。

#### 9.2.4 `after_run_hook_skipped_on_abort`（新增）

**验证目标**：abort 后 `AfterRunHook` 仍被调用（`run_loop` cleanup → `PhaseGuard` drop → `after_run`），但 `SpawnAfterRun` 检查 `aborted` 标志跳过 wait task。

agent crate 层面只验证 `AfterRunHook` 本身被调用（不验证 spawn 行为），spawn 行为放到集成测试 9.3.6。

#### 9.2.5 `continue_run_calls_before_run_hook`（新增）

**验证目标**：`continue_run` 现在调用 `before_run_hook`（之前不调）。

场景：注册计数 `BeforeRunHook` → `prompt("first")` 消费第一条响应 → `continue_run()` → 断言 before_run 计数 == 2（prompt 一次 + continue_run 一次）。

通过条件：`counter == 2`。
失败信号：`counter == 1`（`continue_run` 未调 `before_run_hook`）。

#### 9.2.6 `continue_run_drives_loop_with_empty_initial`（保留，需确认无破坏）

**验证目标**：`continue_run` 仍能驱动 LLM 调用。

**行为变化分析**：`make_harness` 不注册 `before_run_hook`，也无 `readable_skills`。`run_before_run_hook` 在 `!has_skill_readiness && self.hooks.before_run.is_empty()` 时提前返回 `Ok(initial)`（loop_driver.rs:70-72）。因此 `continue_run` 新增的 `before_run_hook` 调用对此测试**无影响**。

通过条件：`assistant_count == 2`（与当前一致）。

#### 9.2.7 `abort_emits_aborted_event`（保留，无需修改）

不使用 spawn hook，不受影响。

**命令**：
```bash
cargo test -p llm-harness-agent --features test-utils -- lifecycle
```

### 9.3 集成测试（runtime crate 端到端）——核心验证

每个测试场景验证一条行为链路，每条链路在新设计中必须保持等价。所有测试的改造模式相同：3 个 setter + `IdleWatcher` 构造 → `SpawnPlugin::new(bus)` + `builder.install(plugin)` + `plugin.set_harness_weak(Arc::downgrade(&harness))`。

#### 9.3.1 异步投递——运行期间完成

文件：`async_delivery_integration.rs` → `async_delivery_running_period_sub_completes_during_loop`

**验证链路**：主 agent spawn 子 agent → 子 agent 在主 loop 期间完成 → `SpawnAfterTurn` drain queue → 格式化 → `follow_up_message` 注入 → 主 agent 下一 turn 看到结果。

**断言**：
1. `user_text_seen_by_llm` 包含 `"sub-agent output: 42"` 或 `"completed"` — 子 agent 结果到达 LLM
2. `main_client.call_count >= 3` — follow_up 注入触发额外 LLM 调用

**失败信号**：
- `call_count == 2`：`SpawnAfterTurn` 未 drain queue 或 `follow_up_message` 未投递
- `seen` 不包含结果文本：格式化断裂或 `SubAgentMessageConverter` 未注册

#### 9.3.2 异步投递——Idle 期间完成（wait task 唤醒）

文件：`async_delivery_integration.rs` → `async_delivery_idle_period_watcher_wakes_main`

**验证链路**：主 agent spawn 后进 Idle → `SpawnAfterRun` spawn wait task → 子 agent 完成 → track task 通过 `event_sink` push 事件到 queue + `notify_one` → wait task 收到通知 → `continue_run()` → `BeforeRunHook` drain queue → `run_loop` → LLM 看到结果。

**断言**：
1. `user_text_seen_by_llm` 包含 `"async work done"` 或 `"completed"`
2. `main_client.call_count >= 3` — wait task 唤醒成功

**失败信号**：
- `call_count == 2` 且超时：wait task 未被唤醒。可能原因：`event_sink` 未注入、`Notify` 未触发、`Weak<AgentHarness>` upgrade 失败、`continue_run` Idle 守卫失败
- `seen` 不包含结果：drain 路径断裂

**这是最关键的验证**：覆盖新设计的核心链路 `event_sink → queue → Notify → wait task → continue_run → BeforeRunHook drain → run_loop`。任何一环断裂都会导致 `call_count` 停在 2。

#### 9.3.3 continue_run 预排空优化

文件：`async_delivery_integration.rs` → `continue_run_idle_path_no_wasted_llm_call`

**验证链路**：主 agent spawn 后进 Idle → 子 agent 延迟 50ms 完成 → wait task 唤醒 → `continue_run` → `BeforeRunHook` drain queue 返回 `additional_messages` → LLM 第一轮直接看到结果。

**断言**：
1. `main_client.call_count == 3` — 恰好 3 次（无浪费）
2. `user_text_in_call(client, 2)` 包含 `"async work done"` 或 `"completed"` — 第 3 次调用直接包含结果

**行为变化**：当前 `continue_run` 用 `AsyncSpawnHookHandle::drain_completions` 预排空。新设计用 `BeforeRunHook::additional_messages` 预排空。结果相同——LLM 第一轮看到结果。

**失败信号**：
- `call_count == 4`：`BeforeRunHook` 未 drain，结果在 `after_turn` 才注入（多浪费一轮）
- `call2_text` 不包含结果：drain 时机错误或格式化断裂

#### 9.3.4 abort 级联取消 in-flight sub-agent

文件：`abort_propagation.rs` → `abort_cancels_inflight_subagent`

**验证链路**：主 agent + 子 agent 都阻塞在 BlockingTool 中 → `abort()` → `SpawnOnAbort::on_abort` → `bus.cancel_all()` → 子 agent abort token 被 cancel → BlockingTool 返回 `Aborted` → sub-agent 终止 → `in_flight` 归零。

**断言**：
1. `in_flight_count == 0`（在 3s 内，远短于 30s sleep）
2. `harness.state().phase == Idle`
3. `main_client.call_count == 2`
4. `sub_client.call_count == 1`

**失败信号**：
- `in_flight_count` 3s 后仍为 1：`OnAbortHook` 未调 `cancel_all()` 或 token 未注册
- `phase != Idle`：abort 未正确终止 run

#### 9.3.5 abort 不影响后续 spawn

文件：`abort_propagation.rs` → `abort_does_not_affect_future_spawns`

**验证链路**：第一轮 spawn 慢 sub-agent → 主轮次结束 → abort 取消第一个 sub-agent → 第二轮 prompt + spawn 快 sub-agent → 第二个 sub-agent 正常完成。

**断言**：
1. 第一次 abort 后 `in_flight_count == 0`
2. 第二轮后 `in_flight_count == 0`（正常完成非取消）
3. `main_client.call_count == 4`（两轮各 2 次）
4. `sub_client.call_count == 2`

**这是回归测试**：验证 `OnAbortHook` 的幂等性——`cancel_all()` 只 cancel 当前 in-flight token，不影响后续注册的 token。

**失败信号**：
- 第二轮 spawn 失败：abort 污染了 `MessageBus` 状态（`event_sink` 被清空、`agents` registry 未清理）
- `in_flight` 第二轮后不为 0：第二个 sub-agent 被错误 cancel

#### 9.3.6 Idle 状态下 abort 不重唤醒

文件：`abort_propagation.rs` → `idle_watcher_abort`

**验证链路**：主 agent spawn 后进 Idle → wait task 运行中 → `abort()` → `SpawnOnAbort` 设置 `aborted` 标志 + cancel wait task token + cancel 子 agent → 子 agent 产生 aborted completion → push 到 queue → `AfterRunHook` 检查 `aborted` 标志 → 跳过 wait task → **不调 `continue_run`**。

**断言**：
1. `in_flight_count == 0`（子 agent 被 cancel）
2. `main_client.call_count == 2`（不被 aborted completion 重新唤醒）

**这是竞态安全的核心验证**：验证 `OnAbortHook` → `AfterRunHook` 的时序保证。`OnAbortHook` 在 `abort()` 中同步执行，`aborted` 标志的 `store` 在 `OnAbortHook` 中，`swap` 在 `AfterRunHook` 中。`AfterRunHook` 在 `run_loop` 的 `PhaseGuard` drop 之后调用，而 `abort()` 的 `cancel self token` 导致 `drive_loop` 返回 `Aborted` → `run_loop` cleanup → `AfterRunHook`。时序确定。

**失败信号**：
- `call_count == 3`：aborted completion 触发了 `continue_run`。可能原因：`aborted` 标志未检查、`OnAbortHook` 未在 `AfterRunHook` 之前执行、wait task 未被 cancel

#### 9.3.7 spawn 通信集成

文件：`spawn_communication_integration.rs`

**验证链路**：`MessageBus::send` → 主 agent 事件通过 `event_sink` 推入 queue → `AfterTurnHook` / `BeforeRunHook` drain。

MessageBus-level 测试（直接调 `send`/`wait`/`register`）需要验证：
1. `event_sink` 未设置时 `send` 返回 `Injected`（不 panic，事件丢弃）
2. `event_sink` 设置后事件到达 queue

Harness-level 测试与 9.3.1-9.3.3 一致。

**改造**：`message_bus_pair()` → `MessageBus::new()`。`IdleWatcher` import 删除。

### 9.4 Live test（真实 LLM provider）

文件：`crates/llm-harness-live-tests/examples/11_spawn_subagent.rs`

**验证链路**：真实 LLM 调用 → spawn sub-agent → 子 agent 完成 → 结果投递回主 agent → 主 agent 回复。

**命令**（需 `.env` 配置 API key）：
```bash
cargo run -p llm-harness-live-tests --example 11_spawn_subagent
```

通过条件：程序正常退出，输出包含 sub-agent 结果。
失败信号：超时、panic、sub-agent 结果未出现在主 agent 对话中。

### 9.5 验证执行顺序

每步通过后才进行下一步。步骤 4 是最关键的——集成测试全过即说明行为等价性已验证。

``+pre
1. cargo build --workspace                                         → 编译期门
2. cargo test -p llm-harness-types                                 → hook trait 定义
3. cargo test -p llm-harness-agent --features test-utils           → 单元测试
4. cargo test -p llm-harness-runtime                               → 集成测试（核心）
5. cargo test -p llm-harness-runtime -- spawn_communication        → 通信集成
6. cargo run -p llm-harness-live-tests --example 11_spawn_subagent → 真实 LLM
```

### 9.6 验证矩阵：行为等价性

| 行为 | 当前实现 | 新实现 | 验证测试 |
|------|---------|--------|---------|
| 运行期间子 agent 完成 → 结果注入 | `AsyncSpawnHook::after_turn` drain event channel | `SpawnAfterTurn` drain queue | 9.3.1 |
| Idle 期间子 agent 完成 → 唤醒 | `IdleWatcher::on_idle` recv + re-send + `continue_run` | `SpawnAfterRun` wait task on `Notify` + `continue_run` | 9.3.2 |
| `continue_run` 预排空 | `AsyncSpawnHookHandle::drain_completions` | `BeforeRunHook::additional_messages` | 9.3.3 |
| abort 级联取消子 agent | `AbortCascadeHook::cancel_all_subagents` | `SpawnOnAbort::on_abort` → `bus.cancel_all()` | 9.3.4 |
| abort 不影响后续 spawn | hook 不被 consume（clone 保留） | `Vec<Arc<dyn OnAbortHook>>` 遍历不 consume | 9.3.5 |
| Idle 下 abort 不重唤醒 | 先 cancel watcher 再 cancel sub-agent | `aborted` 标志 + `AfterRunHook` 检查 | 9.3.6 |
| abort hook 生命周期持久 | `clone()` 保留 | `Vec` 不被 take | 9.2.2 |
| `AfterRunHook` 调用时机 | `watcher.on_idle()` in `run_loop` cleanup | `composite.after_run().await` in `run_loop` cleanup | 9.2.3 |
| idle 超时状态提示 | `IdleWatcher` sleep timeout → `set_idle_timeout_fired` → `status_hint` | wait task `tokio::time::sleep` → `set_idle_timeout_fired` → `SpawnAfterTurn::status_hint` | 9.3.2（隐含） |

### 9.7 idle 超时验证（新增测试场景）

当前测试未显式覆盖 idle 超时。新设计应增加一个测试：

场景：主 agent spawn 慢子 agent → 进 Idle → wait task 启动 → `idle_timeout` 设为 100ms → 超时触发 → `set_idle_timeout_fired` → `continue_run` → `SpawnAfterTurn` 检查 `consume_idle_timeout_fired` → 注入状态提示 → LLM 看到 `"N sub-agent(s) still running"`。

**断言**：
1. `main_client.call_count >= 3`（超时唤醒触发额外调用）
2. `user_text_seen_by_llm` 包含 `"still running"` 或 `"query_subagent"`

通过条件：LLM 收到状态提示。
失败信号：超时后未唤醒（`tokio::time::sleep` 未触发或 `set_idle_timeout_fired` 未调）。
