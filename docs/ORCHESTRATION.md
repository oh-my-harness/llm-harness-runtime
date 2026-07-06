# Runtime 编排能力总览

> 本文档面向使用 WorkflowEngine 的上层 agent 开发者，概述 runtime 提供的编排原语、核心机制和使用方式。

## 1. 一句话定位

runtime 提供"怎么跑流程"的通用基础设施，上层 agent 只负责"做什么"（领域逻辑）。
核心思想对齐 LangGraph：流程是一张图，引擎驱动图遍历，状态在节点间流动，引擎负责持久化与恢复。

## 2. 分层边界

```
上层 Agent（EDA / Coding / ...）       Runtime（WorkflowEngine）
"做什么"（领域逻辑）                    "怎么跑"（编排基础设施）
─────────────────────────────          ─────────────────────────
• 流程定义（pipeline.yaml 等 DSL）      • run_loop 驱动循环
• YAML 解析 → PipelineConfig            • 状态机（Step→Exec→Judge→Trans）
• adapter 翻译 → Workflow              • TaskStore 持久化 / restore()
• 实现 StepExecutor（干活）             • EventStream 事件等待/唤醒
• 实现 StepTransitionJudge（路由）      • WorkflowContext 共享黑板
• 领域 prompt / 工具                    • Transition 语义 + 安全阀
```

**关键原则：runtime 不执行领域逻辑。** 不调 PanGen、不跑 beam search、不解析光刻结果。
领域逻辑通过 trait 回调注入，runtime 一行不改即可服务不同 agent。

## 3. 核心数据结构

### 3.1 Workflow（流程定义，纯数据）

```rust
pub struct Workflow {
    pub steps: Vec<Step>,       // 所有步骤
    pub edges: Vec<Edge>,       // 步骤间的边（条件/无条件）
    pub entry_step: StepId,     // 入口步骤
}

pub enum Step {
    Llm { id, name, prompt, allowed_tools, ... },       // LLM step（AgentHarness 单轮）
    Executor { id, name, executor_name, config, ... },  // 非 LLM step（确定性执行）
}

pub struct Edge {
    pub from: StepId,
    pub to: StepId,
    pub condition: EdgeCondition,   // 无条件 / Label(route_key) / 表达式
}
```

来源无关：人类规划 / LLM 生成 / 外部系统产出，都走此结构。runtime 不绑定任何 DSL。

### 3.2 WorkflowState（运行时状态，落盘）

```rust
pub struct WorkflowState {
    pub status: WorkflowStatus,         // Idle/Running/Paused/Succeeded/Failed/Cancelled
    pub current_step: StepId,           // 当前执行步骤（断点续跑的权威来源）
    pub step_history: Vec<StepRecord>,  // 已完成步骤审计链
    pub context: WorkflowContext,       // 共享可变 KV 黑板
    pub planned_by: Option<TaskId>,     // 两阶段 task 关联
    // ... 时间戳、result、error 等
}
```

### 3.3 Transition（跳转决策）

```rust
pub enum Transition {
    To(StepId),             // 跳到指定步骤（含回跳到旧 step = 返工回流）
    Retry,                  // 重跑当前步（带 max_retries 上限）
    Fail { reason: String },// 标记失败
    Abort { reason: String },// 终止流程（正常结束）
}
```

## 4. 编排原语

### 4.1 执行循环（run_loop）

```
engine.run()
  └─ run_loop:
       1. 从 WorkflowState 读 current_step
       2. executor.execute(ctx)  →  StepResult     // 干活
       3. judge.decide(ctx)      →  Transition      // 路由
       4. apply_transition:
            To(target)  → current_step = target, persist
            To(old)     → 回跳返工（回流）
            Retry       → current_step 不变, retry_count++
            Fail        → status = Failed
            Abort       → status = Succeeded
       5. persist WorkflowState（current_step + step_history + context）
       6. emit WorkflowEvent
       7. loop until terminal
```

### 4.2 回流 / 返工（对齐 LangGraph back-edge）

`Transition::To(上游step)` 是回跳原语，等价于 LangGraph 的 conditional edge 指向上游节点。

- **回跳合法性校验**：`To(next)` 要求存在 `edge(from=current, to=next)` 或 `next == entry_step`（入口无条件放行）。非法跳转直接 Failed。
- **循环计数**：业务层在 `StepResult.structured` 里放 `loop_counter`，judge 读出来做条件路由（等价 LangGraph state 里的 iteration 字段）。
- **重试上限**：`Transition::Retry` 受 `max_retries` 约束，超限 → Failed。
- **防死循环**：`with_max_steps(N)` 总步数安全阀（默认 500）。

### 4.3 路由判定（对齐 LangGraph conditional edge）

```rust
pub trait StepTransitionJudge: Send + Sync {
    fn decide<'a>(&'a self, ctx: &StepCtx<'a>) -> BoxFuture<'a, Transition>;
}
```

Judge 收到 `StepCtx`（含当前步定义、执行结果、step_history、共享 context），返回 `Transition`。
等价于 LangGraph 的 conditional edge 函数：读 state → 决定 next node。

内置 `NoopJudge`：自由 task（单步 workflow）用，`run()` 检测无后继 step 自动结束。

### 4.4 领域执行（StepExecutor）

```rust
pub trait StepExecutor: Send + Sync {
    fn execute<'a>(&'a self, ctx: &ExecutorCtx<'a>) -> BoxFuture<'a, Result<StepResult>>;
}
```

Executor 收到 `ExecutorCtx`（含 step 定义、prev_result、env、abort token、共享 context），返回 `StepResult`。

内置 executor：
- `json_transform` — JSON 变换（确定性数据处理）
- `shell` — 执行 shell 命令
- `http_call` — HTTP 请求

