# llm-harness-live-tests

Live LLM integration tests for the `llm-harness-runtime` workspace.

## 核心原则

**这些测试的目的之一是发现 runtime 仓库的 bug 和功能不足。**

当测试失败时，**绝对禁止**直接标记 `#[ignore]` 然后继续。这是想当然地认为问题出在外部（大模型、端点），而不是 runtime 代码。这种思维定式会导致真正的 bug 被掩盖。

### 测试失败时的处理流程

1. **深入调查** — 用 `curl` 直接打 API 看 LLM 实际返回什么，对比 runtime 处理后的结果，定位差异在哪一层（adapter / loop / harness）。
2. **确认是 runtime 的 bug** — 修复 runtime 代码，而不是忽略测试。
3. **确认是 runtime 的功能不足** — 在测试中注释说明缺什么功能，保留测试（不 ignore），标记为期望失败或报告给用户。
4. **只有确认是环境问题且无法通过 runtime 修复时** — 才考虑 `#[ignore]`，且必须在注释中写明完整调查过程。

### 常见误区

- **"glm-5.2 不够聪明"** — 错。如果 OMP harness 能正常工作，runtime 也应该能正常工作。先查 runtime 代码。
- **"这是端点的 bug"** — 大概率不是。先用 `curl` 验证非 streaming 和 streaming 的原始返回，再对比 runtime 的处理逻辑。
- **"reasoning 模式下 tool call 不工作"** — 查 adapter 是否正确解析 `delta.reasoning` 字段，查 `parse_reasoning_content` 是否开启，查 `ThinkingScheme` 配置。
- **"vLLM streaming 对空参数工具返回 stop 是模型行为"** — 部分对。vLLM streaming 确实对 `properties: {}, required: []` 的工具不 emit tool_call chunk，但这不是"模型决定不调用"。测试 fixture 应使用有参数的工具。
- **"follow_up + continue_run 是多轮对话的正确 API"** — 错。`follow_up()` 设计用于 run 进行中 should_stop 后注入消息，`continue_run()` 设计用于 IdleWatcher 唤醒主 agent。多轮对话用 `prompt()`。
- **"测试失败了但不知道原因，先 ignore 以后再看"** — 绝对禁止。每次 ignore 都必须有完整调查过程记录在注释中。

## 构建与运行

```bash
# 编译（默认开启 live-llm feature）
cargo build -p llm-harness-live-tests

# 运行实时测试（需要 .env 中的 API key）
source .env && cargo test -p llm-harness-live-tests --features live-llm

# 运行单个测试文件
source .env && cargo test -p llm-harness-live-tests --features live-llm --test loop_layer

# 运行示例（人类可观察）
source .env && cargo run -p llm-harness-live-tests --example 01_prompt_streaming

# clippy
cargo clippy -p llm-harness-live-tests --features live-llm --all-targets -- -D warnings
```

## 架构

- `src/lib.rs` — 共享测试工具：`LiveProvider`、`make_harness`、`EventCapture`、断言助手、超时常量
- `examples/` — 可运行的文档，人类可观察，弱断言
- `tests/` — 行为断言测试，按架构层组织

### 关键设计

- 所有代码在 `#[cfg(feature = "live-llm")] mod inner { ... }` 内，`pub use inner::*;` 导出
- `default = ["live-llm"]` — 默认开启，方便 `cargo run --example` 无需额外 flag
- `make_harness_with(entry, |b| b)` — 回调接收 builder by-value 并返回，支持链式 API
- `EventCapture::settle()` — 在读取事件前调用，等待 background task drain
- 测试使用弱内容断言（非空、包含关键词），不依赖 LLM 输出的具体内容
- 分级超时：smoke 30s，single-turn 60s，multi-turn/compaction/spawn 120s

## 已踩的坑（经验记录）

### Provider 配置

- `OpenAIProvider::builder(api_key).build()` 默认不开启 `parse_reasoning_content` 和 `tolerant_keepalive`。对官方 OpenAI API 够用，但对 vLLM/GLM/DeepSeek 等兼容端点会导致 reasoning 内容被丢弃。
- **正确做法**：用 `llm_harness_loop::openai_compatible(api_key, base_url)` factory 函数，自动开启这两个选项。
- 参考 senza-studio（`crates/studio-core/src/agents/coding_agent.rs`）和 Senza（`src/pyprovider.rs`）的用法。

### 空参数工具

- vLLM streaming 对 `parameters: {type: "object", properties: {}, required: []}` 的工具不 emit tool_call chunk，直接返回 `finish_reason: "stop"`。
- 非 streaming 模式正常返回 `tool_calls`。
- **正确做法**：测试 fixture 中的工具必须有至少一个 required 参数。

### 多轮对话 API

- `follow_up()` + `continue_run()` **不是**多轮对话的正确 API。消息会丢失（channel receiver 在 run 结束后被 drop）。
- `follow_up()` 设计用于 run 进行中 should_stop 后注入消息。
- `continue_run()` 设计用于 IdleWatcher 唤醒主 agent。
- **正确做法**：多轮对话用 `prompt()`，它保留 session 历史。

