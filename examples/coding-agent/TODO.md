# coding-agent TODO

记录已知问题和待改进方向。每项包含问题描述、影响范围和建议方案。

---

## 1. 缺少 `grep` 工具

**问题**：agent 目前没有专用的文本搜索工具。虽然可以通过 `bash` 执行 `grep`，但这依赖 LLM 自己拼出正确的 shell 命令，容错性差。

**影响**：代码导航体验差，LLM 需要多轮 bash 调用才能定位代码位置。

**建议方案**：新增 `GrepTool`，封装正则/字面量搜索，返回匹配行号和上下文，注册到 `CodingAgentPlugin`。

---

## 2. 缺少危险命令防护 Hook

**问题**：`plugin.rs` 里没有注册任何 `BeforeToolCallHook`。LLM 可以执行 `rm -rf`、`sudo`、`curl | sh` 等高危命令，无任何拦截。

**影响**：Phase 1 明确要求"危险命令防护"，当前实现不满足设计目标。

**建议方案**：在 `hooks.rs` 中实现 `DangerousCommandHook`，对匹配黑名单模式的 bash 命令返回 `Deny`，并在 `CodingAgentPlugin` 的 `register_hooks` 中注入到 `before_tool_call` 插槽。

---

## 3. 事件渲染过于简陋

**问题**：`output.rs` 只渲染 `TextDelta`，工具调用的全过程（调用了什么工具、入参是什么、返回了什么）对用户完全不可见。

**影响**：用户无法感知 agent 正在做什么，调试和信任感都很差。

**建议方案**：在 `render_one_shot` 中额外处理以下事件并打印到终端：
- `ToolCallStart`：打印工具名和关键参数
- `ToolCallEnd`：打印执行结果摘要或退出码
- `PhaseChange`：可选，打印轮次边界

---

## 4. REPL 多轮对话上下文不连续

**问题**：`run_repl` 中每轮都调用 `run_one_shot`，而 `run_one_shot` 每次都独立 prompt，没有携带前几轮的对话历史。对话上下文在轮与轮之间断裂。

**影响**：REPL 模式实际上退化成了多次独立的 one-shot，用户无法进行真正的多轮对话。

**建议方案**：调查 `AgentHarness` 是否已支持持久 session / continue 语义；若支持，`run_repl` 应在同一个 session 内循环调用，而不是每轮重新 prompt。

---

## 5. bash 工具输出截断策略不合理

**问题**：`format_output` 目前对超长输出只保留头部，丢弃尾部。对于编译错误、测试结果等场景，尾部往往是最关键的信息。

**影响**：LLM 可能看不到错误信息，导致误判。

**建议方案**：改为"头尾保留，中间截断"策略：保留前 N 行和后 M 行，中间插入截断提示，类似：

```
<前 N 行>
[... X bytes omitted ...]
<后 M 行>
```

---

## 6. 缺少"询问人类是否继续"的机制

**问题**：agent 在执行多步任务时，没有任何机制在关键节点暂停并询问用户是否继续。LLM 会一路执行到底，用户无法在中途介入、确认或叫停。

**影响**：对于涉及文件修改、命令执行等不可逆操作，用户失去了审查和干预的机会，安全性和可控性差。

**建议方案**：通过 `BeforeToolCallHook` 插槽实现 `HumanConfirmHook`，在执行指定类型的工具（如 `BashTool`、`EditTool`）前，向用户打印操作摘要并等待 stdin 输入（`y/n`）。拒绝时返回 `Deny`，终止本次工具调用。可通过配置项控制是否启用、哪些工具需要确认。
