# Coding Team 设计文档

> 日期：2026-08-29
> 分支：`feat/coding-team`
> 关联：`docs/design/2026-08-29-inspector-two-layer-positioning.md`（inspector 两层定位 ADR）
> 状态：设计已与操作员逐节确认

## 1. 背景与目标

agent-team（P2P 多 agent 协作 crate）与 bench 框架已就绪。在进入 bench 之前，
先构建一个**真正能帮人写代码的 coding team**：多角色 agent 团队，人在前端对话
派活，团队自主协作完成需求澄清 → 计划 → 编码 → 审查闭环。

成功标准：在真实项目仓库上，人只需通过前端对话与确认按钮，团队即可完成一次
完整的功能交付（spec → plan → code → review → PASS）。

## 2. 已确认的关键决策

| 决策点 | 结论 |
|--------|------|
| 交互模型 | 对话式（前端聊天面板派活，随时插话） |
| Issue 存储 | Markdown 文件（非 SQLite、非 GitHub），路径 `~/.harness/coding-team/projects/<project-id>/issues/` |
| 执行能力 | 成员 = 完整 harness agent，`llm-harness-tools` 工具 cwd 绑定项目仓库；无沙箱（后续可 opt-in） |
| 多项目 | 一个项目 = 一个 `AgentTeam`；团队间不共享任何状态（memory_root / issue / 工具 cwd 全隔离） |
| 角色常驻 | 全部常驻（idle 无 LLM 费用），reviewer 常驻以积累项目经验 |
| 运行形态 | 独立 bin 守护进程，挂 inspector Web UI |
| inspector 定位 | 平台层；coding-team 通过 `extra_routes` / `extra_static` 挂载（见 ADR） |
| 仓库归属 | coding-team crate 先在本仓库实现，成熟后拆独立仓库 |

## 3. 架构

```
┌─ llm-harness-coding-team（应用层）─────────────┐
│  roles / project / issue / scout / routes      │
│  bin/coding-team + static/team.html            │
└──────────────┬─────────────────────────────────┘
               │ extra_routes / extra_static 挂载
┌──────────────▼─────────────────────────────────┐
│  llm-harness-inspector（平台层）                │
│  harness registry / events WS / session /      │
│  config / usage / inject / abort / prompt /    │
│  static UI 托管 / token 鉴权                    │
├────────────────────────────────────────────────┤
│  llm-harness-agent-team（P2P 协作）             │
│  SocialContext / delegate / broadcast /        │
│  set_reminder / 团队记忆                        │
└────────────────────────────────────────────────┘
```

### 3.1 项目与团队生命周期

```rust
pub struct Project {
    pub id: String,          // 如 "llm-harness-runtime"
    pub repo_path: PathBuf,  // git 仓库根目录（注册时校验 .git 存在）
    pub team: AgentTeam,     // 一项目一团队
    pub issues: IssueStore,  // Markdown 文件，见 §5
}
```

- `ProjectManager::register(name, repo_path)`：校验仓库 → 建 IssueStore →
  组建 10 角色团队 → `team.build(env, providers)`。所有成员工具的 `cwd`
  绑定 `repo_path`。
- 每团队 `TeamConfig.memory_root` 必须覆盖为项目专属路径（默认值跨团队共享，
  与隔离要求冲突）。
- git：成员经 bash 工具直接用 git；统一在 `team/<project-id>` 工作分支干活，
  不碰 main；push 由人确认。
- bin 重启：项目列表从 `~/.harness/coding-team.toml` 恢复；团队记忆与 issue
  持久化；agent 会话不恢复（经验经团队 SQLite 记忆继承）。

## 4. 角色与协作流程

### 4.1 角色（10 个，全常驻）

| 角色 | 职责 | 模型档位 |
|------|------|----------|
| brainstormer | 设计类请求的需求澄清：与人多轮对话 → 分节设计 → spec 写入项目仓库 `docs/design/`，人确认后交 planner。借鉴 brainstorming skill（spike/bounded/architectural 分类、一次一问、多方案对比）。小修小补不介入 | 强 |
| planner | 按 spec 写实现计划（writing-plans 方法论：bite-sized TDD 任务、Files/Interfaces 精确到签名、spec 覆盖自检）；计划存 `docs/superpowers/plans/YYYY-MM-DD-<feature>.md`；逐任务 `delegate_task` 给 coder（自包含任务描述） | 强 |
| coder | 默认 1 个（`coders` 配置项可加多个），严格按任务步骤 TDD 执行（失败测试→跑→最小实现→跑→commit），无占位符交付 | 主力 |
| reviewer ×5 | performance / robustness / standards / magical-prompt / spec-compliance 五维度审查；P0/P1/P2 分级；只读 | 便宜 |
| judge | 收齐 reviewer findings → 对抗验证 → 独立裁决（keep/drop/降级）→ PASS 知会 operator / NEEDS_CHANGES 退回 coder | 强 |
| debugger | 测试红/运行异常时定位根因 | 主力 |
| scout | 定时巡检仓库（§5） | 便宜 |

### 4.2 请求流转

