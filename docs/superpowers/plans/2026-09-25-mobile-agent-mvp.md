# Mobile Agent MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在手机上跑通"Rust agent loop → LLM → 工具经 gateway 在沙箱执行 → 屏幕显示真实输出"的完整链路。

**Architecture:** 新增 `llm-harness-mobile-core` crate（组装现有 AgentHarness + RemoteSandboxClient），交叉编译到 aarch64-linux-android，uniffi 生成 Kotlin 绑定；Android 壳为单 Activity 最小对话界面；gateway 增加 Seatbelt 后端使全链可在 macOS 本机验证。

**Tech Stack:** Rust workspace（现有）、uniffi、cargo-ndk、Kotlin、Jetpack Compose、macOS sandbox-exec（Seatbelt，现有）。

**Spec:** `docs/superpowers/specs/2026-09-25-mobile-agent-client-design.md`（含 MVP 优先修订）

## Global Constraints

- MVP 砍掉：语音、前台服务、SQLite 持久化、通知、UI 打磨、公网穿透。不做即不写。
- mobile-core 不吞错、不重试、不兜底，错误透传 UI（spec 错误处理原则）。
- 不修改 `llm-harness-runtime`、`llm-harness-sandbox-remote` 的任何行为。
- gateway 现有 Bwrap/Firecracker 路径行为不变；Seatbelt 仅 macOS 可选。
- 提交遵循 CLAUDE.md：每任务一 commit，`cargo fmt`，push 需用户确认。
- Rust 测试命令：`CARGO_HTTP_PROXY="" cargo test --workspace --exclude llm-harness-live-tests`。

---

### Task 1: 解除 gateway crate 的 Linux 门禁

**Files:**
- Modify: `crates/llm-harness-sandbox-gateway/src/lib.rs:1`（删除 `#![cfg(target_os = "linux")]`）
- Modify: `crates/llm-harness-sandbox-gateway/Cargo.toml`
- Test: `crates/llm-harness-sandbox-gateway/tests/gateway.rs`（现有测试在 Mac 上必须原样通过）

**Interfaces:**
- Consumes: 现有 `GatewayConfig::with_backend(GatewayBackend)`。
- Produces: gateway crate 在 macOS 上可编译、现有 Linux 测试语义不变。这是 Task 2 的前提。

- [ ] **Step 1: 验证当前在 Mac 上编译失败的原因范围**

Run: `cargo check -p llm-harness-sandbox-gateway --all-features`
Expected: 编译通过（crate 级 cfg 让内容为空壳）。改为先临时删除 `#![cfg(target_os = "linux")]` 行，再 `cargo check -p llm-harness-sandbox-gateway`，记录真实报错清单（预期集中在 `state.rs` 对 `BwrapSandbox::new_mapped`、Firecracker 类型、unix 权限 API 的引用）。

- [ ] **Step 2: 按报错逐个加模块级 cfg**

`lib.rs` 删除第 1 行 crate 级门禁；在 `state.rs` 中将 Firecracker 专属装配（`create_firecracker_sandbox`、`FirecrackerLifecycle` 引用、vsock CID 校验）包进 `#[cfg(target_os = "linux")]` 模块/分支；`Cargo.toml` 中把 `llm-harness-vm-firecracker`、`llm-harness-vm-agent-env` 依赖改为 target-gated：

```toml
[target.'cfg(target_os = "linux")'.dependencies]
llm-harness-vm-firecracker.workspace = true
llm-harness-vm-agent-env.workspace = true
```

`GatewayBackend::Firecracker(FirecrackerBackendConfig)` 变体与 `auth.rs` 的校验函数保留全平台编译（纯数据 + 校验，无 Linux 依赖），只在实际装配处 cfg 门禁——避免破坏现有测试 `firecracker_backend()` 的跨平台编译。

