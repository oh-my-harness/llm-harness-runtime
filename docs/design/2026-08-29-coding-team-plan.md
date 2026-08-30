# Coding Team 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 构建一个多角色 coding team（10 常驻角色、一项目一团队、Markdown issue、前端操作面板），让人通过对话完成完整代码交付。

**Architecture:** 应用层 crate `llm-harness-coding-team`（roles/project/issue/scout/routes + 薄 bin + static 面板），经 inspector 新增的 `extra_routes`/`extra_static` 扩展点挂载；每个项目注册时构建独立 `AgentTeam`，`OsEnv::new(repo_path)` 绑定工具 cwd，`TeamConfig.memory_root` 按项目覆盖实现全隔离。

**Tech Stack:** Rust workspace（axum、tokio、serde、parking_lot）；前端为纯 HTML/JS 单页（无构建步骤）；测试用 `MockLlmClient`。

**Spec:** `docs/design/2026-08-29-coding-team-design.md`（连同 ADR `docs/design/2026-08-29-inspector-two-layer-positioning.md` 一起阅读）

## Global Constraints

- 每个项目一个 `AgentTeam`；团队间 memory_root / issue 目录 / env working_dir 全隔离，不得共享
- issue 存储为 Markdown 文件（frontmatter：`id/status/source/created/title`），路径 `~/.harness/coding-team/projects/<project-id>/issues/`；只有人能转换 issue 状态
- 工作分支约定 `team/<project-id>`，不碰 main；push 由人确认
- 全部测试离线（`MockLlmClient`），测试命令统一 `CARGO_HTTP_PROXY="" cargo test -p <crate>`
- 领域代码不得写入 `llm-harness-inspector` crate（ADR 约束）；inspector 只加扩展点
- 角色 id 固定：`brainstormer/planner/coder/reviewer-performance/reviewer-robustness/reviewer-standards/reviewer-magical-prompt/reviewer-spec-compliance/judge/debugger/scout`
- 提交信息遵循仓库现有 conventional commits 风格（feat/fix/docs + scope）

---

### Task 1: inspector 扩展点 extra_routes / extra_static

**Files:**
- Modify: `crates/llm-harness-inspector/src/server.rs`（`InspectorBuilder`，约 63-80 行）
- Modify: `crates/llm-harness-inspector/src/routes/mod.rs`（`build_router` 签名）
- Test: `crates/llm-harness-inspector/tests/integration.rs`

**Interfaces:**
- Consumes: 现有 `InspectorBuilder { registry, port, shared }`、`routes::build_router(state) -> Router`
- Produces: `InspectorBuilder::extra_routes(self, router: axum::Router) -> Self`、`InspectorBuilder::extra_static(self, prefix: &str, dir: impl Into<PathBuf>) -> Self`；`mount()` 将二者合并进 server（extra 路由同受 token 鉴权中间件保护——合并发生在 `with_state` 之后的同一 Router 上）

- [ ] **Step 1: 写失败测试**

在 `crates/llm-harness-inspector/tests/integration.rs` 追加（沿用文件内现有的 tower ServiceExt 模式）：

```rust
#[tokio::test]
async fn extra_routes_are_served_and_authed() {
    let harness = test_harness().await; // 文件内已有的构造 helper
    let extra = axum::Router::new()
        .route("/api/team/ping", axum::routing::get(|| async { "pong" }));
    let inspector = Inspector::builder(harness)
        .extra_routes(extra)
        .mount()
        .await
        .unwrap();
    let addr = inspector.bound_addr().unwrap();
    // 无 token → 401/403；带 token → 200 "pong"
    let client = reqwest_client(); // 文件内已有或用 hyper 直连
    let no_auth = client
        .get(format!("http://{addr}/api/team/ping"))
        .send().await.unwrap();
    assert_ne!(no_auth.status(), 200);
    let token = /* 从集成测试现有的 token 获取方式 */;
    let authed = client
        .get(format!("http://{addr}/api/team/ping?token={token}"))
        .send().await.unwrap();
    assert_eq!(authed.status(), 200);
    inspector.shutdown().await;
}
```

> 注意：执行者先读 `tests/integration.rs` 现有的鉴权测试（搜 `token`），复用其 client/token 模式；上面骨架中的 `/* ... */` 必须替换为文件内真实写法。

- [ ] **Step 2: 跑测试确认失败**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-inspector --test integration extra_routes`
Expected: FAIL — `extra_routes` 方法不存在

- [ ] **Step 3: 实现**

`server.rs` 的 `InspectorBuilder` 增加字段与 builder 方法：

```rust
pub struct InspectorBuilder {
    registry: Arc<HarnessRegistry>,
    port: u16,
    shared: Option<Arc<crate::state::SharedEvents>>,
    extra_routes: Option<axum::Router>,
    extra_static: Option<(String, std::path::PathBuf)>,
}

/// Merge additional domain routes (same auth as the built-in API).
pub fn extra_routes(mut self, router: axum::Router) -> Self {
    self.extra_routes = Some(router);
    self
}

