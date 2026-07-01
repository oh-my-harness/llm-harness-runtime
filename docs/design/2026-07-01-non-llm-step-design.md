# Non-LLM Step 支持设计

> **状态：初版已实现**。当前已支持业务注入 `StepExecutor`，并由
> `WorkflowEngine` 在 LLM step 与 executor step 之间分发。默认内置
> `JsonTransformExecutor` 已可用；`ShellExecutor` 与 `HttpCallExecutor` 已提供，
> 但必须由业务层显式注册并配置安全策略。

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
- runtime 提供内置 `EdgeConditionJudge`，可直接按 `edges.condition` 中的结构化条件路由；复杂业务仍可注入自定义 judge。

### Executor 定义模型

- 定义 `StepExecutor` trait，业务层实现。
- 提供常用 executor：`json_transform` 默认可用；`shell` 与 `http_call` 需业务层显式注册并配置安全边界。
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy: Option<StepExecutionPolicy>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy: Option<StepExecutionPolicy>,
    },
}

pub struct StepExecutionPolicy {
    /// 单次执行尝试的 runtime 超时。None 表示不由 workflow runtime 统一超时。
    pub timeout_ms: Option<u64>,
    /// 执行失败后的最大尝试次数，包含第一次执行。None 等价于 1。
    pub max_attempts: Option<u32>,
    /// 每次失败重试前的固定等待时间。None 等价于 0。
    pub retry_backoff_ms: Option<u64>,
}

pub struct Edge {
    pub from: StepId,
    pub to: StepId,
    pub condition: Option<EdgeCondition>,
}

#[serde(untagged)]
pub enum EdgeCondition {
    /// 兼容旧 plan：字符串标签仍交给自定义 judge 解释。
    Label(String),
    /// runtime 内置可求值条件。
    Expr(ConditionExpr),
}

#[serde(tag = "op", rename_all = "snake_case")]
pub enum ConditionExpr {
    Exists { pointer: String },
    Missing { pointer: String },
    Eq { pointer: String, value: serde_json::Value },
    Ne { pointer: String, value: serde_json::Value },
    Gt { pointer: String, value: f64 },
    Gte { pointer: String, value: f64 },
    Lt { pointer: String, value: f64 },
    Lte { pointer: String, value: f64 },
}
```

**关键点**：

1. **`#[serde(tag = "kind")]`**：序列化带 `kind: "llm"` / `kind: "executor"` 标签。
2. **兼容旧 LLM step**：新写出的 `Step` 会带 `kind`，但反序列化兼容旧的无 `kind` LLM step JSON，避免已有 TaskStore 存档和调用方计划直接失效。
3. **executor step 不需要 `prompt` / `allowed_tools`**：enum 变体只携带自己需要的字段。
4. **`config: Option<serde_json::Value>`**：executor 自定义 schema，引擎不需要知道参数细节，保持开放扩展。
5. **`policy: Option<StepExecutionPolicy>`**：LLM step 和 executor step 共用同一套 runtime 执行策略，旧 plan 不带该字段时保持原行为。
6. **`Edge.condition` 向后兼容**：旧字符串条件仍是自定义 judge 标签；对象条件由内置 `EdgeConditionJudge` 根据 `StepResult.structured` 求值。

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
    /// 执行环境。需要文件系统或 shell 的 executor 必须经此访问 sandbox。
    pub env: Arc<dyn ExecutionEnv>,
    /// step 级 abort token。
    pub abort: CancellationToken,
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

    // 2. 构造 ExecutionEnv + ExecutorCtx
    let history = self.workflow_state.lock().await.step_history.clone();
    let prev_result = history.last().and_then(|r| r.result.as_ref());
    let env = self.config.env_factory.create(&std::env::current_dir()?)?;
    let ctx = ExecutorCtx {
        current_step: step,
        prev_result,
        step_history: &history,
        env,
        abort: abort.clone(),
    };

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

1. **abort 传播**：executor step 用 `select!` 包裹 `executor.execute()`；同时 `ExecutorCtx.abort` 会传入 executor。LLM step 的 abort 通过 `harness.set_abort_token` 实现；executor 内部做长 IO、shell 或 HTTP 请求时应继续传递 abort，或使用带 timeout 的 client。

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

5. **事件流**：executor step 发 `WorkflowEvent::StepStarted` / `StepFinished`（与 LLM step 一致），外部订阅者无需区分 step 类型。

6. **执行策略**：step 可选 `policy` 由 runtime 统一执行，覆盖 LLM step 与 executor step。
   `timeout_ms` 是单次 attempt 超时；`max_attempts` 只针对 step 执行错误重试，不改变
   judge 返回 `Transition::Retry` 的语义；`retry_backoff_ms` 是失败 attempt 之间的固定等待。