- [ ] **Step 3: 跑现有测试确认零回归**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-sandbox-gateway`
Expected: 现有测试在 Mac 上的表现与改动前一致（此前空壳测试为 0 通过，改动后 Bwrap/Firecracker 装配类测试在非 Linux 上应保持 skip 语义——用与 `llm-harness-sandbox` crate 相同的 `cfg(target_os = "linux")` 测试门禁模式，参照 `seatbelt_tests.rs` 的做法反向应用）。

- [ ] **Step 4: Commit**

```bash
git add crates/llm-harness-sandbox-gateway
git commit -m "refactor(sandbox-gateway): lift crate-level linux gate for seatbelt backend"
```

---

### Task 2: gateway 接入 Seatbelt 后端

**Files:**
- Modify: `crates/llm-harness-sandbox-gateway/src/auth.rs`（`GatewayBackend` 加 `Seatbelt` 变体）
- Modify: `crates/llm-harness-sandbox-gateway/src/state.rs`（`GatewaySandbox` 加 `Seatbelt` 分支 + 装配）
- Modify: `crates/llm-harness-sandbox-gateway/Cargo.toml`（`llm-harness-sandbox` 的 `seatbelt` feature，macOS gated）
- Test: `crates/llm-harness-sandbox-gateway/tests/gateway.rs`

**Interfaces:**
- Consumes: `llm_harness_sandbox::SeatbeltSandbox::new(SandboxConfig) -> anyhow::Result<Self>`（macOS，fail-closed 拒绝不支持的策略）；Task 1 的模块级 cfg 结构。
- Produces: `GatewayBackend::Seatbelt` 变体；`backend_label()` 返回 `"macos-seatbelt"`；`SandboxRecord.env` 为 `Arc<SeatbeltEnv>`（实现 `ExecutionEnv`，协议层无感知）。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
#[cfg(target_os = "macos")]
fn gateway_accepts_seatbelt_backend() {
    let work_root = private_tempdir();
    let gateway = SandboxGateway::new(
        GatewayConfig::new(vec![principal("tenant-a", TOKEN_A)])
            .expect("gateway config")
            .with_work_root(work_root.path().to_path_buf())
            .with_backend(GatewayBackend::Seatbelt),
    );
    assert!(gateway.is_ok());
    // backend_label 通过 API 无法直接触达；在 state 单元测试中断言
}

#[test]
#[cfg(target_os = "macos")]
fn seatbelt_gateway_rejects_denylist_policy() {
    // SeatbeltSandbox 对 fs_denylist fail-closed；创建带 denylist 的 sandbox
    // 走 POST /v1/sandboxes 应得到 4xx，且远端不留残骸
    // 复用 tests/gateway.rs 现有 start_gateway helper + GatewayBackend::Seatbelt
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-sandbox-gateway --features seatbelt 2>&1 | tail -20`
Expected: FAIL — `GatewayBackend::Seatbelt` 不存在。

- [ ] **Step 3: 实现变体与装配**

`auth.rs`：

```rust
pub enum GatewayBackend {
    Bwrap,
    #[cfg(target_os = "macos")]
    Seatbelt,
    Firecracker(FirecrackerBackendConfig),
}
```

`validate()`：`GatewayBackend::Seatbelt => {}`（校验下沉到 SeatbeltSandbox::new 的 fail-closed）。`state.rs` 的 `backend_label()` 加 `"macos-seatbelt"` 分支；`create_sandbox` 的非 Firecracker 分支按 backend 分派：Seatbelt 时构造 `SeatbeltSandbox::new(sandbox_config)` → `GatewaySandbox::Seatbelt(...)`。`GatewaySandbox` 的 `Sandbox` impl 各 match 臂加 Seatbelt 分支，委托 `SeatbeltSandbox` 的同名方法。注意 `SeatbeltSandbox` 未实现 `Sandbox` trait 的可能性——若只有 `ExecutionEnv`，包装结构里 start/stop 逻辑参照 Bwrap 臂的最小语义（start 置 running 标志）。