### 调查方法

- 测试失败时，先用 `curl` 直接打 API 验证 LLM 原始返回，再对比 runtime 处理结果。
- 对比 streaming 和 non-streaming 模式的差异。
- 查看其他 runtime 消费者（senza-studio、Senza、Folumi）如何使用同一 API。
- 查设计文档（`docs/ai-agent-book-layer-analysis.md`）确认 API 的设计意图，不要凭名字猜测。

### Compaction 测试配置

- `compact()` 需要调 LLM 生成摘要，超时必须用 `MULTI_TURN`（120s），不能用 `SMOKE_TIMEOUT`（30s）。
- `context_window` 设置过小（如 500）会导致 `NoValidBoundary`：session 内容超过窗口但找不到有效切割点（`keep_recent_tokens` 保留太多，没有可压缩的空间）。
- `context_window` 设置过大（如 2000）会导致 auto-compact 在短对话中不触发：tokens 达不到阈值。
- **正确做法**：`context_window=800` + `keep_recent_tokens=100` + `reserve_tokens=50` + 10 轮对话 + 较长 prompt（要求 LLM 给 3+ 句回复），能稳定触发 auto-compact 且不报 `NoValidBoundary`。
- `compaction_model` 必须用真实大 `context_window`（如 200_000），否则 summary LLM 自己也受小窗口限制导致 `SummaryFailed`。
- `NoValidBoundary` 不是 bug——框架正确行为：上下文超窗口但无法压缩时报错，比静默发送注定失败的 API 请求更好。业务用户正常配置 `context_window` 不会遇到。

### Model Switch

- `set_model("不存在的模型名")` 不会报错——harness 接受任何字符串作为模型名。但后续 `prompt()` 会向 provider 发送请求，provider 返回错误或空响应，`cap.texts()` 为空字符串。
- **正确做法**：测试 model switch API 时，切换到一个 alternate 名再切回真实模型，在真实模型上做实际对话。或配置第二个 provider 做真正的模型切换。
- `ModelChange` session entry 在 `set_model()` 时立刻记录（如果 harness 处于 Idle），可在 `read_active_path()` 中 filter `SessionEntryPayload::ModelChange` 验证。

### Tool trait 实现

- `Tool::parameters_schema(&self) -> &serde_json::Value` 返回引用，不能用 `serde_json::json!()` 作为 `static`（非 const fn）。
- **正确做法**：用 `static SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| json!({...}))`，`parameters_schema` 中返回 `&SCHEMA`（自动 deref）。
- `Tool::execute` 签名是 `fn execute<'a>(&'a self, args: serde_json::Value, ctx: &'a ToolContext) -> BoxFuture<'a, Result<ToolResult, ToolFailure>>`（注意不是 `&serde_json::Value`）。
- `ToolResult::full(vec![DataBlock::text("msg")], serde_json::Value::Null, false)` 构造成功结果。
- `ToolFailure::invalid_arguments(message)` 只接受一个参数（不是 tool_name + message）。

### Session/Branch 测试

- `AgentHarness.session` 字段是 `pub(super)`——crate 外不可访问。需要 session ID 时，从 Jsonl session 目录的子目录名获取。
- `SessionRepo` trait 需要 `use llm_harness_agent::SessionRepo` 导入才能调用 `repo.create()` / `repo.open()`。
- `CreateSessionOptions` 未被 re-export，需要用 `llm_harness_agent::session::CreateSessionOptions` 完整路径。
- `BranchInfo` 字段是 `leaf_id`（不是 `leaf`）、`label`、`message_count`、`summary`。
- `make_harness_with_session()` 用于 Jsonl 持久化测试，`make_harness_with_env()` 返回 env 供 `load_skills` / `load_prompt_templates` 使用。

### Hooks 测试

- `NextTurnDirective` 没有 `Default` impl——需要手动构造所有字段为 `None`。
- `HarnessHooks` 有 11 个 hook 向量，通过 `Plugin::register_hooks` 注册。
- 用 `Arc<HookCounters>` + `AtomicU32` 跟踪 hook 调用次数，验证对称性不变量（如 `before_tool_call == after_tool_call`）。
- `before_compact >= 0` 对 `u32` 恒为 true——clippy 会报 `absurd_extreme_comparisons`，用 `let _ = before_compact` 替代。

### 测试轮次选择

- Examples（人类可观察文档）：10 轮对话，充分覆盖功能。
- Tests（CI 自动化）：3 轮对话，快速验证功能正确性。compaction 测试需要 10 轮 + 长 prompt 才能触发 auto-compact。
- 用户要求"对话轮次多一些才能测出真实问题"——examples 遵守 10 轮，tests 在保证覆盖的前提下尽量精简。

### 并行子代理写文件

- 用 `task` 并行派发多个子代理写测试文件时，子代理可能写入失败或写到错误路径（如 `session_branch.rs` 缺失）。
- **正确做法**：子代理完成后，用 `find` 验证所有文件存在，用 `cargo build --tests` 验证编译通过，再逐个运行。