7. **声明式条件路由**：内置 `EdgeConditionJudge` 按 edge 顺序求值结构化条件，命中第一个条件即
   `Transition::To(edge.to)`；若没有条件命中但存在唯一无条件边，则走该 fallback；若没有匹配也没有 fallback，则
   返回 `Transition::Fail`，task 进入 Failed。

## 受影响的代码

| 文件 | 变更 |
|------|------|
| `model.rs` | `Step` 结构体 → enum；新增辅助方法 `id()` / `name()`；新增 `EdgeCondition` / `ConditionExpr` / `Transition::Fail` |
| `executor.rs` | **新增**：`StepExecutor` trait、`JsonTransformExecutor`、显式注册用 `ShellExecutor` / `HttpCallExecutor` |
| `engine.rs` | `run_step` → `run_llm_step`（pattern match）；新增 `run_executor_step`；新增 `step_executors` 字段 + `with_executor` builder；`from_parts_with_state` 初始化 `step_executors`；`step_tools_cache` 构造 match；`new_free_task` 构造 `Step::Llm`；统一执行 `StepExecutionPolicy` |
| `plan.rs` | `validate_workflow` 里 `s.id` → `s.id()`；`PlanWorkflowTool` 的 `PlanInput` 适配 enum；校验 step `policy` 上限和 edge 条件格式 |
| `gate.rs` | `step_tools` 映射构造适配（executor step → 空 Vec） |
| `judge.rs` | `StepCtx.current_step: &Step` 类型不变；新增内置 `EdgeConditionJudge` |
| `mod.rs` | 新增 `pub mod executor;` |

## 当前实现状态

已落地：

- `Step` 已改为 `#[serde(tag = "kind")]` enum：
  - `kind = "llm"`：构造 `AgentHarness` 并执行 prompt。
  - `kind = "executor"`：查找注册的 `StepExecutor` 并执行确定性逻辑。
- 反序列化兼容旧的无 `kind` LLM step JSON；新序列化格式统一输出带
  `kind` 的 tagged enum。
- 新增 `workflow::executor::{StepExecutor, ExecutorCtx}`。
- 新增 `StepExecutionPolicy`，支持 step 级 `timeout_ms`、`max_attempts`、
  `retry_backoff_ms`；runtime 对 LLM step 和 executor step 统一执行该策略。
- 新增 `EdgeCondition` / `ConditionExpr`，支持 runtime 内置声明式条件路由。
- 新增 `EdgeConditionJudge`，可按 `StepResult.structured` 和 edge 条件决定跳转；复杂业务仍可继续使用自定义 `StepTransitionJudge`。
- 新增 `Transition::Fail`，用于路由失败、声明式条件无匹配等明确失败场景，区别于正常终止用的 `Transition::Abort`。
- `WorkflowEngine::with_executor(name, executor)` 支持业务层注入 executor。
- `json_transform` 是默认注册的内置 executor；业务层可以用
  `with_executor("json_transform", ...)` 覆盖。
- `ShellExecutor` 已实现，但不在默认 executor 列表中；必须由业务层显式
  `with_executor("shell", Arc::new(ShellExecutor::new([...])))` 注册，并建议配合
  sandbox-backed `ExecutionEnv` 使用。
- `HttpCallExecutor` 已实现，但不在默认 executor 列表中；必须由业务层显式
  `with_executor("http_call", Arc::new(HttpCallExecutor::new(policy)))` 注册，并配置
  `HttpCallPolicy` 的 host/scheme/method allowlist、超时、大小限制和脱敏策略。
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
- `shell_executor_rejects_command_not_allowlisted`
- `shell_executor_runs_allowed_command_through_env`
- `shell_executor_rejects_parent_traversal_cwd`
- `http_executor_rejects_host_not_allowlisted`
- `http_executor_rejects_http_by_default`
- `http_executor_rejects_private_ip_target_by_default`
- `http_executor_rejects_body_conflict`
- `http_executor_rejects_hop_by_hop_header`
- `http_executor_redacts_sensitive_response_headers`
- `http_executor_truncates_large_response_body`
- `http_executor_rejects_redirect_to_disallowed_host`
- `http_executor_rejects_unexpected_status`
- `http_executor_accepts_configured_status`
- `unregistered_executor_fails_task`
- `cancel_executor_step_marks_task_cancelled`
- `executor_step_requires_executor_name`
- `step_execution_policy_roundtrip`
- `invalid_step_policy_fails`
- `step_policy_retries_executor_failure`
- `step_policy_times_out_blocking_executor`
- `edge_condition_legacy_label_roundtrip`
- `edge_condition_expression_roundtrip`
- `invalid_edge_condition_pointer_fails`
- `edge_condition_judge_takes_first_matching_condition`
- `edge_condition_judge_falls_back_to_unconditional_edge`
- `edge_condition_judge_fails_when_no_condition_matches`
- `edge_condition_judge_fails_legacy_label_without_custom_judge`
- `transition_fail_marks_task_failed`