- [ ] **Step 4: 跑测试确认通过 + 全量回归**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-sandbox-gateway && CARGO_HTTP_PROXY="" cargo test --workspace --exclude llm-harness-live-tests`
Expected: 新测试 PASS，全 workspace 零回归。

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-sandbox-gateway
git commit -m "feat(sandbox-gateway): add seatbelt backend for macos"
```

---

### Task 3: mobile-core crate 骨架（Mac 本机可测）

**Files:**
- Create: `crates/llm-harness-mobile-core/Cargo.toml`
- Create: `crates/llm-harness-mobile-core/src/lib.rs`
- Create: `crates/llm-harness-mobile-core/src/session.rs`
- Test: `crates/llm-harness-mobile-core/tests/session.rs`
- Modify: 根 `Cargo.toml`（workspace members + workspace.dependencies）

**Interfaces:**
- Consumes: `llm_harness_agent::AgentHarness`（`prompt()`、`subscribe() -> broadcast::Receiver<Arc<AgentEvent>>`）；`llm_harness_sandbox_remote::{RemoteSandboxClient, RemoteSandboxClientConfig}`。
- Produces（Task 4/5 依赖，签名冻结）：

```rust
pub struct MobileSession;
impl MobileSession {
    /// gateway endpoint、bearer token、沙箱与 LLM 配置在构造时注入
    pub async fn connect(config: MobileConfig) -> anyhow::Result<Arc<Self>>;
    /// 发送用户消息，返回 run id；事件经 events() 流出
    pub async fn send(self: &Arc<Self>, text: String) -> anyhow::Result<u64>;
    /// 订阅事件流（UI 消费）；MVP 仅透传 AgentEvent 的可显示子集
    pub fn events(&self) -> broadcast::Receiver<MobileEvent>;
    pub async fn shutdown(&self) -> anyhow::Result<()>;
}
#[derive(Debug, Clone)]
pub enum MobileEvent {
    TextDelta { text: String },
    ToolStarted { name: String },
    ToolFinished { name: String, summary: String },
    RunFinished { run_id: u64 },
    Failed { message: String },
}
```

- [ ] **Step 1: crate 骨架 + 空实现编译通过**

`lib.rs` 导出上述类型（`MobileSession::connect` 暂 `bail!("not implemented")` 除外——只导出类型与枚举映射测试可跑的部分）。加 workspace 成员。

Run: `cargo check -p llm-harness-mobile-core`
Expected: 编译通过。

- [ ] **Step 2: 写失败测试（事件映射，纯单元）**

```rust
// tests/session.rs
fn map_event(e: &AgentEvent) -> Option<MobileEvent> {
    // 经由 lib 导出的映射函数：AgentEvent::TextDelta → MobileEvent::TextDelta 等
}

#[test]
fn maps_text_delta() {
    assert!(matches!(
        map_event(&test_agent_event_text_delta("hi")),
        Some(MobileEvent::TextDelta { .. })
    ));
}

#[test]
fn drops_non_exhaustive_unknowns() {
    // AgentEvent 是 #[non_exhaustive]：未识别变体 → None，不 panic
}
```

