# llm-harness-runtime 框架文档

> 定位：在 `types` / `loop` / `agent` 之上的**平台层**。
> 不含具体后端实现（sandbox/audit/trace/mcp 各有独立 crate），只定义 trait + 编排逻辑 + 默认参考实现。

## 1. 在 workspace 中的位置

```
types  ──┐
loop   ──┼──→  runtime  ──→  runtime-sandbox-os / bwrap / seatbelt
agent  ──┘                ──→  runtime-audit-jsonl
                          ──→  runtime-trace-otel
                          ──→  runtime-mcp
                          ──→  ffi
```

- `types`：纯数据 trait（`Tool`/`ExecutionEnv`/hooks/`CostAggregate`/`PricingProvider`）。
- `loop`：单 agent 的 turn 循环 + LLM client。
- `agent`：`AgentHarness` + `Session` + `Plugin` + `SubAgentSpawner` trait。
- **`runtime`（本 crate）**：把 agent 拼装成可运行系统——harness 构建、sub-agent 派发、workflow 编排、cost/budget/audit/trace/sandbox 的 trait 与粘合层。

`runtime` 不直接依赖任何 sandbox/audit/trace 后端 crate；后端 crate 依赖 runtime 实现 trait。dev-dependencies 里有 `runtime-sandbox-os` / `runtime-audit-jsonl` / `runtime-trace-otel`，仅用于测试。

## 2. 目录结构

```
src/
├── lib.rs
├── builder.rs          # HarnessBuilder——所有路径汇聚点，留顶层
├── platform/           # 横切 trait + adapter（后端 crate 实现的接口）
│   ├── sandbox.rs      # Sandbox trait + SandboxConfig/NetRule/ResourceLimits
│   ├── resource.rs     # ResourceProvider trait + ResourceInjector
│   ├── prompt.rs       # PromptSource trait + PromptTemplateCompiler
│   ├── mcp.rs          # McpClientHandle trait + Mcp* 类型
│   ├── tool_registry.rs# ToolRegistry/ToolSource trait + ToolDefinition
│   └── tool_adapter.rs # ToolSourceAdapter：ToolDefinition → core Tool
├── observability/      # 可观测 + 安全审计
│   ├── audit.rs        # AuditSink trait + AuditEntry（SHA-256 hash chain）
│   ├── tracer.rs       # TraceExporter trait + Span* 类型
│   └── tracing_adapter.rs # core hooks → span 事件桥接
├── control/            # 运行时控制流：成本/预算/审批
│   ├── cost.rs         # 从 types re-export CostAggregate/ModelCost/PricingProvider/TokenPrice
│   ├── cost_hook.rs    # CostAccumulatorHook（AfterProviderResponseHook）
│   ├── budget.rs       # BudgetExceededHook trait + BudgetControlAdapter（ShouldStopHook）
│   └── human_approval.rs # HumanApprover trait + HumanApprovalWrapper（BeforeToolCallHook）
├── spawn/              # sub-agent 派发 + peer 通信
│   ├── spawner.rs      # HarnessSubAgentSpawner（impl SubAgentSpawner）+ EnvFactory/SessionFactory/JsonlSessionFactory
│   ├── async_spawn.rs  # AsyncSpawnRegistry/AsyncSpawnHandle/async_spawn_pair
│   ├── spawn_tool.rs   # SpawnAgentTool（LLM 可见的 spawn_agent tool）
│   └── peer.rs         # PeerDirectory/SharedPeerRx/PeerMessageHook/WaitForPeerMessageTool/SendToPeerTool
├── lifecycle/          # task 持久化 + 事件
│   ├── task.rs         # TaskId/TaskError/TaskResult/Checkpoint 纯数据
│   ├── task_store.rs   # TaskStore trait + JsonlTaskStore 参考实现
│   └── event.rs        # EventStream trait + WaitForExternalEventTool
└── workflow/           # 流程编排引擎
    ├── model.rs        # 纯数据模型（Workflow/Step/Edge/WorkflowState/Transition/...）
    ├── engine.rs       # WorkflowEngine——唯一执行引擎（~3000 行）
    ├── executor.rs     # StepExecutor trait + JsonTransformExecutor
    ├── judge.rs        # StepTransitionJudge trait + NoopJudge/EdgeConditionJudge
    ├── gate.rs         # WorkflowGateHook + build_gate_approval_hook（门禁双闸）
    └── plan.rs         # validate_workflow 公共校验 + PlanWorkflowTool
```

