# Non-LLM Step 支持设计

> **状态：初版已实现**。当前已支持业务注入 `StepExecutor`，并由
> `WorkflowEngine` 在 LLM step 与 executor step 之间分发。内置
> `JsonTransformExecutor` 已可用；`HttpCall` / `Script` executor 仍属后续扩展。

## 背景与动机

### 当前架构

workflow 引擎（`crates/llm-harness-runtime/src/workflow/`）的初始执行模型：

- `WorkflowEngine::run_loop` 遍历 step，每个 step 调 `run_step`
- `run_step` 做的事：构造 `AgentHarness` → `harness.prompt()` → 收集事件流 → 产出 `StepResult`
- 即每个 step 的执行 = 一次完整的 LLM agent 会话
- step 间跳转由 `StepTransitionJudge::decide` 决定（异步 trait，目前唯一实现是 `NoopJudge`）

核心数据模型：

- `Step`：结构体，含 `id`、`name`、`prompt`、`allowed_tools`
- `Workflow`：`steps: Vec<Step>` + `edges: Vec<Edge>` + `entry_step`
- `StepResult`：单步执行结果（`output`、`structured`、`cost` 等）
- `Transition`：`To(StepId)` / `Retry` / `Abort`
- `StepCtx`：传给 judge 的上下文（`current_step`、`last_result`、`step_history`）

### 问题

初始架构隐含假设：**每一步都需要 LLM 推理**。但真实 workflow 里很多步骤不需要：

1. **纯数据变换**：提取/映射/聚合上一步的 `structured` 结果。让 LLM 做既浪费 token 又不可靠。
2. **外部系统调用**：调 HTTP API、查 DB、跑脚本。确定性 IO，不需要 LLM 多轮工具选择。
3. **确定性条件路由**：基于结构化结果做条件跳转，替代 LLM judge。
4. **编排胶水**：合并多个 step 输出、格式化报告。纯数据操作。

强行用 LLM step 做这些事的问题：
- **成本和延迟**：每次构造 harness + 发 prompt，即使只是 "提取 JSON 字段"。
- **可靠性**：LLM 对确定性操作结果不稳定，中间步骤应保证可复现。

## 设计决策

### 方案选择：Step 改为 Enum（方案 A 演进）

 brainstorming 阶段评估了三种方案：

| 方案 | 描述 | 结论 |
|------|------|------|
| A: `Step` 加 `kind` 字段 | `kind: StepKind` 枚举分发 | 用户建议直接用 enum 更干净 |
| B: Step 改 trait 多态 | LLM/Executor 各自实现 trait | 破坏序列化模型，影响面大，不推荐 |
| C: 执行塞进 judge | judge.decide 同时做执行+跳转 | 语义错位，无法避免 LLM 开销，不推荐 |

最终方案：**`Step` 本身改为 enum**，`Llm` 和 `Executor` 作为变体。比 `kind` 字段更符合 Rust 表达习惯——变体只携带自己需要的字段。

### 跳转决策

- executor 产出 `StepResult` 后，**仍由 `judge.decide` 决定跳转**。
- executor 不返回 `Transition`，职责单一：执行 + 产出结果。
- judge 可以是纯函数逻辑（确定性路由）或 LLM 逻辑，保持现有 trait 接口不变。

### Executor 定义模型

- 定义 `StepExecutor` trait，业务层实现。
- 内置几个常用实现（HttpCall、Script、JsonTransform）开箱即用。
- executor 通过 `config: Option<serde_json::Value>` 携带参数，executor 自己解析 schema。

## 数据模型设计

