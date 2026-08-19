# llm-harness-runtime 开发原则

## 工作风格

### 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

LLMs often pick an interpretation silently and run with it. This principle forces explicit reasoning:

- **State assumptions explicitly** — If uncertain, ask rather than guess
- **Present multiple interpretations** — Don't pick silently when ambiguity exists
- **Push back when warranted** — If a simpler approach exists, say so
- **Stop when confused** — Name what's unclear and ask for clarification

### 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

Combat the tendency toward overengineering:

- No features beyond what was asked
- No abstractions for single-use code
- No "flexibility" or "configurability" that wasn't requested
- No error handling for impossible scenarios
- If 200 lines could be 50, rewrite it

**The test:** Would a senior engineer say this is overcomplicated? If yes, simplify.

### 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:

- Don't "improve" adjacent code, comments, or formatting
- Don't refactor things that aren't broken
- Match existing style, even if you'd do it differently
- If you notice unrelated dead code, mention it — don't delete it

When your changes create orphans:

- Remove imports/variables/functions that YOUR changes made unused
- Don't remove pre-existing dead code unless asked

**The test:** Every changed line should trace directly to the user's request.

### 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform imperative tasks into verifiable goals:

| Instead of... | Transform to... |
|--------------|-----------------|
| "Add validation" | "Write tests for invalid inputs, then make them pass" |
| "Fix the bug" | "Write a test that reproduces it, then make it pass" |
| "Implement a hook" | "Implement the trait, write tests with mock implementations" |

For multi-step tasks, state a brief plan:

```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let the LLM loop independently. Weak criteria ("make it work") require constant clarification.

### 5. 代码语言规范

**仅使用英文和中文。禁止使用日文、韩文等其他语言。**

代码注释、文档字符串、提交信息统一用英文或中文：

- 新增代码优先用中文注释（与现有项目文档保持一致）
- 技术术语保持英文（如 `LLM`、`Hook`、`Budget`）
- 禁止用日文、韩文、其他非英中语言

**审查点：** 在代码审查和提交前，检查是否有日文/韩文残留。

### 6. 提交前必做清理

**每次 commit 前必须：**

1. 运行 `cargo fmt` — 确保代码格式统一
2. 运行 `cargo clippy --all-targets --all-features` — 修复所有 clippy 警告（除非有充分理由保留）
3. 运行 `CARGO_HTTP_PROXY="" cargo test --workspace` — 跑 Rust workspace 测试

> **环境注意**：clippy 需设 `PYO3_PYTHON=/usr/local/bin/python3.14`，否则会找到系统 python3.6 报错。

这三项检查是 CI 的硬要求，在本地提前发现能避免提交被拒。


## 项目简介

本仓库是 oh-my-harness 的主 monorepo，包含从 API 适配层到运行时基础设施的全部 crate。`llm-api-adapter` 作为独立 git repo 引用。

**核心职责：**
1. **类型与 trait 定义**：Hook trait、Tool trait、Session/Message 类型（`llm-harness-types`）
2. **Agent loop 引擎**：ReAct 循环、工具调度、streaming（`llm-harness-loop`）
3. **高层 agent 抽象**：`AgentHarness`、session 树、compaction、skills（`llm-harness-agent`）
4. **运行时基础设施**：CompositeHook、ToolRegistry、TaskRunner、Sandbox、MCP、Audit、Auth（`llm-harness-runtime` 及各 `-runtime-*` crate）

## Crate 结构与依赖规则

```
llm-harness-runtime/          ← 本仓库根
├── Cargo.toml                (workspace)
├── CLAUDE.md
├── crates/
│   ├── llm-harness-types/    (Run/Hook/Tool trait + 所有共享类型)
│   ├── llm-harness-loop/     (agent loop 引擎)
│   ├── llm-harness-agent/    (AgentHarness, Session, skills)
│   ├── llm-harness-runtime/  (CompositeHook, ToolRegistry, TaskRunner)
│   ├── llm-harness-runtime-sandbox/  (os/bwrap/seatbelt backends, feature-gated)
│   ├── llm-harness-runtime-mcp/
│   ├── llm-harness-runtime-audit-jsonl/
│   ├── llm-harness-runtime-trace-otel/
│   └── (llm-harness-py 已迁移到 github.com/oh-my-harness/Senza)
├── docs/

外部依赖（独立 git repo）：
  llm_adapter  ← git: github.com/oh-my-harness/llm-api-adapter
