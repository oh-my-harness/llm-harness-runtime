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
- 技术术语保持英文（如 `LLM`、`Hook`、`AuthInfo`、`Budget`）
- 禁止用日文、韩文、其他非英中语言

**审查点：** 在代码审查和提交前，检查是否有日文/韩文残留。

### 6. 提交前必做清理

**每次 commit 前必须：**

1. 运行 `cargo fmt` — 确保代码格式统一
2. 运行 `cargo clippy --all-targets --all-features` — 修复所有 clippy 警告（除非有充分理由保留）

这两个检查是 CI 的硬要求，在本地提前发现能避免提交被拒。


## 项目简介

`llm-harness-runtime` 是 `llm-harness-core` 之上的平台层，定义框架扩展点（trait）并提供核心编排引擎。

**核心职责：**
1. **Trait 定义**：`AuthHook`、`BudgetExceededHook`、`InfrastructureHook` 等框架扩展接口
2. **核心编排**：`AgentHarness` 配置、`HarnessHooks` 组装、事件管道
3. **基础设施接口**：文件系统、环境变量、CancellationToken 等运行时资源抽象

不写具体的认证/预算/基础设施实现（用户自实现 trait）。

## 工作流与决策

### 范围边界

**Runtime 提供（机制层）：**
- Trait 声明与语义定义
- CompositeHook 模式的三种合成语义
- HarnessHooks 配置结构（11 个 hook 插槽）
- TaskRunnerImpl 状态机（Pending→Running→Succeeded/Failed 等）
- 单元测试（trait 实现示例，mock 对象）

**业务层负责（策略层）：**
- 具体 auth、budget、infrastructure provider（用户自实现 trait）
- LlmClient 持有与实例化
- AgentHarness 创建、hooks 组装、prompt() 驱动
- 真实文件系统、环境变量、CancellationToken 的编排

## Crate 结构与依赖规则

```
llm-harness-runtime/
├── Cargo.toml               (workspace)
├── crates/
│   ├── llm-harness-traits/  (trait 声明，纯定义)
│   ├── auth/                (AuthHook + EnvAuthHook, FileAuthHook 示例)
│   ├── budget/              (BudgetExceededHook + BudgetControlAdapter)
│   ├── infrastructure/      (InfrastructureHook + ExecutionEnv)
│   └── orchestration/       (AgentHarness + HarnessHooks + 事件管道)
├── docs/
│   └── runtime-v0.2-design.md       (总体设计决策)
│   └── superpowers/plans/*.md       (分阶段实现计划)
└── CLAUDE.md
```

依赖方向（A → B 表示 A 依赖 B）：

```
orchestration → auth, budget, infrastructure → llm-harness-traits
auth, budget, infrastructure → llm-harness-core::types
orchestration → llm-harness-core (loop, harness)
```

**硬性规则：**
- `llm-harness-traits` 不依赖任何外部 crate，保持纯 trait 声明。
- auth / budget / infrastructure 不相互依赖（允许用户单独实现一个 hook）。
- 所有 hook 实现遵循 `llm-harness-core` 的语义（如 `AuthHook::resolve()` 返回 `BoxFuture`）。
- 违反依赖方向或语义的修改不允许。

## 设计文档

设计决策记录在 `docs/` 下：

| 文件 | 内容 |
|---|---|
| `runtime-v0.2-design.md` | 总纲：决策表、v0.2 范围、路线图 |
| `superpowers/plans/2026-06-12-runtime-v0.2-phase0-*.md` | Core 阻塞项修复 |
| `superpowers/plans/2026-06-12-runtime-v0.2-phase1a-*.md` | Trait 定义与 CompositeHook 模式 |
| `superpowers/plans/2026-06-12-runtime-v0.2-phase1b-*.md` | Auth、Budget 基础设施 |
| `superpowers/plans/2026-06-12-runtime-v0.2-phase2-*.md` | Tool 注册与发现 |
| `superpowers/plans/2026-06-12-runtime-v0.2-phase3-*.md` | Orchestration（AgentHarness、HarnessHooks） |
| `superpowers/plans/2026-06-12-runtime-v0.2-phase4-*.md` | Observability（TracingHookAdapter、TraceExporter） |
| `superpowers/plans/2026-06-12-runtime-v0.2-phase5-*.md` | Integration（ExecutionEnv、CancellationToken） |
| `coding-agent-on-runtime.md` | 使用 runtime 构建 coding agent 的指南 |

实现前先读相关阶段的设计文档。每个 struct / trait 的设计理由在设计文档的 `>` 块引用中。

## 核心开发原则

### Trait 与实现的分离

- **trait 声明**（`llm-harness-traits` crate）：只定义签名、语义、错误类型，不含逻辑。
- **trait 实现**（`auth`、`budget`、`infrastructure` crate）：提供示例实现（EnvAuthHook、FileAuthHook、BudgetControlAdapter 等），用户可覆盖。
- 不在 trait 中硬编码具体行为，所有行为可替换。

### CompositeHook 合成语义

Runtime 支持用户注入多个 hook 的组合（CompositeHook 模式）。三种合成语义：

