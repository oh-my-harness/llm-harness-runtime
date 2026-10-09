# Mobile Work Assistant — 实施计划

日期：2026-10-10
输入：`docs/superpowers/specs/2026-10-10-mobile-work-assistant-spec.md`
原则：每批独立可交付、真机可验证、独立 commit；Rust 先行（协议/核心），Kotlin 跟进，UI 重构最后。

---

## Batch 1 — 无沙箱 session + setModel（Rust，手机无感）

### Task 1.1 gateway `os` 后端
- `crates/llm-harness-sandbox-gateway/src/config.rs`：`BackendConfig::Os` → `GatewayBackend::Os`。
- `state.rs`：`os` 分支建 `OsEnvSandbox`（work_root 下 per-sandbox 目录）。
- 测试：config 解析 `{"type":"os"}`；建沙箱 exec 落宿主目录。
- 验收：gateway.json 用 os 后端启动，`POST /v1/sandboxes` + exec 走通；`cargo test -p llm-harness-sandbox-gateway`。

### Task 1.2 mobile-core 无沙箱组装
- `MobileSession::assemble` 泛化：沙箱参数改 `Arc<dyn Sandbox>`；`config.gateway_url` 空时跳过 `connect_sandbox`，组 harness 不挂工具（`tools = vec![]`），system_prompt 去掉 sandbox 措辞。
- `MobileConfig.gateway_url` 语义改为可空（空 = 纯对话）。`SessionState.sandbox_id` 空串表纯对话。
- uniffi：`MobileSession::setLlm(model, provider, baseUrl, apiKey, contextWindow?, maxTokens?)` → harness `switch_llm`
  （`AgentHarness.client` 改 `RwLock<Arc<dyn LlmClient>>` + `set_client`/`switch_llm`，下一轮生效；跨 provider 热切换不再是限制）。
- 测试：`cargo test -p llm-harness-mobile-core`（无沙箱组装、poll 循环、setLlm 切 provider）。

### 验收
- 纯对话任务真机可用：无 gateway 配置发"你好"收到流式回复。

## Batch 2 — Kotlin 数据层：Machine 与 LLM 分家

### Task 2.1 Machine 实体与仓库
- `data/Machine.kt`（实体 + DataStore `machines` JSON list）+ `data/MachineRepository.kt`（CRUD + healthz 探活：`GET {url}/healthz`，2s 超时，结果缓存 30s）。
- `AppConfig` 拆分：保留 LLM 字段 + fontScale；gateway 三字段迁出。DataStore 迁移：旧字段 → machines[0]。
- `LlmProfile` 列表（`models` DataStore）+ `activeLlmId`。

### Task 2.2 迁移与默认
- `defaults.json` 结构升级（gatewayUrl 等 → machines 数组 + llm 块）；旧 preferences 迁移逻辑（读到旧 key → 建机器 + LLM profile）。
- 验收：现有真机配置无损迁移（升级安装后 connected 不变）。

## Task 3 — 三层导航与任务列表

- 路由：`machines` / `tasks/{machineId}` / `chat/{taskId}`（+ `settings`）。
- 机器列表页：卡片（名称/在线态/运行中任务数），长按菜单（编辑/删除），"+" 新建（复用 ConfigForm 拆出机器字段）。
- 任务列表页：`TaskSummary` 卡片（标题/摘要/时间/状态点/未读徽标），顶部"+"新建。
- 自动标题：首条用户消息后，mobile-core 侧用 active LLM 生成（prompt 压缩成 10 字内标题，失败回退首条前 30 字）。
- 验收：真机三层导航走通；任务列表状态点/未读徽标正确刷新（后台任务来消息未读 +1）。

## Task 4 — 懒连接 + 排队注入
- `switchSession` 去 connect：只读 history replay；`send()` 时才 `openSession`（machine 任务）或直连 LLM（纯对话）。
- 忙时发送走 `steer`；UI"排队中"标记（pendingSend 状态，首个 TextDelta 后清除）。
- 删 Steer 按钮；streaming 时发送键 = stop。
- 验收：真机并行两任务（同机双沙箱）+ 忙时发消息排队注入 + stop 保留排队消息。

## Task 5 — 工具卡片折叠 + diff 着色
- `ToolStatusCard` 折叠/展开；diff 行着色。
- system prompt 追加"结论先行"约定。
- 验收：真机跑 `cargo test`（会失败的）看结论行 + 折叠卡。

## Task 6 — 通知 + 前台服务
- `MobileTaskService`：running 时启动、空闲停止；任务结束（非前台）发通知，点击深链回会话。
- 验收：熄屏等任务跑完收通知；全程无 ANR。

## Task 7 — 视觉收尾与回归
- 新页面套 ohm token（机器卡/任务卡/空态）。
- 全量真机回归：纯对话、多机多任务、排队、通知、模型切换。

## 里程碑验收（每批通用门）

- `cargo test --workspace --exclude llm-harness-live-tests`（Rust 批）
- `JAVA_HOME=... gradle assembleDebug`（Kotlin 批）
- 真机：`ANDROID_SERIAL=dc8a6246` 安装 + uiautomator dump / 截屏证据
- 每批 1-2 个 commit（conventional，--no-verify），不 push（等确认）

## 风险与顺序依赖

- setModel 跨 provider 切换若 harness 不支持热切 provider → 降级为"仅切模型 id"，spec 记录。
- 排队注入依赖 steer 通道容量（默认 cap），UI 侧需处理 Full 错误（提示重发）。
- 无沙箱 os 后端安全性弱：配置示例注释标明仅自用。
- 迁移必须真机验证 defaults.json + DataStore 双路径。

---

## 进度

- 2026-10-10 Batch 1（Rust 热切换）完成：`AgentHarness::set_client` / `switch_llm`，mobile-core `setLlm`，
  Kotlin 绑定已重生成（`uniffi.llm_harness_mobile_core.MobileSession.setLlm`）。
  验证：`cargo test -p llm-harness-agent --features test-utils`（299 passed）、
  `cargo test -p llm-harness-mobile-core`（17 passed）、`cargo clippy` 两个 crate 无告警。
  未做：Batch 2（LlmProfile 接线 + defaults.json 嵌套 bug）、Batch 3（Settings/chat 顶栏切换 UI）。