```

依赖方向（A → B 表示 A 依赖 B）：

```
# llm-harness-py 已迁移到 Senza 仓库，通过 git 依赖引用本仓库的 crate
llm-harness-runtime-*  → llm-harness-runtime
llm-harness-runtime    → llm-harness-agent
llm-harness-agent      → llm-harness-loop
llm-harness-loop       → llm-harness-types, llm_adapter
llm-harness-types      → llm_adapter (外部 git 依赖，非内部 crate)
```

**硬性规则：**
- `llm-harness-types` 不依赖其他内部 crate，保持纯类型/trait 声明。可依赖外部 git crate（如 `llm_adapter`）用于 re-export 共享类型。
- `llm-harness-runtime-sandbox` 不依赖其他 runtime 后端 crate；其他 `llm-harness-runtime-*` 各 crate 不相互依赖（可单独使用）。
- `llm-harness-py` 已迁移到 Senza 仓库，通过 git 依赖引用本仓库 crate，不再在本 workspace 内。
- 依赖方向不允许反转（下层不依赖上层）。


## 设计文档

设计决策记录在 `docs/` 下：

| 文件 | 内容 |
|---|---|
| `gap-analysis-platform-vs-business.md` | 平台 vs 业务层 gap 分析，trait 引入决策框架 |
| `reference-production-multi-agent-harness.md` | 外部参考文章（李维山博客） |
| `superpowers/specs/2026-06-17-plugin-system-design.md` | Plugin 系统设计规范 |

## 核心开发原则

### Trait 与实现的分离

- **trait 声明**（`llm-harness-types` crate）：只定义签名、语义、错误类型，不含逻辑。
- **实现**（如 `BudgetControlAdapter` 等）：提供示例实现，用户可覆盖。

### CompositeHook 合成语义

Runtime 支持用户注入多个 hook 的组合（CompositeHook 模式）。三种合成语义：

| 类型 | 语义 | 例子 |
|------|------|------|
| **All-Execute（观测型）** | 所有 hook 依次执行，返回第一个 hook 的结果 | `BeforeProviderRequestHook`、`AfterProviderResponseHook`、`AfterTurnHook` |
| **First-Non-Allow（决策型）** | 顺序执行，第一个返回 `Allow` 以外的决策立即短路 | `BeforeToolCallHook`（返回 `Allow` / `Deny` / `Modify`） |
| **Chain（链式修改型）** | 按序执行，每个 hook 看到前一个 hook 的修改结果；后续 hook 基于累积的修改继续处理 | `AfterToolCallHook`（返回 `Passthrough` / `Patch`） |

不同类型的 hook 合成方式不同，实现时严格遵守对应语义。

### HarnessHooks 配置

`HarnessHooks` 持有 11 个 hook 插槽，每个插槽为 `Vec<Arc<dyn Hook>>`（支持多个同类型 hook 通过 Composite 合成）：

```rust
pub struct HarnessHooks {
    // 生命周期边界
    pub before_run: Vec<Arc<dyn BeforeRunHook>>,
    pub before_turn: Vec<Arc<dyn BeforeTurnHook>>,
    pub after_turn: Vec<Arc<dyn AfterTurnHook>>,

    // 工具执行
    pub before_tool_call: Vec<Arc<dyn BeforeToolCallHook>>,
    pub after_tool_call: Vec<Arc<dyn AfterToolCallHook>>,

    // 转换与决策
    pub transform_context: Vec<Arc<dyn TransformContextHook>>,
    pub prepare_next_turn: Vec<Arc<dyn PrepareNextTurnHook>>,
    pub should_stop: Vec<Arc<dyn ShouldStopHook>>,

    // Provider 请求/响应
    pub before_provider_request: Vec<Arc<dyn BeforeProviderRequestHook>>,
    pub after_provider_response: Vec<Arc<dyn AfterProviderResponseHook>>,

