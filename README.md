# llm-harness-runtime

> **[MISSION.md](MISSION.md)** — 本仓库及 harness 小组的纲领文档。从第一性原理推导 harness 的本质与使命，所有开发/业务决策以此为准。

Rust implementation of the oh-my-harness LLM agent orchestration runtime: workflow engine, agent harness, tools, event stream, and sandboxing.

> **Private repo.** Source is not distributed. Public consumers receive only git rev crates for Rust downstreams.

---

## 架构总览

```mermaid
graph TD
    subgraph "上层 Agent"
        SA[业务 Agent]
    end

    subgraph "策略层（可选）"
        ST[llm-harness-strategy<br/>safety / loop-safety / status-panel<br/>event-stream / injection-filter<br/>observation-shielding / memory-defense<br/>notify / budget]
    end

    subgraph "编排层"
        RT[llm-harness-subagents + llm-harness-workflow<br/>spawn / workflow / lifecycle / rules]
    end

    subgraph "多 Agent 协作"
        TEAM[llm-harness-agent-team<br/>AgentTeam / SocialContext / 社交工具]
    end

    subgraph "调试观测"
        INSP[llm-harness-inspector<br/>REST + WebSocket 事件流]
    end

    subgraph "Agent 层"
        AG[llm-harness-agent<br/>HarnessBuilder / AgentHarness<br/>Session / Compaction / Skills]
    end

    subgraph "Loop 层"
        LP[llm-harness-loop<br/>ReAct 循环 / streaming / retry]
    end

    subgraph "类型层"
        TY[llm-harness-types<br/>Tool / Hook / Message / RunContext]
    end

    subgraph "平台层"
        PL[llm-harness-platform<br/>Sandbox / AuditSink / TraceExporter / PromptSource]
    end

    subgraph "后端 Crate（独立可选）"
        TOOLS[llm-harness-tools<br/>bash/read/write/edit/grep/glob<br/>code_exec/web_search/web_fetch]
        MCP[llm-harness-mcp]
        KN[llm-harness-knowledge<br/>+ knowledge-local]
        MEM[llm-harness-memory]
        SR[llm-harness-session-recall]
        SB[llm-harness-sandbox<br/>bwrap/seatbelt/os]
        AJ[llm-harness-audit-jsonl]
        TO[llm-harness-trace-otel]
    end

    SA --> ST
    SA --> RT
    ST --> RT
    RT --> AG
    RT --> PL
    AG --> LP
    LP --> TY
    PL --> TY
    TOOLS --> TY
    TEAM --> AG
    TEAM --> RT
    INSP --> AG
    MCP --> AG
    KN --> TY
    MEM --> AG
    SR --> AG
    SB --> PL
    AJ --> PL
    TO --> PL
```

---

## Crate 地图

| Crate | 说明 | 文档 |
|-------|------|------|
| `llm-harness-types` | 核心 trait 与类型（Tool/Hook/Message/RunContext） | [crate-reference.md](docs/crate-reference.md) |
| `llm-harness-loop` | Agent loop 引擎：ReAct 循环、streaming、retry+backoff | crate 内 lib.rs |
| `llm-harness-agent` | HarnessBuilder、AgentHarness、Session、Compaction、Skills | crate 内 lib.rs |
| `llm-harness-subagents` | Sub-agent spawn、async delivery、main↔sub 通信（SpawnPlugin） | [runtime-framework.md](docs/runtime-framework.md) · [ORCHESTRATION.md](docs/ORCHESTRATION.md) |
| `llm-harness-workflow` | Workflow 引擎、task 持久化、规则匹配 | [runtime-framework.md](docs/runtime-framework.md) · [ORCHESTRATION.md](docs/ORCHESTRATION.md) |
| `llm-harness-agent-team` | 去中心化多 Agent 协作（AgentTeam、社交工具、团队记忆） | **[agent-team-guide.md](docs/agent-team-guide.md)** |
| `llm-harness-inspector` | Agent 调试 Web API（REST + WebSocket 事件流） | **[inspector-guide.md](docs/inspector-guide.md)** |
| `llm-harness-platform` | 横切 trait（Sandbox/AuditSink/TraceExporter/PromptSource） | [crate-reference.md](docs/crate-reference.md) |
| `llm-harness-strategy` | 策略层：safety、loop-safety、status-panel、event-stream、injection-filter、observation-shielding、memory-defense、notify、budget | **[strategy-guide.md](docs/strategy-guide.md)** |
| `llm-harness-tools` | 内置工具：bash、read、write、edit、grep、glob、code_exec、web_search、web_fetch | **[tools-guide.md](docs/tools-guide.md)** |
| `llm-harness-mcp` | MCP 协议集成：传输、会话、多服务器管理、版本锁定 | **[mcp-guide.md](docs/mcp-guide.md)** |
| `llm-harness-knowledge` | 后端无关的知识访问契约 | [crate-reference.md](docs/crate-reference.md) |
| `llm-harness-knowledge-local` | 本地 Markdown/Text 知识源（BM25） | [crate-reference.md](docs/crate-reference.md) |
| `llm-harness-memory` | 长期记忆系统（读写分离 + 变更网关） | [crate-reference.md](docs/crate-reference.md) |
| `llm-harness-session-recall` | 跨 Session 历史召回（in-memory + SQLite FTS5） | [session-recall-integration.md](docs/session-recall-integration.md) |
| `llm-harness-sandbox` | 沙箱后端：bwrap（Linux）、seatbelt（macOS）、os | crate 内 lib.rs |
| `llm-harness-sandbox-remote` | 远程沙箱协议客户端：HTTPS/Bearer、文件操作、流式 shell、abort/timeout | **[remote-sandbox-backend.md](docs/design/2026-09-15-remote-sandbox-backend.md)** |
| `llm-harness-sandbox-gateway` | Linux Bwrap 远程沙箱网关：认证、租户绑定、默认断网、生命周期与失败关闭策略 | **[remote-sandbox-backend.md](docs/design/2026-09-15-remote-sandbox-backend.md)** |
| `llm-harness-audit-jsonl` | JSONL 审计后端（SHA-256 hash chain） | [crate-reference.md](docs/crate-reference.md) |
| `llm-harness-trace-otel` | OpenTelemetry 追踪导出（OTLP → Jaeger/Tempo） | [crate-reference.md](docs/crate-reference.md) |
| `llm-harness-live-tests` | 真实 LLM 集成测试（27 examples + 97 tests） | 见下方快速上手 |
| `session-viewer` | Session 可视化工具 | 见下方 |