```
用户（前端对话）
  ├─ 设计类 ──→ brainstormer（澄清→spec→人确认）→ planner（plan→delegate）
  ├─ 小修小补 ─→ planner（直接拆任务）
  ↓
coder（TDD 执行）→ planner 召集 reviewer×5 并行审查 → judge 裁决
  ├─ NEEDS_CHANGES → 问题 delegate 回 coder → 重审
  └─ PASS → 知会 operator
debugger：由 planner/coder 在异常路径委派
scout：独立巡检 → issue → 人确认 → planner
```

review 裁决规则沿用 workflow-code-review：P0/P1 keep → NEEDS_CHANGES；
无 keep 的 P0/P1 → PASS；P2 不阻塞。

## 5. Scout 与 Issue 流转

- **触发**：`set_reminder` 自续约——装配层在 build 后投递首个巡检任务，scout
  完成后自己预约下一次。间隔 `scout_interval_secs`（TeamConfig 新字段，默认
  3600，0 关闭）。
- **巡检内容**：跑测试、TODO/FIXME 扫描、最近 commit 可疑改动、warning 积累。
  产出候选 issue，不改代码。
- **Issue 文件**：一 issue 一 Markdown，frontmatter 存 `id / status / source /
  created / title`；文件名 `I-NNN-<slug>.md`。
- **状态机**：`pending → confirmed → done`；`pending → rejected`。**只有人能
  转换**（前端按钮或手改 frontmatter）；planner 只认 `confirmed`。
- **确认衔接**：确认时 bin 层改 status + `team.submit("planner", issue 全文)`，
  复用 operator 通道，不加消息类型。
- **去重与上限**：scout 写 issue 前 grep 查重；单项目 `pending` > 50 时暂停
  提报，改为提醒操作员清理。

## 6. 前端

### 6.1 inspector 平台层扩展点（唯一改动）

- `InspectorBuilder::extra_routes(Router)`：领域路由合并进 server，同 token 鉴权。
- `InspectorBuilder::extra_static(前缀, 目录)`：上层静态页托管。
- 现有 `/` 观测 UI 不动；`/` 页面加导航链接到 `/app/team.html`。

### 6.2 操作面板 `/app/team.html`（coding-team static，单页三区）

1. **项目侧栏**：项目列表（含各团队 agent 状态汇总）；"注册项目"表单。
2. **对话区**：聊天流（WS 实时）+ 输入框 + 模式开关（**Chat / Design**）。
   `POST /api/team/chat` 携带用户显式选择的目标：Chat → planner（直接拆任务），
   Design → brainstormer（需求澄清）。后端不做内容猜测路由。
3. **Issue 区**（tab）：按 status 分组；`pending` 带确认/驳回按钮。

### 6.3 领域 API（全部走 extra_routes）

| 端点 | 作用 |
|------|------|
| `GET /api/team/projects` | 项目列表 + agent 状态汇总 |
| `POST /api/team/chat` | 对话输入；body 含 `target: "planner"|"brainstormer"`（前端模式开关显式选择） |
| `GET /api/team/issues?project=` | issue 列表 |
| `POST /api/team/issue/:id/confirm\|reject` | 人确认/驳回 |
| `WS /api/team/events` | 项目级事件流（对话/issue/任务状态） |

观测 UI（`/`）与操作面板通过 agent id 关联：对话中的成员消息可点进观测端看
完整 session 回放。

## 7. Crate 布局

```
crates/llm-harness-coding-team/
  Cargo.toml              # deps: agent-team, agent, types, inspector, tools, tokio, axum...
  src/lib.rs
  src/roles.rs            # 10 角色 IndividualSpec 工厂 + system prompt
  src/project.rs          # Project / ProjectManager
  src/issue.rs            # IssueStore：Markdown 读写 + 状态机
  src/routes.rs           # 领域 axum 路由
  src/scout.rs            # 巡检模板 + 自续约 + 上限检查
  src/bin/coding-team.rs  # 薄 bin：读配置→注册项目→挂 inspector→hold
  static/team.html
  tests/integration.rs
```

配置 `~/.harness/coding-team.toml`：项目列表、模型映射、scout 间隔。

## 8. 测试策略

- **单元**（MockLlmClient，全离线）：
  - issue：创建/读取/状态机（非法转换拒绝）。
  - roles：system prompt 含关键指令断言。
  - project：注册校验（repo 不存在拒绝）、memory_root 隔离。
  - routes：确认 issue 后 planner 收到 submit。
- **集成**（MockLlmClient 驱动真实 AgentTeam）：
  - 全链路 chat → plan → delegate → review → judge → PASS。
  - scout 链路：巡检 → issue → 人确认 → planner 收到。
- 不含真实 LLM 冒烟（live-tests 惯例，手动跑）。

## 9. 非目标（YAGNI）

- 沙箱隔离（后续 opt-in）
- writer / ops 角色（第二阶段按痛点加）
- GitHub issue 同步
- 看板视图（对话派活跑顺后再说）
- 多机部署、鉴权体系升级（沿用 inspector 本地 token）
