# Agent Team 指南

> `llm-harness-agent-team` 提供去中心化（P2P）的多 Agent 协作：成员之间直接委派任务、互发消息，无需中央调度器。人类操作员（operator）通过 `AgentTeam::submit` 派活、`AgentTeam::receive` 收结果。
>
> 与 `llm-harness-subagents` 的区别：subagents 是**主从模式**（main agent fork 子 agent、等待结果）；agent-team 是**对等模式**（每个成员是自驱动的 agent，可互相委派与通信，无主从关系）。

---

## 快速开始

```rust
use llm_harness_agent_team::{AgentTeam, IndividualSpec, TeamConfig};
use llm_harness_types::ExecutionEnv;
use std::sync::Arc;

let mut team = AgentTeam::new()
    .member(IndividualSpec::new("researcher", "信息检索与事实核查", "claude-sonnet-4"))
    .member(IndividualSpec::new("writer", "把研究结论写成报告", "claude-sonnet-4"));

let env: Arc<dyn ExecutionEnv> = /* ... */;
let providers = vec![("claude-sonnet-4".to_string(), client.clone())];
team.build(env, providers).await?;

// 操作员派活
team.submit("researcher", "调研 X 的现状，结论发给 writer")?;

// 收操作员可见的消息（Stream）
use futures::StreamExt;
let mut stream = std::pin::pin!(team.receive()?);
while let Some(msg) = stream.next().await {
    println!("[{}] {}", msg.from, msg.text);
}

// 结束
team.shutdown();
```

运行完整示例：`cargo run -p llm-harness-live-tests --example 26_agent_team`。

---

## 核心概念

### IndividualSpec（成员声明）

每个成员 = `HarnessBuilder` + 社交元数据：

| 方法 | 说明 |
|------|------|
| `new(id, expertise, model)` | 成员 id、专长描述（进入其 system prompt 与通讯录）、模型 |
| `configure(\|b\| b.system_prompt(...))` | 定制内部 `HarnessBuilder`（工具、hooks 等全部可用） |
| `reflection_interval(n)` | 每 n 次 run 触发一次反思（见下文反思循环） |
| `allow_subagents(true)` | 允许该成员派生自己的子 agent（默认关闭——spawn 会放大 LLM 流量与失控半径） |

### AgentTeam（组队与生命周期）

| 方法 | 说明 |
|------|------|
| `new()` / `with_config(config)` | 创建团队（可配 `TeamConfig`） |
| `member(spec)` | 追加成员（build 前可链式多次） |
| `submit(agent_id, task)` | 操作员向指定成员派任务 |
| `receive()` | 操作员收消息流（所有成员发来的 operator 消息） |
| `agent_status(agent_id)` | 查成员状态（Busy/Idle/不存在）——超时诊断用 |
| `shutdown()` | 停止所有成员 |

### TeamConfig

| 字段 | 默认 | 说明 |
|------|------|------|
| `max_contacts_per_agent` | 20 | 每个成员通讯录上限 |
| `broadcast_timeout_secs` | 60 | broadcast 默认等待（成员可按次覆盖） |
| `delegate_timeout_secs` | 300 | delegate 后续等待默认值 |
| `session_base_dir` | 系统临时目录 | 允许子 agent 的成员存放 JSONL session 的根目录 |
| `memory_root` | 系统临时目录 | 团队共享知识文档根目录（`build` 时自动创建）。默认跨团队共享——需要隔离的团队必须覆盖此值 |

### SocialContext（社交基础设施）

`build` 时创建，所有成员共享：

- **AgentRegistry**：成员注册表 + 状态（Busy/Idle）
- **InboxStore**：每个成员的收件箱（`InboxMessage`，含 `MessageType`：Task/Reply/Reminder/ReflectionTrigger 等）
- **ContactIndex/ContactStore**：通讯录（成员间自动互知 id + 专长）
- **TimeoutManager**：委派/广播/提醒的超时管理

---

## 社交工具（LLM 可见）

`SocialToolPlugin` 在 build 时自动注册到每个成员：

| 工具 | 作用 |
|------|------|
| `delegate_task` | 把任务委派给另一成员（可带超时），等对方完成后拿到回复 |
| `send_message` | 给另一成员（或 operator）发消息，不等待 |
| `broadcast` | 群发消息给多个成员 |
| `search_contacts` | 按专长/名字搜索通讯录，找到合适的委派对象 |
| `set_reminder` | 给自己设置未来提醒（配合 TimeoutManager 唤醒） |

协作模式完全由成员自主决定——没有中央路由。典型流：操作员 → `submit` 给 a1 → a1 `delegate_task` 给 a2 → a2 `send_message` 直接回复操作员。

---

## 团队记忆与反思循环

每个团队带一个 SQLite 团队记忆（`TeamSqliteMemorySource`/`TeamSqliteMemoryStore`，位于 `TeamConfig::memory_root`）：

1. **反思触发**：成员配置 `reflection_interval(n)` 后，每完成 n 次 run，runner 向其收件箱投递 `ReflectionTrigger`
2. **反思 run**：成员以自己的对话 transcript 为输入，把可复用的经验写成结构化条目（`memory_write`）进团队记忆
3. **去重**：反思前先 `knowledge_search` 团队记忆，已存在的经验不重复写入
4. **跨进程存活**：SQLite 落盘，团队重启后经验仍在

> 反思-only 的 run（仅由 ReflectionTrigger 触发）不计入下一次触发间隔——设 `reflection_interval(1)` 是安全的，不会自我递归。

---

## 与 subagents 组合

`allow_subagents(true)` 的成员可使用 `llm-harness-subagents` 的全部 spawn 工具；其子 agent 的 session 落在 `TeamConfig::session_base_dir` 下该成员的子目录中。

---

## Agent Team Studio：模板化团队工作台

`llm-harness-agent-team-studio`（由 coding-team 演化而来）把团队变成
**声明式配置 + 模板**：团队由 `TeamSpec`（成员 persona / model / 工具包 /
行为）描述，从模板一键创建实例；内置 `coding-team` 模板（brainstormer /
planner / coder / 5×reviewer / judge / debugger / scout）是第一个官方模板。

```bash
./scripts/studio.sh            # 构建 → 起服务 → 打开面板
# 面板：http://127.0.0.1:<port>/app/team.html（数据目录 ~/.harness/agent-team-studio）
```

面板三列工作区：左列成员卡片（状态/插话/停止）、中列选中成员 session
历史、右列 per-agent 配置（persona 热应用；model/api key/base url/工具包
改动重建该成员 harness）；右上角 ⚙ 是全局设置模态框。

关键 API：

- `GET /api/team/templates` — 模板列表
- `POST /api/team/projects {id, template_id, repo_path}` — 从模板创建团队
- `GET/PUT /api/team/agent/config` — per-agent 配置（PUT persona 热应用，
  model/key/url 重建）
- `POST /api/team/upgrade-check` — 升级检查（逐字段相等判定哪些成员会跟随
  模板升级、哪些是本地覆写）


设计：`docs/design/2026-08-30-agent-team-studio-design.md`（含 inspector 两层定位
ADR 摘要；coding-team v1 设计与实现计划已并入该文档，原文件删除）。

---

## 设计文档

- 当前状态与评审结论：`docs/design/2026-08-25-agent-team-current-state.md`
- 示例：`crates/llm-harness-live-tests/examples/26_agent_team.rs`
