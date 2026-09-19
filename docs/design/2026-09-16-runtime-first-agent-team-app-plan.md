# Runtime-first AgentTeam 应用计划

> 日期：2026-09-16
> 状态：已接受，实施中（M1/M2 已合并；M3 React 工作区已迁入 runtime app 并完成浏览器模式验证）
> 范围：`llm-harness-runtime` 的应用化演进，以及 AgentTeam 在 runtime 应用中的集成方式

## 1. 结论

方向成立：`llm-harness-runtime` 应该拥有一个 first-party 本地应用入口，AgentTeam 应该是这个应用的内置能力，而不是由 `senza-studio` 单独承载的外部产品。

但当前状态还不是完整的 runtime 应用。现有 `llm-harness-agent-team-studio` 已经具备后端、UI、认证、持久化、诊断和宿主监督的雏形，足够作为演进基础；缺口主要在应用边界、通用入口、UI 迁移、跨平台打包和数据迁移。因此不应推倒重写，而应采用增量迁移。

本计划的目标形态是：

1. `llm-harness-runtime` 提供 first-party 应用二进制，默认启动本地服务与应用界面。
2. AgentTeam 成为 runtime 应用中的一个 feature module，而不是独立 sidecar。
3. `senza-studio` 不再拥有 AgentTeam 的核心能力；它可以作为兼容消费者或通用 shell 存在。
4. UI 复用已验证的 React 工作区，但由 runtime 应用直接托管。
5. 桌面形态优先使用 Rust + 系统 WebView 的方案，避免把 runtime 应用变成 Electron/Node 应用。

## 2. 目标与非目标

### 目标

- 建立稳定的 runtime 应用入口和生命周期。
- 将 AgentTeam 的团队声明、模板、运行、恢复、诊断和授权能力纳入 runtime 应用。
- 保持现有 `agent-studio` 与 `/api/team/*` 的兼容路径，避免一次性破坏。
- 复用 `senza-studio` 已验证的 UI 与打包经验，但把所有权迁回 runtime。
- 支持 Windows 作为第一优先桌面平台，Linux 保持开发与 CI 平台。
- 支持本地 LLM，默认推荐 `GLM-5.3-Flash`，但保留模型配置能力。
- 达到生产级验收标准：可安装、可启动、可恢复、可诊断、可安全退出、可回滚。

### 非目标

- 不把 runtime 做成通用 UI 框架。
- 不在本阶段实现云端多用户 SaaS。
- 不重写 AgentTeam 的协作内核。
- 不要求 `senza-studio` 立即删除；它可以在过渡期继续作为外部消费者。
- 不承诺 macOS 立即发布；macOS 只保留架构兼容，后续单独排期。

## 3. 当前事实

### 3.1 已具备的能力

`origin/main` 当前已经包含：

- `crates/llm-harness-agent-team`：去中心化 AgentTeam 内核，包括成员、社交工具、团队记忆、反思循环等基础能力。
- `crates/llm-harness-agent-team-studio`：
  - `TeamSpec` / `MemberSpec` / `Behavior` 声明式团队模型。
  - 内置 `coding-team` 模板，角色不再写死在引擎中。
  - 团队模板、实例、升级检查与覆写。
  - Issue 工作流、Patrol 与 Handoff 行为。
  - Loopback HTTP 服务与本地 API。
  - Per-process bearer token 认证。
  - Panel descriptor 生命周期。
  - 数据根目录锁、团队恢复、降级启动报告。
  - Workspace grants 与私密秘密文件写入。
  - 生命周期事件、诊断事件与 panic reporter。
  - `StudioHost` 后端进程监督，覆盖启动、就绪、退出和优雅关闭。
  - 内嵌 `static/team.html` 面板。
- `scripts/studio.sh`：本地开发启动入口。
- `docs/agent-team-guide.md` 与 `docs/design/2026-08-30-agent-team-studio-design.md`：现有设计与使用说明。
- GLM AgentTeam E2E 已通过 PR #195 合入 `main`。

`senza-studio` 侧已经验证：

- React 版 AgentTeam 工作区。
- 与 runtime 的 API contract 测试。
- 桌面宿主、认证、打包与 E2E 流程。
- PR #13 已合并，说明 UI 与宿主集成路径可复用。

### 3.2 当前缺口

