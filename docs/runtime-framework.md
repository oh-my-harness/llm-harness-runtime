# 编排层框架文档（subagents + workflow）

> 定位：在 `types` / `loop` / `agent` / `platform` 之上的**编排层**。
> 不含具体后端实现（sandbox/audit/trace/mcp 各有独立 crate），只定义 sub-agent 派发（`llm-harness-subagents`）、workflow 编排 + task 持久化 + 规则匹配（`llm-harness-workflow`）。横切 trait 直接由 `llm-harness-platform` 提供。

## 1. 在 workspace 中的位置

```
types  ──┐
loop   ──┼──→  agent  ──→  runtime  ──→  sandbox (os/bwrap/seatbelt)
platform ─┘                │           ──→  audit-jsonl
                           │           ──→  trace-otel
                           │           ──→  mcp
                           │           ──→  strategy
                           └──→  knowledge / knowledge-local / memory / session-recall / tools
```

- `types`：纯数据 trait（`Tool`/`ExecutionEnv`/hooks/`CostAggregate`/`PricingProvider`）。
- `loop`：单 agent 的 turn 循环 + LLM client。
- `agent`：`HarnessBuilder` + `AgentHarness` + `Session` + `Plugin` + cost tracking + compaction + skills。
- `platform`：横切 trait 定义（`Sandbox`/`AuditSink`/`TraceExporter`/`PromptSource`）+ `env.rs`。
- **`runtime`（本 crate）**：把 agent 拼装成可运行系统——sub-agent 派发、workflow 编排、规则匹配、task 持久化。从 `platform` re-export 横切 trait。

- `runtime` 不直接依赖任何 sandbox/audit/trace 后端 crate；后端 crate 依赖 `platform` 实现 trait。dev-dependencies 里有 `sandbox` / `audit-jsonl` / `trace-otel`，仅用于测试。

## 2. 目录结构

```
src/
├── lib.rs             # pub mod 声明: rules / workflow / lifecycle + pub use EnvFactory
│                      #（横切 trait 由 llm-harness-platform 直接提供，不再 re-export）
├── rules/             # 规则匹配层（Predicate/Rule/RuleChain/RuleBasedApprovalHook）
│   ├── predicate.rs    # Predicate trait
│   ├── rule.rs         # Rule + Decision
│   ├── chain.rs        # RuleChain + RuleChainBuilder
│   ├── predicates.rs   # 内置 predicate：Contains/NumberRangeField/RegexField/RateLimit
│   └── hook.rs         # RuleBasedApprovalHook（impl BeforeToolCallHook）
├── spawn/              # sub-agent 派发 + 通信
│   ├── spawner.rs      # HarnessSubAgentSpawner（impl SubAgentSpawner）+ EnvFactory/SessionFactory/JsonlSessionFactory
│   ├── message_bus.rs  # MessageBus：统一事件通道 + agent 注册表 + 消息路由
│   ├── delivery.rs     # AsyncSpawnHandle/SubAgentMessageConverter（异步投递辅助）
│   ├── plugin.rs       # SpawnPlugin：通用 hook（before_run/after_turn/after_run/on_abort）+ 完成队列，替代旧 AsyncSpawnHook/IdleWatcher/AbortCascadeHook
│   ├── tools.rs        # LLM 可见 tools：SpawnAgentTool/MessageSubagentTool/AwaitSubagentReplyTool/QuerySubagentTool/AbortSubagentTool/MessageMainTool/AwaitMainMessageTool
│   └── spawn_types.rs  # SpawnOptions/ProviderSelection/ContextMode/SubAgentHandle/SubAgentResult
├── lifecycle/          # task 持久化 + 事件
│   ├── task.rs         # TaskId/TaskError/TaskResult/Checkpoint 纯数据
│   ├── task_store.rs   # TaskStore trait + JsonlTaskStore 参考实现
│   └── event.rs        # EventStream trait + WaitForExternalEventTool
└── workflow/           # 流程编排引擎
    ├── model.rs        # 纯数据模型（Workflow/Step/Edge/WorkflowState/Transition/...）
    ├── engine/         # WorkflowEngine——唯一执行引擎（拆为子模块）
    │   ├── mod.rs       # WorkflowEngine 主体 + run() 主循环
    │   ├── config.rs    # 引擎配置
    │   ├── event.rs     # WorkflowEvent 广播
    │   ├── request.rs   # LLM step 请求构建
    │   ├── runner.rs    # step 执行（run_llm_step / run_executor_step）
    │   ├── structured.rs # SubmitStepResultTool + 结构化结果传递
    │   ├── prompt_context.rs # step prompt 构建
    │   ├── json_parse.rs # JSON 解析辅助
    │   └── tests.rs     # 引擎测试
    ├── executor.rs     # StepExecutor trait + ExecutorCtx（含 event_tx）+ JsonTransformExecutor
    ├── judge.rs        # StepTransitionJudge trait + NoopJudge/EdgeConditionJudge
    ├── gate.rs         # WorkflowGateHook + build_gate_approval_hook（门禁双闸）
    ├── plan.rs         # validate_workflow 公共校验 + PlanWorkflowTool
    └── error.rs        # workflow 错误类型
```