## 运行时执行策略

每个 step 可选配置 `policy`：

```json
{
  "policy": {
    "timeout_ms": 30000,
    "max_attempts": 3,
    "retry_backoff_ms": 500
  }
}
```

语义：

- `timeout_ms`：单次 attempt 的 runtime 超时。超时会取消当前 attempt。
- `max_attempts`：执行失败后的最大尝试次数，包含第一次执行。缺省为 1。
- `retry_backoff_ms`：失败 attempt 之间的固定等待时间，缺省为 0。
- 该策略同时适用于 LLM step 和 executor step。
- 该策略只处理 step 执行错误；业务 judge 返回的 `Transition::Retry` 仍由
  `WorkflowEngine::with_max_retries` 管控。

校验上限：

- `timeout_ms`: `1..=86_400_000`。
- `max_attempts`: `1..=10`。
- `retry_backoff_ms`: `0..=300_000`。

## 声明式条件路由

对于常见分支，可以不用业务层手写 judge，而是使用内置 `EdgeConditionJudge`：

```rust
let judge = Arc::new(EdgeConditionJudge::from_workflow(&workflow));
let engine = WorkflowEngine::new(workflow, config, judge)?;
```

edge 条件示例：

```json
{
  "from": "inspect",
  "to": "approve",
  "condition": { "op": "eq", "pointer": "/status", "value": "ok" }
}
```

支持的 `op`：

- `exists` / `missing`：判断 `StepResult.structured` 中 JSON Pointer 是否存在。
- `eq` / `ne`：按 JSON 值比较。
- `gt` / `gte` / `lt` / `lte`：按数字比较。

路由规则：

- 只读取 `StepResult.structured`，不解析自然语言 `output`。
- 按 workflow 中 edge 顺序求值，命中第一个结构化条件就跳转到该 edge 的 `to`。
- 若结构化条件都不匹配，但存在唯一无条件 edge，则使用该 edge 作为 fallback。
- 若没有匹配也没有 fallback，则返回 `Transition::Fail`，task 标记为 Failed。
- 旧字符串 `condition` 仍保留为自定义 judge 标签；内置 `EdgeConditionJudge` 不猜测字符串标签语义，遇到仅有字符串条件时会返回 `Transition::Fail`。

## 内置 Executor

默认注册：

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

可显式注册：

- **ShellExecutor**（建议注册名 `shell`）：通过 `ExecutionEnv::execute_shell`
  执行命令，不默认开放。业务层必须传入命令 allowlist，例如：
  `ShellExecutor::new(["python", "node"])`。

`shell` config：

- `command`：必填，必须命中 `ShellExecutor` 构造时的 allowlist。
- `args`：可选字符串数组，由 executor 进行 shell quoting 后拼接。
- `cwd`：可选相对路径，禁止绝对路径和 `..` 父目录穿越。
- `env`：可选对象，key 只允许 `[A-Za-z_][A-Za-z0-9_]*`，value 必须为字符串。
- `timeout_ms`：可选，默认 30 秒。

`ShellExecutor` 的生产约束：

- 不加入 `builtin_executors()`，避免计划阶段生成 shell step 后直接执行。
- 必须走 `ExecutorCtx.env`，所以可接入 OS / bwrap / seatbelt 等 sandbox backend。
- 必须设置命令 allowlist；不支持任意 shell script 文本。
- stdout/stderr 会按 executor 的 `max_output_bytes` 截断后写入 `StepResult.structured`。

### HttpCallExecutor

`HttpCallExecutor` 不应默认注册。HTTP 调用具备 SSRF、凭据外泄、响应过大和
审计脱敏风险，应采用与 `ShellExecutor` 类似的显式注册策略：

```rust
WorkflowEngine::new(workflow, config, judge)?
    .with_executor("http_call", Arc::new(HttpCallExecutor::new(policy)));
```

已实现的 `HttpCallPolicy`：

- `allowed_hosts`：必填，精确 host allowlist。初版不支持通配符。
- `allowed_schemes`：默认仅 `https`；`http` 必须显式开启。
- `allowed_methods`：默认 `GET` / `POST`。
- `max_timeout`：请求超时上限，默认 30 秒。
- `max_request_bytes`：请求 body 上限，默认 1 MiB。
- `max_response_bytes`：响应 body 上限，默认 1 MiB。
- `allow_redirects`：默认 `false`；若开启，每次 redirect 后必须重新校验 scheme/host。
- `redact_headers`：默认包含 `authorization`、`cookie`、`set-cookie`、`x-api-key`。
- `allow_private_ip_targets`：默认 `false`。当前实现会拒绝 URL 中直接出现的
  private/link-local/loopback/multicast/documentation IP；对 DNS host，仅校验精确 host
  allowlist，不在 HTTP client 内复验解析后的 IP。因此生产环境仍应配合出站网络策略、
  proxy 或 sandbox 做网络边界隔离。