/// Serve a static directory under `prefix` (e.g. "app" → /app/*).
pub fn extra_static(mut self, prefix: &str, dir: impl Into<std::path::PathBuf>) -> Self {
    self.extra_static = Some((prefix.to_string(), dir.into()));
    self
}
```

`mount()` 中 `build_router(state)` 之后合并（鉴权中间件已在 build_router 内层时，extra 路由需要套同一中间件——执行者检查 `routes/mod.rs` 中 token 校验的实现位置，保证 extra 路由同样被保护；若鉴权在每 handler 内做，则 extra 无需处理）：

```rust
let mut app = crate::routes::build_router(state);
if let Some(extra) = self.extra_routes {
    app = app.merge(extra);
}
if let Some((prefix, dir)) = self.extra_static {
    app = app.nest_service(
        &format!("/{prefix}"),
        tower_http::services::ServeDir::new(dir),
    );
}
```

> 若 workspace 无 `tower-http` 依赖：在 `crates/llm-harness-inspector/Cargo.toml` 加 `tower-http = { workspace = true, features = ["fs"] }`，并在根 `Cargo.toml` 的 `[workspace.dependencies]` 加 `tower-http`（版本对齐 axum 0.x 兼容矩阵）。静态目录也可以手写 fallback handler 避免新依赖——优先检查 axum 版本自带能力，YAGNI：能不新增依赖就不新增。

- [ ] **Step 4: 跑测试确认通过**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-inspector`
Expected: 全部 PASS（含原有 160 个测试，无回归）

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-inspector
git commit -m "feat(inspector): extra_routes/extra_static mount points for upper-layer apps"
```

---

### Task 2: coding-team crate 骨架 + IssueStore

**Files:**
- Create: `crates/llm-harness-coding-team/Cargo.toml`
- Create: `crates/llm-harness-coding-team/src/lib.rs`
- Create: `crates/llm-harness-coding-team/src/issue.rs`
- Modify: `Cargo.toml`（workspace members 加 `"crates/llm-harness-coding-team"`）
- Test: `crates/llm-harness-coding-team/src/issue.rs`（单元测试内置 `#[cfg(test)]`）

**Interfaces:**
- Consumes: 无（本 crate 第一个模块）
- Produces:
  - `pub struct Issue { pub id: String, pub status: IssueStatus, pub source: String, pub created: String, pub title: String, pub body: String }`
  - `pub enum IssueStatus { Pending, Confirmed, Rejected, Done }`（`as_str()` → "pending"/"confirmed"/"rejected"/"done"，`FromStr` 反解）
  - `pub struct IssueStore { root: PathBuf }`，方法：
    - `new(root: impl Into<PathBuf>) -> Self`
    - `create(&self, source: &str, title: &str, body: &str) -> Result<Issue, IssueError>`（自动分配 `I-NNN`，写 `<id>-<slug>.md`）
    - `list(&self) -> Result<Vec<Issue>, IssueError>`
    - `set_status(&self, id: &str, status: IssueStatus) -> Result<Issue, IssueError>`（重写 frontmatter；允许的转换：Pending→Confirmed/Rejected、Confirmed→Done，其余返回 `IssueError::InvalidTransition`）
  - 错误类型 `pub enum IssueError { Io(std::io::Error), Parse(String), InvalidTransition { from: IssueStatus, to: IssueStatus }, NotFound(String) }`（impl `Display`/`Error`）

- [ ] **Step 1: 建 crate**

`crates/llm-harness-coding-team/Cargo.toml`：

```toml
[package]
name = "llm-harness-coding-team"
version.workspace = true
edition.workspace = true

[dependencies]
llm-harness-agent.workspace = true
llm-harness-agent-team.workspace = true
llm-harness-types.workspace = true
llm-harness-tools.workspace = true
llm-harness-inspector.workspace = true
llm-harness-loop.workspace = true
axum.workspace = true
tokio.workspace = true
serde.workspace = true
serde_json.workspace = true
futures.workspace = true
tracing.workspace = true
chrono.workspace = true
parking_lot.workspace = true
```

根 `Cargo.toml` members 插入 `"crates/llm-harness-coding-team"`。`lib.rs` 暂时只有 `pub mod issue;`。

- [ ] **Step 2: 写失败测试**（`issue.rs` 内置）

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> IssueStore {
        let dir = std::env::temp_dir().join(format!("ct-issue-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        IssueStore::new(dir)
    }

    #[test]
    fn create_assigns_sequential_ids_and_roundtrips() {
        let store = temp_store();
        let a = store.create("scout", "Fix io error context", "body a").unwrap();
        let b = store.create("scout", "Second issue", "body b").unwrap();
        assert_eq!(a.id, "I-001");
        assert_eq!(b.id, "I-002");
        assert_eq!(a.status, IssueStatus::Pending);
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 2);
    }

    #[test]
    fn pending_to_confirmed_to_done_ok() {
        let store = temp_store();
        let i = store.create("scout", "t", "b").unwrap();
        let c = store.set_status(&i.id, IssueStatus::Confirmed).unwrap();
        assert_eq!(c.status, IssueStatus::Confirmed);
        let d = store.set_status(&i.id, IssueStatus::Done).unwrap();
        assert_eq!(d.status, IssueStatus::Done);
    }

    #[test]
    fn invalid_transition_rejected() {
        let store = temp_store();
        let i = store.create("scout", "t", "b").unwrap();
        // pending → done 非法
        assert!(matches!(
            store.set_status(&i.id, IssueStatus::Done),
            Err(IssueError::InvalidTransition { .. })
        ));
        // agent 无法确认（只有人）——这里测的是状态机本身；scout 侧没有任何
        // set_status 的调用入口由 Task 5 的 scout 模板保证
    }

    #[test]
    fn frontmatter_roundtrip_preserves_body() {
        let store = temp_store();
        let i = store.create("scout", "Title with spaces", "## 现象\n- a\n- b").unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed[0].body, "## 现象\n- a\n- b");
        assert_eq!(listed[0].title, "Title with spaces");
    }
}
```

- [ ] **Step 3: 跑测试确认失败**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team`
Expected: 编译失败（模块为空）