> **注**：`HarnessBuilder` 在 `llm-harness-agent` crate（`crates/llm-harness-agent/src/builder.rs`）。
> 横切 trait（Sandbox/AuditSink/TraceExporter/PromptSource）在 `llm-harness-platform` crate，消费者直接依赖 platform（subagents/workflow 不再 re-export）。
> cost/budget 控制流：`CostAccumulatorHook`/`UsageLedger` 在 `llm-harness-agent/src/harness/`，`BudgetControlAdapter`/`BudgetExceededHook` 在 `llm-harness-strategy/src/budget.rs`。
> `ResourceProvider`/`ResourceInjector` 在 `llm-harness-strategy/src/resource.rs`。

### 2.1 Sub-agent 派发（`spawn/`）
| 模块 | 职责 |
|---|---|
| `spawner.rs` | `HarnessSubAgentSpawner`（impl `SubAgentSpawner`）。`spawn_inner` 统一构建：env → message bus 注册 → sub plugin → session → harness → abort token → tokio task + 事件收集。公开入口：`spawn` / `spawn_async`。还含 `EnvFactory` / `SessionFactory` / `JsonlSessionFactory`。 |
| `message_bus.rs` | `MessageBus`：统一事件通道（`SubAgentEvent` 广播）+ agent 注册表（`AgentEntry`/`AgentStatus`）+ 消息路由（`SendReceipt`）。`EventRxGuard` RAII 借还 event rx。`message_bus_pair()` 工厂。 |
| `delivery.rs` | 异步投递辅助：`AsyncSpawnHandle`（abort 句柄）、`SubAgentMessageConverter`（sub 消息格式转换）。 |
| `plugin.rs` | `SpawnPlugin`（Plugin）：替代旧 AsyncSpawnHook/IdleWatcher/AbortCascadeHook。持有自己的完成队列（VecDeque + Notify），路由 sub-agent 事件经 4 个通用 hook（`before_run`/`after_turn`/`after_run`/`on_abort`）。`new_with_idle_timeout` 配置 idle 超时。 |
| `tools.rs` | LLM 可见 tools：`SpawnAgentTool`（`spawn_agent`，带 `wait` 参数区分 sync/async）、`MessageSubagentTool` / `MessageMainTool`（方向消息发送）、`AwaitSubagentReplyTool` / `AwaitMainMessageTool`（阻塞等待回复）、`QuerySubagentTool`（查 agent 列表，含角色描述）、`AbortSubagentTool`（中止 sub-agent）。 |
| `spawn_types.rs` | `SpawnOptions`（启动配置）、`ProviderSelection`（provider 选择策略）、`ContextMode`（上下文传递方式）、`SubAgentHandle`（join + abort）、`SubAgentResult`（最终答案 + cost）。 |

