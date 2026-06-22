# examples/coding-agent 开发规范

> **本文件只定义 coding-agent 特有的规则。** 框架层通用规则（工作风格、TDD 流程、CompositeHook 语义、Git 提交格式、`cargo fmt`/`clippy` 要求等）见根目录 `CLAUDE.md`，此处不重复。

## 角色定位

**coding-agent 是一个长期项目，不是一个一次性 example。** 它有两个同等重要的目标：

1. **做一个好用的 coding agent** — 完整展示框架所有能力，作为框架的"活文档"
2. **推动框架成熟** — coding agent 是框架最苛刻的用户，它缺的东西中，属于通用框架层的那部分，框架就应该补上

`coding-agent/` 目录是 **application 层**，依赖但不修改底层框架 crate。

---

## 核心原则

### 1. 严格区分框架功能与 App 功能

**框架（`crates/*`）= 机制，App（`examples/coding-agent/*`）= 策略。**

| 层 | 职责 | 反例 |
|----|------|------|
| **框架** | 提供 trait 定义、生命周期管理、编排基础设施、通用组件 | 在框架里写死 coding-agent 特有的 prompt、工具选择逻辑 |
| **App** | 实现具体工具、编写 Hook 策略、定义 UI/交互、配置 prompt | 在 app 里重复实现 AgentHarness 已有的 loop、session 管理 |

判断标准 — 问自己：**"这个功能是一个通用的 agent 基础设施，还是 coding-agent 特有的？"**

- "命令执行需要超时控制" → 框架（ShellOptions 已有）
- "bash 工具执行后按语言分类输出" → App（coding-agent 特有的渲染策略）
- "工具注册和发现机制" → 框架（ToolRegistry 已有）
- "Phase 1 需要哪 4 个工具" → App

**绝对不允许：** 在 coding-agent 中重新实现框架已提供的功能（AgentHarness、CompositeHook、ToolRegistry、SessionRepo 等）。如果你发现自己在写 agent loop、session 管理、hook 组合逻辑，停下来——这是框架的职责。

### 2. Coding Agent 是框架的验证器和需求源

**这是反馈循环，不是单向消费。**

```
coding agent 写了一个工具，签名不顺手
  → Tool trait 需要调整接口
  → 框架 types crate 升级
  → coding agent 更顺滑
```

当你在 coding-agent 开发中遇到以下情况时，**不要绕过，去改框架：**

- "框架的 trait 签名让我的实现很别扭" → 优化 trait，而不是在 app 里写 adapter/wrapper 绕过去
- "框架缺少某个 hook 点，我只能 hack" → 在框架中添加 hook 点
- "框架的类型表达力不够" → 扩展类型，而不是在 app 层做类型转换

**工作流：**
1. 在 coding-agent 中按框架设计正常使用
2. 发现框架不足 → 回框架优化
3. 框架优化后 → coding-agent 变强 → 发现新瓶颈

### 3. 基于框架设计，不重复造轮子

**在写任何代码之前，先回答：框架能帮我做什么？**

每次新增功能前，强制检查：

- [ ] `AgentHarness` 是否已经提供了 session 管理、prompt 循环？→ 直接用，不要自己写 loop
- [ ] `HarnessHooks` 的 11 个 hook 插槽是否已覆盖我的需求？→ 优先用 hook，不要绕过
- [ ] `ToolRegistry` / `ToolSource` 是否已有工具发现的机制？→ 注册到 registry，不要自己维护工具列表
- [ ] `CompositeHook` 是否已处理多 hook 组合？→ 注入即可，不要自己实现组合逻辑
- [ ] `ExecutionEnv` / `Sandbox` 是否已提供执行隔离？→ 通过它们访问文件系统和执行命令

**原则：** 如果 50 行框架使用代码可以实现，不要写 200 行框架绕过代码。

### 4. 简单优先：Phase 1 就是 Phase 1

**不要为后续 Phase 写代码。**

- Phase 1 的目标是"能跑起来"：4 个工具 + stdin/stdout + 危险命令防护 + 事件渲染
- 不要提前实现 diff-based edit、TUI、持久化、compaction——这些是 Phase 2/3 的事
- 不要为"将来可能需要"的配置项、抽象层写代码

**检验标准：** 一个 Phase 1 功能的 PR 是否包含了 Phase 3 才需要的代码？如果是，删掉。

---

## 目录结构

```
examples/coding-agent/
├── CLAUDE.md          ← 本文件
├── Cargo.toml         ← 独立 package，依赖框架 crate
├── src/
│   ├── main.rs        ← CLI 入口
│   ├── tools/         ← Tool trait 实现（read/edit/bash/grep/...）
│   ├── hooks.rs       ← 自定义 Hook（危险命令防护等）
│   ├── output.rs      ← 事件→终端渲染
│   ├── prompt.rs      ← 系统提示词
│   └── ...
└── tests/             ← 集成测试（模拟 AgentHarness + mock provider）
```

---

## 编码规范

### Tool 实现

- 所有工具实现 `Tool` trait，注册到 `ToolRegistry`
- 工具不持有全局状态；所有上下文通过 `ToolContext` 获取
- 工具的参数结构体实现 `Serialize` + `Deserialize`（框架需要 JSON Schema 生成）
- 大输出必须截断（对 LLM 来说，"中间截断但头尾完整"比"只剩最后 500 行"有用）

### Hook 实现

- Hook 注入到 `HarnessHooks` 对应插槽，遵循根 CLAUDE.md 定义的合成语义
- Hook 实现无内部可变状态；需要状态的通过 `Arc<Mutex<T>>` 外部传入

### 依赖声明

`Cargo.toml` 中依赖框架 crate 时使用 path 引用：

```toml
[dependencies]
llm-harness-runtime = { path = "../../crates/llm-harness-runtime" }
llm-harness-agent = { path = "../../crates/llm-harness-agent" }
llm-harness-types = { path = "../../crates/llm-harness-types" }
```

不要重复引入框架已有的依赖（`tokio`、`serde`、`tracing` 的版本由 workspace 统一管理）。

---

## 开发流程

### 实现前

1. 明确当前阶段的交付标准
2. 检查框架相关 trait 的签名和语义——确保理解后再写

### 实现中

1. 先写测试（mock `ToolContext`、mock provider），再写实现
2. 发现框架不足时 → 回框架修改，完善后再回 app 继续
3. 保持每次 commit 的范围小且独立（一个工具 = 一个 commit）

### 实现后

除了根 CLAUDE.md 的实现后自查外，额外检查：

- 是否有功能本应由框架提供但我在 app 中重复实现了？
- 是否有为后续 Phase 提前写的代码？

---

## Commit 规范

不要自动提交，等待用户指令，确认后再 Commit

Scope 统一用 `coding-agent`：

```
feat(coding-agent): add ReadTool with line-range support
fix(coding-agent): truncate bash output to head+tail instead of tail-only
docs(coding-agent): clarify Phase 1 scope in CLAUDE.md
```

其余提交规则（type 枚举、提交前 `cargo fmt` + `cargo clippy`、完成后自动推送）见根 CLAUDE.md。

---

## 参考文档

| 文件 | 内容 |
|------|------|
| `../CLAUDE.md` | 框架层开发规范、通用工作风格、Git 规则 |