| 缺口 | 影响 | 结论 |
| --- | --- | --- |
| 入口仍是 `agent-studio`，不是通用 runtime 应用 | 产品心智和二进制边界不清晰 | 需要新增 first-party app entrypoint |
| 应用服务与 AgentTeam Studio 代码耦合在一个 crate | 后续难以接入其他 runtime feature | 需要拆出通用 app shell |
| UI 是内嵌静态 `team.html` | 可用但不足以承载生产级工作区 | 需要迁移 React 工作区 |
| `StudioHost` 使用 Unix 进程扩展 | Windows 桌面宿主路径不完整 | 需要跨平台生命周期抽象 |
| runtime 仓库没有桌面打包流水线 | 无法独立交付应用 | 需要引入打包与签名策略 |
| 数据根目录仍是 `agent-team-studio` 语义 | 迁移为 runtime 应用后需要兼容 | 需要显式 schema 与迁移 |
| API 路径以 `/api/team/*` 为主 | AgentTeam feature 与 app API 混在一起 | 需要分层命名但保留兼容 |
| 缺少统一应用级 diagnostics summary | 用户和运维难以判断整体健康度 | 需要聚合健康、事件与恢复报告 |
| 缺少应用级 restart/crash-loop 策略 | 桌面体验不完整 | 需要由 shell/launcher 承担，避免后端自旋重启 |

## 4. 目标架构

### 4.1 分层

```text
llm-harness-app
├── Application shell / launcher
│   ├── process lifecycle
│   ├── readiness / health
│   ├── graceful shutdown
│   ├── restart policy
│   └── desktop window or browser mode
├── Runtime application server
│   ├── loopback HTTP server
│   ├── local token auth
│   ├── app API
│   ├── diagnostics
│   ├── data root / migration
│   └── workspace grants
├── AgentTeam feature module
│   ├── team spec / templates
│   ├── team lifecycle
│   ├── issue / patrol / handoff
│   ├── member sessions
│   └── runtime settings
└── First-party web UI
    ├── React workspace
    ├── static assets embedded in binary
    └── no direct filesystem or secret access
```

### 4.2 Crate 演进

建议新增：

- `crates/llm-harness-runtime-app`
  - 负责 first-party 应用入口、生命周期、HTTP host、认证、数据根、诊断与通用 app API。
  - 不包含 AgentTeam 业务语义。

保留并逐步收敛：

- `crates/llm-harness-agent-team-studio`
  - 短期继续提供现有 AgentTeam feature 实现与兼容 API。
  - 中期改名为或拆出 `llm-harness-agent-team-app`，只保留 AgentTeam feature。
  - 最终避免“Studio”命名承载 runtime 应用职责。

保留：

- `crates/llm-harness-agent-team`
  - 继续作为纯协作内核，不依赖 HTTP、UI 或应用生命周期。

这样做的原因是：

1. 先建立应用边界，再做大规模改名，降低回归风险。
2. AgentTeam 可以成为第一个 app feature，但不阻塞未来接入 workflow、inspector、tools 或其他 runtime 能力。
3. 现有测试和兼容路径可以继续工作。

### 4.3 应用入口

新增二进制：

```text
llm-harness-app
```

行为：

1. 初始化数据根目录与 singleton lock。
2. 加载应用配置与状态 schema。
3. 启动 loopback HTTP server。
4. 注册 AgentTeam feature routes。
5. 写入 panel descriptor。
6. 输出 ready 信号。
7. 等待退出信号。
8. 停止后台任务、刷新状态、清理 descriptor，并优雅退出。

兼容策略：

- `agent-studio` 保留为 alias，至少两个 minor release。
- 旧数据根目录和旧 API 路径继续支持。
- 新入口启用新路径前必须提供迁移与回滚测试。

### 4.4 API 分层

目标命名：

```text
GET  /api/app/health
GET  /api/app/diagnostics
GET  /api/app/settings
GET  /api/agent-team/templates
POST /api/agent-team/teams
GET  /api/agent-team/teams/{team_id}
```

兼容策略：

- 现有 `/api/team/*` 保留并标记 deprecated。
- 新 API 与旧 API 共享 handler，避免两套行为。
- API contract 测试必须同时覆盖新旧路径。
- Breaking change 必须通过版本化路径或明确 release note 处理。

### 4.5 UI 策略

短期：

- 保留 `static/team.html` 作为最小 fallback。

中期：

- 将 `senza-studio` 中已经验证的 React AgentTeam 工作区迁入 runtime 仓库。
- 前端构建产物嵌入 Rust binary。
- CI 校验 lockfile、构建可重复性、类型检查和单元测试。
- UI 只通过本地 API 访问能力，不直接读写文件或秘密。

长期：