- [ ] **Step 3: 跑测试确认失败 → 实现映射 → 通过**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-mobile-core`
Expected: 先 FAIL（映射未实现），实现后 PASS。映射覆盖：TextDelta、ToolExecutionStart、ToolExecutionEnd/结果、AgentEnd、错误路径 → Failed。

- [ ] **Step 4: connect/send 最小实现（真实 AgentHarness + RemoteSandboxClient 组装）**

`session.rs`：tokio runtime 由调用方提供（mobile-core 不自建 runtime，Task 4 在 Kotlin 侧起一个并持有）。`connect` 组装 RemoteSandboxClient + 创建 sandbox + 构造 AgentHarness（LLM provider 凭据来自 `MobileConfig`）。`send` 调 `prompt()` 并 spawn 事件泵任务：`subscribe()` 循环 → map_event → broadcast 到 `MobileEvent` 通道。单测用 loopback mock 无法复用（live-tests 被排除），MVP 的组装正确性由 Task 6 端到端冒烟覆盖，此处只测类型与映射。

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-mobile-core`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add crates/llm-harness-mobile-core Cargo.toml
git commit -m "feat(mobile-core): add mobile session skeleton with event mapping"
```

---

### Task 4: Android 交叉编译 + uniffi 绑定

**Files:**
- Create: `crates/llm-harness-mobile-core/Cargo.toml`（追加 uniffi 依赖 + `[lib] crate-type = ["lib", "cdylib"]`）
- Create: `crates/llm-harness-mobile-core/src/udl/llm_harness_mobile.udl`（或 proc-macro 宏模式，二选一在实现时定，倾向宏模式少一个构建步骤）
- Create: `crates/llm-harness-mobile-core/build.rs`（uniffi::build_foreign_language_bindings）
- Create: `scripts/build-android.sh`（cargo-ndk 封装）
- Create: `mobile/android/`（Gradle 工程，含 `app/build.gradle.kts`、单 Activity `MainActivity.kt`）

**Interfaces:**
- Consumes: Task 3 冻结的 `MobileSession`/`MobileEvent` 接口。
- Produces: `mobile/android/app/libs/llm_harness_mobile.aar`（或 bindings + jniLibs 目录），Kotlin 侧可 `MobileSession.connect(...)`。

- [ ] **Step 1: 安装工具链并验证裸交叉编译**

```bash
rustup target add aarch64-linux-android
cargo install cargo-ndk
cargo ndk -t arm64-v8a build -p llm-harness-mobile-core
```

Expected: `.so` 产出。报错则逐个处理（tokio/reqwest/rustls 在 android target 的编译问题；`std::os::unix` 均可用，无 Windows 路径）。

- [ ] **Step 2: uniffi 宏模式暴露接口**

`lib.rs` 加 `uniffi::setup_scaffolding!()`；`MobileSession`/`MobileEvent` 加 `uniffi::Object`/`uniffi::Enum` derive。`MobileConfig` 暴露为 record（endpoint、token、api_key、model、work_root 宿主路径不需要——沙箱路径全是 guest 坐标系）。事件流跨越 FFI：用 uniffi 的回调接口 `MobileEventSink { on_event(e: MobileEvent) }` 替代 broadcast receiver（Kotlin 侧注册 sink，Rust 侧泵任务回调）。

Run: `cargo ndk -t arm64-v8a build -p llm-harness-mobile-core && cargo test -p llm-harness-mobile-core`
Expected: host 测试仍 PASS，android .so 产出。

- [ ] **Step 3: 生成 Kotlin 绑定并装配 Gradle 工程**

`scripts/build-android.sh` 一次完成：ndk build → uniffi-bindgen kotlin 输出到 `mobile/android/app/src/main/kotlin/` → jniLibs 拷贝。Gradle 工程最小化：`MainActivity.kt`（Compose 单屏）、`MobileApplication.kt`（进程内持有一个全局 tokio…不——tokio runtime 在 Rust 侧由 uniffi async 支持管理，或 Rust 侧 `OnceLock<TokioRuntime>` 自建。实现时定：**倾向 Rust 侧自建全局 runtime**，Kotlin 零异步概念）。

Run: `cd mobile/android && ./gradlew assembleDebug`
Expected: BUILD SUCCESSFUL。

- [ ] **Step 4: Commit**

```bash
git add crates/llm-harness-mobile-core scripts/build-android.sh mobile/android
git commit -m "feat(mobile): android cross-compile with uniffi bindings and minimal shell"
```

---

### Task 5: 最小对话 UI（单 Activity）

**Files:**
- Modify: `mobile/android/app/src/main/kotlin/.../MainActivity.kt`
- Modify: `mobile/android/app/src/main/kotlin/.../SessionViewModel.kt`（新建）

**Interfaces:**
- Consumes: Task 4 的 Kotlin 绑定（`MobileSession.connect/send`、`MobileEventSink` 回调）。
- Produces: 可跑的 MVP 界面。

- [ ] **Step 1: ViewModel 持有 session + 消息列表状态**

```kotlin
class SessionViewModel : ViewModel() {
    private val _messages = MutableStateFlow(listOf<ChatMessage>())
    val messages: StateFlow<List<ChatMessage>> = _messages
    private var session: MobileSession? = null