### 2.1 Harness 构建
| 模块 | 职责 |
|---|---|
| `builder.rs` | `HarnessBuilder`：fluent API 收集 tools/hooks/skills/templates/providers/pricing/budget/compaction_model，解析 provider（精确 + 最长前缀 glob），惰性创建共享 `CostAggregate`，注入 `CostAccumulatorHook` + 可选 `BudgetControlAdapter`，产出 `AgentHarness`。 |

### 2.2 Sub-agent 派发（`spawn/`）
| 模块 | 职责 |
|---|---|
| `spawner.rs` | `HarnessSubAgentSpawner`（impl `SubAgentSpawner`）。`spawn_inner` 统一构建：env → peer 注册 → sub plugin → session → PeerMessageHook → harness → abort token → tokio task + 事件收集。4 个公开入口：`spawn` / `spawn_with_peer` / `spawn_async` / `spawn_async_with_peer`。还含 `EnvFactory` / `SessionFactory` / `PluginFactory` / `JsonlSessionFactory`。 |
| `async_spawn.rs` | 异步投递：`AsyncSpawnRegistry`（completion channel + in-flight 计数 + abort map + `cancel_all`）、`AsyncSpawnHandle`、`CompletionRxGuard`、`async_spawn_pair()`。 |
| `spawn_tool.rs` | `SpawnAgentTool`：LLM 可见的 `spawn_agent` tool，带 `wait` 参数区分 sync/async 语义。 |
| `peer.rs` | Sub↔sub 通信：`PeerDirectory`（agent_id → tx）、`SharedPeerRx`、`PeerMessageHook`（should_stop 时 `try_recv`）、`WaitForPeerMessageTool`、`SendToPeerTool`、`PeerCleanupGuard`。 |

### 2.3 Workflow 编排（`workflow/`）
| 模块 | 职责 |
|---|---|
| `model.rs` | 纯数据模型：`Workflow`/`Step`（Llm\|Executor）/`Edge`/`EdgeCondition`/`ConditionExpr`/`WorkflowStatus`/`WorkflowState`/`StepResult`/`StepRecord`/`Transition`/`WorkflowContext`。可序列化，走 TaskStore 落盘。 |
| `engine.rs` | `WorkflowEngine`——**唯一执行引擎**。`new`/`new_free_task`/`restore` 三入口；`run()` 主循环：取 current_step → max_steps 阀 → 构 abort → `run_step_with_policy`（retry/timeout）→ `decide_transition`（judge）→ `apply_transition`（校验+落盘）→ cancel/pause 守卫。`run_llm_step` 每步用 `HarnessBuilder` 起新 harness，注入 gate hook + SubmitStepResultTool + SyncSpawnAgentTool。还含 `WorkflowEvent` 广播。 |
| `executor.rs` | 非 LLM step：`StepExecutor` trait + `ExecutorCtx` + 内置 `JsonTransformExecutor`（`json_transform`）。`shell`/`http_call` 常量定义但实现在 engine 内联。 |
| `judge.rs` | `StepTransitionJudge` trait + `NoopJudge`（自由 task）+ `EdgeConditionJudge`（声明式条件，对 `StepResult.structured` 求 `ConditionExpr`）。 |
| `gate.rs` | 流程门禁双闸：`WorkflowGateHook`（`PrepareNextTurnHook`，按 current_step 收窄工具集）+ `build_gate_approval_hook`（`RuleBasedApprovalHook` 兜底 Deny）。 |
| `plan.rs` | `validate_workflow` 公共校验（来源无关）+ `PlanWorkflowTool`（LLM 产出 plan 落盘）。 |