---

## 快速上手

`llm-harness-live-tests` 包含 27 个可运行的 example，覆盖全部架构层。这是最好的上手入口：

```bash
# 配置 API key
source .env

# 运行单个 example（以工具调用为例）
cargo run --features live-llm -p llm-harness-live-tests --example 02_tool_calling
```

### Example 索引

| # | Example | 覆盖能力 |
|---|---------|---------|
| 01 | `prompt_streaming` | 流式输出 |
| 02 | `tool_calling` | 工具调用 |
| 03 | `dynamic_config_multi_turn` | 多轮对话 + 动态配置 |
| 04 | `session_branch` | Session 分支 |
| 05 | `compaction` | 上下文压缩 |
| 06 | `skills_model_switch` | Skills + 模型切换 |
| 07 | `hooks` | Hook 机制 |
| 08 | `workflow` | Workflow 引擎 |
| 09 | `workflow_recovery` | 崩溃恢复 |
| 10 | `sandbox` | 沙箱隔离 |
| 11 | `spawn_subagent` | 子 Agent 派生 |
| 12 | `tracing_audit` | 追踪 + 审计 |
| 13 | `budget_pricing` | 预算 + 定价 |
| 14 | `rules_approval` | 规则审批 |
| 15 | `safety_injection` | 安全 + 注入防御 |
| 16 | `status_panel` | 状态栏 + TODO |
| 17 | `source_instruction` | 项目指令注入 |
| 18 | `loop_safety` | 循环安全（max_turns/repetition/failure_breaker/death_spiral/truncation） |
| 19 | `memory_defense_guard` | 记忆防线 |
| 20 | `notify` | 通知 |
| 21 | `context_aware_compact` | Context-aware 压缩 |
| 22 | `fs_tools` | 文件工具（bash/read/write/edit/grep/glob） |
| 23 | `infra_integration` | 基础设施集成 |
| 24 | `otlp_jaeger` | OTLP 追踪导出 |
| 25 | `inspector_interact` | Inspector Web UI 交互（steer/abort/usage） |
| 26 | `agent_team` | 多 Agent 团队协作（delegate/reflection/团队记忆） |
| 27 | `multimodal` | 多模态（图片/PDF，base64 与 URL） |

### 运行全部测试

```bash
source .env
cargo test --features live-llm -p llm-harness-live-tests -- --test-threads=1
```

---

## 核心能力

### Loop Engine
ReAct 循环（思考→调工具→看结果→再思考），支持 streaming、retry + 指数退避 + 随机抖动、中止/取消传播、steer/follow-up 消息队列。

### Agent Harness
`HarnessBuilder` fluent API → `AgentHarness`。Session 持久化（InMemory + Jsonl）、分支/fork/navigate、compaction（可自定义 prompt + 熔断器）、skills、prompt templates、model switching、AfterRun/OnAbort hooks、事件广播（`harness.subscribe()`）。

### Workflow Engine
多步骤工作流：分阶段执行、条件跳转、回流/返工、崩溃恢复、事件广播（`WorkflowEvent`，executor 可通过 `ExecutorCtx::event_tx` 发事件）。详见 [ORCHESTRATION.md](docs/ORCHESTRATION.md)。

