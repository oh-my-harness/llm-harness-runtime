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

### GLM-5.3-Flash live E2E

`crates/llm-harness-live-tests/tests/agent_team.rs` 提供默认 ignored 的
真实模型 E2E，用于验证完整三跳协作链：

```text
operator → planner(delegate_task) → coder(delegate_task)
         → reviewer(send_message) → operator
```

运行命令：

```bash
OPENAI_API_KEY=<key> \
OPENAI_API_BASE=https://api.hyper-op.com \
OPENAI_MODEL=GLM-5.3-Flash \
cargo test -p llm-harness-live-tests --features live-llm --test agent_team \
  glm_5_3_flash_agent_team_delegates_coding_review_to_operator -- --ignored --nocapture
```

测试不仅等待 reviewer 的 operator 回复，还会读取三个成员的 session
transcript，并断言 `planner → coder`、`coder → reviewer`、
`reviewer → operator` 各发生一次正确的目标工具调用。这样可以防止测试把
“planner 直接联系 reviewer”或“成员只输出文本”误判为三跳闭环。

Hyperop 网关使用内网 CA 时，rustls 默认 WebPKI root 无法完成 TLS 校验；
workspace 的 `reqwest` 依赖启用 `rustls-tls-native-roots`，使 provider
请求可同时加载系统 native roots。reflection 持久化不在该 E2E 的通过条件内，
`26_agent_team` 中的合法弃权仍不能被宣传为 reflection 已持久化验证。

---

## 核心概念

### IndividualSpec（成员声明）

每个成员 = `HarnessBuilder` + 社交元数据：

| 方法 | 说明 |
|------|------|
| `new(id, expertise, model)` | 成员 id、专长描述（进入其 system prompt 与通讯录）、模型 |
| `with_persona(persona)` | 设置成员性格/协作风格；在没有显式 prompt 时自动生成包含 id、专长和 persona 的成员专属 system prompt |
| `with_prompt(prompt)` | 设置该成员的完整 system prompt；显式完整 prompt 优先于 `configure(system_prompt)` 和 persona 生成 prompt |
| `configure(\|b\| b.system_prompt(...))` | 定制内部 `HarnessBuilder`（工具、hooks 等全部可用） |
| `prompt()` | 读取当前生效 prompt；优先级为 `with_prompt` > `configure(system_prompt)` > persona 生成 prompt |
| `reflection_interval(n)` | 每 n 次 run 触发一次反思（见下文反思循环） |
| `allow_subagents(true)` | 允许该成员派生自己的子 agent（默认关闭——spawn 会放大 LLM 流量与失控半径） |

```rust
let researcher = IndividualSpec::new("researcher", "信息检索与事实核查", "claude-sonnet-4")
    .with_persona("严谨、保守、只基于证据下结论");
let writer = IndividualSpec::new("writer", "把研究结论写成报告", "claude-sonnet-4")
    .with_persona("直接、面向决策者、避免术语堆砌");
```

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
行为）描述，从模板一键创建实例；内置 `coding-team` 模板（orchestrator /
coder / reviewer，即 V0 冻结三成员设计：Orchestrator 独占下一步决策，
reviewer 只产出 findings，无 judge 裁决层）是第一个官方模板。

```bash
./scripts/studio.sh            # 构建 → headless 启动
```

`llm-harness-app` 是 runtime headless 服务入口。它只提供 AgentTeam HTTP/WS
API、生命周期和诊断接口；产品 UI 由 Senza Studio 承载。

服务只绑定 loopback，并对所有 `/api/*` 请求启用 per-process bearer token。
`StudioRuntime` 就绪后会把 runtime service URL 写入 data root 的 `panel.json`（Unix 权限
`0600`），并采用同目录临时文件 + rename 原子替换。桌面宿主和 launcher 只接收
descriptor 路径；后端不会把含 token 的 URL 打进 stdout。干净退出或 runtime drop
时会删除该文件。
Studio 数据根目录使用进程级独占锁，避免同一份团队数据被多个桌面实例并发修改。
全局/成员级 API key 和 workspace grant 持久化采用“创建即私有 + 唯一临时文件原子
替换”，失败路径不会留下可读取的临时秘密；成员 ID 会在构造秘密路径前再次校验。
Studio 主进程会在退出前把 panic 事件追加到数据根目录的
`logs/crash.jsonl`（Unix 权限 `0600`）；该报告包含时间、线程、panic 消息和
backtrace，便于桌面宿主或运维在进程退出后定位原因。

启动时会恢复 `teams/*/team.json` 中持久化的团队。任一团队无法恢复时服务仍会启动，
但面板会显示降级横幅；后端通过 `GET /api/team/startup` 暴露恢复结果，包含
`persisted_teams`、`restored_teams` 和逐项 `failed_teams`（`invalid_spec`、
`workspace_unauthorized`、`workspace_check`、`team_create`）。这样可以先处理已恢复
的团队，同时定位未恢复团队的数据或授权问题，而不是把失败静默吞掉。

面板三列工作区：左列成员卡片（状态/插话/停止/动态增删）、中列共享聊天与
思考过程流、右列 per-agent 配置（persona 热应用；model/api key/base url/
工具包改动重建该成员 harness）；右上角 ⚙ 是全局设置模态框。

关键 API：

`/api/agent-team/*` 是 runtime 应用中的 feature API 前缀；`/api/team/*`
保留为兼容别名，两者绑定同一组 handler，便于现有面板和外部消费者平滑迁移。

- `GET /api/team/templates` — 模板列表
- `POST /api/team/projects {id, template_id, repo_path}` — 从模板创建团队
- `GET/POST/DELETE /api/team/members` — 声明式成员列表、动态新增/删除；
  新成员会与既有成员建立双向 contact，删除会停止 runner 并更新 `team.json`
- `POST /api/team/chat {project, target, text}` — 操作员发给指定成员；
  `target="team"` 会广播给全部成员
- `GET/PUT /api/team/agent/config` — per-agent 配置（PUT persona 热应用，
  model/key/url 重建）
- `POST /api/team/upgrade-check` — 升级检查（逐字段相等判定哪些成员会跟随
  模板升级、哪些是本地覆写）
- `GET /api/team/startup` — 启动恢复报告

`/api/team/events` WebSocket 会回放并直播 `team_message` 与
`agent_thought`。`team_message` 覆盖 operator→member、operator→team、
member→member 的落库投递；团队广播对同一 `broadcast_id` 去重后显示一次。
`agent_thought` 只用于操作员可见的过程流，不注入其它成员的模型上下文。


设计：`docs/design/2026-08-30-agent-team-studio-design.md`（含 inspector 两层定位
ADR 摘要；coding-team v1 设计与实现计划已并入该文档，原文件删除）。

---

## 设计文档

- 当前状态与评审结论：`docs/design/2026-08-25-agent-team-current-state.md`
- 示例：`crates/llm-harness-live-tests/examples/26_agent_team.rs`