### 2.2 Workflow 编排（`workflow/`）
| 模块 | 职责 |
|---|---|
| `model.rs` | 纯数据模型：`Workflow`/`Step`（Llm\|Executor）/`Edge`/`EdgeCondition`/`ConditionExpr`/`WorkflowStatus`/`WorkflowState`/`StepResult`/`StepRecord`/`Transition`/`WorkflowContext`。可序列化，走 TaskStore 落盘。 |
| `engine/` | `WorkflowEngine`——**唯一执行引擎**。`new`/`new_free_task`/`restore` 三入口；`run()` 主循环：取 current_step → max_steps 阀 → 构 abort → `run_step_with_policy`（retry/timeout）→ `decide_transition`（judge）→ `apply_transition`（校验+落盘）→ cancel/pause 守卫。`run_llm_step` 每步用 `HarnessBuilder`（来自 `llm-harness-agent`）起新 harness，注入 gate hook + SubmitStepResultTool + SyncSpawnAgentTool。还含 `WorkflowEvent` 广播。 |
| `executor.rs` | 非 LLM step：`StepExecutor` trait + `ExecutorCtx`（含 `event_tx: broadcast::Sender<WorkflowEvent>`，executor 可向订阅者发事件）+ 内置 `JsonTransformExecutor`（`json_transform`）。`shell`/`http_call` 常量定义但实现在 engine 内联。 |
| `judge.rs` | `StepTransitionJudge` trait + `NoopJudge`（自由 task）+ `EdgeConditionJudge`（声明式条件，对 `StepResult.structured` 求 `ConditionExpr`）。 |
| `gate.rs` | 流程门禁双闸：`WorkflowGateHook`（`PrepareNextTurnHook`，按 current_step 收窄工具集）+ `build_gate_approval_hook`（`RuleBasedApprovalHook` 兜底 Deny）。 |
| `plan.rs` | `validate_workflow` 公共校验（来源无关）+ `PlanWorkflowTool`（LLM 产出 plan 落盘）。 |

### 2.3 持久化与生命周期（`lifecycle/`）
| 模块 | 职责 |
|---|---|
| `task.rs` | `TaskId`/`TaskError`/`TaskResult`/`Checkpoint` 纯数据。 |
| `task_store.rs` | `TaskStore` trait：workflow_state / workflow / checkpoints / event_wait 四类持久化。`JsonlTaskStore` 参考实现。单进程独占（`tokio::Mutex`），崩溃恢复=旧进程确认死后新进程 load。 |
| `event.rs` | 通用事件唤醒：`EventStream` trait + `WaitForExternalEventTool`（阻塞等事件，落盘 descriptor 供恢复）。 |

### 2.4 规则匹配（`rules/`）
| 模块 | 职责 |
|---|---|
| `predicate.rs` | `Predicate` trait——规则的条件判定抽象。 |
| `rule.rs` | `Rule` + `Decision`——规则定义与决策结果。 |
| `chain.rs` | `RuleChain` + `RuleChainBuilder`——多规则链式组合。 |
| `predicates.rs` | 内置 predicate：`Contains`/`NumberRangeField`/`RegexField`/`RateLimit`。 |
| `hook.rs` | `RuleBasedApprovalHook`（impl `BeforeToolCallHook`）——把规则链接入工具审批。 |

### 2.5 横切 trait（`llm-harness-platform`）
消费者直接依赖 `llm-harness-platform` 使用以下模块：

| 模块（在 `llm-harness-platform` 中） | 职责 |
|---|---|
| `sandbox.rs` | `Sandbox` trait（start/env/reset/shutdown）+ `SandboxConfig`/`NetRule`/`ResourceLimits`。Bwrap 后端通过 cgroup v2 fail-closed 地执行 CPU/进程/内存上限；磁盘容量上限仍拒绝。 |
| `audit.rs` | `AuditSink` trait + `AuditEntry`（含 SHA-256 hash chain）+ `AuditEventType`/`AuditDecision`/`CostDelta`。 |
| `tracer.rs` | `TraceExporter` trait + `SpanEvent`/`SpanId`/`TraceId`/`SpanKind`/`SpanStatus`/`AttributeValue`。 |
| `tracing_adapter.rs` | 把 core hooks（BeforeProviderRequest/AfterProviderResponse/AfterToolCall/AfterTurn）桥接成 span 事件，写入 `TraceExporter`。 |
| `prompt.rs` | `PromptSource` trait + `PromptTemplateCompiler`（`{{name}}` → `$1` 位置参数）。 |
| `env.rs` | `EnvFactory` + 环境抽象辅助（subagents 通过 `pub use llm_harness_platform::env::EnvFactory` 转出口）。 |