- [ ] **Step 4: 实现 issue.rs**

frontmatter 用最简手工解析（不引入 yaml 库——字段固定四行）：

```rust
//! Markdown-file issue store. One issue = one `.md` file with a fixed
//! four-line frontmatter (id/status/source/created/title); everything
//! after the second `---` is the body. State transitions are validated
//! here; WHO may transition (only the human) is enforced by callers.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueStatus { Pending, Confirmed, Rejected, Done }

impl IssueStatus {
    pub fn as_str(&self) -> &'static str {
        match self { Self::Pending => "pending", Self::Confirmed => "confirmed",
                     Self::Rejected => "rejected", Self::Done => "done" }
    }
}

impl std::str::FromStr for IssueStatus {
    type Err = IssueError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "pending" => Ok(Self::Pending), "confirmed" => Ok(Self::Confirmed),
            "rejected" => Ok(Self::Rejected), "done" => Ok(Self::Done),
            other => Err(IssueError::Parse(format!("unknown status: {other}"))),
        }
    }
}

impl std::fmt::Display for IssueStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Issue {
    pub id: String, pub status: IssueStatus, pub source: String,
    pub created: String, pub title: String, pub body: String,
}

#[derive(Debug, thiserror::Error)]
pub enum IssueError {
    #[error("io: {0}")] Io(#[from] std::io::Error),
    #[error("parse: {0}")] Parse(String),
    #[error("invalid transition: {from} -> {to}")]
    InvalidTransition { from: IssueStatus, to: IssueStatus },
    #[error("issue not found: {0}")] NotFound(String),
}
```

`IssueStore` 实现要点：`create` 扫描现有 `I-*.md` 取最大 NNN +1；文件名 `{id}-{slug}.md`，slug = title 转小写、非字母数字折叠为 `-`、截断 40 字符；写入顺序 id/status/source/created/title（RFC3339，用 `chrono::Utc::now().to_rfc3339()`）。`set_status` 读文件 → 校验转换合法性 → 只替换 status 行 → 原子写（先写 `.tmp` 再 `rename`）。

- [ ] **Step 5: 跑测试确认通过**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/llm-harness-coding-team
git commit -m "feat(coding-team): crate skeleton + markdown issue store with human-only state machine"
```

---

### Task 3: roles.rs — 10 角色预设

**Files:**
- Create: `crates/llm-harness-coding-team/src/roles.rs`
- Modify: `crates/llm-harness-coding-team/src/lib.rs`（`pub mod roles;`）
- Test: `crates/llm-harness-coding-team/src/roles.rs` 内置 `#[cfg(test)]`