### Sub-Agent Spawn
主 Agent 派生子 Agent：同步/异步 spawn、消息总线（MessageBus）双向通信、abort 传播。通过 `SpawnPlugin` 一行接入。详见 [runtime-framework.md](docs/runtime-framework.md)。

### Agent Team
去中心化多 Agent 协作：声明式组队（`IndividualSpec` → `AgentTeam`）、P2P 社交工具（delegate_task/send_message/broadcast/search_contacts/set_reminder）、SQLite 团队记忆 + 反思循环。详见 [agent-team-guide.md](docs/agent-team-guide.md)。

### Strategy Layer
16 个策略 Plugin：安全防护、循环安全（5 个 Hook）、状态栏+TODO、事件流（timer/heartbeat/filter/webhook/shell_monitor）、注入防御、来源标记、记忆防线、通知、工具输出保护、观察屏蔽、MCP 凭证保护、compaction prompt、项目指令注入、审计、预算控制。详见 [strategy-guide.md](docs/strategy-guide.md)。

### Tools
9 个内置工具：bash、read、write、edit、grep、glob、code_exec、web_search（9 个搜索 provider）、web_fetch。详见 [tools-guide.md](docs/tools-guide.md)。

### Inspector
Agent 调试 Web API：REST 配置/session/usage 端点 + WebSocket 实时事件流 + 内置 Web UI，支持 steer/abort 交互。详见 [inspector-guide.md](docs/inspector-guide.md)。

### Multimodal
图片与 PDF 文档：`ContentBlock::Document`、`ImageSource::Url`/`BinarySource::Url`，base64 与 URL 两种来源，工具结果可返回真实图片块（`DataBlock::Image`）。

### MCP
完整 MCP 协议实现：stdio/HTTP/SSE 传输、多服务器管理、工具适配、提示与资源发现、版本锁定、凭证作用域。详见 [mcp-guide.md](docs/mcp-guide.md)。

### Knowledge & Memory
后端无关的知识访问契约 + 本地 BM25 知识源 + 长期记忆系统（读写分离 + 变更网关）。详见 [crate-reference.md](docs/crate-reference.md)。

### Session Recall
跨 Session 历史召回（in-memory + SQLite FTS5）。详见 [session-recall-integration.md](docs/session-recall-integration.md)。

### Sandboxing
Linux（bubblewrap）、macOS（seatbelt）、跨平台（OS 原生 + 路径限制）。

### Observability
审计日志（JSONL + SHA-256 hash chain）+ OpenTelemetry 追踪（OTLP → Jaeger/Tempo）。

## 文档索引

| 文档 | 内容 |
|------|------|
| [MISSION.md](MISSION.md) | 使命与第一性原理 |
| [strategy-guide.md](docs/strategy-guide.md) | 策略层 16 个模块使用指南 |
| [mcp-guide.md](docs/mcp-guide.md) | MCP 集成指南 |
| [tools-guide.md](docs/tools-guide.md) | 内置工具使用指南 |
| [agent-team-guide.md](docs/agent-team-guide.md) | 多 Agent 团队协作指南 |
| [inspector-guide.md](docs/inspector-guide.md) | Agent Inspector 调试指南 |
| [crate-reference.md](docs/crate-reference.md) | 基础设施 crate 参考（types/platform/audit/trace/knowledge/memory） |
| [runtime-framework.md](docs/runtime-framework.md) | 编排层框架文档（subagents + workflow） |
| [ORCHESTRATION.md](docs/ORCHESTRATION.md) | WorkflowEngine 编排能力总览 |
| [product-roadmap.md](docs/product-roadmap.md) | 产品路线图 |
| [session-recall-integration.md](docs/session-recall-integration.md) | Session Recall 集成指南 |
| [docs/design/](docs/design/) | 设计规范文档（历史 spec） |
| [docs/archive/](docs/archive/) | 已归档的研究/分析文档 |

---

## 作为依赖消费

```toml
llm-harness-workflow = { git = "https://github.com/oh-my-harness/llm-harness-runtime", rev = "<commit-sha>" }
```

---

## Session Viewer

`session-viewer` 可视化 `JsonlSessionRepo` 持久化的 agent session。

```bash
cargo run -p session-viewer --bin session_viewer -- /path/to/sessions [--port PORT]
```

读取磁盘格式（`meta.json` + `entries.jsonl`），在浏览器渲染自包含 HTML 页面。支持 Session 树分支切换、消息渲染（含折叠的 thinking 和 tool-use 块）、token 用量统计、配置条目展示、图片 base64 内联。

---

## 版本管理

Workspace version 在根 `Cargo.toml` 的 `[workspace.package]`。发布新版本时 bump `Cargo.toml` 中的 version。

---

## Python SDK

Python SDK（`llm_harness_py`）已迁移到 [github.com/oh-my-harness/Senza](https://github.com/oh-my-harness/Senza)。Senza 通过 git 依赖引用本仓库的 crate。
