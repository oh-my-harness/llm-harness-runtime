# Agent Team Studio 设计文档

> 日期：2026-08-30
> 分支：`feat/coding-team`
> 前置：`docs/design/2026-08-29-coding-team-design.md`（coding-team v1）、
> `docs/design/2026-08-29-inspector-two-layer-positioning.md`（两层定位 ADR）
> 状态：已实施（见 docs/design/2026-08-30-agent-team-studio-plan.md 各任务提交）

## 1. 背景与目标

coding-team v1 把 10 角色硬编码在 `roles.rs`、面板对着写死的角色名工作。
操作员的真实需求是**通用的 agent team 生成器**：

- 团队 = 声明式配置（成员数量、个性、模型、工具包），不是代码；
- coding-team 降级为**第一个官方模板**；
- UI 对任意 team 配置工作（三列：成员列表 / 运行历史 / 配置）；
- 用户通过 studio 配置一切：persona、api key、base url、model、工具包。

**已确认的决策**：

| 决策点 | 结论 |
|--------|------|
| 配置形态 | B：模板（可分享蓝图）+ 实例（模板引用 + 覆写）；覆写 = 全字段替换 |
| 改造策略 | A：就地演化——`llm-harness-coding-team` 重构改名 `llm-harness-agent-studio` |
| 工具包粒度 | A：预设包名数组（`fs`/`web`/`memory`/`knowledge`/`mcp:<server>`），数据结构兼容将来单工具细选 |
| per-agent model/key/url 生效方式 | 重建该成员 harness（磁盘 session 保留，内存上下文重置）；不做平台层热切换 |
| 全局设置入口 | 右上角 ⚙ 模态框（原设置 tab 移除），全局默认 = 三档模型 + base_url + api key + 巡检间隔 |

## 2. 架构

```
crates/llm-harness-agent-studio/       ← 原 llm-harness-coding-team 就地改名（git mv）
  src/
    team_spec.rs        ← 新：TeamSpec/MemberSpec/Behavior 数据模型（serde）
    template_store.rs   ← 新：模板目录（内置 include_str! + 用户目录）+ 实例 CRUD + 升级检查
    engine.rs           ← 新：TeamSpec → AgentTeam 装配器（纯函数）
    team_manager.rs     ← 原 project.rs 泛化（create/start，走 assembler）
    pulse.rs / events.rs / settings.rs / config.rs / routes.rs / scout.rs ← 保留，去 coding 化
    issue.rs             ← 保留为引擎通用能力（Markdown 资源状态机，去 coding 文案，
                            由模板的 Handoff 行为声明消费方）
    roles.rs             → 删除（persona 数据化为模板）
  templates/coding-team/
    team.json           ← 10 角色声明 + scout Patrol 行为 + Handoff 行为
    prompts/*.md        ← 各角色 persona 正文
  static/studio.html    ← 面板泛化重写（三列工作区）
  src/bin/agent-studio.rs ← 原 coding-team bin
```

引擎只认识 `TeamSpec`，不含任何具体角色名；"coding"的所有痕迹都在模板数据里。
内置模板 `include_str!` 编入二进制保证零配置开箱；用户目录模板可覆盖/新增。

## 3. TeamSpec 数据模型

```rust
pub struct TeamSpec {
    pub id: String,                    // 实例 id
    pub template: Option<TemplateRef>, // None = 纯手工团队
    pub name: String,
    pub members: Vec<MemberSpec>,
}

pub struct TemplateRef {
    pub template_id: String,           // "coding-team"
    pub version: String,               // 升级检测用
}

pub struct MemberSpec {
    pub id: String,
    pub persona: String,               // system prompt 全文（覆写时也是全文，非 diff）
    pub role_label: String,            // UI 展示名
    pub model: ModelRef,
    pub toolkits: Vec<String>,         // ["fs", "web", "mcp:xxx"]
    pub behaviors: Vec<Behavior>,
    pub provider_override: Option<ProviderOverride>,
}

pub enum ModelRef {
    Tier(String),                      // "strong"/"main"/"cheap" → 全局 settings 解析
    Explicit(String),                  // 钉死的模型 id
}

pub enum Behavior {
    /// 定时向该成员自己投递任务文本（scout 泛化）
    Patrol { interval_secs: u64, task: String, issues_dir: Option<String> },
    /// 人工确认某资源后路由到另一成员（issue 确认 → planner 的泛化）
    Handoff { on: Trigger, to: String, message_template: String },
}

pub enum Trigger {
    ResourceConfirmed { resource: String }, // "issues"
}

pub struct ProviderOverride {          // None 字段 = 继承全局
    pub api_key: Option<String>,       // 落盘单独 0600 文件，GET 永不回显
    pub base_url: Option<String>,
    pub model: Option<String>,
}
```