### 4.5 共享状态（对齐 LangGraph State）

```rust
pub struct WorkflowContext {
    pub variables: serde_json::Map<String, Value>,  // 跨 step 可读写 KV 黑板
}
```

- 随 `WorkflowState` 一起持久化到 TaskStore
- 运行时包装为 `Arc<Mutex<WorkflowContext>>` 共享
- Executor 可读写，Judge 只读
- **与 LangGraph 的差异**：我们是 mutable blackboard，LangGraph 是 immutable state + reducer。
  这是有意设计——EDA 场景需要大量共享中间变量，blackboard 更灵活。

### 4.6 持久化（TaskStore）

```rust
pub trait TaskStore: Send + Sync {
    async fn save_workflow(&self, task_id: &TaskId, wf: &Workflow) -> Result<()>;
    async fn load_workflow(&self, task_id: &TaskId) -> Result<Option<Workflow>>;
    async fn save_workflow_state(&self, task_id: &TaskId, state: &WorkflowState) -> Result<()>;
    async fn load_workflow_state(&self, task_id: &TaskId) -> Result<Option<WorkflowState>>;
    // + checkpoint / event_wait
}
```

参考实现 `JsonlTaskStore`：临时文件 + rename 原子写，四类数据落盘（workflow 定义 / 运行状态 / checkpoint / event_wait）。

### 4.7 崩溃恢复

```
engine.restore(task_id)
  └─ 从 TaskStore 加载 WorkflowState
     ├─ status = Running  → 崩溃恢复，从 current_step 续跑
     ├─ status = Paused   → 等业务层重建 EventStream 后 resume()
     ├─ status = Failed   → resume() 重置为 Paused 后续跑
     └─ status = Succeeded→ 终态，不恢复
```

- **精确续跑**：从 `current_step` 恢复，不靠文件扫描重放
- **状态完整**：`step_history` + `WorkflowContext` 都从盘上恢复
- **事务性**：原子写，状态和实际进度不会不一致

### 4.8 事件唤醒

```rust
pub trait EventStream: Send {
    async fn wait(&self, descriptor: &Value) -> Result<Value>;
}
```

- Judge 返回等待事件 → status = Paused，存 descriptor 到 TaskStore
- 外部触发事件 → `EventStream.send(event)`
- `engine.resume()` → 从 current_step 续跑
- 崩溃后：`load_event_wait` 读回 descriptor，重建 EventStream，调 `resume()`

### 4.9 事件流（进度可见性）

```rust
pub enum WorkflowEvent {
    StepStarted { step_id, step_name },
    StepFinished { step_id, result },
    Paused { reason },
    Resumed,
    Cancelled { reason },
    Failed { error },
}
```

`engine.subscribe()` → `broadcast::Receiver<WorkflowEvent>`，上层 agent 可实时监听进度。

### 4.10 结构化结果传递

内置 `SubmitStepResultTool`：step agent 提交 JSON → 填入 `StepResult.structured` → judge 直接读。
步骤间结构化数据传递不需要自己经 TaskStore 反查 checkpoint。

## 5. 与 LangGraph 对照

| 能力 | LangGraph | Runtime WorkflowEngine | 对齐？ |
|------|-----------|----------------------|--------|
| 图模型 | StateGraph(node, edge) | Workflow(steps, edges) | ✅ |
| 路由 | conditional_edge 函数 | StepTransitionJudge trait | ✅ |
| 回流 | edge 指向上游 | Transition::To(上游) | ✅ |
| 重试 | 用户在 edge 里写 | Transition::Retry + max_retries | ✅ |
| 循环计数 | state 字段 | structured.loop_counter | ✅ |
| 状态模型 | immutable + reducer | mutable blackboard | ⚠️ 有意差异 |
| 持久化 | checkpointer | TaskStore | ✅ |
| 恢复 | get_state + invoke | restore() + resume() | ✅ |
| 事件等待 | interrupt | EventStream + WaitForEvent | ✅ |
| 并行 fan-out | Send API | 待实现（issue #37） | ❌ |
| 子图 | subgraph | 待实现（issue #38） | ❌ |
| 时间旅行 | get_state_history | 待实现（issue #39） | ❌ |
| State reducer | reducer 函数 | 待实现（issue #40） | ❌ |

## 6. 上层 Agent 接入方式

以 EDA Agent 为例（`eda-agent/src/workflow_adapter/`）：

```
EDA 领域概念            adapter 翻译               runtime 抽象
─────────────         ──────────────────         ──────────────
pipeline.yaml  →  converter  →  Workflow {steps, edges}
StageConfig    →  converter  →  Step::Executor
routes         →  converter  →  Edge
execute_stage  →  executor   →  StepExecutor trait
resolve_next   →  judge      →  StepTransitionJudge trait
route_key      →  judge      →  Transition
StageContext   →  executor   →  WorkflowContext
```

接入步骤：
1. **converter**：领域 DSL → `Workflow{steps, edges, entry_step}`
2. **executor**：实现 `StepExecutor`，调用已有领域逻辑
3. **judge**：实现 `StepTransitionJudge`，根据结果返回 `Transition`
4. **启动**：`WorkflowEngine::new(workflow, config, judge)` → `engine.run()`
5. **恢复**：`scan_for_resumable_task()` → `WorkflowEngine::restore()` → `engine.run()`

## 7. 已知限制 / 待实现

见 GitHub issues：
- #37 并行 fan-out / fan-in（LangGraph Send API）
- #38 子图嵌套
- #39 时间旅行（历史 checkpoint 恢复）
- #40 State reducer（并行分支 state 合并）
- #41 Step 级结果缓存（通用 done marker）
- #42 Judge 主动暂停（对齐 LangGraph interrupt）
