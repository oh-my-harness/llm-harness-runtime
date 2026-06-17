# Coding Agent — 路线图

> 这不是一个一次性的 example，这是一个长期项目。最终目标：做一个很好用的 coding agent，完整展示框架所有能力，并用实战反哺框架优化。

---

## 愿景

```
                  ┌──────────────────────────┐
                  │     coding-agent CLI     │
                  │  (用户面对的产品)          │
                  ├──────────────────────────┤
                  │  tools/ | hooks/ | ui/    │
                  │  prompt | session | skill │
                  ├──────────────────────────┤
                  │   llm-harness-runtime     │
                  │  (AgentHarness | Loop |   │
                  │   Adapter | Types)        │
                  └──────────────────────────┘
                         ↑  反馈循环  ↓
                   框架发现不足 → 框架优化 → agent 更强
```

两个目标同等重要：

1. **做一个好用的 coding agent** — 而不是"一个能用但很糙的 demo"
2. **推动框架成熟** — coding agent 是框架最苛刻的用户。它缺的东西，框架就应该补上

---

## Phase 1：跑起来（现在）

**目标**：一个能用的交互式编码助手，展示 AgentHarness + Tool + Hook + Event 的全链路。

| 模块 | 内容 | 覆盖的框架能力 |
|---|---|---|
| `main.rs` | CLI 入口 + stdin 交互循环 | AgentHarness::new_in_memory, prompt(), subscribe() |
| `tools/read_tool.rs` | 读取文件（行范围） | `impl Tool`, ExecutionEnv, ContentBlock |
| `tools/edit_tool.rs` | 搜索替换编辑 | `impl Tool`, Sequential 模式 |
| `tools/bash_tool.rs` | shell 执行（超时+截断） | `impl Tool`, ShellOptions |
| `tools/grep_tool.rs` | 文件搜索 | `impl Tool`, 委托 shell |
| `hooks.rs` | 危险命令防护 | `impl BeforeToolCallHook` |
| `output.rs` | 事件→终端渲染 | AgentHarnessEvent, TextDelta, ToolCallStart/End |
| `prompt.rs` | 系统提示词常量 | AgentHarnessOptions.system_prompt |

**交付标准**：用户 `cargo run` 就能用，能完成"读取代码 → 理解 → 修改 → 运行测试"这个最小闭环。

**不做**：TUI、持久化、compaction、diff-based edit。

---

## Phase 2：够好用

**目标**：从"能用"到"想用"。改进工具质量、交互体验和可靠性。

### 工具升级

| 工具 | Phase 1 | Phase 2 |
|---|---|---|
| edit | search/replace 简单替换 | unified diff 格式：LLM 输出 diff → apply + lint → 失败回退 |
| bash | 委托 shell，简单截断 | 自适应截断（头500行 + 尾500行），\n working_dir 管理，env 继承 |
| read | 行范围 | 语法高亮（如 bat），大文件自动分段 |
| grep | 委托 shell grep | 内置递归搜索（rust native），glob 过滤 |
| **新增** | — | `ls`（目录浏览）、`find`（文件查找）、`file_search`（模糊路径搜索） |

### 交互改进

- edit 后自动运行 `cargo check` / `cargo build`，失败自动回退或重试
- 支持 `!npm test` 语法（用户绕过 agent 直接执行命令，结果可注入上下文）
- 更好的输出截断策略（对 LLM 来说"看到了中间被截断的内容"比"只看到最后几行"更有用）
- BashTool 支持长时间运行的后台任务（编译时 agent 可以做别的）

### 框架反馈（P2.1）

这些问题在写 Phase 2 时一定会遇到，遇到就改框架：

- `Tool::execute` 的签名是否需要支持长时间运行 + 进度推送 → 检查 `update_tx` 的现有设计是否够用
- `AgentHarness::subscribe()` 返回的是 `Arc<AgentHarnessEvent>`，每轮 prompt 需要重新 subscribe 还是可以复用 → 检查 broadcaster 的 buffer 策略
- 多工具并发执行时，事件顺序的可预测性 → StreamingState 的 index 排序逻辑
- tool 参数的深拷贝/引用问题 → `&'a ToolContext` 的 lifetime 约束是否合理

