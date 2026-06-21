# examples/coding-agent — Phase 1 设计规范

> 日期：2026-06-21
> 范围：`examples/coding-agent` Phase 1
> 目标：基于框架 Plugin 系统构建一个可用的 coding agent，展示框架能力

---

## 一、背景与目标

`examples/coding-agent` 是 llm-harness-runtime 的旗舰示例，有两个同等重要的目标：

1. **做一个好用的 coding agent** — 完整展示框架能力，作为框架的"活文档"
2. **推动框架成熟** — coding agent 是框架最苛刻的用户，它缺的东西中属于框架层的部分，框架就应该补上

Phase 1 目标：**能跑起来** — 4 个核心工具 + one-shot/pipe + 交互式 REPL + 事件渲染。

参考实现：`~/projs/oh-my-harness/coding-agent`（独立 repo，使用旧 API）。本设计是以参考实现为灵感的重新设计，而非直接移植。

---

## 二、架构

### 核心决策：coding-agent IS a Plugin

框架已提供 `Plugin` trait（`llm-harness-agent`）和 `HarnessBuilder`（`llm-harness-runtime`），coding-agent **不需要自己写 builder 或 `CodingAgent` 结构体**。

```
coding-agent = CodingAgentPlugin（注册工具 + 系统提示词）
             + HarnessBuilder（组装 AgentHarness）
             + main.rs（CLI 入口）
```

### 目录结构

```
examples/coding-agent/
├── Cargo.toml              ← workspace member，path 依赖框架 crate
├── spec.md                 ← 本文件
├── src/
│   ├── main.rs             ← CLI 入口：参数解析、provider 配置、REPL / one-shot 分发
│   ├── plugin.rs           ← CodingAgentPlugin（实现 Plugin trait）
│   ├── tools/
│   │   ├── mod.rs
│   │   ├── read.rs         ← ReadTool
│   │   ├── bash.rs         ← BashTool
│   │   ├── edit.rs         ← EditTool
│   │   └── write.rs        ← WriteTool
│   ├── prompt.rs           ← 系统提示词构建 + CLAUDE.md/AGENTS.md 自动加载
│   └── output.rs           ← AgentHarnessEvent → 终端文本渲染 + REPL 循环
└── tests/
    └── integration.rs      ← MockLlmClient + NoOpEnv + CodingAgentPlugin 集成测试
```

---

## 三、CodingAgentPlugin

### 接口

```rust
pub struct CodingAgentPlugin {
    system_prompt: String,
}

impl CodingAgentPlugin {
    /// 从 cwd 向上查找 CLAUDE.md / AGENTS.md，构建系统提示词。
    pub fn new(cwd: &Path) -> Self;

    /// 返回系统提示词字符串，供 HarnessBuilder 注入。
    pub fn system_prompt(&self) -> &str;
}

impl Plugin for CodingAgentPlugin {
    fn name(&self) -> &str { "coding-agent" }

    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        // 注册 ReadTool、BashTool、EditTool、WriteTool
    }

    // register_hooks: Phase 1 暂无自定义 hook，留空（默认实现）
}
```

### 框架扩展点

`HarnessBuilder` 目前没有 `.system_prompt()` 方法。这是 coding-agent 推动框架改进的**第一个反馈点**：

- **方案 A**：`CodingAgentPlugin` 重写 `Plugin::register_hooks()`，通过一个 `TransformContextHook` 在每次请求前注入 system prompt
- **方案 B**：向框架提 PR，给 `HarnessBuilder` 加 `.system_prompt(String)` 方法

Phase 1 先用方案 A 验证，同步向框架提 issue。

---

## 四、4 个工具

工具参数结构体实现 `serde::Deserialize`（框架需要 JSON Schema 生成）。工具本体无内部状态，所有上下文通过 `ToolContext` 获取。

| 工具 | 参数 | 关键约束 |
|------|------|---------|
| `read` | `path: String, offset?: u32, limit?: u32` | 按行号范围读取；大文件按 limit 截断，默认 2000 行 |
| `bash` | `command: String, timeout_ms?: u64` | 通过 `ExecutionEnv` 执行；stdout+stderr 合并；超大输出截头保尾（保留各 500 行）；默认超时 30s |
| `edit` | `path: String, old_string: String, new_string: String` | 精确字符串替换；`old_string` 在文件中不唯一时返回错误，不执行替换 |
| `write` | `path: String, content: String` | 覆盖写入；路径不存在时自动创建父目录 |

**Phase 2+ 工具（明确不在 Phase 1）：** grep、find、ls、git、run_tests

---

## 五、CLI

### 模式

```
coding-agent -p "prompt"          # one-shot
coding-agent "prompt"             # 位置参数（同上）
echo "prompt" | coding-agent      # pipe
coding-agent --interactive        # 交互式 REPL
coding-agent -i                   # 同上
```