`http_call` config：

- `url`：必填，必须是绝对 URL，且 scheme/host 命中 policy。
- `method`：可选，默认 `GET`，必须命中 policy。
- `headers`：可选对象，key/value 均为字符串；写入 `StepResult.structured`
  前必须按 policy 脱敏。
- `query`：可选对象，key/value 支持字符串、数字、布尔值。
- `body_json`：可选 JSON body。
- `body_text`：可选文本 body。
- `timeout_ms`：可选，不能超过 policy 上限。
- `expect_status`：可选数组或范围；默认接受 `200..299`。
- `response_format`：可选，`json` / `text` / `bytes_base64`，默认按
  `content-type` 自动判断，未知类型按文本截断。

互斥规则：

- `body_json` 与 `body_text` 不能同时存在。
- `GET` / `HEAD` 不允许 body。
- `headers` 中不允许注入 hop-by-hop header，例如 `connection`、`transfer-encoding`。

`StepResult` 建议结构：

```json
{
  "status": 200,
  "url": "https://api.example.com/v1/items",
  "method": "GET",
  "headers": {"content-type": "application/json"},
  "body": {"items": []},
  "body_truncated": false,
  "elapsed_ms": 123
}
```

错误处理：

- host/scheme/method 不在 allowlist → `Err`，step 失败。
- timeout → `Err`，step 失败。
- 响应超过 `max_response_bytes` → 截断并设置 `body_truncated = true`；
  如果业务要求完整响应，可由 policy 改成直接 `Err`。
- `expect_status` 不匹配 → `Err`，错误中只包含脱敏后的摘要。
- redirect 目标不合法 → `Err`。

已覆盖测试：

- allowlist 不命中。
- 禁止 `http` 默认访问。
- 禁止 redirect 到未授权 host。
- response size 截断。
- sensitive header 脱敏。
- `body_json` / `body_text` 互斥。
- 非 2xx 或 `expect_status` 不匹配。

## 错误处理

- executor 未注册 → `Err`，step 失败 → task Failed（与 LLM step 失败一致）
- executor.execute 返回 `Err` → 若 step 配置了 `policy.max_attempts > 1`，runtime 自动重试；最终仍失败则 task Failed
- LLM step 执行返回 `Err` → 与 executor step 一样受 `policy.max_attempts` 管控；最终仍失败则 task Failed
- step `policy.timeout_ms` 超时 → 当前 attempt 被取消；若还有剩余 attempt 则重试，否则 task Failed
- executor 内部 IO 超时 → executor 自己处理，返回 `Err` 或部分结果
- `EdgeConditionJudge` 无匹配条件且无 fallback → `Transition::Fail` → task Failed
- 自定义 judge 返回 `Transition::Fail` → task Failed
- abort 传播 → `Err("executor step aborted")`

## 待解决问题

1. ~~`StepCtx.last_result` 是 `&StepResult`（非 Option），workflow 第一步无前驱结果时如何处理？~~ **已解决**：引入 `ExecutorCtx`，`prev_result: Option<&StepResult>` 明确表达可能无前驱结果。LLM step 的 `StepCtx` 不变（`last_result` 语义是 "当前步执行完的结果"，在 `decide_transition` 中构造，此时已有结果）。
2. ~~`PlanWorkflowTool` 是否需要支持 LLM 生成 executor step？~~ **已解决**：schema 支持 executor step；真正可执行的 executor 仍由业务层显式注册。
3. ~~executor step 的 `WorkflowEvent` 是否需要区分类型（如 `ExecutorStepStarted`）？~~ **已决策**：初版不区分，统一使用 `StepStarted` / `StepFinished`。
4. ~~HttpCall executor 的安全策略、代码实现和本地网络边界测试是否落地？~~ **已解决**：`HttpCallExecutor` 已提供显式注册实现，并覆盖 host/scheme/method allowlist、redirect 复验、响应截断、敏感 header 脱敏、body 互斥和非预期状态。DNS 解析后的私网 IP 复验仍属于部署层网络隔离范围。
5. ~~生产 runtime 是否需要 step 级超时和执行失败重试？~~ **已解决**：`StepExecutionPolicy` 已落地，LLM step 与 executor step 共用同一套 `timeout_ms` / `max_attempts` / `retry_backoff_ms` 语义。
6. ~~常见条件分支是否必须业务层手写 judge？~~ **已解决**：内置 `EdgeConditionJudge` 已落地，支持基于 `StepResult.structured` 的声明式 edge 条件；复杂业务仍可注入自定义 judge。
