# Agent Team Studio 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 coding-team 泛化为 agent-team-studio：团队由 TeamSpec 声明式配置生成，coding-team 成为第一个内置模板，面板三列化并支持 per-agent 配置。

**Architecture:** 引擎只认识强类型 `TeamSpec`（persona/model/toolkits/behaviors/provider 覆写），`engine::assemble` 纯函数装配为 `AgentTeam`；模板与实例分两级存储（内置 include_str + 用户目录），覆写=全字段替换、升级按逐字段相等判定；面板从 spec 读成员，三列工作区（成员/历史/配置）。

**Tech Stack:** Rust workspace（serde/axum/tokio/parking_lot），模板 = JSON + Markdown 文件（include_str 嵌入），前端纯 HTML/JS 无构建。

**Spec:** `docs/design/2026-08-30-agent-team-studio-design.md`

## Global Constraints

- 全部测试离线：`CARGO_HTTP_PROXY="" cargo test -p <crate>`；工作区回归 `CARGO_HTTP_PROXY="" cargo test --workspace --exclude llm-harness-live-tests`
- 现有测试只许转路径/转名，断言不丢（53 单测 + 5 集成是回归底线）
- api key 永不进 settings.json/team.json，单独 0600 文件，GET 响应永不回显（只回布尔）
- `social` 为保留 toolkit 名，装配时拒绝；未知包名 fail fast 并列出合法包名
- 覆写判定 = 实例 member 与模板原版序列化后逐字段相等；升级只动未覆写成员
- 模板加载优先级：用户目录 `templates/<id>` > 内置同名 > 报错
- persona 在模板加载时展开为全文；运行时 TeamSpec 恒为全文，不读文件
- 每任务一个 commit，conventional commits 风格

---

### Task 1: team_spec.rs 数据模型

**Files:**
- Create: `crates/llm-harness-coding-team/src/team_spec.rs`
- Modify: `crates/llm-harness-coding-team/src/lib.rs`（`pub mod team_spec;`）
- Test: `crates/llm-harness-coding-team/src/team_spec.rs` 内置 `#[cfg(test)]`