- UI 变成 runtime first-party app 的 feature workspace。
- `senza-studio` 可以继续提供自己的 shell，但不再拥有 AgentTeam 专属 UI。

### 4.6 桌面 shell

推荐采用 Tauri 2 或等价的 Rust + 系统 WebView 方案：

- Windows 优先，基于 WebView2。
- Linux 使用 WebKitGTK，保留开发与 CI 支持。
- macOS 架构上可兼容 WKWebView，但不作为第一阶段验收目标。
- Shell 只负责窗口、launcher、更新和重启，不承载业务逻辑。

不推荐把 Electron 作为 runtime 应用的最终形态：

- 会把 runtime 应用绑定到 Node/Electron 运行时。
- 打包体积和升级链路更重。
- 与“runtime 自身成为应用”的目标不完全一致。

但 Electron 版 `senza-studio` 的 UI、E2E、认证与打包经验应被复用，而不是丢弃。

### 4.7 数据根目录与迁移

阶段一：

- 继续使用现有 `agent-team-studio` 数据根目录，避免无收益迁移。

阶段二：

- 引入 runtime 应用数据根目录，例如 `~/.harness/runtime-app`。
- 写入 `state/version` 与 `app.json`。
- 提供一次性迁移：
  - 备份旧目录。
  - 复制或移动团队、设置、grants、secrets、logs。
  - 校验团队数量、设置 hash、秘密文件权限。
  - 失败时回滚，不留下半迁移状态。

迁移必须满足：

- 不丢数据。
- 不重复创建团队。
- 不泄露 token。
- 迁移报告可导出。
- 支持从备份恢复。

### 4.8 认证与安全

保持并强化：

- 服务只绑定 `127.0.0.1`。
- `/api/*` 使用 per-process bearer token。
- Token 只写入 panel descriptor，不打日志。
- Panel URL 使用 fragment 传递 token，避免进入 server log 和 referrer。
- 秘密文件使用私有权限和原子替换。
- Workspace access 必须经过 grants。
- API 响应不返回 secret 明文。

新增要求：

- App readiness report 中包含认证初始化状态。
- 诊断事件不得包含 token、API key 或完整 panel URL。
- UI 不得把 token 写入 localStorage。
- API contract 测试覆盖未认证、错误 token、过期 descriptor 和重复启动。

### 4.9 生命周期与诊断

应用生命周期必须区分三种角色：

1. **Core process**：runtime app 自身，负责服务和业务状态。
2. **Launcher/shell**：桌面进程，负责窗口、用户交互和重启策略。
3. **External consumer**：`senza-studio` 或其他客户端，只通过 API 消费。

Core process 不应自己无限重启自己；crash-loop 控制放在 launcher 或操作系统服务层。Core process 负责提供：

- startup report
- readiness descriptor
- health endpoint
- shutdown state
- panic report
- background task registry
- degraded recovery report

Launcher 负责：

- 启动 core process
- 等待 ready
- 展示错误
- 按退避策略重启
- 用户主动退出时请求 graceful shutdown
- 超时后升级为强制退出

## 5. 实施里程碑

### M0：计划与契约冻结

交付：

- 本计划评审通过。
- 定义 `/api/app/*` 与 `/api/agent-team/*` contract。
- 明确 Windows 打包策略与最小发布平台。
- 建 issue 跟踪 UI 迁移、数据迁移、桌面 shell 三个主线。

验收：

- API contract 文档进入 runtime。
- 每个后续 PR 都能独立 review 和回滚。

### M1：Runtime application shell

交付：

- 新增 `llm-harness-runtime-app` crate。
- 新增 `llm-harness-app` binary。
- 抽出通用 startup、auth、health、diagnostics、shutdown。
- `agent-studio` 变成兼容 alias。
- 保留现有 Studio API。

验收：

- 新旧入口均能启动并生成 descriptor。
- Singleton lock、token 认证、优雅退出测试通过。
- 现有 AgentTeam integration tests 不回退。

### M2：AgentTeam feature module 化

交付：

- AgentTeam routes 挂载到 app server。
- 新增 `/api/agent-team/*` 路径。
- 旧 `/api/team/*` 继续工作。
- App health 汇总 AgentTeam 恢复状态。

验收：

- 新旧 API contract tests 通过。
- 团队创建、恢复、issue、handoff、patrol 行为保持一致。
- GLM-5.3-Flash live E2E 可选开启且不进入默认 CI。