    // Compaction
    pub before_compact: Vec<Arc<dyn BeforeCompactHook>>,
}
```

用户可为任何插槽注入多个自定义 hook，或留空 `vec![]`（框架跳过该阶段）。多个 hook 的合成语义由各 Composite 包装器实现。

所有 run 内 Hook context 都携带同一个只读 `&RunContext`，`ToolContext` 携带对应的
`Arc<RunContext>`。应用通过 `AgentHarness::run(RunRequest)` 提供 typed `RunExtensions`；
Plugin 的单次运行状态只能写入 `RunContext.local`，不能保存在可复用 Hook 或 Plugin 实例上。
`BeforeCompactHook` 同时服务手动 compaction，不属于必然存在 run 的 Hook，因此不携带
`RunContext`。

### Hook 与 Event 的分工

- **Hook**（`HarnessHooks` 中的 trait）：参与框架流程、可影响行为，构造时注入，有类型安全的返回值。
- **Event**（`AgentHarnessEvent` enum）：纯通知，运行时动态订阅，无返回值。
- 不要把 hook 调用点暴露为事件，也不要把纯通知实现为 hook。

### 并发与锁

- 用 `std::sync::Mutex` + 快照模式（lock → clone → drop lock → 操作快照）。不用 `RwLock`。
- 不在持锁状态下跨 `.await`。
- **例外**：当 Mutex 保护一个必须在 `.await` 期间持有的异步资源（如 `mpsc::Receiver` 需 `recv().await`）时，用 `tokio::sync::Mutex`——`std::sync::Mutex` 在 `recv().await` 被 cancel 时会 poison。前提是确保不与其他 `Mutex` 构成死锁（如 `SharedPeerRx` 仅 hook `try_lock` + tool `lock().await`，且两者在 agent loop 单线程上不并发）。
- Hook 实现无内部业务状态（计数器、缓存等影响行为的可变状态），所有业务状态通过 `ToolContext`、`AgentHarness` 等外部传入。**例外**：Hook 可持有资源句柄（如 channel receiver 的 `CompletionRxGuard`、`FollowUpHandle`）——这些是 RAII 所有权流转（借出/归还），非业务状态，且 `ShouldStopHook::should_stop` 的 `&self` 签名无法通过参数传入。此类资源句柄用 `Mutex` 保护（见上条例外）。

### 错误处理与失败语义

- Hook 返回 `Result<T, AgentError>`，失败通过 `AgentError` 表达（不是 panic）。
- 框架在第一个 hook 失败时停止链式调用（first-non-allow 语义）并向用户返回错误。
- 不强制成功，允许用户决定是否重试、回退或中止。

### Python FFI 绑定（已迁移）

`llm-harness-py`（PyO3 绑定层）已迁移到 Senza 仓库（`github.com/oh-my-harness/Senza`）。本仓库不再包含 Python 绑定代码。Senza 通过 git 依赖引用本仓库的 crate。

## 测试原则

**TDD 流程（实现 trait 时）：**

1. **先写测试**：用 mock 实现 trait，编写单元测试验证合成逻辑
2. **停下来**：提交测试，等用户审查——审查的是"trait 行为预期"，不是"实现细节"
3. **实现**：用户批准后，实现具体 hook
4. **重构**：在测试保护下整理代码

**基础设施：**
- Trait 的 mock 实现放在测试模块中（`#[cfg(test)]`）
- 示例实现在各自的 crate 中，带单独测试
- 集成测试用真实 `AgentHarness` + mock core types，不依赖外部 LLM

**运行测试：**
- 一键跑全部 Rust workspace 测试：
  ```
  CARGO_HTTP_PROXY="" cargo test --workspace
  ```
- 仅跑特定 crate：
  ```
  cargo test -p llm-harness-loop
  ```

## 实现后自查

每次实现完成后，在提交前自行检查以下维度。有 ❌ 项自行修复，有 ⚠️ 项列出后交用户判断：

**架构合规**
- ❌ trait 声明是否引入了不必要的外部 crate 依赖（`llm-harness-types` 应保持纯类型层）？
- ❌ CompositeHook 合成是否遵循了对应的语义（all-execute / first-non-allow / last-non-passthrough）？

**代码质量**
- ❌ 所有 `pub` trait、method、struct field 是否有 `///` doc comment？
- ⚠️ 是否在持 `Mutex` 锁时跨了 `.await`？
- ⚠️ 是否使用了 `RwLock`（项目统一用 `Mutex` + 快照模式）？

**测试**
- ❌ 新增的 pub trait 或 concrete struct 是否有对应测试？
- ❌ CompositeHook 实现是否覆盖了单个 hook 和多个 hook 的场景？
- ⚠️ 单元测试是否依赖真实外部服务（应使用 mock）？

**文档同步**
- ⚠️ 新增或修改了 trait 签名、HarnessHooks 插槽、CompositeHook 语义？更新本文件（CLAUDE.md）对应段落。

**Python SDK 同步（已迁移）**
- `llm-harness-py` 已迁移到 Senza 仓库（`github.com/oh-my-harness/Senza`）。修改本仓库 `pub` API 后，Senza 仓库的 PyO3 绑定可能需要同步更新——由 Senza 仓库的 `check_stubs.py` 在升级 pin 时校验。

## Git 规则

- 完成代码改动后，无需等用户明确要求即可执行：
  1. 自行 review 本次改动（diff 正确、无多余文件、无调试代码）
  2. 运行 `cargo fmt`
  3. `git commit`（或 `git commit --amend`）
  不要累积多个阶段后一次性提交。每完成一个独立的阶段或功能单元即可提交。
- `git push` 之前**必须等待用户确认**。
- Commit message 格式：`<type>(<scope>): <description>`
  - type：`feat` / `fix` / `docs` / `test` / `refactor` / `chore`
  - scope：crate 名，如 `traits`、`budget`、`orchestration`
  - 示例：`feat(budget): implement BudgetControlAdapter`、`test(orchestration): add CompositeHook tests`
- description 说明 *为什么* 而非 *做了什么*。