### 2.4 持久化与生命周期（`lifecycle/`）
| 模块 | 职责 |
|---|---|
| `task.rs` | `TaskId`/`TaskError`/`TaskResult`/`Checkpoint` 纯数据。 |
| `task_store.rs` | `TaskStore` trait：workflow_state / workflow / checkpoints / event_wait 四类持久化。`JsonlTaskStore` 参考实现。单进程独占（`tokio::Mutex`），崩溃恢复=旧进程确认死后新进程 load。 |
| `event.rs` | 通用事件唤醒：`EventStream` trait + `WaitForExternalEventTool`（阻塞等事件，落盘 descriptor 供恢复）。 |

### 2.5 平台层 trait（`platform/`）
| 模块 | 职责 |
|---|---|
| `sandbox.rs` | `Sandbox` trait（start/env/reset/shutdown）+ `SandboxConfig`/`NetRule`/`ResourceLimits`。 |
| `resource.rs` | `ResourceProvider` trait + `ResourceInjector`（把 resource 内容拼到 system prompt 尾部）。 |
| `prompt.rs` | `PromptSource` trait + `PromptTemplateCompiler`（`{{name}}` → `$1` 位置参数）。 |
| `mcp.rs` | `McpClientHandle` trait + `McpToolDef`/`McpResult`/`McpContent`/`McpError`。 |
| `tool_registry.rs` | `ToolRegistry` trait + `ToolSource` trait + `ToolDefinition`。 |
| `tool_adapter.rs` | `ToolSourceAdapter`：把 `ToolDefinition` 适配成 core `Tool`（MCP 工具用 mcp crate 的 `McpToolAdapter`）。 |

### 2.6 可观测 + 审计（`observability/`）
| 模块 | 职责 |
|---|---|
| `audit.rs` | `AuditSink` trait + `AuditEntry`（含 SHA-256 hash chain）+ `AuditEventType`/`AuditDecision`/`CostDelta`。 |
| `tracer.rs` | `TraceExporter` trait + `SpanEvent`/`SpanId`/`TraceId`/`SpanKind`/`SpanStatus`/`AttributeValue`。 |
| `tracing_adapter.rs` | 把 core hooks（BeforeProviderRequest/AfterProviderResponse/AfterToolCall/AfterTurn）桥接成 span 事件，写入 `TraceExporter`。 |

### 2.7 运行时控制流（`control/`）
| 模块 | 职责 |
|---|---|
| `cost.rs` | 从 types re-export `CostAggregate`/`ModelCost`/`PricingProvider`/`TokenPrice`。 |
| `cost_hook.rs` | `CostAccumulatorHook`（`AfterProviderResponseHook`），写共享 `Arc<Mutex<CostAggregate>>`。 |
| `budget.rs` | `BudgetExceededHook` trait（业务层）+ `BudgetControlAdapter`（`ShouldStopHook`，读 cost → 超限问 hook）。surveillance 模式只统计不停。 |
| `human_approval.rs` | `HumanApprover` trait + `HumanApprovalWrapper`（`BeforeToolCallHook`，默认 timeout=Deny fail-secure）。 |

### 2.8 杂项
| 模块 | 职责 |
|---|---|
| `lib.rs` | `pub mod` 声明：顶层 `builder`/`workflow` + 5 个分组 `control`/`lifecycle`/`observability`/`platform`/`spawn`。无 re-export，路径即物理路径。错误处理直接用 `anyhow::Result`，无独立 error 模块。 |
## 3. 核心抽象 trait 清单

runtime 定义的对外 trait（业务层/后端 crate 实现）：