2026-09-16 实施记录：`/api/agent-team/*` 与旧 `/api/team/*` 共享同一组
handler；`/api/app/health` 输出 `feature_health.agent-team` 与启动恢复摘要。

### M3：React 工作区迁入 runtime

交付：

- 将已验证的 React AgentTeam workspace 迁入 runtime 仓库。
- 前端 build 产物嵌入 app binary。
- 保留 `team.html` 作为 fallback。
- 建立 frontend lint、typecheck、unit test 与 API contract test。

2026-09-17 实施记录：

- React 工作区已迁入 `crates/llm-harness-runtime-app/frontend`。
- 前端使用 `/api/agent-team/*`，token 仅保存在 `sessionStorage`。
- 生产构建产物嵌入 Rust binary，由 `/app/agent-team` 提供服务。
- 静态服务拒绝路径穿越，并输出 CSP、`Cache-Control` 与 `nosniff` 等安全头。
- `llm-harness-app` 的 panel descriptor 指向 React 工作区；`/app/team.html`
  保留为 fallback。
- CI 覆盖 frontend lint、unit test、typecheck/build 与提交产物一致性校验。

验收：

- 无前端 secret 访问。
- UI 在 browser mode 与 desktop shell 中均能完成团队创建、配置、运行和诊断查看。
- 构建产物可重复生成。

当前边界：

- 已完成 browser mode 的服务与 API 集成验证。
- Desktop shell、数据迁移与安装包仍分别属于 M4/M5。
- 团队创建、配置、运行和诊断的完整 UI E2E 尚待 M5 前补齐。

### M4：跨平台生命周期与数据迁移

交付：

- Windows-compatible process lifecycle abstraction。
- 优雅退出与超时强制退出的跨平台实现。
- Runtime 应用数据根目录与迁移工具。
- 迁移报告和回滚机制。

验收：

- Linux 与 Windows 均通过 lifecycle tests。
- 迁移失败时不破坏原数据。
- 重启后团队与设置完整恢复。

2026-09-18 实施记录：

- 已加入进程级 lifecycle 配置，`RUNTIME_APP_SHUTDOWN_TIMEOUT_SECS` 支持
  1–3600 秒，默认 30 秒，非法值以退出码 `2` 拒绝。
- Linux 处理 `CtrlC` 与 `SIGTERM`；Windows 处理 `CtrlC`、`CtrlBreak`
  与 `CtrlClose`。
- 信号处理在应用启动前安装，并在启动过程中与 `RuntimeApp::start` 并发等待；
  启动期间收到信号会记录 `shutdown_signal` 并以退出码 `130` 中止启动。
- 优雅退出统一带超时；超时记录 `shutdown_timeout` 并以退出码 `124` 退出。
- Windows `CtrlClose` 在系统默认 5 秒强杀窗口内预留日志与退出余量，将有效超时上限压到 4 秒。
- 二进制集成测试覆盖 `SIGTERM` 后的 `shutdown_signal` 与 normal
  `shutdown` 事件，以及非法 shutdown timeout 配置。
- 同日完成数据根目录迁移控制面：
  - `llm-harness-app migrate-data --from SOURCE --to DESTINATION [--backup BACKUP]`
    先完整复制到私有 staging，写入 `migration-report.json` 与事务 journal，
    再将原目录原子改名为备份、staging 改名为目标目录。
  - `llm-harness-app rollback-data --from SOURCE --to DESTINATION --backup BACKUP`
    恢复原数据根，并把迁移后产生变更的目标目录保留到独立 sibling 目录，
    同时生成私有 `migration-rollback-report.json`。
  - journal 覆盖 Prepared、source renamed、destination renamed、completed、
    rolled back 等中断点；重复执行同一命令可恢复提交、补写回滚报告、
    清理 rolled-back 残留并移除 journal。
  - 迁移与回滚分别持有源/目标 `studio.lock`，拒绝操作正在运行的数据根；
    journal 与目标 sibling lock 防止同一路径并发迁移。
  - 复制保留普通文件、目录与 Unix 权限，报告和 journal 以 `0600` 写入；
    源、目标、备份路径嵌套与 symlink 均被拒绝，运行时 `studio.lock`
    不会被复制为目标数据。
  - 文件复制、报告/journal 写入和关键 rename 后执行 fsync/目录同步，
    降低进程中断与主机掉电后的元数据丢失风险。
  - 单元测试覆盖设置、团队、备份、回滚保留、运行锁、symlink、
    preparation 失败清理和多个 journal 恢复点；二进制集成测试覆盖
    migrate/rollback CLI。