### 模式判断逻辑

1. 有 `--interactive` / `-i` → REPL 模式
2. stdin 是 TTY 且无 `-p` / 位置参数 → REPL 模式（直接运行不带参数时的自然行为）
3. 否则 → one-shot 模式

### 环境变量

| 变量 | 必须 | 说明 |
|------|------|------|
| `ANTHROPIC_API_KEY` | 是 | Anthropic provider API key |
| `CODING_AGENT_MODEL` | 否 | 覆盖默认模型（默认 `claude-sonnet-4-6`） |

不引入 clap，手动解析（3 个 flag，逻辑 < 40 行）。Phase 1 不含 `--session-id`、`--list-sessions`、`--audit-log`、`--budget` 等选项。

### 组装流程（main.rs 核心）

```rust
let cwd = std::env::current_dir()?;
let plugin = CodingAgentPlugin::new(&cwd);
// system prompt 通过 CodingAgentPlugin::register_hooks() 内的
// TransformContextHook 注入（见第三节框架扩展点方案 A）

let harness = HarnessBuilder::new(&model)
    .install(&plugin)
    .provider("claude-*", anthropic_client)
    .build(env)
    .await?;
```

---

## 六、输出渲染

### one-shot / pipe 模式

订阅 `AgentHarnessEvent`，处理两类事件：
- `AgentEvent::TextDelta { text }` → `print!("{text}")`，flush stdout
- `HarnessPhase::Idle` → 打印换行，退出监听循环

错误事件打印到 stderr。

### REPL 模式

用 `rustyline` 实现行编辑 + 历史记录：

```
> <用户输入>
<agent 流式响应>

> <下一条输入>
...
```

- 历史文件：`$XDG_DATA_HOME/coding-agent/history`（回退到 `~/.local/share/coding-agent/history`）
- `exit` / `quit` / Ctrl-D / Ctrl-C → 退出

---

## 七、测试策略

集成测试用 `MockLlmClient` + `NoOpEnv`，不依赖真实 API：

```rust
let plugin = CodingAgentPlugin::new(tmp_dir.path());
let harness = HarnessBuilder::new("mock-model")
    .install(&plugin)
    .provider("mock-model", Arc::new(MockLlmClient::new(responses)))
    .build(Arc::new(NoOpEnv))
    .await?;
```

**覆盖场景：**
- Agent 调用工具，工具正确执行
- 响应 TextDelta 流式输出完整
- `old_string` 不唯一时 EditTool 返回错误
- 无 API key 时 main.rs 友好报错退出

工具单元测试（`#[cfg(test)]`）：各工具用真实临时目录验证读写行为，不依赖 LLM。

---

## 八、依赖（Cargo.toml 草稿）

```toml
[package]
name = "coding-agent"
version.workspace = true
edition.workspace = true

[[bin]]
name = "coding-agent"
path = "src/main.rs"

[dependencies]
llm-harness-agent    = { path = "../../crates/llm-harness-agent" }
llm-harness-runtime  = { path = "../../crates/llm-harness-runtime" }
llm-harness-types    = { path = "../../crates/llm-harness-types" }
llm_adapter          = { workspace = true }

tokio   = { workspace = true, features = ["macros", "rt-multi-thread"] }
serde   = { workspace = true }
serde_json = { workspace = true }
thiserror  = { workspace = true }
rustyline  = "14"
atty       = "0.2"
dirs-next  = "2"

[dev-dependencies]
llm-harness-loop = { workspace = true, features = ["test-utils"] }
tempfile = "3"
```

---

## 九、明确不在 Phase 1 的功能

| 功能 | 推迟原因 |
|------|---------|
| 会话持久化（`--session-id`） | Phase 2，需要 `JsonlSessionRepo` + CLI 选项扩展 |
| Settings 系统（`settings.json`） | Phase 2，配置层不是 Phase 1 的核心 |
| Phase manager（工具白名单分阶段） | Phase 2，需要 `PrepareNextTurnHook` 实现 |
| Audit hook（JSONL 审计日志） | Phase 2，`--audit-log` 选项 |
| CLI 人工审批（`--require-approval`） | Phase 2，`HumanApprovalWrapper` |
| Budget 控制（`--budget`） | Phase 2，`BudgetControlAdapter` |
| grep / find / ls / git / run_tests 工具 | Phase 2 |

---

## 十、框架反馈点（开发中记录）

开发过程中发现框架层不足时，在此记录，开发完成后统一向框架提 issue / PR：

| # | 问题 | 建议 |
|---|------|------|
| 1 | `HarnessBuilder` 缺少 `.system_prompt()` 方法 | 给 builder 加该方法，或提供 `SystemPromptHook` |