## 3. 核心抽象 trait 清单

runtime 直接定义的对外 trait（业务层/后端 crate 实现）：

```
EnvFactory          SessionFactory         PluginFactory
TaskStore           EventStream
StepExecutor        StepTransitionJudge
```

从 `llm-harness-platform` 直接使用的 trait：

```
Sandbox             AuditSink              TraceExporter
PromptSource
```

> `PricingProvider`/`BudgetExceededHook` 定义在 `types`，`ResourceProvider` 在 `strategy`。
> `McpTransport` trait 定义在 `llm-harness-mcp` crate。

runtime 自己实现的（impl core trait）：
- `HarnessSubAgentSpawner` impl `SubAgentSpawner`
- `WorkflowGateHook` impl `PrepareNextTurnHook`
- `JsonTransformExecutor` impl `StepExecutor`
- `EdgeConditionJudge` / `NoopJudge` impl `StepTransitionJudge`

> `CostAccumulatorHook`（impl `AfterProviderResponseHook`）在 `llm-harness-agent/src/harness/cost_hook.rs`。
> `BudgetControlAdapter`（impl `ShouldStopHook`）在 `llm-harness-strategy/src/budget.rs`。
> `SpanContextMap`（tracing_adapter）impl 一组 core hooks，在 `llm-harness-platform`。

## 4. 关键数据流

### 4.1 Harness 构建（`HarnessBuilder::build`，在 `llm-harness-agent`）
```
install(plugin) ─→ tools/hooks/skills/templates 累积
provider(pattern, client) ─→ ProviderMatcher 入列
build(env)
  ├─ resolve_provider(model): exact → longest-prefix glob → err
  ├─ resolve_compaction(): 同上（可选）
  ├─ build_options():
  │    ├─ 惰性建 cost Arc
  │    ├─ 注入 CostAccumulatorHook → hooks.after_provider_response
  │    └─ 若有 budget → BudgetControlAdapter → hooks.should_stop
  └─ AgentHarness::new_in_memory(client, env, opts)
```

### 4.2 Sub-agent spawn（`spawn_inner`）
```
opts + bus + preregister_abort?
  ├─ 1. 解析 provider（Inherit / Alias）
  ├─ 2. env_factory.create(cwd)
  ├─ 3. session_factory.create() → (storage, session_id)
  ├─ 4. sub_plugin_factory(cwd, bus.clone(), session_id)
  ├─ 5. HarnessBuilder.build_with_session(env, Session::new(storage))
  ├─ 6. harness = Arc::new(harness)
  ├─ 7. 构建 prompt（可选 context 注入）
  ├─ 8. abort_token；bus.register_subagent_abort + bus.register(AgentEntry)
  ├─ 9. tokio::spawn:
  │      harness.set_abort_token
  │      broadcast→mpsc bridge（防 Lagged 死等）
  │      event_task 收集 final_answer/text_delta/tool_count
  │      harness.prompt(&prompt)
  │      drop(bridge) → event_task 收尾
  │      → SubAgentResult { final_answer, tool_calls_count, session_id, cost }
  └─ 返回 (SubAgentHandle, session_id)
```
- `spawn` / `spawn_with_peer`：同步，`preregister_abort=false`，调用方 join。
- `spawn_async` / `spawn_async_with_peer`：`preregister_abort=true`，经 `registry.track()` 返回 `(AsyncSpawnHandle, oneshot::Receiver)`。