**Interfaces:**
- Consumes: `llm_harness_agent_team::IndividualSpec`、`HarnessBuilder`（经 `spec.configure(|b| b.system_prompt(...))` 与 `spec.configure(|b| b.install(&FsToolsPlugin::new(None)))`）
- Produces: `pub struct RoleSet { pub specs: Vec<IndividualSpec> }` 与构造器 `RoleSet::standard(model_tier: &ModelTier) -> Self`；`pub struct ModelTier { pub strong: String, pub main: String, pub cheap: String }`（字段为模型 id 字符串，由 bin/配置层填）；`pub fn role_ids() -> [&'static str; 10]` 常量清单

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn tier() -> ModelTier {
        ModelTier { strong: "strong-model".into(), main: "main-model".into(), cheap: "cheap-model".into() }
    }

    #[test]
    fn standard_yields_ten_members_with_fixed_ids() {
        let specs = RoleSet::standard(&tier()).specs;
        assert_eq!(specs.len(), 10);
        let ids: Vec<&str> = specs.iter().map(|s| s.id().as_str()).collect();
        assert_eq!(ids, role_ids());
    }

    #[test]
    fn reviewer_specs_use_cheap_model_and_readonly_prompt() {
        let specs = RoleSet::standard(&tier()).specs;
        let r = specs.iter().find(|s| s.id().as_str() == "reviewer-performance").unwrap();
        assert_eq!(r.model(), "cheap-model");
    }

    #[test]
    fn coder_prompt_contains_tdd_discipline() {
        let specs = RoleSet::standard(&tier()).specs;
        // coder 的 system prompt 通过 builder 不可直接读；改为把 prompt 生成
        // 抽成 pub(crate) fn prompts::coder() -> String 并对字符串断言
        let p = crate::roles::prompts::coder();
        assert!(p.contains("TDD") || p.contains("失败测试"));
        assert!(p.contains("占位符") || p.contains("placeholder"));
    }

    #[test]
    fn planner_prompt_contains_writing_plans_discipline() {
        let p = crate::roles::prompts::planner();
        assert!(p.contains("delegate_task"));
        assert!(p.contains("Files") && p.contains("Interfaces"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team roles`
Expected: FAIL — `roles` 模块不存在

- [ ] **Step 3: 实现 roles.rs**

结构：`pub mod prompts { pub fn brainstormer() -> String { ... } ... }`（10 个函数，返回完整 system prompt 字符串），`RoleSet::standard` 对每个角色：

```rust
fn spec(id: &str, expertise: &str, model: &str, prompt: String) -> IndividualSpec {
    IndividualSpec::new(id, expertise, model)
        .configure(move |b| {
            b.system_prompt(Some(prompt))
                .install(&llm_harness_tools::FsToolsPlugin::new(None))
        })
}
```

prompt 内容要求（每条都写实文本，禁止"参考 skill"这类外部引用——agent 运行时读不到用户机器的 skill 文件）：
- `planner()`：完整嵌入 writing-plans 方法论要点——任务=独立可测试交付单元；每任务给 Files(精确路径)/Interfaces(精确签名)/TDD 步骤（写失败测试→跑→最小实现→跑→commit）；spec 覆盖自检；无占位符；派发用 `delegate_task`（内容自包含）；计划存档到项目仓库 `docs/superpowers/plans/YYYY-MM-DD-<feature>.md`
- `coder()`：严格按任务步骤 TDD 执行；每步跑验证；交付无占位符；完成后 `send_message` 回 planner 结果；只用项目仓库内操作，工作分支 `team/<project-id>`
- 5 个 reviewer()：各自维度定义（性能/健壮性/工程规范/契约与信任链/需求符合度）+ P0/P1/P2 分级标准（从 workflow-code-review 的 Severity 节复制原文）+ 只报本维度 + findings `send_message` 给 judge
- `judge()`：收 finding → 去重 → 对抗验证 → 独立裁决（keep/drop/降级，裁决理由必须引用具体代码位置）→ P0/P1 keep 则 NEEDS_CHANGES 退回 coder，否则 PASS 并 `send_message("operator")` 报告
- `brainstormer()`：brainstorming 方法论——spike/bounded/architectural 分类、一次一个问题、2-3 方案带权衡、分节设计逐节确认；产物 spec 写项目仓库 `docs/design/YYYY-MM-DD-<topic>-design.md`；spec 获人确认后 `send_message("planner", ...)` 交接
- `debugger()`：根因定位（最小复现→假设→验证→修复建议），不直接改业务代码除非被明确委派
- `scout()`：巡检清单（跑测试/TODO 扫描/最近 commit/warning）+ 输出格式（issue 标题+现象+建议）+ 写 issue 用 write 工具到 issues 目录 + 查重（先读现有 issue）+ 完成后必须 `set_reminder` 预约下次巡检（间隔由任务消息携带）

- [ ] **Step 4: 跑测试确认通过**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-coding-team/src
git commit -m "feat(coding-team): 10 resident role presets with embedded methodology prompts"
```

---

### Task 4: project.rs — ProjectManager

**Files:**
- Create: `crates/llm-harness-coding-team/src/project.rs`
- Modify: `crates/llm-harness-coding-team/src/lib.rs`
- Test: `crates/llm-harness-coding-team/src/project.rs` 内置 `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 2 `IssueStore`、Task 3 `RoleSet`；`AgentTeam::with_config(TeamConfig)` / `member` / `build(env, providers)`；`llm_harness_sandbox::OsEnv::new(working_dir)`；`llm_harness_loop::test_utils::MockLlmClient`（仅测试）
- Produces:
  - `pub struct Project { pub id: String, pub repo_path: PathBuf, pub team: AgentTeam, pub issues: IssueStore }`
  - `pub struct ProjectManager { ... }`：
    - `pub fn new(data_root: impl Into<PathBuf>) -> Self`（data_root 默认 `~/.harness/coding-team`，由 bin 决定）
    - `pub async fn register(&mut self, id: &str, repo_path: PathBuf, providers: Vec<(String, Arc<dyn LlmClient>)>, tier: &ModelTier, scout_interval_secs: u64) -> Result<&Project, ProjectError>`
    - `pub fn get(&self, id: &str) -> Option<&Project>` / `pub fn ids(&self) -> Vec<String>`
    - `pub async fn shutdown(&mut self, id: &str) -> Result<(), ProjectError>`
    - `pub async fn shutdown_all(&mut self)`

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_loop::test_utils::MockLlmClient;
    use std::sync::Arc;

    fn repo() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ct-repo-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    fn providers() -> Vec<(String, Arc<dyn llm_harness_loop::LlmClient>)> {
        vec![("m".into(), Arc::new(MockLlmClient::new(vec![])) as Arc<dyn llm_harness_loop::LlmClient>)]
    }

    fn tier() -> ModelTier { ModelTier { strong: "m".into(), main: "m".into(), cheap: "m".into() } }

    #[tokio::test]
    async fn register_builds_team_with_isolated_roots() {
        let root = std::env::temp_dir().join(format!("ct-pm-{}", std::process::id()));
        let mut pm = ProjectManager::new(&root);
        pm.register("proj-a", repo(), providers(), &tier(), 0).await.unwrap();
        pm.register("proj-b", repo(), providers(), &tier(), 0).await.unwrap();
        // 两项目 memory_root / issues / 会话目录互不相同
        let a = pm.get("proj-a").unwrap();
        let b = pm.get("proj-b").unwrap();
        assert_ne!(a.team_memory_root(), b.team_memory_root());
        assert_ne!(a.issues.root(), b.issues.root());
        assert_eq!(a.issues.list().unwrap().len(), 0);
        pm.shutdown_all().await;
    }

    #[tokio::test]
    async fn register_rejects_missing_repo() {
        let mut pm = ProjectManager::new(std::env::temp_dir().join("ct-pm-neg"));
        let err = pm.register("x", std::env::temp_dir().join("no-such-repo"), providers(), &tier(), 0).await.unwrap_err();
        assert!(matches!(err, ProjectError::NotAGitRepo(_)));
    }
}
```

> `team_memory_root()`/`issues.root()` 是为断言暴露的只读 accessor（`pub fn`），实现时加上。

- [ ] **Step 2: 跑测试确认失败**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team project`
Expected: FAIL

- [ ] **Step 3: 实现 project.rs**

`register` 内部：

```rust
// 校验
if !repo_path.join(".git").exists() { return Err(ProjectError::NotAGitRepo(repo_path)); }
// 目录布局（全在 data_root/projects/<id>/ 下，天然按项目隔离）
//   issues/         ← IssueStore
//   memory/         ← TeamConfig.memory_root
//   sessions/       ← TeamConfig.session_base_dir
let issues = IssueStore::new(dir.join("issues"));
let mut config = TeamConfig::default();
config.memory_root = dir.join("memory");
config.session_base_dir = dir.join("sessions");
let mut team = AgentTeam::with_config(config);
for spec in RoleSet::standard(tier).specs { team = team.member(spec); }
let env: Arc<dyn ExecutionEnv> = Arc::new(OsEnv::new(repo_path.clone()));
team.build(env, providers).await?;
// scout_interval_secs > 0 时由 Task 5 的 scout 模块投递首个巡检任务（此处留 hook）
```

`ProjectError`：`NotAGitRepo(PathBuf)`、`UnknownProject(String)`、`Team(#[from] TeamError)`、`Io(#[from] std::io::Error)`。

- [ ] **Step 4: 跑测试确认通过**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-coding-team/src
git commit -m "feat(coding-team): ProjectManager — per-project team with isolated memory/issues/env"
```

---

### Task 5: scout 模块 — 首投与自续约装配

**Files:**
- Create: `crates/llm-harness-coding-team/src/scout.rs`
- Modify: `crates/llm-harness-coding-team/src/project.rs`（register 里调 scout 装配）
- Test: `crates/llm-harness-coding-team/src/scout.rs` 内置 `#[cfg(test)]`

**Interfaces:**
- Consumes: `SocialContext::push_message(InboxMessage)`（投递巡检任务）、`InboxMessage`（`MessageType` 取现有变体，优先 Task；scout 的 agent id = "scout"）；scout 的 system prompt 已含自续约指令（Task 3）
- Produces: `pub fn arm_scout(ctx: &SocialContext, interval_secs: u64, issues_dir: &Path) -> String`——组装巡检任务文本（含 issues 目录绝对路径、间隔、pending 上限 50）并 `push_message`；返回任务文本（供测试断言）。`Project::register` 在 `team.build` 后经 `team.social_context().unwrap()` 调用它（interval=0 时跳过）

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_text_contains_contract() {
        let text = scout_task_text("/abs/issues", 3600, 50);
        assert!(text.contains("/abs/issues"));
        assert!(text.contains("3600"));
        assert!(text.contains("50"));
        assert!(text.contains("set_reminder"));
    }

    #[tokio::test]
    async fn arm_scout_pushes_inbox_message() {
        // 构建一个只含 scout 的最小团队（MockLlmClient），build 后
        // arm_scout(team.social_context().unwrap(), 3600, &issues_dir)
        // 断言 scout inbox 非空（drain_inbox 后消息体包含任务文本关键词）
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team scout`
Expected: FAIL

- [ ] **Step 3: 实现 scout.rs**

```rust
pub fn scout_task_text(issues_dir: &str, interval_secs: u64, pending_cap: usize) -> String {
    format!(
        "执行仓库巡检。\n\
         1. 在项目仓库运行测试、扫描 TODO/FIXME、检查最近 commit 与 warning 积累。\n\
         2. 发现问题写候选 issue 到 {issues_dir}（write 工具，文件名 I-NNN-<slug>.md，\n\
            frontmatter 含 id/status: pending/source: scout/created/title）。\n\
         3. 写之前先读该目录现有 issue 查重；pending 数量达到 {pending_cap} 时本次不写，\n\
            改为发消息提醒 operator 清理。\n\
         4. 完成后必须调用 set_reminder 预约 {interval_secs} 秒后的下一次巡检。\n\
         你只提报，不改代码；issue 状态只有人能改。"
    )
}
```

`arm_scout` 用 `InboxMessage` 现有构造路径（执行者读 `inbox.rs` 里 `InboxMessage::new`/`MessageType` 的真实签名后填入；`push_message` 直接入队）。

- [ ] **Step 4: 跑测试确认通过 + 全量回归**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team && CARGO_HTTP_PROXY="" cargo test -p llm-harness-agent-team`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-coding-team/src
git commit -m "feat(coding-team): scout arming — first patrol task + self-renewal contract"
```

---

### Task 6: 领域路由 routes.rs + ProjectManager 上的 chat/issues API

**Files:**
- Create: `crates/llm-harness-coding-team/src/routes.rs`
- Modify: `crates/llm-harness-coding-team/src/lib.rs`
- Modify: `crates/llm-harness-coding-team/src/project.rs`（`ProjectManager` 加 `Arc` 共享包装，供 axum state 使用：`pub struct SharedPm(Arc<RwLock<ProjectManager>>)` 或等价——执行者按 routes 的实际并发需求定，推荐 `Arc<parking_lot::RwLock<ProjectManager>>`）
- Test: `crates/llm-harness-coding-team/src/routes.rs` 内置 `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 2/4 的 `IssueStore::list/set_status`、`ProjectManager::register/get/ids`；`AgentTeam::submit(target, text)`；axum `Router`
- Produces: `pub fn team_router(pm: Arc<parking_lot::RwLock<ProjectManager>>) -> axum::Router`，端点：
  - `GET /api/team/projects` → `{"projects":[{"id":...,"repo_path":...}]}`（各团队 agent 状态汇总放 P2，本任务只出基础字段）
  - `POST /api/team/projects` body `{"id":"...","repo_path":"..."}` → 201；repo 不存在 → 400 带错误文本
  - `POST /api/team/chat` body `{"project":"...","target":"planner"|"brainstormer","text":"..."}` → 202；`team.submit` 失败 → 404/409
  - `GET /api/team/issues?project=...` → `{"issues":[{id,status,title,created}...]}`
  - `POST /api/team/issue/:id/confirm|reject` → 200；非法转换 → 409
  - `WS /api/team/events`：本任务先返回 501 占位说明（事件流在 Task 8 实装）——**这不是 TODO 蒙混**，Task 8 有对应实现任务；本任务交付时该路由必须存在且返回明确的 501 文本

- [ ] **Step 1: 写失败测试**

用 `axum::Router` + `tower::ServiceExt::oneshot`（参考 `llm-harness-inspector/tests/integration.rs` 的既有写法）：

```rust
#[tokio::test]
async fn confirm_issue_submits_to_planner() {
    // 1. ProjectManager + 注册 mock 项目（Task 4 的测试 helper 复用）
    // 2. IssueStore::create 一条 pending issue
    // 3. POST /api/team/issue/I-001/confirm
    // 4. 断言 200 且 issue 文件 status=confirmed
    //    （planner 收到 submit 的验证：mock 项目里 planner harness 的收件箱
    //     经 social_context drain 断言——执行者用 ProjectManager 暴露的
    //     social_context accessor 或 team 引用拿到）
}

#[tokio::test]
async fn chat_routes_by_explicit_target() {
    // POST /api/team/chat {"target":"planner"} → 202，且 planner inbox 收到
    // POST /api/team/chat {"target":"brainstormer"} → 202，且 brainstormer inbox 收到
}

#[tokio::test]
async fn register_via_http_validates_repo() {
    // POST /api/team/projects 带不存在路径 → 400
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team routes`
Expected: FAIL

- [ ] **Step 3: 实现 routes.rs**

handler 直接持有 `Arc<RwLock<ProjectManager>>`；`submit` 是 `&self`（team.rs:398 签名已确认），读锁内可调。WS 路由：`get(ws_placeholder_handler)` 返回 501。

- [ ] **Step 4: 跑测试确认通过**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-coding-team/src
git commit -m "feat(coding-team): domain routes — projects/chat/issues/confirm-reject over extra_routes"
```

---

### Task 7: 薄 bin + 配置文件 + UI 静态页挂载

**Files:**
- Create: `crates/llm-harness-coding-team/src/bin/coding-team.rs`
- Create: `crates/llm-harness-coding-team/static/team.html`
- Create: `crates/llm-harness-coding-team/src/config.rs`（配置解析）
- Modify: `crates/llm-harness-coding-team/src/lib.rs`（`pub mod config;`）
- Modify: `crates/llm-harness-coding-team/Cargo.toml`（bin section + `serde` derive 如缺）
- Test: `crates/llm-harness-coding-team/src/config.rs` 内置测试

**Interfaces:**
- Consumes: Task 1 `Inspector::builder(...).extra_routes(...).extra_static("app", dir)`；Task 4/6 全部产物；`HarnessRegistry`（`Inspector::registry(registry)` 多 agent 路径，参考 bench `mech/mod.rs:166` 的用法：把每个项目团队的成员 harness 逐个 `registry.register(agent_id, harness)`）
- Produces: 可执行 `cargo run -p llm-harness-coding-team --bin coding-team`；配置文件 `~/.harness/coding-team.toml`：

```toml
[data_root]                     # 可省，默认 ~/.harness/coding-team
port = 8080
[models]                        # ModelTier 的模型 id 映射
strong = "..."
main  = "..."
cheap = "..."
scout_interval_secs = 3600
[[project]]
id = "my-repo"
repo_path = "/path/to/repo"
coders = 1                      # 预留（spec §4.1），本期固定 1，解析但忽略多余值
```

`pub struct Config { pub data_root: Option<PathBuf>, pub port: u16, pub models: ModelTier, pub scout_interval_secs: u64, pub projects: Vec<ProjectCfg> }`；`Config::load(path) -> Result<Config, ConfigError>`（手写 toml 解析或引入 `toml` crate——优先 workspace 已有依赖，没有则新增 `toml.workspace = true`）。

- [ ] **Step 1: 写失败测试**（config.rs）

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_full_config() {
        let text = r#"
port = 9999
scout_interval_secs = 60
[models]
strong = "s"
main = "m"
cheap = "c"
[[project]]
id = "a"
repo_path = "/tmp/a"
"#;
        let cfg = Config::parse(text).unwrap();
        assert_eq!(cfg.port, 9999);
        assert_eq!(cfg.models.strong, "s");
        assert_eq!(cfg.projects.len(), 1);
        assert_eq!(cfg.projects[0].id, "a");
    }

    #[test]
    fn defaults_apply() {
        let cfg = Config::parse("").unwrap();
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.scout_interval_secs, 3600);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team config`
Expected: FAIL

- [ ] **Step 3: 实现 config.rs + bin**

bin 流程（`#[tokio::main]`）：

```rust
// 1. 读 ~/.harness/coding-team.toml（--config 参数可覆盖）
// 2. 构造 providers：从环境变量读 API key，按 models.* 建 LlmClient
//    （llm-harness-live-tests/src/lib.rs:93-105 有现成的 OpenAIProvider 构造
//     惯例：base_url/chat_path 处理，抄该模式；provider 名 = 模型 id）
// 3. ProjectManager::register 每个项目（team.build + arm_scout）
// 4. HarnessRegistry::new("default")，每个项目每个成员 harness 注册
//    （key 用 "<project-id>/<agent-id>" 避免跨项目撞名）
// 5. Inspector::registry(registry).extra_routes(team_router(pm)).extra_static("app", static_dir).port(cfg.port).mount()
// 6. 收 operator 流（每项目 team.receive() spawn 一个转发任务，打印 + 投给 WS 事件用），
//    Ctrl+C 优雅退出（shutdown_all）
```

`team.html` 本任务只交付**最小可用骨架**（能列项目、发一条 chat、列 issue——三个 fetch 各一段），完整三区交互在 Task 8 打磨。static 目录路径在 bin 内用 `env!("CARGO_MANIFEST_DIR")/static`（dev）或运行时参数。

- [ ] **Step 4: 跑测试 + 冒烟**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team && cargo build -p llm-harness-coding-team --bins`
Expected: PASS + bin 编译成功
冒烟（不调真实 LLM）：`cargo run -p llm-harness-coding-team --bin coding-team -- --config <写一个指向本仓库路径、models 指向假模型 id 的临时 toml>`，确认启动日志打印 inspector URL、`curl -s localhost:8080/api/team/projects` 返回 JSON、`curl localhost:8080/app/team.html` 返回 HTML。冒烟完 Ctrl+C。

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-coding-team
git commit -m "feat(coding-team): coding-team bin — config, provider setup, inspector mount, minimal panel"
```

---

### Task 8: team.html 完整操作面板 + WS 事件流

**Files:**
- Modify: `crates/llm-harness-coding-team/static/team.html`
- Modify: `crates/llm-harness-coding-team/src/routes.rs`（WS 实装替换 501）
- Modify: `crates/llm-harness-coding-team/src/lib.rs`（如需事件总线模块：`pub mod events;`）
- Test: `crates/llm-harness-coding-team/src/routes.rs` WS 单元测试 + 集成验证

**Interfaces:**
- Consumes: Task 6 的全部 REST 端点；`SharedEvents` 模式（inspector `state.rs:105`——`publish/subscribe` broadcast channel）作为 coding-team 自己的事件总线的参考实现；operator 流（`team.receive()`）
- Produces: `WS /api/team/events` 消息格式 `{"type":"operator_msg"|"issue_changed"|"task_status","project":"...","from":"...","text":"...","ts":...}`；前端三区完整交互

- [ ] **Step 1: 写失败测试**（routes.rs 内）

```rust
#[tokio::test]
async fn ws_streams_operator_messages() {
    // 建立 WS 连接（tokio-tungstenite 或 axum 的 oneshot 升级测试模式——
    // 执行者参考 llm-harness-inspector/src/routes/events.rs 现有 ws_handler
    // 的测试方式；若该处无测试先例，用真实 TCP 连接）
    // 然后 team.submit("planner", "hi")，断言收到一条 type=operator_msg 帧
    // （planner mock 回复经 operator 通道回流）
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team ws`
Expected: FAIL（501）

- [ ] **Step 3: 实装 WS + 前端**

后端：`events.rs` 模块——`EventBus`（`tokio::sync::broadcast`）；`ProjectManager` 暴露 `publish_operator(project, from, text)`；bin 里每个项目的 `receive()` 循环把 operator 消息 publish 上去；issue confirm/reject handler 里同步 publish `issue_changed`。WS handler：订阅 + 回放最近 50 条（`RwLock<VecDeque>` 环形缓冲，对齐 inspector `SharedEvents::replay` 模式）。

前端 team.html（纯 JS，无依赖，风格沿用 inspector index.html 的内联 CSS 惯例）：
1. 项目侧栏：`GET /api/team/projects` 渲染列表 + 注册表单；点击切换高亮，所有后续请求带 `?project=`
2. 对话区：模式开关 Chat/Design（radio）；输入框 `POST /api/team/chat`；WS `operator_msg` 帧追加聊天流（带 from/时间）
3. Issue 区：`GET /api/team/issues` 按 status 分组渲染；pending 项「确认/驳回」按钮 → POST confirm/reject → 成功后本行状态翻转 + WS 事件兜底刷新
4. 顶部导航链接回 `/`（agent 观测），观测端 index.html 侧加一个指向 `/app/team.html` 的链接（Modify: `crates/llm-harness-inspector/static/index.html`，一行 `<a>`，这是平台层允许的最小改动——ADR 允许 UI 导航互链）

- [ ] **Step 4: 跑测试确认通过 + 冒烟**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team && CARGO_HTTP_PROXY="" cargo test -p llm-harness-inspector`
Expected: PASS
冒烟：Task 7 的假模型配置重启 bin，浏览器开 inspector URL → /app/team.html：注册一个真实目录项目、发一条 chat（会因假模型失败——确认错误以消息形式出现在聊天流而非页面崩溃）、建 issue 手动写入文件后确认按钮可用。页面不白屏、WS 重连正常即算过。

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-coding-team crates/llm-harness-inspector/static/index.html
git commit -m "feat(coding-team): full operation panel — project sidebar, chat, issue triage, WS event stream"
```

---

### Task 9: 端到端集成测试 + 文档

**Files:**
- Create: `crates/llm-harness-coding-team/tests/integration.rs`
- Modify: `docs/agent-team-guide.md`（加 coding-team 一节，5-10 行：定位、bin 启动、面板地址）
- Test: 上述集成测试

**Interfaces:**
- Consumes: Task 2-8 全部公开 API；`MockLlmClient` 预录响应（`MockResponse::tool_use(...)` 构造 delegate/send_message 工具调用，参考 `llm-harness-workflow/tests.rs:3164` 的预录模式）
- Produces: 端到端回归保障

- [ ] **Step 1: 写集成测试**

```rust
//! 端到端：mock LLM 驱动的完整链路。
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse};

// T1: chat→planner 链路：POST /api/team/chat target=planner，
//     预录 planner 回复一条 delegate_task 工具调用给 coder + 一条 send_message
//     给 operator；断言 operator 流收到消息、coder inbox 收到 delegate。
//
// T2: issue 确认链路：create pending issue → HTTP confirm → planner inbox
//     收到含 issue 全文的 submit → 状态文件已翻转。
//
// T3: scout 链路：arm_scout → 预录 scout 回复 write 工具调用写 issue 文件 +
//     set_reminder 调用 → 断言 issue 文件存在且 pending、reminder 注册成功。
//
// T4: 隔离性：两个项目同时注册，A 项目的 chat 消息不出现在 B 的任何 inbox，
//     A 的 issue 不出现在 B 的列表。
//
// T5: review 闭环（简化）：预录 coder→planner→reviewer chain：一条
//     broadcast + reviewer 回 send_message 给 judge + judge 回 operator
//     "PASS"；断言 operator 流出现 PASS 报告。
```

每个测试独立 temp dir；MockLlmClient 按角色模型 id 分别实例化（providers vec 里 "strong/main/cheap" 各一个 client，便于按角色预录不同响应序列）。

- [ ] **Step 2: 跑测试确认通过**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-coding-team --test integration`
Expected: PASS

- [ ] **Step 3: 全 workspace 回归**

Run: `CARGO_HTTP_PROXY="" cargo test --workspace --exclude llm-harness-live-tests`
Expected: 全 PASS，无回归

- [ ] **Step 4: Commit**

```bash
git add crates/llm-harness-coding-team/tests docs/agent-team-guide.md
git commit -m "test(coding-team): end-to-end integration coverage + guide section"
```

---

## Self-Review 结论（写计划后自检）

1. **Spec 覆盖**：§2 决策表→Global Constraints；§3 生命周期→Task 4/7；§4 角色→Task 3，流程→Task 3 prompt + Task 9 T1/T5；§5 scout/issue→Task 2/5；§6 前端/API→Task 1/6/7/8；§7 布局→Task 2-8 文件清单；§8 测试→Task 2-6 单元 + Task 9 集成。无缺口。
2. **占位符**：Task 8 WS 在 Task 6 为显式 501 + Task 8 实装（非蒙混）；Task 1 测试骨架中的 token 获取注明"复用文件内现有写法"并给出搜索指引——这是对执行者的显式指令而非占位符。`/* ... */` 注释仅出现在标注"必须替换"的骨架里。
3. **类型一致性**：`IssueStore::set_status(IssueStatus)` 在 Task 2 定义、Task 6 消费一致；`team_router(pm: Arc<RwLock<ProjectManager>>)` Task 6 定义、Task 7 消费一致；`arm_scout` Task 5 定义、Task 7 bin 消费一致；`ModelTier` Task 3 定义、Task 4/7 消费一致。