- 2026-09-19：React 面板加入数据迁移入口，通过认证的
  `/api/app/data-migration` 读取当前数据根，生成 offline
  migrate/rollback 命令与中断恢复指引；面板不在线迁移运行中的数据根。

当前边界：

- Windows 信号路径、数据根目录 rename/锁行为与目录元数据落盘仍需真实
  Windows 环境验证；本仓库当前本地验证只覆盖 Linux。
- 迁移入口目前是 browser panel 的命令生成与恢复指引；桌面壳仍需在
  packaged app 中承担退出、执行迁移、重启与失败展示的编排。

### M5：桌面打包与发布

交付：

- Tauri 2 或等价 Rust shell。
- Windows 安装包。
- Linux 开发包或 CI artifact。
- 启动、更新、诊断、日志收集策略。

验收：

- Windows clean machine 安装后可启动。
- 应用可打开 UI、创建团队、授权 workspace、运行 AgentTeam。
- 异常退出后可重启并恢复。
- 卸载不残留用户数据，或提供显式数据保留说明。

### M6：生产级 hardening

交付：

- 安全审查。
- 性能基线。
- 长时间运行稳定性测试。
- 真实 workspace 的 coding-team 验收。
- 版本升级与回滚测试。

验收：

- `cargo fmt`、`cargo clippy --all-targets --all-features`、workspace tests 通过。
- Frontend tests、API contract tests、packaged E2E 通过。
- 安全测试覆盖 token、secrets、workspace grants 和日志脱敏。
- 文档包含安装、启动、诊断、备份、恢复和卸载。

## 6. 测试策略

### 单元与集成

- App startup / shutdown / singleton lock。
- Token auth 与 descriptor 生命周期。
- AgentTeam 团队恢复与降级启动。
- API 新旧路径兼容。
- Workspace grants 与秘密文件权限。

### API contract

- 使用稳定 schema 校验响应。
- 覆盖错误码、未认证请求、无效团队配置、workspace 未授权。
- 与 UI mock server 共享同一 contract fixture。

### UI / E2E

- 团队创建。
- 成员配置。
- 会话历史。
- Issue 确认与 handoff。
- Patrol 行为。
- 诊断面板。
- 异常恢复后的 UI 状态。

### 打包验收

- Linux CI 使用 Xvfb 或等价 headless 机制。
- Windows 在真实 Windows 机器上执行安装与 E2E。
- 验证 shortcut、数据目录、日志目录、卸载行为。

### 真实 LLM

- 默认 CI 使用 mock provider。
- 手动或 nightly 可选启用 `GLM-5.3-Flash`。
- Live test 必须验证本地模型配置、工具调用、团队消息和最终产出，不允许用固定响应替代。

## 7. 风险与应对

| 风险 | 应对 |
| --- | --- |
| 应用化范围失控 | 每个 milestone 只交付一个可回滚 PR，禁止一次性大重构 |
| UI 迁移引入回归 | 复用已验证 React workspace 与 E2E，先迁移不改交互 |
| Windows 生命周期差异 | 单独抽象 process lifecycle，不在 Unix 假设上堆条件分支 |
| 数据迁移丢状态 | 先备份、后迁移、失败回滚，并加入迁移 property tests |
| Token 或 secret 泄露 | 日志与诊断统一脱敏，E2E 断言输出不含 secret |
| AgentTeam 语义被 UI 绑架 | UI 只走 API，核心逻辑留在 Rust crate |
| 桌面打包依赖复杂 | 先 browser mode，再 Tauri shell，避免 blocking API 设计 |

## 8. 发布判定

只有同时满足以下条件，才能称为生产级 runtime 应用：

1. 有稳定应用入口与版本号。
2. Windows 安装包通过 clean machine E2E。
3. 本地 API、认证、workspace 授权和安全审计通过。
4. 数据迁移与回滚通过。
5. 团队恢复、诊断、优雅退出和异常重启通过。
6. React 工作区完成核心用户路径。
7. 文档覆盖安装、配置、备份、诊断和卸载。
8. 所有默认 CI 检查通过。

当前状态满足“可以开始应用化演进”，但不满足“已经生产级应用”。

## 9. 建议的下一步

1. 评审并合并本计划。
2. 建 issue：
   - Runtime application shell。
   - AgentTeam feature API migration。
   - React workspace migration。
   - Windows lifecycle。
   - Desktop packaging。
3. 先做 M1，产出独立 PR。
4. M1 合并后再做 M2，避免 UI、API 和 lifecycle 混在一个变更中。