```
EnvFactory          SessionFactory         PluginFactory
TaskStore           EventStream
PricingProvider*    BudgetExceededHook     HumanApprover
AuditSink           TraceExporter
Sandbox             ResourceProvider       PromptSource
ToolRegistry        ToolSource             McpClientHandle
StepExecutor        StepTransitionJudge
```
> `PricingProvider` 实际定义在 types，runtime re-export。

runtime 自己实现的（impl core trait）：
- `HarnessSubAgentSpawner` impl `SubAgentSpawner`
- `CostAccumulatorHook` impl `AfterProviderResponseHook`
- `BudgetControlAdapter` impl `ShouldStopHook`
- `HumanApprovalWrapper` impl `BeforeToolCallHook`
- `WorkflowGateHook` impl `PrepareNextTurnHook`
- `SpanContextMap`（tracing_adapter）impl 一组 core hooks
- `JsonTransformExecutor` impl `StepExecutor`
- `EdgeConditionJudge` / `NoopJudge` impl `StepTransitionJudge`

## 4. 关键数据流

### 4.1 Harness 构建（`HarnessBuilder::build`）
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
opts + peer_meta? + preregister_abort?
  ├─ 1. 解析 provider（Inherit / Alias）
  ├─ 2. env_factory.create(cwd)
  ├─ 3. 若 peer_meta + directory: directory.register(meta) → peer_rx
  ├─ 4. sub_plugin_factory(cwd, peer_rx, agent_id)
  ├─ 5. session_factory.create() → (storage, session_id)
  ├─ 6. PeerMessageHook::new(peer_rx)（若有）
  ├─ 7. HarnessBuilder.build_with_session(env, Session::new(storage))
  ├─ 8. peer: hook.set_follow_up_handle + PeerCleanupGuard
  ├─ 9. abort_token；若 preregister → registry.register_subagent_abort
  ├─ 10. tokio::spawn:
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
6. **peer 通信**：`SharedPeerRx = Arc<tokio::sync::Mutex<Receiver>>`，hook（`try_lock`）和 tool（`lock().await`）不会并发（agent loop 单线程）。`PeerCleanupGuard` drop 触发幂等 unregister。
7. **事件收集防死等**：sub-agent task 内 broadcast→mpsc bridge，`recv()` Err（Closed/Lagged）直接退出，让 mpsc_tx drop，event_task 经 `recv()==None` 收尾。
8. **cost 共享**：`CostAggregate` 用 `Arc<Mutex<...>>` 在 `CostAccumulatorHook` / `BudgetControlAdapter` / `AgentHarnessOptions.cost` 间共享，`usage()` 读 live 状态。
9. **崩溃恢复**：单进程独占，`TaskStore` 无跨进程锁；恢复语义=旧进程确认死后新进程 load + `validate_workflow` 重校验。
10. **后端解耦**：runtime 只定义 trait（Sandbox/AuditSink/TraceExporter/McpClientHandle/...），具体后端在独立 crate 实现，dev-deps 拉来测试。

## 6. 阅读顺序建议

1. `lib.rs`（模块清单）→ `control/cost.rs` / `lifecycle/task.rs`（基础类型）
2. `builder.rs`（HarnessBuilder 是所有路径的汇聚点）
3. `spawn/spawner.rs` + `spawn/async_spawn.rs` + `spawn/peer.rs`（sub-agent 派发 + 通信）
4. `workflow/model.rs`（纯数据模型）→ `workflow/judge.rs` → `workflow/gate.rs` → `workflow/plan.rs`（编排外围）
5. `workflow/engine.rs`（核心，~3000 行，最后精读）
6. 横切 trait 模块按需读：`control/`（cost_hook/budget/human_approval）、`observability/`（audit/tracer/tracing_adapter）、`platform/`（sandbox/resource/prompt/mcp/tool_registry/tool_adapter）