### Step 改为 Enum

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Step {
    /// LLM step：构造 harness + prompt + 事件收集（现有行为）。
    Llm {
        id: StepId,
        name: String,
        prompt: String,
        allowed_tools: Vec<String>,
    },
    /// Non-LLM step：调用注入的 executor，不构造 harness。
    Executor {
        id: StepId,
        name: String,
        /// executor 标识名（如 "http_call"），运行时由引擎注入 Arc<dyn StepExecutor>。
        executor_name: String,
        /// executor 参数（JSON），如 HTTP url/method/body 或脚本命令。
        /// 序列化友好，executor 自己解析。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<serde_json::Value>,
    },
}
```

**关键点**：

1. **`#[serde(tag = "kind")]`**：序列化带 `kind: "llm"` / `kind: "executor"` 标签。
2. **兼容旧 LLM step**：新写出的 `Step` 会带 `kind`，但反序列化兼容旧的无 `kind` LLM step JSON，避免已有 TaskStore 存档和调用方计划直接失效。
3. **executor step 不需要 `prompt` / `allowed_tools`**：enum 变体只携带自己需要的字段。
4. **`config: Option<serde_json::Value>`**：executor 自定义 schema，引擎不需要知道参数细节，保持开放扩展。

### 辅助方法

```rust
impl Step {
    pub fn id(&self) -> &StepId {
        match self {
            Step::Llm { id, .. } | Step::Executor { id, .. } => id,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            Step::Llm { name, .. } | Step::Executor { name, .. } => name,
        }
    }
}
```

### StepExecutor trait

新增文件 `crates/llm-harness-runtime/src/workflow/executor.rs`：

```rust
/// Non-LLM step 的执行体。业务层实现，也可用内置实现。
pub trait StepExecutor: Send + Sync {
    /// 执行 non-LLM step，返回 StepResult。
    ///
    /// ctx.current_step 是 `Step::Executor` 变体，executor 从中读取 config。
    /// ctx.prev_result 是上一步的 StepResult（若有）。
    /// executor 应把结果写入 StepResult.structured（JSON），judge 据此决定跳转。
    async fn execute(&self, ctx: &ExecutorCtx<'_>) -> Result<StepResult>;
}
```

- 复用 `StepResult`——格式与 LLM step 一致，`step_history`、审计链、cost 聚合不变。
- executor 产出的 `StepResult` 中：`output` 填人类可读摘要，`structured` 填 JSON 结果，`tool_calls_count` 为 0，`cost` 为 `CostAggregate::default()`。

### ExecutorCtx（新增）

**注意**：executor 的上下文与 judge 的 `StepCtx` 不同。`StepCtx.last_result` 语义是
"当前步执行完的结果"（传给 judge 做跳转决策，在 `decide_transition` 中构造），
而 executor 在执行**之前**就需要上下文，此时还没有当前步结果。
因此引入独立的上下文类型：

```rust
/// 传给 StepExecutor::execute 的上下文。
pub struct ExecutorCtx<'a> {
    /// 当前步定义。executor 从 Step::Executor.config 读取自己的参数。
    pub current_step: &'a Step,
    /// 上一步的执行结果（若有）。workflow 第一步时为 None。
    pub prev_result: Option<&'a StepResult>,
    /// 已完成的步骤历史（审计链）。
    pub step_history: &'a [StepRecord],
}
```

**设计决策**：不复用 `StepCtx`，因为 `StepCtx.last_result: &StepResult`（非 Option）
的语义在 executor 场景不成立。引入 `ExecutorCtx` 更准确，
`prev_result: Option<&StepResult>` 明确表达 "可能没有前驱结果"。

## 引擎执行流程

### run_loop 分发

```rust
// 现有：无条件调 run_step
let step_result = match self.run_step(&step, step_abort.clone()).await { ... };

// 改为：按 step 类型分发
let step_result = match &step {
    Step::Llm { .. } => self.run_llm_step(&step, step_abort.clone()).await,
    Step::Executor { .. } => self.run_executor_step(&step, step_abort.clone()).await,
};
```

### run_llm_step

现有 `run_step` 原封不动搬过来改名为 `run_llm_step`。唯一变更：内部访问 `step.prompt` / `step.allowed_tools` 改成 pattern match。

### run_executor_step（新增）