---

## Phase 3：产品化

**目标**：一个可以日常使用的产品级 coding agent。

| 能力 | 描述 |
|---|---|
| 会话持久化 | `JsonlSessionRepo`，支持 resume 中断的会话 |
| 上下文压缩 | 注入 `BeforeCompactHook`，超出阈值自动压缩 |
| 分支管理 | 支持 `fork()` + `navigate_tree()`（实验不同的修改方案） |
| Token 计价 | `AfterProviderResponseHook` 统计每次调用的 token 成本 |
| 超时 / 重试 | Provider 容错（网络波动时自动重试） |
| 环境隔离 | Sandbox 限制 agent 能访问的目录和命令 |
| 配置文件 | 支持 `~/.coding-agent.toml`（默认 model、工具开关、安全策略） |

### 框架反馈（P3.1）

- 产品级 agent 的配置加载 → 框架是否需要一个标准化的 `AgentConfig` 合并机制（CLI args > env > config file > defaults）
- 会话恢复时的事件流重建 → `AgentHarness` 的事件广播在恢复后是否应该重放历史事件
- Hook 执行顺序是否满足产品需求 → `HarnessHooks` 的 Composite 组合语义是否需要调整

---

## Phase 4：生态化

**目标**：插件系统落地，coding agent 本身也变成"别人可以扩展的平台"。

| 能力 | 描述 |
|---|---|
| 迁移到 HarnessBuilder | `install(CodingToolsPlugin).install(AuditPlugin)` 替代手动构造 |
| 第三方工具 | 用户自己写 `impl Tool` 注册 |
| MCP 集成 | McpToolSource 从 MCP server 发现工具 |
| Skill 系统 | 加载社区编写的编码 skill（"重构这段代码"、"加单元测试"） |
| 子代理 | SubAgentSpawner 实现 （"读一下这个文件的结构，总结给我"） |

**框架反馈（P4.1）**：
- MCP 集成在真实 coding agent 中的实用性 → `McpClientHandle` 的接口是否需要调整
- Skill 的加载路径和版本管理 → 框架的 Skill trait 和 PromptSource 是否够用
- Plugin 的冲突诊断在实际组合多个插件时是否准确

---

## 路线图总览

```
Phase 1               Phase 2               Phase 3               Phase 4
 跑起来              够好用                产品化               生态化
───────              ───────               ───────              ───────
AgentHarness          diff edit             会话持久化           HarnessBuilder
4 个基本工具          自适应截断             上下文压缩           Plugin 生态
stdin/stdout          bash 增强             分支/回退            MCP 集成
dangerous guard       lint 验证             成本追踪             Skill 系统
事件流渲染            !cmd 语法             Sandbox             子代理

                      ┌──────────────────────────────────────┐
                      │          持续: 框架反馈循环           │
                      │  coding agent 遇到瓶颈 → 优化框架    │
                      │  → coding agent 变强 → 发现新瓶颈    │
                      └──────────────────────────────────────┘
```

---

## 反馈循环为什么是核心

Phase 2 开始，coding agent 会成为框架最挑剔的用户。它会有最复杂的工具调用模式、最长上下文、最严格的 prompt 工程需求。

每一次"这里框架不方便"都是在告诉我们应该改什么：

```
编码 agent 写了一个工具，签名不顺手
  → Tool trait 需要调整接口
  → 框架类型模块升级
  → agent 更顺滑

编码 agent 的事件渲染性能不够
  → broadcast::Receiver 的 batch 策略
  → 输出层更流畅
  → 用户体验提升
```

这个反馈循环本身就是这个 example 作为"活文档"的终极形态——它不只是展示框架怎么用，它还在告诉框架应该变成什么样。