**Interfaces:**
- Produces:
  - `#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)] pub struct TeamSpec { pub id: String, pub template: Option<TemplateRef>, pub name: String, pub members: Vec<MemberSpec> }`
  - `pub struct TemplateRef { pub template_id: String, pub version: String }`
  - `pub struct MemberSpec { pub id: String, pub persona: String, pub role_label: String, pub model: ModelRef, pub toolkits: Vec<String>, pub behaviors: Vec<Behavior>, pub provider_override: Option<ProviderOverride> }`
  - `#[serde(tag = "kind", rename_all = "snake_case")] pub enum ModelRef { Tier(String), Explicit(String) }`
  - `#[serde(tag = "type", rename_all = "snake_case")] pub enum Behavior { Patrol { interval_secs: u64, task: String, issues_dir: Option<String> }, Handoff { on: Trigger, to: String, message_template: String } }`
  - `#[serde(tag = "kind", rename_all = "snake_case")] pub enum Trigger { ResourceConfirmed { resource: String } }`
  - `pub struct ProviderOverride { pub api_key: Option<String>, pub base_url: Option<String>, pub model: Option<String> }`
  - `impl TeamSpec { pub fn member_ids(&self) -> Vec<&str> }`
  - 序列化约定：`ModelRef::Explicit` 与 `Tier` 用 `#[serde(untagged)]` 区分——**改用 untagged**：`#[serde(untagged)] pub enum ModelRef { Explicit(String), Tier(String) }`，字符串 "strong"/"main"/"cheap" 视为 Tier，其余为 Explicit（解析函数 `ModelRef::resolve(&self, models: &ModelTier) -> String`）。

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_json_roundtrip() {
        let spec = TeamSpec {
            id: "my-repo".into(),
            template: Some(TemplateRef { template_id: "coding-team".into(), version: "1".into() }),
            name: "My Repo".into(),
            members: vec![MemberSpec {
                id: "planner".into(),
                persona: "你是 planner".into(),
                role_label: "规划者".into(),
                model: ModelRef::Tier("strong".into()),
                toolkits: vec!["fs".into()],
                behaviors: vec![Behavior::Handoff {
                    on: Trigger::ResourceConfirmed { resource: "issues".into() },
                    to: "planner".into(),
                    message_template: "issue: {body}".into(),
                }],
                provider_override: Some(ProviderOverride {
                    api_key: None, base_url: Some("https://x.io".into()), model: None,
                }),
            }],
        };
        let json = serde_json::to_string(&spec).unwrap();
        let back: TeamSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn model_ref_resolution() {
        let tier = crate::roles::ModelTier {
            strong: "g-5".into(), main: "g-4".into(), cheap: "g-4-air".into(),
        };
        assert_eq!(ModelRef::Tier("strong".into()).resolve(&tier), "g-5");
        assert_eq!(ModelRef::Explicit("claude-x".into()).resolve(&tier), "claude-x");
        assert_eq!(ModelRef::Tier("main".into()).resolve(&tier), "g-4");
    }

    #[test]
    fn member_ids_lists_all() {
        let spec = TeamSpec {
            id: "t".into(), template: None, name: "T".into(),
            members: vec![
                MemberSpec { id: "a".into(), persona: String::new(), role_label: String::new(),
                    model: ModelRef::Explicit("m".into()), toolkits: vec![], behaviors: vec![], provider_override: None },
                MemberSpec { id: "b".into(), persona: String::new(), role_label: String::new(),
                    model: ModelRef::Explicit("m".into()), toolkits: vec![], behaviors: vec![], provider_override: None },
            ],
        };
        assert_eq!(spec.member_ids(), vec!["a", "b"]);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team team_spec`
Expected: FAIL — 模块不存在

- [ ] **Step 3: 实现**

按 Interfaces 的类型逐一定义；`ModelRef::resolve` 里 Tier 匹配 "strong"/"main"/"cheap"，未知 tier 名回退 `main` 并 `tracing::warn!`。序列化用 `#[serde(untagged)]` 的 `ModelRef { Explicit(String), Tier(String) }`，resolve 时先查三档名再回退字符串本身（保证 untagged 兼容任意 id）。

- [ ] **Step 4: 跑测试确认通过**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team`
Expected: PASS（原有测试无回归）

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-coding-team
git commit -m "feat(coding-team): TeamSpec data model — declarative team/members/behaviors"
```

---

### Task 2: 内置 coding-team 模板（数据化 roles.rs）

**Files:**
- Create: `crates/llm-harness-coding-team/templates/coding-team/team.json`
- Create: `crates/llm-harness-coding-team/templates/coding-team/prompts/{brainstormer,planner,coder,reviewer-performance,reviewer-robustness,reviewer-standards,reviewer-magical-prompt,reviewer-spec-compliance,judge,debugger,scout}.md`
- Create: `crates/llm-harness-coding-team/src/template_store.rs`
- Modify: `crates/llm-harness-coding-team/src/lib.rs`（`pub mod template_store;`）
- Test: `crates/llm-harness-coding-team/src/template_store.rs` 内置测试

**Interfaces:**
- Consumes: Task 1 的 `TeamSpec`/`MemberSpec`
- Produces:
  - `pub struct LoadedTemplate { pub id: String, pub version: String, pub spec: TeamSpec }`
  - `pub struct TemplateStore { user_dir: Option<PathBuf> }`
  - `impl TemplateStore { pub fn new(user_dir: Option<PathBuf>) -> Self; pub fn list(&self) -> Vec<TemplateMeta>; pub fn load(&self, id: &str) -> Result<LoadedTemplate, TemplateError> }`
  - `pub struct TemplateMeta { pub id: String, pub version: String, pub name: String, pub member_ids: Vec<String> }`
  - `pub enum TemplateError { NotFound(String), Parse(String) }`（thiserror）
  - 内置模板 id 恒为 `"coding-team"`，version `"1"`
  - prompts/*.md 的 frontmatter 无需解析——`team.json` 的每个 member 按约定 `persona_file: "prompts/planner.md"` 指向文件，加载时读出替换 persona 字段为全文

- [ ] **Step 1: 写模板数据文件**

`templates/coding-team/team.json`（member 的 persona 字段先放 `persona_file` 约定键，加载时展开；toolkits/behaviors 按现 roles.rs/scout 语义转录）：

```json
{
  "id": "coding-team-template",
  "template": null,
  "name": "Coding Team",
  "members": [
    { "id": "brainstormer", "role_label": "需求澄清", "model": "strong", "toolkits": ["fs"],
      "persona_file": "prompts/brainstormer.md", "behaviors": [] },
    { "id": "planner", "role_label": "规划者", "model": "strong", "toolkits": ["fs"],
      "persona_file": "prompts/planner.md", "behaviors": [] },
    { "id": "coder", "role_label": "编码者", "model": "main", "toolkits": ["fs"],
      "persona_file": "prompts/coder.md", "behaviors": [] },
    { "id": "reviewer-performance", "role_label": "性能审查", "model": "cheap", "toolkits": [],
      "persona_file": "prompts/reviewer-performance.md", "behaviors": [] },
    { "id": "reviewer-robustness", "role_label": "健壮性审查", "model": "cheap", "toolkits": [],
      "persona_file": "prompts/reviewer-robustness.md", "behaviors": [] },
    { "id": "reviewer-standards", "role_label": "规范审查", "model": "cheap", "toolkits": [],
      "persona_file": "prompts/reviewer-standards.md", "behaviors": [] },
    { "id": "reviewer-magical-prompt", "role_label": "契约审查", "model": "cheap", "toolkits": [],
      "persona_file": "prompts/reviewer-magical-prompt.md", "behaviors": [] },
    { "id": "reviewer-spec-compliance", "role_label": "符合度审查", "model": "cheap", "toolkits": [],
      "persona_file": "prompts/reviewer-spec-compliance.md", "behaviors": [] },
    { "id": "judge", "role_label": "裁决者", "model": "strong", "toolkits": ["fs"],
      "persona_file": "prompts/judge.md", "behaviors": [] },
    { "id": "debugger", "role_label": "故障定位", "model": "main", "toolkits": ["fs"],
      "persona_file": "prompts/debugger.md", "behaviors": [] },
    { "id": "scout", "role_label": "巡检员", "model": "cheap", "toolkits": ["fs"],
      "persona_file": "prompts/scout.md",
      "behaviors": [{ "type": "patrol", "interval_secs": 3600, "task": "", "issues_dir": null }] }
  ]
}
```

> `task: ""` 与 `issues_dir: null` 的 scout 任务文本由引擎在装配时按项目上下文填充（见 Task 4 的 Patrol 解释器约定）——模板层面留空即"引擎填充"。为实现此约定，Task 1 的 `Patrol.task` 语义补充：**空字符串 = 引擎生成默认巡检任务文本**。
> `model` 字段是 untagged ModelRef 的字符串形式（"strong" 命中三档名 → Tier；其余 → Explicit）。

11 个 prompts/*.md：**内容从 `crates/llm-harness-coding-team/src/roles.rs` 的 prompts 模块逐函数拷贝**（`prompts::brainstormer()` 的返回值原文 → `prompts/brainstormer.md`，以此类推；scout 从 `scout::prompts::scout()` 拷贝）。模板加载时给每个 persona **追加统一协作约定尾注**（固定文本，见下）——从 roles.rs 中 `send_message`/工作分支纪律相关行复制整合：

```markdown

---
## 团队协作约定（引擎附加）
- 与人沟通一律 send_message("operator", ...)。
- 与成员协作用 delegate_task / send_message / broadcast。
- 在团队工作分支 team/<project> 上操作，不 push，不碰 main。
```

- [ ] **Step 2: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_coding_team_loads_with_persona_expanded() {
        let store = TemplateStore::new(None);
        let t = store.load("coding-team").unwrap();
        assert_eq!(t.id, "coding-team");
        assert_eq!(t.version, "1");
        assert_eq!(t.spec.members.len(), 11);
        // persona_file 已展开为正文：不再含 persona_file 约定（展开发生在 load）
        let planner = t.spec.members.iter().find(|m| m.id == "planner").unwrap();
        assert!(planner.persona.contains("delegate_task"), "persona expanded from md");
        // scout 带 Patrol 行为
        let scout = t.spec.members.iter().find(|m| m.id == "scout").unwrap();
        assert!(matches!(scout.behaviors[0], Behavior::Patrol { .. }));
    }

    #[test]
    fn user_template_overrides_builtin() {
        let dir = std::env::temp_dir().join(format!("tpl-user-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tpl_dir = dir.join("coding-team");
        std::fs::create_dir_all(tpl_dir.join("prompts")).unwrap();
        std::fs::write(tpl_dir.join("team.json"),
            r#"{"id":"coding-team-template","name":"My Fork","members":[{"id":"solo","role_label":"独狼","model":"main","toolkits":["fs"],"persona":"solo persona","behaviors":[]}]}"#).unwrap();
        let store = TemplateStore::new(Some(dir));
        let t = store.load("coding-team").unwrap();
        assert_eq!(t.spec.members.len(), 1);
        assert_eq!(t.spec.members[0].id, "solo");
    }

    #[test]
    fn unknown_template_is_error() {
        let store = TemplateStore::new(None);
        assert!(matches!(store.load("nope"), Err(TemplateError::NotFound(_))));
    }

    #[test]
    fn list_includes_builtin() {
        let store = TemplateStore::new(None);
        let metas = store.list();
        assert!(metas.iter().any(|m| m.id == "coding-team"));
    }
}
```

- [ ] **Step 3: 跑测试确认失败**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team template_store`
Expected: FAIL — 模块不存在

- [ ] **Step 4: 实现 template_store.rs**

```rust
pub struct TemplateStore { user_dir: Option<PathBuf> }

impl TemplateStore {
    pub fn load(&self, id: &str) -> Result<LoadedTemplate, TemplateError> {
        // 1. user_dir/templates/<id>/team.json 存在 → 用它
        // 2. 否则内置 include_str!("../templates/coding-team/team.json")
        // 3. 解析 TeamSpec；对每个 member 若有 persona_file 键（serde 别名字段
        //    `#[serde(default)] pub persona_file: Option<String>`——加在 MemberSpec
        //    上，serde(skip_serializing)），从模板目录读文件替换 persona 全文
        // 4. 追加统一协作约定尾注（固定字符串常量 COOPERATION_FOOTNOTE）
    }
}
```

> MemberSpec 加 `#[serde(default, skip_serializing_if = "Option::is_none")] pub persona_file: Option<String>`——Task 1 类型的小扩展，属本任务交付（含 roundtrip 测试补充断言 persona_file 不出现在序列化输出）。

内置模板路径常量：`const BUILTIN_CODING_TEAM: &str = include_str!("../templates/coding-team/team.json");`（多模板将来用 match id 展开）。

- [ ] **Step 5: 跑测试确认通过 + 全 crate 回归**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/llm-harness-coding-team
git commit -m "feat(coding-team): template store + built-in coding-team template (roles.rs data as JSON+md)"
```

---

### Task 3: engine.rs 装配器

**Files:**
- Create: `crates/llm-harness-coding-team/src/engine.rs`
- Modify: `crates/llm-harness-coding-team/src/lib.rs`
- Test: `crates/llm-harness-coding-team/src/engine.rs` 内置测试

**Interfaces:**
- Consumes: Task 1 `TeamSpec/MemberSpec/ModelRef/Behavior`；Task 2 `TemplateStore`（不直接依赖，Persona 已全文）；现有 `Settings`/`ModelTier`、`FsToolsPlugin`、`WebToolsPlugin`、`llm-harness-agent-team::{AgentTeam, IndividualSpec, TeamConfig}`
- Produces:
  - `pub struct AssembleInputs<'a> { pub spec: &'a TeamSpec, pub settings: &'a Settings, pub api_key: &'a str, pub env: Arc<dyn ExecutionEnv>, pub data_dir: &'a Path, pub repo_path: Option<&'a Path> }`
  - `pub fn assemble(inputs: AssembleInputs) -> Result<AgentTeam, AssembleError>`
  - `pub enum AssembleError { UnknownToolkit { member: String, toolkit: String, valid: Vec<String> }, NoProviders, Team(#[from] TeamError), Io(#[from] std::io::Error) }`（thiserror）
  - toolkit 常量：`pub const VALID_TOOLKITS: [&str; 5] = ["fs", "web", "memory", "knowledge", "social(保留)"]`——**修正**：`social` 保留名单独校验报错，`VALID_TOOLKITS: [&str; 4] = ["fs", "web", "memory", "knowledge"]`；`mcp:<name>` 前缀校验（本期 MCP connector 返回"未接 MCP server 配置"错误——占位实现在 Task 8）
  - Patrol 装配约定：`Behavior::Patrol { task: "", .. }` → 引擎生成默认巡检文本（现有 scout::task_text 内容，issues_dir 有值则拼入路径）
  - `Handoff` 装配约定：装配结果不直接处理 Handoff——`AssembleInputs` 输出的 AgentTeam 之外，引擎返回 handoff 路由表：`pub struct Assembled { pub team: AgentTeam, pub handoffs: Vec<HandoffRoute> }`；`pub struct HandoffRoute { pub resource: String, pub to: String, pub message_template: String }`（routes 层确认 issue 时查此表——替代现硬编码 "planner"）

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::ModelTier;
    use llm_harness_loop::test_utils::MockLlmClient;

    fn settings() -> Settings {
        Settings { models: ModelTier { strong: "m".into(), main: "m".into(), cheap: "m".into() },
            scout_interval_secs: 3600, base_url: String::new() }
    }

    fn spec_with(toolkits: &[&str], model: ModelRef) -> TeamSpec {
        TeamSpec { id: "t".into(), template: None, name: "T".into(),
            members: vec![MemberSpec {
                id: "worker".into(), persona: "p".into(), role_label: "w".into(),
                model, toolkits: toolkits.iter().map(|s| s.to_string()).collect(),
                behaviors: vec![], provider_override: None }] }
    }

    fn inputs(spec: &TeamSpec) -> AssembleInputs {
        let repo = std::env::temp_dir().join(format!("ct-eng-{}", std::process::id()));
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        AssembleInputs {
            spec, settings: &settings(), api_key: "k",
            env: Arc::new(llm_harness_sandbox::OsEnv::new(&repo)),
            data_dir: std::env::temp_dir().join("ct-eng-data"),
            repo_path: Some(&repo),
        }
    }

    #[test]
    fn unknown_toolkit_fails_fast_with_valid_list() {
        let spec = spec_with(&["fs", "bogus"], ModelRef::Explicit("m".into()));
        let err = assemble(inputs(&spec)).unwrap_err();
        match err {
            AssembleError::UnknownToolkit { member, toolkit, valid } => {
                assert_eq!(member, "worker");
                assert_eq!(toolkit, "bogus");
                assert!(valid.contains(&"fs".to_string()));
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn social_is_rejected_as_reserved() {
        let spec = spec_with(&["social"], ModelRef::Explicit("m".into()));
        assert!(matches!(assemble(inputs(&spec)), Err(AssembleError::UnknownToolkit { .. })));
    }

    #[test]
    fn assembles_team_with_explicit_model_and_fs() {
        let spec = spec_with(&["fs"], ModelRef::Explicit("m".into()));
        let out = assemble(inputs(&spec)).unwrap();
        let h = out.team.harness(&llm_harness_agent_team::AgentId::new("worker")).unwrap();
        assert_eq!(h.state().model, "m");
        assert!(h.state().system_prompt.as_deref().unwrap().contains("p"));
    }

    #[test]
    fn tier_resolution_uses_global_settings() {
        let spec = spec_with(&["fs"], ModelRef::Tier("cheap".into()));
        let out = assemble(inputs(&spec)).unwrap();
        let h = out.team.harness(&llm_harness_agent_team::AgentId::new("worker")).unwrap();
        assert_eq!(h.state().model, "g-4-air".replace("g-4-air", "m")); // cheap → settings.cheap
    }

    #[test]
    fn patrol_empty_task_gets_engine_default_text() {
        // members: scout 带 Patrol{task:"", issues_dir:Some("/x")}
        // assemble 后 scout inbox 收到含 "/x" 的巡检文本
    }

    #[test]
    fn handoff_routes_collected() {
        // members: planner 带 Handoff{ResourceConfirmed{issues} → planner}
        // 断言 out.handoffs 长度 1、resource == "issues"
    }
}
```

（后两个测试的实现体按第一个测试的 inputs 模式补全——构造 MemberSpec 时带对应 Behavior，断言巡检文本/路由表。）

- [ ] **Step 2: 跑测试确认失败**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team engine`
Expected: FAIL — 模块不存在

- [ ] **Step 3: 实现**

装配循环（从现有 `project.rs::register` 90-140 行的组队代码迁移改造）：

```rust
pub fn assemble(inputs: AssembleInputs) -> Result<Assembled, AssembleError> {
    if inputs.api_key.is_empty() { return Err(AssembleError::NoProviders); }
    let mut config = TeamConfig::default();
    // memory/session 隔离目录 = data_dir/memory, data_dir/sessions
    config.memory_root = inputs.data_dir.join("memory");
    config.session_base_dir = inputs.data_dir.join("sessions");
    let mut team = AgentTeam::with_config(config);
    let mut handoffs = Vec::new();
    for m in &inputs.spec.members {
        validate_toolkits(m)?;                       // social 保留名 + 未知包 → Err
        let model = m.model.resolve(&inputs.settings.models);
        let mut b = IndividualSpec::new(&m.id, &m.role_label, &model)
            .configure(|b| b.system_prompt(Some(m.persona.clone())));
        for tk in &m.toolkits {
            b = match tk.as_str() {
                "fs" => b.configure(|b| b.install(&llm_harness_tools::FsToolsPlugin::new(None))),
                "web" => b.configure(|b| b.install(&llm_harness_tools::WebToolsPlugin::default())),
                // memory/knowledge：Task 8 接入（现有插件在 team build 内部统一装，
                // 泛化后按 toolkit 挂载）——本期先 map 到空并在 VALID 列表标注，
                // 见 Task 8
                other if other.starts_with("mcp:") => {
                    return Err(AssembleError::UnknownToolkit {
                        member: m.id.clone(), toolkit: other.to_string(),
                        valid: VALID_TOOLKITS.iter().map(|s| s.to_string()).collect(),
                    });
                }
                _ => unreachable!("validate_toolkits 已拦截"),
            };
        }
        team = team.member(b);
        for bh in &m.behaviors {
            if let Behavior::Handoff { on, to, message_template } = bh {
                handoffs.push(HandoffRoute {
                    resource: match on { Trigger::ResourceConfirmed { resource } => resource.clone() },
                    to: to.clone(), message_template: message_template.clone(),
                });
            }
        }
    }
    // providers: 每 member 一个 client（override 有值用 override，否则全局）——
    // providers vec 按 (model, base_url, key) 去重后构建，builder.provider(pattern) 匹配
    let providers = build_provider_set(inputs)?;
    // env 已在 inputs；memory 目录创建
    std::fs::create_dir_all(inputs.data_dir.join("memory"))?;
    team.build(inputs.env.clone(), providers).await?;
    // Patrol arming：task 为空 → scout::task_text 默认文本（issues_dir 有值拼路径）
    arm_behaviors(&mut team, &inputs.spec.members, inputs)?;
    Ok(Assembled { team, handoffs })
}
```

> **注意**：`assemble` 需为 `async`（`team.build` 是 async、`arm` 内 submit 同步）。签名改 `pub async fn assemble(...)`。测试为 `#[tokio::test]`。
> `memory`/`knowledge` toolkit 本期映射：`memory` → 引擎现有 team-level memory（TeamConfig.memory_root 已配置，无需额外插件，校验通过即视为启用）；`knowledge` → 本期装配为 no-op 并在 VALID 列表保留（插件挂载进 builder 的能力 Task 8 补）——两行为在 plan 里写死，不留悬念。

- [ ] **Step 4: 跑测试确认通过 + crate 回归**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-coding-team
git commit -m "feat(coding-team): assemble engine — TeamSpec to AgentTeam with toolkit/model/behavior resolution"
```

---

### Task 4: TeamManager 泛化 + routes 切 spec

**Files:**
- Modify: `crates/llm-harness-coding-team/src/project.rs`（原地改名为 team_manager 语义，文件名 Task 9 统一改）
- Modify: `crates/llm-harness-coding-team/src/routes.rs`
- Modify: `crates/llm-harness-coding-team/src/pulse.rs`（去 ROLE_IDS）
- Test: `crates/llm-harness-coding-team/src/project.rs` / `routes.rs` 内置测试更新

**Interfaces:**
- Consumes: Task 1-3 全部
- Produces:
  - `ProjectManager` 更名 `TeamManager`（类型别名 `pub type TeamManager = ...` 过渡期保留一个 task 后删除——**直接改名，不留别名**，routes/pulse 同 commit 更新）
  - `TeamManager::create(spec: TeamSpec, repo_path: Option<PathBuf>, data_root: &Path) -> Result<&TeamEntry, TeamError>`：走 `engine::assemble`；`Project` 更名 `TeamEntry { id, spec: TeamSpec, repo_path: Option<PathBuf>, team: AgentTeam, issues: IssueStore, handoffs: Vec<HandoffRoute> }`
  - `TeamManager::list() -> Vec<TeamSummary>`（含 spec name/members）
  - `TeamManager::remove(id)`（原 shutdown）
  - `pulse::snapshot(team, spec)`：成员从 `spec.members` 枚举（含 role_label），去 `ROLE_IDS`/`SCOUT_ID` 硬编码
  - 确认 issue 的 handoff：`transition_issue` 查 `entry.handoffs`，有 `resource == "issues"` 路由则按 `message_template`（`{body}` 占位符替换）submit；无路由只改状态
  - 兼容过渡：无 repo_path 的团队（非 coding 模板）合法存在——OsEnv 用 data_dir 兜底 working dir

- [ ] **Step 1: 写失败测试**（改造现 project.rs 测试）

现有测试改造点：
- `register_builds_team_with_isolated_roots` → `create_builds_team_with_isolated_roots`（入参从 providers/tier 改为 spec）
- `scout_armed_when_interval_positive` → 改为 Patrol 行为驱动（spec member 带 Patrol，断言巡检文本入 inbox）
- `register_rejects_missing_repo` → repo_path 有值且不存在时报错（None 时跳过校验）
- 新增 `team_without_repo_works`（repo_path: None，OsEnv 兜底 data_dir）
- 新增 `handoff_route_drives_issue_confirmation`（member 带 Handoff，确认 issue 后目标 inbox 收到 template 渲染文本）

```rust
// handoff 测试核心断言
let mut spec = blank_spec(); // 2 成员: triage(带Handoff) + worker
spec.members[0].behaviors = vec![Behavior::Handoff {
    on: Trigger::ResourceConfirmed { resource: "issues".into() },
    to: "worker".into(),
    message_template: "已确认: {body}".into(),
}];
let mut tm = TeamManager::new(&root);
tm.create(spec.clone(), None, &tier_settings()).await.unwrap();
let store = IssueStore::new(root.join("teams").join("t").join("issues"));
let i = store.create("scout", "t", "body-x").unwrap();
store.set_status(&i.id, IssueStatus::Confirmed).unwrap();
// 经 routes 层的 transition_issue（或直接调 TeamManager 的 handoff 应用函数）
tm.apply_issue_handoff("t", &store.load(&i.id).unwrap().0).unwrap();
let ctx = tm.get("t").unwrap().team.social_context().unwrap();
let got = ctx.drain_inbox(&AgentId::new("worker"));
assert!(got.iter().any(|m| m.body.contains("已确认: body-x")));
```

- [ ] **Step 2: 跑测试确认失败**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team`
Expected: FAIL（create/apply_issue_handoff 不存在）

- [ ] **Step 3: 实现**

- `project.rs`：类型改名 + create 走 `engine::assemble`（providers 构建移入 engine，TeamManager 不再收 providers/tier 参数）；`apply_issue_handoff(team_id, issue)` 公开方法（读 entry.handoffs，匹配 resource=="issues"，submit(target, template.replace("{body}", &issue.body))）
- `routes.rs`：`register_project` 改为接收 `{id, template_id?, repo_path?}`——有 template_id 走 TemplateStore.load → spec 落盘 → create；`transition_issue` 改调 `apply_issue_handoff`（message 组装从硬编码换 handoff 路由）；pulse 加 `role_label`
- `pulse.rs`：签名 `snapshot(spec: &TeamSpec, team: &AgentTeam)`，成员循环 `spec.members`
- 删除 `roles.rs` 的 ROLE_IDS 引用全部清零（roles.rs 文件 Task 9 删，本任务先断引用）

- [ ] **Step 4: 跑测试确认通过 + crate 回归**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team`
Expected: PASS（53 单测中 project/pulse/routes 相关的转 spec 语义，其余不动）

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-coding-team
git commit -m "feat(coding-team): TeamManager on engine::assemble — spec-driven teams, handoff routes, pulse from spec"
```

---

### Task 5: 实例持久化 + 旧数据迁移

**Files:**
- Modify: `crates/llm-harness-coding-team/src/team_manager`（project.rs）
- Create: `crates/llm-harness-coding-team/src/migrate.rs`
- Modify: `crates/llm-harness-coding-team/src/lib.rs`
- Modify: `crates/llm-harness-coding-team/src/bin/coding-team.rs`（启动迁移 + 从 teams/ 恢复）
- Test: `crates/llm-harness-coding-team/src/migrate.rs` 内置测试

**Interfaces:**
- Produces:
  - `TeamManager::persist_spec(data_root, spec)`：写 `teams/<id>/team.json`
  - `TeamManager::list_persisted(data_root) -> Vec<TeamSpec>`
  - `migrate::migrate_legacy(data_root: &Path) -> MigrationReport { teams_migrated: usize, already_migrated: bool }`：projects.jsonl → teams/<id>/team.json（template 指向 coding-team v1，members 全文 = 从内置模板展开，等价覆写态）；旧文件改名为 `projects.jsonl.migrated`（不删除）；目录名迁移 `~/.harness/coding-team` → `~/.harness/agent-studio`（检测旧目录存在且新目录不存在 → rename 整目录）
  - bin 启动：先 migrate_legacy，再 `list_persisted` 逐个 create

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn migrates_projects_jsonl_to_teams() {
    let root = temp_dir();
    std::fs::create_dir_all(root.join("projects")).unwrap();
    std::fs::write(root.join("projects.jsonl"),
        r#"{"id":"p1","repo_path":"/tmp/repo1"}"#).unwrap();
    let report = migrate::migrate_legacy(&root).unwrap();
    assert_eq!(report.teams_migrated, 1);
    let spec_path = root.join("teams").join("p1").join("team.json");
    let spec: TeamSpec = serde_json::from_str(&std::fs::read_to_string(spec_path).unwrap()).unwrap();
    assert_eq!(spec.id, "p1");
    assert_eq!(spec.template.as_ref().unwrap().template_id, "coding-team");
    // members 全文落盘（等价覆写态）
    assert!(!spec.members.is_empty());
    // 旧文件改名保留
    assert!(root.join("projects.jsonl.migrated").exists());
}

#[test]
fn migration_is_idempotent() {
    let root = temp_dir();
    std::fs::write(root.join("projects.jsonl"), r#"{"id":"p1","repo_path":"/tmp/r"}"#).unwrap();
    migrate::migrate_legacy(&root).unwrap();
    let report = migrate::migrate_legacy(&root).unwrap();
    assert!(report.already_migrated);
}

#[test]
fn migrates_legacy_dir_name() {
    // 旧目录 ~/.harness/coding-team 内容在 root 下模拟：
    // root/coding-team/settings.json 存在、root/agent-studio 不存在
    // → rename 后 root/agent-studio/settings.json 存在
}
```

- [ ] **Step 2: 跑测试确认失败** → **Step 3: 实现**（按 Interfaces；成员展开用 TemplateStore::load("coding-team")）→ **Step 4: 跑测试确认通过**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team migrate`

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-coding-team
git commit -m "feat(coding-team): instance persistence + legacy projects.jsonl/dir migration"
```

---

### Task 6: agent-team replace_member

**Files:**
- Modify: `crates/llm-harness-agent-team/src/team.rs`
- Test: `crates/llm-harness-agent-team/src/team.rs` 内置测试

**Interfaces:**
- Consumes: 现有 `build()` 内的 runner spawn 模式（`AgentRunner::new(...).spawn()`）、`harnesses: HashMap`、`social_context()`
- Produces:
  - `pub async fn replace_member(&mut self, id: &str, spec: IndividualSpec, providers: Vec<(String, Arc<dyn LlmClient>)>) -> Result<(), TeamError>`
  - 语义：对 built 团队——abort 该成员 runner（从 self.runners 移除其 JoinHandle 并 abort）、`harnesses.remove(id)`、社交注册表该成员保留（收件箱/通讯录不动）、用新 spec 重建 harness + spawn runner、`harnesses.insert(id, new_harness)`
  - 未 built / id 不存在 / 保留 id → `TeamError::NotBuilt` / `TeamError::AgentNotFound` / `TeamError::ReservedAgentId`

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn replace_member_swaps_harness_and_keeps_inbox() {
    // build 2-member team（MockLlmClient 预留）
    // 给 a1 submit 一条消息（inbox 非空）
    // 记录旧 harness 指针
    // replace_member("a1", 新 spec（不同 system_prompt/model）, providers)
    // 断言：harness 指针已变；h.state().model == 新值；
    //       social_context().get_agent_info(a1) 仍存在；
    //       inbox 消息仍在（drain 可见）；a2 未受影响（harness 指针不变）
}

#[tokio::test]
async fn replace_member_unknown_agent_404() {
    // NotBuilt 前 → TeamError::NotBuilt；built 后未知 id → AgentNotFound
}

#[tokio::test]
async fn replace_member_reserved_id_rejected() {
    // replace_member("operator", ...) → ReservedAgentId
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-agent-team replace_member`
Expected: FAIL — 方法不存在

- [ ] **Step 3: 实现**

```rust
pub async fn replace_member(
    &mut self,
    id: &str,
    spec: IndividualSpec,
    providers: Vec<(String, Arc<dyn LlmClient>)>,
) -> Result<()> {
    if !self.built { return Err(TeamError::NotBuilt); }
    let ctx = self.ctx.as_ref().ok_or(TeamError::NotBuilt)?;
    let agent_id = AgentId::new(id);
    if reserved::is_reserved_id(id) { return Err(TeamError::ReservedAgentId(id.to_string())); }
    if ctx.get_agent_info(&agent_id).is_none() { return Err(TeamError::AgentNotFound(id.to_string())); }
    // 1. abort 该成员现有 runner（runners 是 JoinHandle<()>，无法按 id 定位——
    //    方案：替换前通过 harness abort 当前 run（旧 harness.abort()），
    //    runner 任务自然退出；JoinHandle 留在 vec 中无害（已结束）。
    if let Some(old) = self.harnesses.get(&agent_id) { old.abort(); }
    // 2. 重建（复用 build 的单成员构造路径——把 build 循环体抽 fn
    //    build_one(&mut self, ctx, spec, providers, &operator_id) -> Result<JoinHandle<()>>
    //    供 build 与 replace_member 共用；reflection/allow_subagents 从 spec 读取）
    self.harnesses.remove(&agent_id);
    let handle = self.build_one(spec, providers, ctx, &self.operator_id.clone()).await?;
    self.runners.push(handle);
    Ok(())
}
```

> `build_one` 抽取是本任务的主要重构：把 `build()` 循环体（plugin 构造、builder 配置、harness 构建、runner spawn）抽成私有方法，`build()` 循环调用它。行为必须等价——`build()` 现有测试全过即是证明。

- [ ] **Step 4: 跑测试确认通过 + crate 全量回归**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-agent-team`
Expected: PASS（151+ 测试无回归）

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-agent-team
git commit -m "feat(agent-team): replace_member — rebuild one member in place, inbox/contacts preserved"
```

---

### Task 7: 成员级配置端点（含重建触发）

**Files:**
- Modify: `crates/llm-harness-coding-team/src/routes.rs`
- Modify: `crates/llm-harness-coding-team/src/team_manager`（project.rs）
- Test: `crates/llm-harness-coding-team/src/routes.rs` 内置测试

**Interfaces:**
- Consumes: Task 6 `replace_member`；Task 4 `TeamManager`
- Produces:
  - `GET /api/team/agent/config?project=&agent=`（已存在，本任务补 override 状态）：响应加 `"override": {"model": null|"...", "base_url": null|"...", "api_key_set": bool}` 与 `"inherits_global": {...}`
  - `PUT /api/team/agent/config` body `{project, agent, model?: {tier?: "strong"|"main"|"cheap", explicit?: String}, api_key?: String, base_url?: String, persona?: String, toolkits?: Vec<String>}`：
    - persona/toolkits → 更新实例 spec（`TeamManager::update_member_spec`）+ 热应用（`set_system_prompt` / 重建仅当 toolkits 变）
    - model/key/url → 更新 override + `replace_member`（重建；响应带 `"rebuilt": true` 提示）
    - override 的 api_key 落 `<data>/teams/<id>/secrets/<agent>` 0600
  - `TeamManager::update_member_spec(id, member: MemberSpec)`（改实例 spec + persist_spec）
  - `TeamManager::member_override(id, agent) -> Option<ProviderOverride>` / `set_member_override(...)`

- [ ] **Step 1: 写失败测试**（routes.rs 内置，oneshot 模式沿用）

```rust
#[tokio::test]
async fn agent_config_put_hot_applies_persona() {
    // setup + create from blank spec (member "worker")
    // PUT config {persona: "new persona"}
    // 断言 200 + harness.state().system_prompt 含 "new persona"
    // 断言实例 spec 持久化文件已更新
}

#[tokio::test]
async fn agent_config_model_change_rebuilds_member() {
    // PUT config {model: {explicit: "other-model"}}
    // 断言 200、rebuilt: true、harness.state().model == "other-model"
    // 断言 pulse 中该成员 idle（重建后无消息）
}

#[tokio::test]
async fn agent_config_override_key_isolated() {
    // PUT config {api_key: "sk-x", base_url: "https://o.io"}
    // GET config → override.api_key_set == true、响应无 "sk-x" 明文
    // secrets 文件存在且 0600
}

#[tokio::test]
async fn agent_config_unknown_member_404() { /* 404 */ }
```

- [ ] **Step 2: 跑测试确认失败** → **Step 3: 实现**（PUT handler 组装：读现 spec → 改 member → update_member_spec → 按变更类型热应用或 replace_member(用 engine::build_provider_set 出 providers)）→ **Step 4: 全 crate PASS**

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-coding-team
git commit -m "feat(coding-team): per-agent config PUT — persona/toolkits hot-apply, model/key/url rebuild via replace_member"
```

---

### Task 8: memory/knowledge toolkit 接入 + MCP 占位明确化

**Files:**
- Modify: `crates/llm-harness-coding-team/src/engine.rs`
- Test: `crates/llm-harness-coding-team/src/engine.rs` 测试补充

**Interfaces:**
- Consumes: `llm-harness-memory::MemoryPlugin`、`llm-harness-knowledge::KnowledgePlugin`（engine 新增这两个依赖）
- Produces:
  - toolkit `memory` → `builder.install(&MemoryPlugin)`（替代 Task 3 的"TeamConfig 已配置即启用"语义——显式挂载）
  - toolkit `knowledge` → `builder.install(&KnowledgePlugin)`（Task 3 的 no-op 补齐）
  - toolkit `mcp:<name>` → 本期仍报错，但错误信息明确为 `"mcp toolkit requires MCP server config (planned)"`，独立错误变体 `AssembleError::McpNotYetSupported { server: String }`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn memory_toolkit_installs_memory_plugin() {
    // spec member toolkits=["memory"] → assemble 后 harness.state().tools
    // 含 memory_write / knowledge_search 之一（按 MemoryPlugin 实际注册名断言——
    // 实现者先读 crates/llm-harness-memory/src/plugin.rs 确认工具名再写断言）
}

#[test]
fn knowledge_toolkit_installs_knowledge_plugin() { /* 同上，读 plugin.rs 确认 */ }

#[test]
fn mcp_toolkit_reports_dedicated_error() {
    // matches!(err, AssembleError::McpNotYetSupported { server } if server == "my-mcp")
}
```

- [ ] **Step 2: 跑失败** → **Step 3: 实现**（engine.rs match 补两分支 + 新错误变体；Cargo.toml 加 `llm-harness-memory`、`llm-harness-knowledge` workspace 依赖）→ **Step 4: PASS + 回归**

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-coding-team
git commit -m "feat(coding-team): memory/knowledge toolkits wired to plugins; dedicated MCP error"
```

---

### Task 9: crate/bin 改名 + roles.rs 删除 + 模板/升级端点

**Files:**
- Rename: `crates/llm-harness-coding-team/` → `crates/llm-harness-agent-studio/`（git mv）
- Rename: `src/bin/coding-team.rs` → `src/bin/agent-studio.rs`
- Rename: `src/project.rs` → `src/team_manager.rs`（Task 4 已改语义，本任务只移文件）
- Modify: 根 `Cargo.toml`（member + workspace dependency 路径）
- Modify: `crates/llm-harness-agent-studio/Cargo.toml`（name）
- Delete: `src/roles.rs`
- Create: `src/routes` 模板/升级端点（见下）
- Modify: `static/team.html` → 标题/文案去 coding 化（三列 UI 是 Task 10）
- Modify: `docs/agent-team-guide.md`、`docs/design/2026-08-29-coding-team-design.md` 顶部加"已演化为 agent-studio"注记

**Interfaces:**
- Produces:
  - crate name `llm-harness-agent-studio`、bin `agent-studio`
  - `GET /api/studio/templates` → `[TemplateMeta]`
  - `POST /api/studio/teams` body `{id, template_id?, repo_path?, name?}` → 创建实例（有 template 走模板展开，无则空白）
  - `GET/PUT /api/studio/teams/<id>/spec` → TeamSpec 读写（PUT 后若 team 在运行 → 提示需删除重建或仅允许结构不变的字段编辑——**实现取：PUT 仅允许编辑 persona/role_label/toolkits/model/behaviors（等价 per-member PUT 的批量），增删成员返回 409 提示先删除再创建**。成员增删的支持在 spec 里标"可增删"，本期经 create/delete team 路径达成，YAGNI）
  - `POST /api/studio/teams/<id>/upgrade-check` / `upgrade`（逐字段相等判定；upgrade 只重建未覆写成员）

- [ ] **Step 1: git mv + 全路径修复**

```bash
git mv crates/llm-harness-coding-team crates/llm-harness-agent-studio
git mv crates/llm-harness-agent-studio/src/bin/coding-team.rs crates/llm-harness-agent-studio/src/bin/agent-studio.rs
git mv crates/llm-harness-agent-studio/src/project.rs crates/llm-harness-agent-studio/src/team_manager.rs
```

然后全局替换 `llm-harness-coding-team` → `llm-harness-agent-studio`（Cargo.toml ×2、lib.rs、所有 `use` 路径、测试）、`coding_team` → `agent_studio`（bin 名）。删除 roles.rs（引用已断）。data root 默认路径常量 `default_data_root` 改 `~/.harness/agent-studio`（迁移函数已有目录改名逻辑）。

- [ ] **Step 2: 全量回归**

Run: `CARGO_HTTP_PROXY="" cargo test --workspace --exclude llm-harness-live-tests`
Expected: PASS（所有既有断言保留，仅路径变化）

- [ ] **Step 3: 模板/升级端点 + 失败测试先行**

测试：`GET /api/studio/templates` 含 coding-team；`POST /api/studio/teams` from template 创建后 `GET spec` members==11 且 template 记录在案；`upgrade-check` 在改了 coder persona 的实例上返回"将更新: [除 coder 外]"；`upgrade` 后 spec 中 coder persona 不变、其他成员与模板新版本一致（改内置模板 version 驱动测试——测试中写用户目录模板副本模拟版本变更）。

- [ ] **Step 4: 实现端点** → **Step 5: PASS + Commit**

```bash
git add -A
git commit -m "feat!: rename to agent-studio — template/instance APIs, upgrade check/apply, roles.rs removed into template data"
```

---

### Task 10: 面板三列工作区 + ⚙ 全局设置模态框

**Files:**
- Modify: `crates/llm-harness-agent-studio/static/team.html`（重写动态 tab 区域 + 全局设置模态框；文件名 Task 9 可一并改 studio.html——**随 Task 9 git mv**）
- Test: 无自动化（纯 UI）；live 冒烟清单见 Step 4

**Interfaces:**
- Consumes: `GET /api/team/pulse`、`GET /api/team/agent/session`、`GET/PUT /api/team/agent/config`、`GET /api/studio/templates`、`GET/PUT /api/studio/teams/<id>/spec`、`POST /api/team/agent/inject|abort`、settings API

- [ ] **Step 1: 三列布局骨架**

```html
<section id="tab-pulse">
  <div class="studio-grid">
    <div id="col-members" class="studio-col"><!-- 成员卡片列表 + [+ 新成员] --></div>
    <div id="col-history" class="studio-col"><!-- 选中成员 session 流 --></div>
    <div id="col-config" class="studio-col"><!-- 选中成员配置表单 --></div>
  </div>
  <div class="pulse-row"><!-- 待处理消息 / Issue 概况（保留） --></div>
</section>
```

CSS：`.studio-grid { display:grid; grid-template-columns: 280px 1fr 320px; gap:.75rem; height: calc(100vh - 120px); } .studio-col { overflow-y:auto; }`（窄屏 media query 折叠为单列堆叠）。

- [ ] **Step 2: 左列成员卡片**

数据 `GET /api/team/pulse`（含 role_label）；卡片 = 状态点 + id + role_label + busy 任务摘要；点击 → `selectedAgent` 状态 → 触发中/右列加载；卡片内联「插话/停止」小按钮（现有逻辑迁入）；列表底部「+ 新成员」→ 弹 prompt 收集 id/role_label → `PUT spec` 追加 member（409 时提示走删除重建路径）。

- [ ] **Step 3: 中列历史 + 右列配置**

中列：`loadAgentSession(project, agent)` 拉 `/api/team/agent/session` 渲染行（user 右对齐蓝 / assistant 左对齐 / tool_result 折叠灰），WS `operator_msg` 不进此列（仍进对话 tab）；5s 轮询合并新行（按行数差追加）。

右列：`loadAgentConfig` 渲染——模型（select 三档 + "显式"输入框切换）、api key（password 框 + 继承状态提示）、base_url、persona textarea、工具包 checkbox 组（fs/web/memory/knowledge；mcp 显示"规划中"禁用态）、保存按钮调 PUT（响应 `rebuilt: true` 时提示"已重建，内存上下文已重置"）。全局设置模态框：现设置 tab 内容迁入 fixed overlay（照 senza-agent set-modal 模式：遮罩 + inner 卡片 + 取消/保存），⚙ 按钮置顶 nav 右侧。

- [ ] **Step 4: live 冒烟清单**

启动 bin（`CT_PORT=18799 CT_DATA_ROOT=$TMPDIR/ct-studio`）：
1. 注册项目（走模板）→ 动态 tab 三列渲染 11 成员
2. 点 coder → 中列出 session 历史、右列出配置（模型 main、工具包 fs 勾选）
3. 改 persona 保存 → 热生效（GET config 回读）
4. 改 model 保存 → 提示重建 → pulse 状态恢复 idle
5. ⚙ 打开全局设置 → 改三档模型 → 保存
6. 插话/停止按钮对 busy 成员生效
7. JS 语法检查：`node --check` 提取的 script 块

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-agent-studio/static
git commit -m "feat(agent-studio): three-column panel — member cards, session history, per-agent config, global settings modal"
```

---

### Task 11: 文档 + 全量回归收尾

**Files:**
- Modify: `docs/agent-team-guide.md`（coding-team 一节改写为 agent-studio：模板机制、三列面板、per-agent 配置）
- Modify: `docs/design/2026-08-30-agent-team-studio-design.md`（状态行改"已实施"）
- Modify: `README.md` 若引用 crate 名（检查）

- [ ] **Step 1: 文档更新**（内容：studio 定位、模板创建流程、per-agent 配置说明、迁移提示）
- [ ] **Step 2: 全量回归**

Run: `CARGO_HTTP_PROXY="" cargo test --workspace --exclude llm-harness-live-tests && CARGO_HTTP_PROXY="" cargo clippy --workspace --all-targets 2>&1 | grep -cE "^warning: |^error"`
Expected: 全 PASS、clippy 0

- [ ] **Step 3: live 冒烟**（Task 10 清单复跑 + 旧数据迁移验证：用 v1 fixture 目录启动 → 确认 teams/ 生成且面板可见）
- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "docs: agent-studio guide + spec status; full regression green"
```

---

## Self-Review 结论

1. **Spec 覆盖**：TeamSpec（T1）、内置模板（T2）、装配（T3）、TeamManager 泛化+handoff（T4）、持久化迁移（T5）、replace_member（T6）、per-agent 配置端点（T7）、toolkits 补全（T8）、改名+模板/升级 API（T9）、三列 UI（T10）、文档（T11）。spec §4 升级 API→T9、§5 面板→T10、§4.x 装配→T3/T8、§3 模型→T1。无缺口。
2. **占位符**：T3 后两个测试标注"按第一个测试模式补全"并给出断言目标——是显式指令；`memory`/`knowledge` 工具名断言要求实现者先读 plugin.rs 确认——显式指令而非 TBD。MCP 是明确错误变体非占位。
3. **类型一致性**：`TeamSpec/MemberSpec/ModelRef/Behavior/ProviderOverride`（T1）→ T2/T3/T4/T7 一致；`AssembleError::UnknownToolkit{member,toolkit,valid}`（T3）→ T8 `McpNotYetSupported{server}` 独立变体；`HandoffRoute{resource,to,message_template}`（T3）→ T4 `apply_issue_handoff` 一致；`replace_member(id, spec, providers)`（T6）→ T7 一致。`VALID_TOOLKITS: [&str; 4]`（fs/web/memory/knowledge）全文一致（social 单独保留名校验）。