### 4.3 Workflow 执行（`WorkflowEngine::run`）
```
run()
  ├─ 重入守卫（running AtomicBool）
  ├─ startup: Idle/Paused→Running；Running→崩溃恢复（回退 current_step 到最后 completed）；terminal→直接返回
  ├─ save workflow + state
  ├─ run_loop():
  │    loop {
  │      取 current_step + history_len
  │      max_steps 阀 → Fail
  │      构 step_abort
  │      run_step_with_policy:
  │        for attempt in 1..=max_attempts:
  │          run_step_attempt（select! abort / step / timeout）
  │          Ok→返回；Err→cancel attempt，retry_backoff 或 bail
  │      run_step:
  │        Llm     → run_llm_step（建 harness + gate hook + submit tool + prompt + 收事件）
  │        Executor→ run_executor_step（StepExecutor.execute(ExecutorCtx)）
  │      decide_transition: 自由 task→Abort；否则 judge.decide(StepCtx)
  │      apply_transition: cancel 守卫 → push StepRecord → 校验 To 合法性 → 更新 state → persist
  │      cancel/pause 守卫
  │      match transition: Fail→fail_task；Abort→Succeeded；To→current_step=next+persist；Retry→重跑
  │    }
```

## 5. 关键设计点

1. **唯一引擎**：`WorkflowEngine` 是唯一执行引擎，自由 task = 单步 workflow（`new_free_task`）。不存在第二套 task 调度路径。
2. **来源无关的 plan**：`Workflow` 可由人类/LLM/外部系统产出，统一过 `validate_workflow` + `TaskStore` 落盘 + `restore` 恢复。
3. **状态单一表达**：`WorkflowState`（status + current_step + step_history + context）覆盖写；`Checkpoint` 是业务层数据快照，append-only，不复制进度。两者不重叠。
4. **门禁双闸**：`WorkflowGateHook`（`PrepareNextTurnHook`，主动收窄工具集）+ `RuleBasedApprovalHook`（`BeforeToolCallHook`，Deny 兜底）。LLM 物理上提不出越界 tool call。
5. **abort 传播**：`CancellationToken` 从 engine `step_abort` → 注入 step harness → sub-agent 的 `handle.abort()` → agent loop。`cancel_all` 先 take 出 map 释放锁再遍历 cancel，避免持锁唤醒 waker。
6. **sub-agent 通信**：`MessageBus` 统一管理 agent 注册表（`AgentEntry`）+ 事件通道（`SubAgentEvent` 广播）+ 消息路由（`send_message` → `SendReceipt`）。`SpawnPlugin`（`plugin.rs`）持有自己的完成队列，经 4 个通用 hook（before_run/after_turn/after_run/on_abort）把 sub 事件接入主 harness——完成通知不再依赖共享 event channel，idle 唤醒内建于 `SpawnAfterRun`。
7. **cost 共享**：`CostAggregate` 用 `Arc<Mutex<...>>` 在 `CostAccumulatorHook` / `BudgetControlAdapter` / `AgentHarnessOptions.cost` 间共享，`usage()` 读 live 状态。
8. **崩溃恢复**：单进程独占，`TaskStore` 无跨进程锁；恢复语义=旧进程确认死后新进程 load + `validate_workflow` 重校验。
9. **后端解耦**：横切 trait（Sandbox/AuditSink/TraceExporter/PromptSource）由 `llm-harness-platform` 直接提供，具体后端在独立 crate 实现，dev-deps 拉来测试。

## 6. 阅读顺序

1. `lib.rs`（模块清单）
2. `lifecycle/task.rs`（基础类型）→ `lifecycle/task_store.rs`（持久化）
3. `spawn/spawner.rs` + `spawn/message_bus.rs` + `spawn/plugin.rs`（sub-agent 派发 + 通信 + SpawnPlugin hook 路由）
4. `workflow/model.rs`（纯数据模型）→ `workflow/judge.rs` → `workflow/gate.rs` → `workflow/plan.rs`（编排外围）
5. `workflow/engine/`（核心，拆为子模块，最后精读 `mod.rs` + `runner.rs`）
6. 横切 trait 模块按需读：`llm-harness-platform`（sandbox/audit/tracer/tracing_adapter/prompt/env）、`llm-harness-strategy`（budget/resource/safety/loop_safety/event_stream 等）