```rust
async fn run_executor_step(&self, step: &Step, abort: CancellationToken) -> Result<StepResult> {
    let Step::Executor { executor_name, .. } = step else { unreachable!() };

    // 1. 查 executor 映射
    let executor = self.step_executors.get(executor_name)
        .ok_or_else(|| anyhow!("executor '{}' not registered", executor_name))?;

    // 2. 构造 ExecutorCtx（prev_result = 上一步的结果，第一步时为 None）
    let history = self.workflow_state.lock().await.step_history.clone();
    let prev_result = history.last().and_then(|r| r.result.as_ref());
    let ctx = ExecutorCtx { current_step: step, prev_result, step_history: &history };

    // 3. abort 感知：select! 包裹 executor.execute
    let started_at = chrono::Utc::now();
    let result = tokio::select! {
        biased;
        _ = abort.cancelled() => anyhow::bail!("executor step aborted"),
        r = executor.execute(&ctx) => r?,
    };
    let ended_at = chrono::Utc::now();

    // 4. 补全 StepResult 时间戳
    Ok(StepResult {
        started_at: Some(started_at),
        ended_at: Some(ended_at),
        ..result
    })
}
```

### 关键设计决策

1. **abort 传播**：executor step 用 `select!` 包裹 `executor.execute()`。LLM step 的 abort 通过 `harness.set_abort_token` 实现；executor 没有该机制，用 select! 做外层取消。executor 内部做长 IO（如 HTTP 请求）时应自己感知 cancel 或用带 timeout 的 client。

2. **executor 注册**：`WorkflowEngine` 新增字段 `step_executors: HashMap<String, Arc<dyn StepExecutor>>`。builder 方法：
   ```rust
   pub fn with_executor(mut self, name: &str, executor: Arc<dyn StepExecutor>) -> Self {
       self.step_executors.insert(name.into(), executor);
       self
   }
   ```
   与现有 `with_step_plugin` / `with_tool` 同模式。

3. **`step_tools_cache`**：构造逻辑改为 match——executor step 无 `allowed_tools`，映射为空 Vec。该 cache 只给 gate 用，executor step 不走 gate，空 Vec 正确。

4. **cost 聚合**：executor step 的 `StepResult.cost` 为 `CostAggregate::default()`（无 LLM 费用）。`aggregate_cost` 函数不需要改。

5. **事件流**：executor step 发 `WorkflowEvent::StepStarted` / `StepCompleted`（与 LLM step 一致），外部订阅者无需区分 step 类型。

## 受影响的代码

| 文件 | 变更 |
|------|------|
| `model.rs` | `Step` 结构体 → enum；新增辅助方法 `id()` / `name()` |
| `executor.rs` | **新增**：`StepExecutor` trait |
| `engine.rs` | `run_step` → `run_llm_step`（pattern match）；新增 `run_executor_step`；新增 `step_executors` 字段 + `with_executor` builder；`from_parts_with_state` 初始化 `step_executors`；`step_tools_cache` 构造 match；`new_free_task` 构造 `Step::Llm` |
| `plan.rs` | `validate_workflow` 里 `s.id` → `s.id()`；`PlanWorkflowTool` 的 `PlanInput` 适配 enum |
| `gate.rs` | `step_tools` 映射构造适配（executor step → 空 Vec） |
| `judge.rs` | `StepCtx.current_step: &Step` 类型不变，但实现可能需要 match |
| `mod.rs` | 新增 `pub mod executor;` |

## 当前实现状态

已落地：

- `Step` 已改为 `#[serde(tag = "kind")]` enum：
  - `kind = "llm"`：构造 `AgentHarness` 并执行 prompt。
  - `kind = "executor"`：查找注册的 `StepExecutor` 并执行确定性逻辑。
- 反序列化兼容旧的无 `kind` LLM step JSON；新序列化格式统一输出带
  `kind` 的 tagged enum。