| 类型 | 语义 | 例子 |
|------|------|------|
| **All-Execute（观测型）** | 所有 hook 依次执行，返回第一个 hook 的结果 | `BeforeProviderRequestHook`、`AfterProviderResponseHook`、`AfterTurnHook` |
| **First-Non-Allow（决策型）** | 顺序执行，第一个返回 `Allow` 以外的决策立即短路 | `BeforeToolCallHook`（返回 `Allow` / `Deny` / `Modify`） |
| **Last-Non-Passthrough（修改型）** | 按序执行，每个 hook 可修改数据，返回最后一个非 passthrough 的结果 | `AfterToolCallHook`（返回 `Passthrough` / `Modify`） |

不同类型的 hook 合成方式不同，实现时严格遵守对应语义。

### HarnessHooks 配置

`HarnessHooks` 结构体持有 11 个 hook 插槽（7 个命名 + 4 个预留）：

```rust
pub struct HarnessHooks {
    // 决策类
    pub before_tool_call: Option<Arc<dyn BeforeToolCallHook>>,
    pub should_stop: Option<Arc<dyn ShouldStopHook>>,
    
    // 观测类
    pub before_provider_request: Option<Arc<dyn BeforeProviderRequestHook>>,
    pub after_provider_response: Option<Arc<dyn AfterProviderResponseHook>>,
    pub after_tool_call: Option<Arc<dyn AfterToolCallHook>>,
    pub after_turn: Option<Arc<dyn AfterTurnHook>>,
    pub prepare_next_turn: Option<Arc<dyn PrepareNextTurnHook>>,
    
    // 预留位置（v0.3+ 或用户扩展）
    // ... 4 个预留位置
}
```

用户可为任何插槽注入自定义 hook，或留空（此时框架跳过该阶段）。

### Hook 与 Event 的分工

- **Hook**（`HarnessHooks` 中的 trait）：参与框架流程、可影响行为，构造时注入，有类型安全的返回值。
- **Event**（`AgentHarnessEvent` enum）：纯通知，运行时动态订阅，无返回值。
- 不要把 hook 调用点暴露为事件，也不要把纯通知实现为 hook。

### 并发与锁

- 用 `std::sync::Mutex` + 快照模式（lock → clone → drop lock → 操作快照）。不用 `RwLock`。
- 不在持锁状态下跨 `.await`。
- Hook 实现无内部状态，所有状态通过 `ToolContext`、`AgentHarness` 等外部传入。

### 错误处理与失败语义

- Hook 返回 `Result<T, AgentError>`，失败通过 `AgentError` 表达（不是 panic）。
- 框架在第一个 hook 失败时停止链式调用（first-non-allow 语义）并向用户返回错误。
- 不强制成功，允许用户决定是否重试、回退或中止。

## 测试原则

**TDD 流程（实现 trait 时）：**

1. **先写测试**：用 mock 实现 trait，编写单元测试验证合成逻辑
2. **停下来**：提交测试，等用户审查——审查的是"trait 行为预期"，不是"实现细节"
3. **实现**：用户批准后，实现具体 hook（如 `EnvAuthHook`）
4. **重构**：在测试保护下整理代码

**基础设施：**
- Trait 的 mock 实现放在测试模块中（`#[cfg(test)]`）
- 示例实现（如 `EnvAuthHook`、`FileAuthHook`）在各自的 crate 中，带单独测试
- 集成测试用真实 `AgentHarness` + mock core types，不依赖外部 LLM

## 实现后自查

每次实现完成后，在提交前自行检查以下维度。有 ❌ 项自行修复，有 ⚠️ 项列出后交用户判断：

**架构合规**
- ❌ trait 声明是否引入了外部 crate 依赖（应仅依赖 `llm-harness-core::types`）？
- ❌ Hook 实现是否违反了 trait 签名（如 `AuthHook::resolve()` 必须返回 `BoxFuture`）？
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
- ⚠️ 实现是否与设计文档有出入？若有，更新对应的 `docs/superpowers/plans/` 文件。
- ⚠️ 新增或修改了 trait 签名、HarnessHooks 插槽、CompositeHook 语义？更新本文件（CLAUDE.md）对应段落。

## Git 规则

- **每个阶段结束后自动提交并推送**。每完成一个独立的阶段或功能单元（如一个 Phase、一个 crate、一批测试），在 `cargo test` 和 `cargo clippy` 通过后：
  1. 自行 review 本次改动（diff 正确、无多余文件、无调试代码）
  2. `git commit`
  3. `git push`
  无需等用户明确要求。不要累积多个阶段后一次性提交推送。
- Commit message 格式：`<type>(<scope>): <description>`
  - type：`feat` / `fix` / `docs` / `test` / `refactor` / `chore`
  - scope：crate 名，如 `traits`、`auth`、`budget`、`orchestration`
  - 示例：`feat(auth): implement EnvAuthHook`、`test(orchestration): add CompositeHook tests`
- description 说明 *为什么* 而非 *做了什么*。

## v0.2 vs v0.3+ 边界标记

在代码和注释中，v0.2 范围边界处用以下标记：

- **`v0.2 明确不实现`**：代码中明确排除，v0.3+ 才支持（如 SubAgentSpawner）
- **`v0.3+ API 预览`**：代码注释中预示未来 API，当前代码中不实现

这些标记帮助用户理解当前版本的范围，避免期望超出设计。

## 收尾

每次开发完成后：提交并推送本仓库变更；如有进度变化同步更新顶层 `STATUS.md` 并推送 `oh-my-harness/oh-my-harness`。
