# Tutor Agent — 基于 llm-harness-runtime 的教学 Agent 设计

> 状态: 设计阶段 | 仓库: 独立新仓库 `tutor-agent` | runtime 依赖: `llm-harness-runtime` v0.2+

---

## 目录

1. [背景与目标](#1-背景与目标)
2. [整体架构](#2-整体架构)
3. [工具集设计](#3-工具集设计)
4. [Capability 设计](#4-capability-设计)
5. [编排层设计](#5-编排层设计)
6. [沙箱设计](#6-沙箱设计)
7. [审计与预算](#7-审计与预算)
8. [Web UI 与 API](#8-web-ui-与-api)
9. [实现计划](#9-实现计划)
10. [与 Runtime 的分工](#10-与-runtime-的分工)

---

## 1. 背景与目标

### 1.1 为什么要做 Tutor Agent

`llm-harness-runtime` 需要一个 **非 coding agent 的场景** 来验证框架的通用性。教学场景天然需要框架的治理能力：

| 框架特性 | 教学场景需求 |
|---------|------------|
| Sandbox 隔离 | 执行不受信任的学生代码 |
| BudgetControl | 学生预算有限，超支必须硬停止 |
| AuditSink | 学习轨迹可追溯（学了什么、掌握程度） |
| HumanApproval | 老师审批试卷内容、批改结果 |
| SubAgentSpawner | 并行研究多主题、TutorBot 自治导师 |
| PrepareNextTurnHook | 教学阶段切换（讲解 → 练习 → 测验） |

参考项目：[DeepTutor](https://github.com/HKUDS/DeepTutor) — agent-native 个性化教学平台，提供 Chat、Deep Solve、Deep Question、Deep Research、Math Animator 等 capability。

### 1.2 v0.1 目标（最小验证）

**交付物：** 一个可通过 Web UI 交互的教学 agent，支持 3 个 capability：

- **Chat** — 基于知识库的问答辅导
- **Deep Solve** — 逐步引导解题（先理解 → 再解答 → 最后验证）
- **Code Exec** — 在沙箱内安全执行代码并解释结果

**验证目标：**

1. Runtime 框架能否支撑教学场景的核心流程（多阶段状态机、动态工具切换）
2. 沙箱隔离在学生代码执行场景是否可靠
3. 预算控制能否在学生场景正常工作（hard-limit 阻断验证）
4. 审计日志能否完整记录学习轨迹

**范围边界（v0.1 不做）：**

- Multi-user 隔离（单用户部署）
- Deep Question（自动出题）/ Deep Research（多子 agent 并行研究）
- Math Animator（数学动画）
- Three-layer Memory（学习记忆系统）
- TutorBot（持久化自治 AI 导师）

---

## 2. 整体架构

### 2.1 仓库与 crate 结构

```
tutor-agent/                        (独立仓库)
├── Cargo.toml                      (workspace)
├── crates/
│   ├── tutor-tools/                (教学工具实现)
│   │   ├── src/
│   │   │   ├── rag_search.rs       # RAG 知识库检索
│   │   │   ├── web_search.rs       # 网络搜索
│   │   │   ├── code_exec.rs        # 沙箱代码执行
│   │   │   └── lib.rs
│   │   └── Cargo.toml              # dep: llm-harness-runtime (Tool trait), sandbox crate
│   │
│   ├── tutor-agent/                (编排核心)
│   │   ├── src/
│   │   │   ├── capability.rs       # CapabilityRouter (Chat/DeepSolve/CodeExec)
│   │   │   ├── phase_manager.rs    # PrepareNextTurnHook 实现
│   │   │   ├── verifier.rs         # TaskVerifier 实现
│   │   │   ├── kb.rs               # KnowledgeBase (ResourceProvider 实现)
│   │   │   └── lib.rs
│   │   └── Cargo.toml              # dep: tutor-tools, llm-harness-runtime
│   │
│   └── tutor-web/                  (Web 后端 API)
│       ├── src/
│       │   ├── routes/             # REST + WebSocket endpoints
│       │   ├── session.rs          # Session 管理
│       │   └── main.rs             # Server 入口
│       └── Cargo.toml              # dep: tutor-agent, axum / actix-web
│
├── web-ui/                         (前端)
│   ├── src/
│   │   ├── components/             # ChatBox, CapabilitySelector, BudgetPanel
│   │   └── App.tsx
│   └── package.json                # Next.js / React
│
└── docs/
    └── tutor-agent-design.md       # 本设计文档

依赖:
  llm-harness-runtime               (git dep or crates.io v0.2)
  llm-harness-runtime-sandbox-bwrap (Linux 生产)
  llm-harness-runtime-sandbox-seatbelt (macOS 生产)
  llm-harness-runtime-trace-otel
  llm-harness-runtime-audit-jsonl
  llm-harness-runtime-auth
```

### 2.2 运行时架构

```
┌──────────────────────────────────────────────┐
│                Web UI (Next.js)               │
│  ChatBox | CapabilitySelector | BudgetPanel   │
└──────────────────┬───────────────────────────┘
                   │ WebSocket (实时消息)
                   │ REST (session 管理、KB 操作)
┌──────────────────v───────────────────────────┐
│              tutor-web (axum)                 │
│  WS handler | REST routes | Session pool     │
└──────────────────┬───────────────────────────┘
                   │ 内部调用
┌──────────────────v───────────────────────────┐
│             tutor-agent (编排)                │
│                                               │
│  CapabilityRouter ◄── 选择 capability         │
│       │                                       │
│  ┌────v────┐  ┌──────────┐  ┌─────────────┐ │
│  │  Chat   │  │Deep Solve│  │  Code Exec  │ │ │
│  └────┬────┘  └────┬─────┘  └──────┬──────┘ │
│       │            │               │         │
│       └────────────v───────────────┘         │
│            PhaseManager                       │
│       (PrepareNextTurnHook impl)             │
│                                               │
│  KnowledgeBase    TaskVerifier                │
│  (ResourceProvider)  (答案判定)              │
└──────────────────┬───────────────────────────┘
                   │ Tool trait
┌──────────────────v───────────────────────────┐
│              tutor-tools                      │
│  rag_search | web_search | code_exec         │
│                  ↕                             │
│    Sandbox (code_exec 内部)                   │
└──────────────────────────────────────────────┘
                   │
┌──────────────────v───────────────────────────┐
│         llm-harness-runtime                   │
│  TaskRunner | ToolRegistry | TraceExporter   │
│  BudgetControlAdapter | AuditSink | AuthHook │
└──────────────────────────────────────────────┘
```

---

## 3. 工具集设计

### 3.1 工具清单

```
T0 读取型（无副作用，直接放行）
├── rag_search(query: str, kb: str) → str
│   从指定知识库检索教材/讲义内容
│   impl: ResourceProvider::read() 包装为 Tool
│
├── web_search(query: str) → str
│   搜索互联网资源（通过 Brave/SerpAPI MCP）
│   impl: 通过 McpClientHandle 调用外部 MCP server
│
└── get_session_context() → SessionContext
    读取当前学习会话的上下文（进度、已学内容、笔记）

T1 可逆写入
└── save_note(content: str, topic: str) → NoteId
    保存学习笔记（可编辑/删除）
    impl: 写入本地文件或 SQLite

T2 外部副作用（推荐 HumanApproval 门控）
├── code_exec(language: str, code: str) → ExecResult
│   在沙箱内安全执行代码，返回 stdout/stderr + exit_code
│   impl: 调用 Sandbox::env() → ExecutionEnv::exec()
│   安全约束：网络默认禁止、超时 30s、内存限制 256MB
│
└── submit_answer(question_id: str, answer: str) → Verdict
    提交解题答案，触发 TaskVerifier 判定
    impl: TaskVerifier::verify() 的封装
```

### 3.2 工具激活矩阵

通过 `PrepareNextTurnHook` (PhaseManager) 动态切换：

| Capability | 理解/探索阶段 | 解答/执行阶段 | 验证/总结阶段 |
|-----------|-------------|-------------|-------------|
| **Chat** | `rag_search` + `web_search` | — | `save_note` |
| **Deep Solve** | `rag_search` + `web_search` | `code_exec` + `submit_answer` | `save_note` |
| **Code Exec** | `rag_search` | `code_exec` | — |

每阶段活跃工具数控制在 2-3 个，符合 runtime 建议的 5-12 个上限。

---

## 4. Capability 设计

### 4.1 Chat — 基于知识库的问答

**流程：**
```
User: "解释一下牛顿第三定律"
  → Phase::Explore: rag_search("牛顿第三定律", "physics-kb")
     └→ ResourceProvider 检索教材内容
  → Phase::Respond: agent 基于检索结果生成回答
  → User 可追问，循环 Explore → Respond
```

**状态机：**
```
Explore ←→ Respond → Complete
  │                    │
  └── (追问) ──→ Explore
```

**Hook 配置：**
- `PrepareNextTurnHook` → PhaseManager（Explore ↔ Respond 切换）
- `AfterTurnHook` → 记录问答对到审计日志

### 4.2 Deep Solve — 逐步引导解题

**流程：**
```
User: "求解积分 ∫x²dx 从 0 到 2"
  → Phase::Understand: rag_search("定积分", "calculus-kb") + web_search
     └→ Agent 理解题目，展示相关概念
  → Phase::Solve: code_exec("python", "from sympy import ...")
     └→ Agent 分步求解，每一步都在沙箱验证
  → Phase::Verify: submit_answer(question_id, answer)
     └→ TaskVerifier 判定答案正确性
  → Phase::Review: 展示解题过程 + 知识点总结
```

**状态机：**
```
Understand → Solve → Verify → Review → Complete
     │         │        │
     └── (需要更多信息) ──→ Understand
               │
               └── (答案错误) ──→ Solve (重试，最多 3 次)
```

**TaskVerifier 判定规则（Deep Solve）：**
1. 代码执行结果是否与预期一致（数值/符号比对）
2. 解题步骤是否完整（通过 checkpoint 检查是否覆盖所有步骤）
3. 若 3 次提交都错误 → `TaskVerdict::Fail`，进入 Review 阶段展示正确答案

**Hook 配置：**
- `PrepareNextTurnHook` → PhaseManager（四阶段切换）
- `BeforeToolCallHook` → `HumanApprovalWrapper`（`code_exec` 执行前审批，可选关闭）
- `AfterToolCallHook` → 采集解题步骤（审计）
- `ShouldStopHook` → 预算超限时阻断

### 4.3 Code Exec — 沙箱代码执行

**流程：**
```
User: "帮我运行这段 Python 代码，看看输出什么"
  → Agent: code_exec("python", user_code)
     └→ Sandbox 内执行，返回 stdout/stderr
  → Agent: 解释执行结果
```

**安全约束（通过 SandboxConfig 配置）：**
```rust
SandboxConfig {
    fs_allowlist: vec!["/tmp/sandbox"],  // 只能写临时目录
    net_allowlist: vec![],               // 默认禁止网络
    resource_limits: ResourceLimits {
        max_memory_mb: Some(256),
        max_cpus: Some(1),
        timeout: Some(Duration::from_secs(30)),
        max_disk_mb: Some(100),
    },
    work_dir: Some(PathBuf::from("/tmp/sandbox")),
}
```

---

## 5. 编排层设计

### 5.1 CapabilityRouter

用户选择 capability → 路由到对应的 AgentHarness 配置：

```rust
struct CapabilityConfig {
    capability: Capability,
    tools: Vec<String>,           // ToolRegistry 子集
    phases: Vec<Phase>,           // 状态机阶段定义
    sandbox_config: SandboxConfig,
    verifier: Arc<dyn TaskVerifier>,
}

enum Capability {
    Chat,
    DeepSolve,
    CodeExec,
}

struct CapabilityRouter {
    configs: HashMap<Capability, CapabilityConfig>,
    registry: Arc<dyn ToolRegistry>,
}

impl CapabilityRouter {
    /// 根据 capability 创建完整的 HarnessHooks + TaskRunner 配置
    fn build_harness(&self, capability: Capability) -> (TaskRunner, HarnessHooks) {
        let config = &self.configs[&capability];
        let tools = self.registry.subset(&config.tools);
        let phase_manager = PhaseManager::new(&config.phases);
        let hooks = HarnessHooks {
            prepare_next_turn: Some(Arc::new(phase_manager)),
            should_stop: Some(budget_adapter.clone()),
            after_tool_call: Some(audit_adapter.clone()),
            // ...
            ..HarnessHooks::none()
        };
        (runner, hooks)
    }
}
```

### 5.2 PhaseManager (PrepareNextTurnHook)

```rust
struct PhaseManager {
    phases: Vec<Phase>,
    history: Mutex<Vec<TurnRecord>>,
}

#[derive(Clone)]
struct Phase {
    name: String,
    active_tools: Vec<String>,
    /// 进入下一阶段的触发条件
    transition_rule: TransitionRule,
}

enum TransitionRule {
    AfterNTurns(usize),                  // N 个 turn 后自动切换
    OnToolSuccess(String),               // 某工具成功后切换
    OnVerificationComplete,              // TaskVerifier 返回非 Indeterminate 后
    Manual,                              // 由用户消息触发
}

impl PrepareNextTurnHook for PhaseManager {
    async fn prepare_next_turn(&self, ctx: &TurnContext) -> NextTurnDirective {
        let history = self.history.lock().unwrap();
        let current_phase = self.current_phase(&history);

        // 判断是否应该切换阶段
        if let Some(next_phase) = self.should_transition(&current_phase, &history, ctx) {
            NextTurnDirective {
                tools: Some(next_phase.active_tools.clone()),
                ..NextTurnDirective::default()
            }
        } else {
            NextTurnDirective {
                tools: Some(current_phase.active_tools.clone()),
                ..NextTurnDirective::default()
            }
        }
    }
}
```

### 5.3 TaskVerifier

```rust
struct DeepSolveVerifier {
    expected_answer: Option<Value>,  // 可选的预设答案
    max_retries: usize,
}

impl TaskVerifier for DeepSolveVerifier {
    async fn verify(&self, trace: &TaskTrace) -> TaskVerdict {
        // 1. 检查是否有 submit_answer 调用
        let submission = trace.checkpoints.iter()
            .find(|cp| cp.payload.get("type") == Some(&json!("submit_answer")));

        match submission {
            None => TaskVerdict::Indeterminate {
                reason: "awaiting answer submission".into()
            },
            Some(cp) => {
                // 2. 与预期答案比对（如果有）
                if let Some(expected) = &self.expected_answer {
                    let student_answer = &cp.payload["answer"];
                    if student_answer == expected {
                        TaskVerdict::Pass
                    } else {
                        // 3. 让 agent 重试（最多 max_retries 次）
                        let retries = trace.checkpoints.iter()
                            .filter(|c| c.payload.get("type") == Some(&json!("submit_answer")))
                            .count();
                        if retries < self.max_retries {
                            TaskVerdict::Fail {
                                reason: format!("incorrect answer (attempt {}/{})",
                                    retries, self.max_retries)
                            }
                        } else {
                            TaskVerdict::Fail {
                                reason: "max retries exceeded".into()
                            }
                        }
                    }
                } else {
                    // 无预期答案时，由 agent 提交自我评估
                    TaskVerdict::Indeterminate {
                        reason: "no expected answer — requires teacher review".into()
                    }
                }
            }
        }
    }
}
```

---

## 6. 沙箱设计

### 6.1 code_exec 的安全隔离

`code_exec` 是唯一需要沙箱的工具。用户（学生）提交的代码不可信，必须隔离执行。

```rust
// tutor-tools/src/code_exec.rs

pub struct CodeExecTool {
    sandbox: Arc<dyn Sandbox>,
}

#[async_trait]
impl Tool for CodeExecTool {
    fn name(&self) -> &str { "code_exec" }
    fn description(&self) -> &str {
        "Execute code in a sandboxed environment. Supports Python, JavaScript, and Bash."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "language": { "type": "string", "enum": ["python", "javascript", "bash"] },
                "code": { "type": "string", "description": "Code to execute" }
            },
            "required": ["language", "code"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let lang = args["language"].as_str().unwrap_or("python");
        let code = args["code"].as_str().ok_or(AgentError::InvalidArgument)?;

        // 1. 每次执行前 reset 沙箱到干净状态
        self.sandbox.reset().await?;

        // 2. 将代码写入沙箱临时文件
        let filename = match lang {
            "python" => "script.py",
            "javascript" => "script.js",
            "bash" => "script.sh",
            _ => return Err(AgentError::UnsupportedOperation),
        };
        self.sandbox.env().write_file(filename, code.as_bytes()).await?;

        // 3. 在沙箱内执行
        let cmd = match lang {
            "python" => format!("python3 {}", filename),
            "javascript" => format!("node {}", filename),
            "bash" => format!("bash {}", filename),
            _ => unreachable!(),
        };
        let result = self.sandbox.env().exec(&cmd).await?;

        // 4. 返回结果
        Ok(ToolResult::success(json!({
            "stdout": result.stdout,
            "stderr": result.stderr,
            "exit_code": result.exit_code,
            "duration_ms": result.duration_ms,
        })))
    }
}
```

### 6.2 部署环境切换

```rust
// tutor-agent 启动时按 OS 选择沙箱后端
fn create_sandbox(config: &SandboxConfig) -> Arc<dyn Sandbox> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(BwrapSandbox::new(config.clone()))
    }
    #[cfg(target_os = "macos")]
    {
        Arc::new(SeatbeltSandbox::new(config.clone()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        // 开发时 fallback 到 OsEnvSandbox（无隔离）
        Arc::new(OsEnvSandbox::new(config.clone()))
    }
}
```

---

## 7. 审计与预算

### 7.1 审计事件

教学场景需要记录的审计事件：

| 事件类型 | 触发点 | 记录内容 |
|---------|-------|---------|
| `SessionStart` | TaskRunner 启动新 session | 学生 ID、capability 选择、KB 名称 |
| `ToolCall` | 每个工具调用 | 工具名、参数（脱敏）、是否成功 |
| `CodeExec` | `code_exec` 调用（独立事件） | 语言、代码长度、exit_code、执行时长 |
| `AnswerSubmission` | `submit_answer` 调用 | 题目 ID、答案（hash）、判定结果 |
| `PhaseTransition` | PhaseManager 切换阶段 | 阶段名、触发原因 |
| `BudgetExceeded` | BudgetControlAdapter 超限 | 累计成本、预算上限 |

**代码执行审计示例：**
```json
{
  "timestamp": "2026-06-12T10:30:00Z",
  "event_type": "CodeExec",
  "principal": "student:alice",
  "payload": {
    "language": "python",
    "code_length": 256,
    "exit_code": 0,
    "duration_ms": 1200
  },
  "decision": "allow",
  "hash": "sha256:abc123..."
}
```

### 7.2 预算控制

**硬限制配置（学生场景）：**
```rust
let pricing = Arc::new(DeepTutorPricingProvider);
let budget_adapter = BudgetControlAdapter::new(
    pricing,
    max_total_cost: 2.00,     // 每 session 最多 $2.00
    exceeded_hook: Some(hook), // 超限时通知学生
);
// 通过 ShouldStopHook 阻断：预算超限 → task 进入 Failed
```

**Web UI 暴露**：实时显示 `AgentHarness::current_cost()`，让学生看到余额。

---

## 8. Web UI 与 API

### 8.1 后端 API (axum)

```
REST:
  GET  /api/sessions              — 列出 session
  POST /api/sessions              — 创建新 session
       body: { capability: "chat" | "deep_solve" | "code_exec", kb?: "physics" }
  GET  /api/sessions/:id          — 获取 session 详情
  DELETE /api/sessions/:id        — 结束 session

  GET  /api/sessions/:id/cost     — 当前成本
  GET  /api/sessions/:id/audit    — 审计日志

  GET  /api/kb                    — 列出知识库
  POST /api/kb                    — 创建知识库

WebSocket:
  WS   /ws/sessions/:id           — 实时消息流
       客户端 → 服务端: { type: "message", content: "..." }
       服务端 → 客户端: { type: "message", content: "...", tool_calls: [...] }
                      { type: "phase_change", phase: "solve" }
                      { type: "budget_warning", remaining: 0.50 }
                      { type: "approval_request", tool: "code_exec", ... }
                      { type: "error", message: "..." }
```

### 8.2 前端组件

```
┌────────────────────────────────────────────┐
│  Tutor Agent                              │ │
├────────────────────────────────────────────┤
│  [CapabilitySelector]  Chat | Deep Solve   │
│  [KbSelector]  physics ▼                   │
├────────────────────────────────────────────┤
│                                            │
│  ┌──────────────────────────────────────┐  │
│  │          ChatBox                     │  │
│  │                                      │  │
│  │  User: 解释牛顿第三定律               │  │
│  │                                      │  │
│  │  Agent: 牛顿第三定律指出...           │  │
│  │  [📖 rag_search: "牛顿第三定律"]      │  │
│  │                                      │  │
│  │  User: 给个例子                       │  │
│  │  ...                                  │  │
│  └──────────────────────────────────────┘  │
│                                            │
├────────────────────────────────────────────┤
│  [MessageInput]               [Send]       │
│                                            │
│  Budget: $0.42 / $2.00  ● Phase: Solve     │
└────────────────────────────────────────────┘
```

前端技术栈：**Next.js + Tailwind CSS + shadcn/ui**（快速搭建，社区成熟）。不选 Yew/Leptos 的原因：
- 教学 agent 的目标用户通过浏览器访问，不需要 Rust 前端
- Next.js 生态的 streaming/SSE 支持更成熟
- 前端不涉及 runtime 框架的核心验证目标，用最熟悉的工具即可

---

## 9. 实现计划

### Phase 1 — 最小骨架（Week 1-2）

```
目标: 用 tutor-agent + tutor-tools + runtime 跑通一个端到端的 Chat capability

1. [ ] 搭建 tutor-agent 仓库 + Cargo workspace
2. [ ] 实现 tutor-tools: rag_search, web_search, code_exec (3 个工具)
3. [ ] 实现 tutor-agent: CapabilityRouter + PhaseManager (Chat only)
4. [ ] 配置 runtime: TaskRunner + OsEnvSandbox + auth
5. [ ] CLI 验证: 发一条 chat 消息，agent 回答

验证标准:
  - rag_search 能从知识库检索内容
  - code_exec 在 OsEnvSandbox 中执行 Python 代码
  - Chat capability 的 Explore → Respond 状态机正常流转
```

### Phase 2 — Deep Solve + Code Exec（Week 3-4）

```
目标: 实现完整的 3 个 capability + 沙箱隔离 + TaskVerifier

1. [ ] 实现 DeepSolve 状态机 (Understand → Solve → Verify → Review)
2. [ ] 实现 DeepSolveVerifier
3. [ ] 切换到 BwrapSandbox / SeatbeltSandbox
4. [ ] 实现 CodeExec capability（独立 capability，非 DeepSolve 子步骤）
5. [ ] 实现 HumanApprovalWrapper（code_exec 审批门控）
6. [ ] 集成测试: Chat + Deep Solve + Code Exec 全部跑通

验证标准:
  - Deep Solve 能逐步引导解题 + 验证答案
  - 沙箱内代码执行安全隔离（网络禁止、超时生效）
  - HumanApprovalWrapper 对 code_exec 正确触发审批
```

### Phase 3 — 审计 + 预算（Week 4-5）

```
目标: 加入治理能力

1. [ ] 配置 AuditSink (JSONL)，采集教学事件
2. [ ] 配置 BudgetControlAdapter + PricingProvider
3. [ ] 验证: 审计 hash 链完整性、预算超限→Failed 状态转换
4. [ ] 实现 session 持久化（cost、audit log 跨 session 保留）

验证标准:
  - 审计日志包含所有关键事件（SessionStart, CodeExec, AnswerSubmission 等）
  - 预算超限后 agent 停止，向学生展示超限信息
  - hash 链验证：修改一条 entry → 检测到断裂
```

### Phase 4 — Web UI（Week 5-6）

```
目标: 可通过浏览器使用

1. [ ] 实现 tutor-web: REST API + WebSocket
2. [ ] 实现 web-ui: ChatBox + CapabilitySelector + BudgetPanel
3. [ ] WebSocket 实时流：agent 消息、阶段切换、预算警告
4. [ ] 人工审批 UI（浏览器弹窗，WebSocket 双向通信）
5. [ ] 端到端测试: 浏览器 → WebSocket → agent → 沙箱 → 返回

验证标准:
  - 浏览器中完成一次完整的 Deep Solve 流程
  - 预算面板实时更新
  - code_exec 审批弹窗正常触发
```

### Phase 5 — 文档 + Demo（Week 6）

```
目标: 产出验证报告

1. [ ] 写 README + 部署指南
2. [ ] 录制 Demo: Chat + Deep Solve + Code Exec 完整流程
3. [ ] 写框架验证报告：
    - 哪些 trait/hook 被使用，使用体验如何
    - 哪些场景发现框架不足或 bug
    - CompositeHook 冲突是否出现
4. [ ] 提交反馈给 llm-harness-runtime
```

---

## 10. 与 Runtime 的分工

| 层 | 负责 | 具体 |
|----|------|------|
| **llm-harness-runtime** | 机制 | TaskRunner、Sandbox trait、ToolRegistry、TraceExporter、AuditSink、BudgetControlAdapter、HumanApprovalWrapper |
| **tutor-agent** | 策略 | 工具实现（rag_search 等）、PhaseManager 阶段逻辑、TaskVerifier 判定规则、KnowledgeBase 管理 |
| **tutor-web / web-ui** | 体验 | REST API、WebSocket 实时流、ChatBox UI、BudgetPanel、人工审批弹窗 |

**关键原则：** 不在 tutor-agent 中重新实现 runtime 已有的能力。如预算控制只配置 `BudgetControlAdapter`，不自写 cost tracking。

---

## 附录 A: 依赖版本锁定

```toml
# tutor-agent/Cargo.toml (workspace)
[workspace.dependencies]
llm-harness-runtime = { git = "https://github.com/.../llm-harness-runtime", branch = "v0.2" }
llm-harness-runtime-sandbox-bwrap = { git = "...", branch = "v0.2" }
llm-harness-runtime-sandbox-seatbelt = { git = "...", branch = "v0.2" }
llm-harness-runtime-trace-otel = { git = "...", branch = "v0.2" }
llm-harness-runtime-audit-jsonl = { git = "...", branch = "v0.2" }
llm-harness-runtime-auth = { git = "...", branch = "v0.2" }
axum = "0.8"
tokio = "1"
serde = "1"
serde_json = "1"
```

## 附录 B: 验证清单

完成 Phase 4 后逐项验证：

- [ ] Chat capability: 问答 + 追问循环正常
- [ ] Deep Solve capability: 四阶段状态机完整流转
- [ ] Code Exec capability: 沙箱隔离生效、超时/资源限制生效
- [ ] 预算超限: hard-limit 触发后 task 进入 Failed，agent 停止
- [ ] 审计: hash 链完整，篡改一条 entry 后验证失败
- [ ] 人工审批: code_exec 触发审批弹窗、Deny 后工具不执行
- [ ] 动态工具切换: PhaseManager 在不同阶段激活不同工具子集
- [ ] Web UI: WebSocket 实时流、预算面板实时更新
- [ ] 沙箱后端切换: Linux (Bwrap) 和 macOS (Seatbelt) 都可运行