- 新增 `workflow::executor::{StepExecutor, ExecutorCtx}`。
- `WorkflowEngine::with_executor(name, executor)` 支持业务层注入 executor。
- `json_transform` 是默认注册的内置 executor；业务层可以用
  `with_executor("json_transform", ...)` 覆盖。
- `WorkflowEngine::run_loop` 按 step 类型分发。
- executor step 与 LLM step 共享：
  - `WorkflowEvent::StepStarted`
  - `WorkflowEvent::StepFinished`
  - `StepRecord`
  - `StepTransitionJudge`
  - `WorkflowState` 持久化
- executor step 取消时不会把已取消任务覆盖为 `Failed`。
- `PlanWorkflowTool` schema 支持 LLM 生成 `kind = "llm"` 或
  `kind = "executor"` 的 step；运行时仍要求业务层注册对应 executor。

已覆盖测试：

- `mixed_llm_and_executor_workflow`
- `builtin_json_transform_executor_runs_without_manual_registration`
- `json_transform_selects_pointer_from_config_input`
- `json_transform_builds_fields_from_previous_structured_result`
- `json_transform_rejects_missing_pointer_without_default`
- `unregistered_executor_fails_task`
- `cancel_executor_step_marks_task_cancelled`
- `executor_step_requires_executor_name`

## 内置 Executor

当前内置：

- **JsonTransformExecutor**（`executor_name = "json_transform"`）：纯 JSON
  变换，不构造 LLM harness，不产生 token cost。

`json_transform` config：

- `source`：可选，`auto` / `config.input` / `prev_result.structured` /
  `prev_result.output`，默认 `auto`。
- `input`：当 `source = "config.input"` 或 `auto` 优先使用时的 JSON 输入。
- `pointer`：可选 RFC 6901 JSON Pointer，例如 `/device/voltage`。
- `fields`：可选对象，输出字段名映射到 pointer 字符串、
  `{ "pointer": "...", "default": ... }` 或 `{ "value": ... }` 常量。
- `include_nulls`：可选 bool，默认 `true`。为 `false` 时跳过 null 字段。

示例：

```json
{
  "kind": "executor",
  "id": "extract_voltage",
  "name": "Extract voltage",
  "executor_name": "json_transform",
  "config": {
    "source": "config.input",
    "input": { "measurement": { "voltage": 3.3, "unit": "V" } },
    "fields": {
      "voltage": "/measurement/voltage",
      "unit": "/measurement/unit"
    }
  }
}
```

仍计划内置的实现，具体安全边界留待后续：

- **HttpCallExecutor**：调 HTTP API，config 含 `url` / `method` / `headers` / `body`
- **ScriptExecutor**：跑脚本，config 含 `command` / `args` / `env`

## 错误处理（待细化）

- executor 未注册 → `Err`，step 失败 → task Failed（与 LLM step 失败一致）
- executor.execute 返回 `Err` → step 失败 → task Failed
- executor 内部 IO 超时 → executor 自己处理，返回 `Err` 或部分结果
- abort 传播 → `Err("executor step aborted")`

## 待解决问题

1. ~~`StepCtx.last_result` 是 `&StepResult`（非 Option），workflow 第一步无前驱结果时如何处理？~~ **已解决**：引入 `ExecutorCtx`，`prev_result: Option<&StepResult>` 明确表达可能无前驱结果。LLM step 的 `StepCtx` 不变（`last_result` 语义是 "当前步执行完的结果"，在 `decide_transition` 中构造，此时已有结果）。
2. ~~`PlanWorkflowTool` 是否需要支持 LLM 生成 executor step？~~ **已解决**：schema 支持 executor step；真正可执行的 executor 仍由业务层显式注册。
3. ~~executor step 的 `WorkflowEvent` 是否需要区分类型（如 `ExecutorStepStarted`）？~~ **已决策**：初版不区分，统一使用 `StepStarted` / `StepFinished`。
4. 内置 executor 的安全边界、超时与 sandbox 策略仍需单独设计。