**关键语义**：

1. **覆写 = 全字段替换**。实例 spec 里存在的 `MemberSpec` 完全接管该成员（升级不跟随）；不存在（或与模板原版逐字段相等）的成员跟随模板升级。不做字段级 diff 合并。
2. **`ModelRef::Tier` 复用全局三档**；用户改全局设置后 Tier 成员在下次注册/重建时生效，`Explicit` 不受影响。
3. **`behaviors` 是引擎解释的声明**，非 prompt 的一部分。先只实现 Patrol 与 Handoff 两种解释器（YAGNI）。
4. **toolkits 是包名数组**；`social` 为保留名（引擎自动注入社交工具，不可配置），未知包名装配失败并报出合法列表（fail fast）。
5. **provider_override** 持久化到实例目录独立文件，api_key 0600、API GET 只回 `api_key_set` 布尔。

## 4. 模板/实例存储与生命周期

### 磁盘布局（data root，默认 `~/.harness/agent-studio/`，自动迁移旧目录）

```
~/.harness/agent-studio/
├── settings.json / apikey          # 全局（继承自 v1）
├── templates/                      # 用户自定义/覆盖模板
│   └── <template-id>/ team.json + prompts/*.md
└── teams/
    └── <team-id>/
        ├── team.json               # 实例 spec（模板引用 + 成员全文）
        ├── overrides.json          # per-agent provider_override
        └── data/                   # memory/sessions/issues（隔离模型不变）
```

### 模板加载优先级

用户目录 `templates/<id>` > 内置 `include_str!` 同名 > 报错。官方 coding-team 模板
内置二进制；用户可 fork 到用户目录微调，也可完全自建。

### 模板 API

| 端点 | 语义 |
|------|------|
| `GET /api/studio/templates` | 列出内置+用户模板（member 概览） |
| `POST /api/studio/teams` | 从模板（或空白）创建实例：拷贝 members 为全文，记录 `{template_id, version}` |
| `GET/PUT /api/studio/teams/<id>/spec` | 读/编辑实例 spec |
| `POST /api/studio/teams/<id>/upgrade-check` | 对比模板版本：列"会更新的成员（未覆写）/保持不动的（已覆写）" |
| `POST /api/studio/teams/<id>/upgrade` | 只更新未覆写的成员 |

### 覆写判定

实例 member 与模板原版**序列化后逐字段相等** = 未覆写（跟随升级）；不等 = 用户改过（保持不动）。不引入 dirty 标记。

### 旧数据迁移

启动发现 `projects.jsonl` → 逐项目转为 `teams/<id>/team.json`（template 指向内置
coding-team，members 全文落盘，等价覆写态），完成删除旧文件。一次性、幂等、带 fixture 单测。
旧目录 `~/.harness/coding-team/` → `~/.harness/agent-studio/` 同批迁移。

## 4.x 引擎装配（TeamSpec → AgentTeam）

`engine.rs::assemble(spec, global_settings, env) -> AgentTeam`，纯函数；吸收现有
register 的组队逻辑（TeamConfig 隔离、OsEnv 绑定仓库、行为 arming）。

1. **toolkits → plugins**：`fs`→FsToolsPlugin、`web`→WebToolsPlugin、
   `memory`→MemoryPlugin、`knowledge`→KnowledgePlugin、`mcp:<name>`→MCP connector
   （读全局 MCP 配置）；`social` 保留名拒绝；未知名报错并列出合法包名。
2. **ModelRef → provider**：`Tier` 查全局三档；`Explicit` 直用。全局 key/url 构建
   默认 client；有 `provider_override` 的成员用 override 值单独构建 client（成员间不共享）。