    fun connect(config: MobileConfig) { /* suspend 调 Rust connect，注册 sink */ }
    fun send(text: String) { /* 更新列表 + session.send(text) */ }
    // sink 回调切主线程 append TextDelta / ToolStarted / RunFinished / Failed
}
```

- [ ] **Step 2: Compose 单屏**

消息列表（`LazyColumn`，assistant 流式文本直接拼接渲染）+ 底部输入框 + 发送按钮 + 连接配置入口（MVP 用一个简单的设置对话框填 endpoint/token/api key，存 `SharedPreferences` 明文——MVP 接受，spec 正式版换 Keystore）。工具事件显示为居中灰字状态行（"执行工具: exec_shell"）。无语音按钮、无前台服务。

- [ ] **Step 3: 构建并安装到模拟器**

Run: `./gradlew installDebug && adb shell am start -n .../.MainActivity`
Expected: 界面出现，输入可发送（此刻 gateway 未连，应显示 Failed 事件文案——这本身就是透传验证）。

- [ ] **Step 4: Commit**

```bash
git add mobile/android
git commit -m "feat(mobile): minimal chat ui with streaming event display"
```

---

### Task 6: 端到端冒烟（验收）

**Files:**
- Create: `scripts/run-mobile-mvp-gateway.sh`（Mac 本机起 gateway：生成 token、work_root、以 `GatewayBackend::Seatbelt` 装配，loopback HTTP）

**Interfaces:**
- Consumes: Task 2 的 Seatbelt gateway + Task 5 的 app。

- [ ] **Step 1: 写 gateway 启动脚本**

脚本内容：`cargo run --example` 或小型 bin（放 `crates/llm-harness-sandbox-gateway/examples/seatbelt_demo.rs`，因为 gateway 无 main 二进制）：`GatewayConfig::new(vec![GatewayPrincipalConfig { principal: "phone", bearer_token: <随机 32+ 字节>, fs_allowlist: ["/workspace"] }])` + `.with_backend(GatewayBackend::Seatbelt)` + `with_work_root(临时目录)`，`routes::serve` 绑 `127.0.0.1:7080`（协议允许 loopback HTTP）。打印 endpoint 与 token。

Run: `cargo run -p llm-harness-sandbox-gateway --example seatbelt_demo`
Expected: gateway 监听；`curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7080/v1/sandboxes -X POST -d '{...}'` 返回 201。

- [ ] **Step 2: 真机/模拟器端到端**

模拟器网络地址用 `10.0.2.2:7080` 指向宿主。手机 app 填 endpoint+token+api key → 发送："在沙箱里执行 ls -la，告诉我输出"。
Expected: 屏幕先出现工具状态行，随后 agent 回复含 `/workspace` 下文件列表（真实沙箱输出，非编造）。

- [ ] **Step 3: 错误路径冒烟**

填错 token → 应显示 Failed（401 透传）。杀 gateway 进程 → 下一条消息应 Failed（transport 错误透传）。

- [ ] **Step 4: Commit + 收尾**

```bash
git add scripts/run-mobile-mvp-gateway.sh crates/llm-harness-sandbox-gateway/examples
git commit -m "feat(mobile): mvp e2e smoke scripts for seatbelt gateway"
```

验收汇报：四项成功判据截图/日志 + 全 workspace 测试通过记录。MVP 结论回写 spec（验证了什么、工具链坑、UI 下一阶段的输入）。