3. **persona**：模板 prompts/*.md 在**模板加载时**展开进 member persona；运行时 TeamSpec
   恒为全文，不读文件。
4. **behaviors**：
   - `Patrol`：build 后投递任务文本 + set_reminder 自续约（现有 scout::arm 泛化，
     task 文本来自 Behavior 声明）；
   - `Handoff`：确认路由泛化——引擎按实例 spec 找 `Handoff{on: ResourceConfirmed{
     resource:"issues"}}` 决定"确认后 submit 给谁"；无声明则确认只改状态。
5. **ProjectManager → TeamManager**：API 更名（register→create/start），组队走 assembler；
   pulse/routes 的 `ROLE_IDS` 引用全部改为读 spec members（UI 通用化的数据基础）。

## 5. 面板：三列工作区与 UI 通用化

```
┌──────────────┬───────────────────────┬────────────────┐
│ 成员列表(左)  │ 运行输出/历史(中)       │ 配置(右)        │
│ 状态+任务摘要 │ session 流（WS+历史）   │ model/key/url/ │
│ 插话/停止按钮 │                        │ persona/工具包  │
│ + 新成员      │                        │ 保存→重建策略   │
└──────────────┴───────────────────────┴────────────────┘
```

- **左列**：pulse 卡片（现有），点击选中驱动中/右列；底部保留待处理消息/Issue 概况。
- **中列**：`GET /api/team/agent/session`（已实现）+ WS 增量。
- **右列**：`GET/PUT /api/team/agent/config`（session/config 端点已实现，PUT 见下）。
  字段：模型（三档下拉或显式 id）、连接（key/url，显示"继承全局/已覆写"）、persona
  多行编辑、工具包勾选、保存。
- **保存生效策略**：persona/tools 热应用（现有 setter）；model/key/url → 重建该成员
  （确认弹窗说明内存上下文重置）。
- **⚙ 设置按钮**：全局默认（三档/base_url/key/巡检间隔）模态框，设置 tab 移除。

### agent-team 平台新增：`AgentTeam::replace_member`

shutdown 旧 runner → 按新 spec 重建 harness → 原槽位替换，收件箱/通讯录关系保留。
约 40-60 行 + 单测。团队级变更（全局三档变化重建 Tier 成员）复用同一机制。
成员增删：spec 层只要求"成员可增删"；实现走 replace 机制还是全量重建，实现计划里定。

### 新增端点

| 端点 | 语义 |
|------|------|
| `GET/PUT /api/studio/teams/<id>/spec` | 实例 spec 读写 |
| `POST /api/team/agent/config`（PUT） | per-agent 覆写保存 + 生效策略执行 |
| 模板/升级端点（见 §4 表） | |

## 6. 测试总盘

- **模板**：加载优先级（用户>内置）、升级 check 逐字段判定、升级执行只动未覆写成员。
- **装配**：toolkits 映射与未知包报错、Tier/Explicit 解析、override client 隔离、
  Patrol/Handoff 装配、persona 展开。
- **迁移**：fixture 旧目录 → 新布局断言（projects.jsonl、旧目录名）。
- **回归**：现有 53 单测 + 5 集成随改名迁移，断言不丢。
- **agent-team**：`replace_member` 单测（状态/收件箱/通讯录保留）。
- **live 冒烟**：从模板创建 team → 改成员 model → 重建 → pulse 反映 → 面板三列交互。

## 7. 分阶段交付
1. **数据层**：team_spec + template_store + 内置模板 + 旧数据迁移（引擎仍跑旧路径，纯增量）
2. **装配层**：engine + TeamManager 泛化 + pulse/routes 切 spec 成员 + 删 roles.rs（issue.rs 保留为通用资源）
3. **agent-team 平台**：replace_member + 单测
4. **UI**：三列工作区 + per-agent 配置 + ⚙ 模态框 + 成员管理
5. **收尾**：crate/bin 改名、文档、全量回归

## 8. 非目标（YAGNI）

- 模板市场、导入导出、发现/评分
- 字段级 diff 三方合并（覆写=全字段替换）
- 单工具粒度勾选（数据模型兼容，UI 不做）
- provider 运行时热切换（重建策略替代）
- 多端并发编辑冲突处理（单用户本地，最后写入者胜）

## 9. 风险与缓解

| 风险 | 缓解 |
|------|------|
| 改名造成大 diff，回归面广 | 5 阶段独立验证；现有测试断言全保留只转路径 |
| replace_member 触碰 agent-team 核心 | 最小实现 + 专项单测；失败可回退到"删除重建整个团队" |
| 模板升级语义被误解 | upgrade-check 先行预览，不自动执行 |
| 旧数据迁移丢配置 | 迁移函数幂等 + fixture 单测 + 迁移前不删除原文件（标记 .migrated） |
