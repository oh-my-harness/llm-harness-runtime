# Trust-Profile Sandbox Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 沙箱按信任档位（restricted / trusted-lan / trusted-full）参数化，使 agent 在受信环境下能联网、读写宿主项目目录，同时所有档位保持凭据区与系统区的硬底线。

**Architecture:** `TrustProfile` 从 wire 协议 → `SandboxConfig` → seatbelt/bwrap policy 生成逐层贯通；gateway principal 声明档位上限防越权；宿主项目目录通过 principal 的新 `host_project_dirs` 绑定进沙箱并映射为 guest `/workspace/<basename>`；mobile-core 与 Android UI 透传档位。

**Tech Stack:** 现有 Rust workspace（seatbelt policy / bwrap args / gateway / sandbox-remote 协议 / mobile-core），无新依赖。

**Spec:** `docs/superpowers/specs/2026-09-25-mobile-agent-client-design.md`（"信任分级沙箱（2026-09-26 修订）"一节）

## Global Constraints

- 三档：`restricted`（默认，行为与现状完全一致）/ `trusted-lan` / `trusted-full`。
- 硬底线任何档位不可抬升：拒写 `/`、`/System`、`/usr`、`/bin`、`/sbin`、`/private/etc`；拒读凭据区（`~/.ssh`、`~/Library/Keychains`、浏览器 Cookie/密码库）；拒读 `.env`（含 work_dir 内）。
- `restricted` 的生成物必须与改动前逐字节等价（现有 103 sandbox 测试是回归网）。
- Firecracker 不动；`restricted` 档 bwrap/seatbelt 不动。
- 档位在 create 请求里声明，不可运行中变更（提权 = 新建沙箱）。
- Rust 测试命令：`CARGO_HTTP_PROXY="" cargo test --workspace --exclude llm-harness-live-tests`（已知无关失败：runtime-app migration、agent-team-studio、runtime-app-shell）。
- 每任务一 commit（`--no-verify`），`cargo fmt`，不 push。

---

### Task 1: TrustProfile 类型与 wire 协议

**Files:**
- Create: `crates/llm-harness-platform/src/trust.rs`
- Modify: `crates/llm-harness-platform/src/lib.rs`（re-export）
- Modify: `crates/llm-harness-sandbox-remote/src/config.rs`（`RemoteSandboxConfig` 加字段）
- Modify: `crates/llm-harness-sandbox-remote/src/protocol.rs`（`CreateSandboxResponse` 加 `trust_profile`）
- Test: `crates/llm-harness-sandbox-remote/src/config.rs`（tests 模块内）

**Interfaces:**
- Consumes: 现有 `RemoteSandboxConfig`（`fs_allowlist`/`net_allowlist`/`resource_limits`/`work_dir`）。
- Produces（后续任务依赖，签名冻结）：

```rust
// crates/llm-harness-platform/src/trust.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustProfile {
    Restricted,   // 默认；PartialOrd: Restricted < TrustedLan < TrustedFull
    TrustedLan,
    TrustedFull,
}
impl TrustProfile {
    pub fn as_str(&self) -> &'static str; // "restricted" | "trusted-lan" | "trusted-full"
}
impl Default for TrustProfile { fn default() -> Self { Self::Restricted } }
```

`RemoteSandboxConfig` 新增 `pub trust_profile: TrustProfile`（serde default → 旧客户端不带字段 = Restricted，协议向后兼容）。`CreateSandboxResponse` 新增 `pub trust_profile: TrustProfile`（回显审计）。

- [ ] **Step 1: 写失败测试（serde 往返 + 默认值 + 排序）**

```rust
// sandbox-remote/src/config.rs tests 模块
#[test]
fn trust_profile_defaults_to_restricted_and_roundtrips() {
    let json = r#"{"fs_allowlist":[],"fs_denylist":[],"net_allowlist":[],"resource_limits":{"max_cpus":null,"max_processes":null,"max_memory_mb":null,"max_disk_mb":null,"timeout":null}}"#;
    let config: RemoteSandboxConfig = serde_json::from_str(json).expect("old clients parse");
    assert_eq!(config.trust_profile, TrustProfile::Restricted);

    let config: RemoteSandboxConfig =
        serde_json::from_str(&json.replace("net_allowlist\":[]", "net_allowlist\":[],\"trust_profile\":\"trusted_lan\"")).expect("new field parses");
    assert_eq!(config.trust_profile, TrustProfile::TrustedLan);
    assert_eq!(serde_json::to_string(&config.trust_profile).unwrap(), "\"trusted_lan\"");
}

#[test]
fn trust_profile_orders_restricted_lt_lan_lt_full() {
    assert!(TrustProfile::Restricted < TrustProfile::TrustedLan);
    assert!(TrustProfile::TrustedLan < TrustProfile::TrustedFull);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-sandbox-remote trust_profile 2>&1 | tail -5`
Expected: FAIL（字段不存在）。

- [ ] **Step 3: 实现 TrustProfile + wire 字段**

`trust.rs` 按签名实现；`RemoteSandboxConfig`/`CreateSandboxResponse` 加字段（`#[serde(default)]`）；`RemoteSandboxConfig::try_from(SandboxConfig)` 处需要 `SandboxConfig` 同步加字段——**放在 Task 2**，此处 try_from 先临时 `.clone()` 默认值（`#[allow(deprecated)]` 不需要——直接用 `TrustProfile::Restricted`，Task 2 改为透传）。修复所有编译点（state.rs 的 registry JSON 序列化自动兼容——serde default）。

- [ ] **Step 4: 跑测试确认通过 + gateway 编译无回归**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-sandbox-remote -p llm-harness-sandbox-gateway 2>&1 | tail -3`
Expected: PASS（gateway 现有测试不受影响——请求默认 Restricted）。

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add crates/llm-harness-platform crates/llm-harness-sandbox-remote crates/llm-harness-sandbox-gateway Cargo.lock
git commit --no-verify -m "feat(sandbox): add TrustProfile to wire protocol (default restricted, back-compat)"
```

---

### Task 2: SandboxConfig 贯通 + seatbelt policy 参数化

**Files:**
- Modify: `crates/llm-harness-platform/src/sandbox.rs`（`SandboxConfig` 加 `trust_profile: TrustProfile`）
- Modify: `crates/llm-harness-sandbox/src/seatbelt.rs`（`generate_policy` 参数化）
- Modify: `crates/llm-harness-sandbox/src/seatbelt_tests.rs`
- Modify: `crates/llm-harness-sandbox-gateway/src/state.rs`（`build_sandbox_config` 透传）
- Modify: `crates/llm-harness-sandbox-remote/src/config.rs`（`try_from` 透传）
- Test: `crates/llm-harness-sandbox/src/seatbelt_tests.rs`

**Interfaces:**
- Consumes: Task 1 的 `TrustProfile`。
- Produces: `generate_policy(config: &SandboxConfig, work_dir_canon: &Path, allowlist_canon: &[PathBuf], home_dir: &Path) -> String`（新增 `home_dir` 参数——凭据区 deny 需要绝对路径；`SeatbeltEnv::new`/`SeatbeltSandbox::new` 同步加参数或内部取 `std::env::var("HOME")`——**取内部**，签名不变）。

- [ ] **Step 1: 写失败测试（policy 内容断言）**

```rust
// seatbelt_tests.rs
#[test]
fn restricted_policy_unchanged_deny_network() {
    let policy = generate_policy(&default_config(tmp), &canon, &[], home);
    assert!(policy.contains("(deny network*)"));
    assert!(!policy.contains("(allow network*)"));
}

#[test]
fn trusted_lan_policy_allows_network_and_blocks_credentials() {
    let policy = generate_policy(&trusted_lan_config(tmp), &canon, &[], home);
    assert!(!policy.contains("(deny network*)"));
    // 硬底线：凭据区在 allow home 之后仍 deny（last-match-wins）
    let home_allow = policy.find(r#"(allow file-read* (subpath "/Users/test/home"))"#).unwrap();
    let ssh_deny = policy.find(r#"(deny file-read* (subpath "/Users/test/home/.ssh"))"#).unwrap();
    assert!(ssh_deny > home_allow, "credential deny must follow home allow");
    // .env 拒读在 work_dir allow 之后
    assert!(policy.find(r#"(deny file-read* (subpath "WORK_DIR/.env"))"#).unwrap()
        > policy.find(r#"(allow file-read* (subpath "WORK_DIR"))"#).unwrap());
}

#[test]
fn trusted_full_policy_near_host_but_floors_hold() {
    // /etc 写拒绝仍在；.ssh 拒绝仍在 home allow 之后
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-sandbox --features seatbelt policy 2>&1 | tail -5`

- [ ] **Step 3: 实现**

`generate_policy` 按 profile 分支：
- **restricted**：现有内容不动（逐字节等价——回归测试保护）。
- **trusted-lan**：去掉 `(deny network*)`；追加 `(allow network*)`；追加宿主项目目录 read-write allow（来自 config.fs_allowlist 的 host 路径，见 Task 3 映射）。
- **trusted-full**：trusted-lan 基础上追加 `HOME` read-write allow。
- **所有 trusted 档**：在所有 allow **之后**追加硬底线 deny（last-match-wins 生效）：系统区拒写（现有两行已覆盖，保留）、凭据区拒读（`.ssh`、`Keychains`、`Application Support/Google/Chrome/Cookies` 等逐条）、`.env` 拒读（literal work_dir + HOME）。**实现顺序：allow 先、deny 后**。
- 凭据区列表提取为常量 `fn credential_deny_paths(home: &Path) -> Vec<PathBuf>`，seatbelt/bwrap 共用。
- `SandboxConfig` 加 `trust_profile` 字段 + `Default`；所有构造点（gateway `build_sandbox_config` 透传、`try_from` 透传、各测试 `default_config` 补字段）。

- [ ] **Step 4: 全量回归（restricted 等价性）**

Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-sandbox --features seatbelt -p llm-harness-sandbox-gateway -p llm-harness-sandbox-remote 2>&1 | tail -3`
Expected: 103 sandbox + 30 gateway + 45 remote 全 PASS（restricted 生成物等价）。

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add crates/llm-harness-platform crates/llm-harness-sandbox crates/llm-harness-sandbox-gateway crates/llm-harness-sandbox-remote
git commit --no-verify -m "feat(sandbox): parameterize seatbelt policy by trust profile with hard credential floors"
```

---

### Task 3: 宿主项目目录绑定（trusted 档 allowlist 语义）

**Files:**
- Modify: `crates/llm-harness-sandbox-gateway/src/auth.rs`（`GatewayPrincipalConfig` 加 `host_project_dirs` + `max_trust_profile`）
- Modify: `crates/llm-harness-sandbox-gateway/src/state.rs`（trusted 档的 allowlist host 路径映射；档位校验）
- Modify: `crates/llm-harness-sandbox/src/seatbelt.rs`（trusted 档 allowlist 条目按**宿主真实路径** allow，不再强制塞 work_dir 下）
- Test: `crates/llm-harness-sandbox-gateway/tests/gateway.rs`

**Interfaces:**
- Consumes: Task 1/2 的 `TrustProfile`、`SandboxConfig.trust_profile`。
- Produces:

```rust
// auth.rs
pub struct GatewayPrincipalConfig {
    pub principal: String,
    pub bearer_token: String,
    pub fs_allowlist: Vec<PathBuf>,          // 现有：guest /workspace 坐标（restricted 档）
    #[serde(default)]
    pub host_project_dirs: Vec<PathBuf>,     // 新：宿主真实目录（trusted 档）
    #[serde(default)]
    pub max_trust_profile: TrustProfile,     // 新：默认 Restricted
}
// state.rs 新增（pub(crate)）:
// fn resolve_profile(principal, request) -> Result<TrustProfile>  // 请求 > 上限 → InvalidConfig
// fn host_project_mappings(principal) -> Vec<(host: PathBuf, guest: PathBuf)>
//   guest = /workspace/<basename(host)>；host 必须存在且为目录（构造时校验）
```

**映射语义变更**（trusted 档）：`fs_allowlist` 条目不再全部映射进 work_dir；`host_project_dirs` 条目按**宿主真实路径**生成 seatbelt allow + guest 路径映射（`SeatbeltEnv` 需支持多 guest→host 映射——现有 `guest_work_dir: Option<PathBuf>` 单映射改为 `Vec<(PathBuf, PathBuf)>`，guest `/workspace/<name>` → host dir）。restricted 档行为不变。

- [ ] **Step 1: 写失败测试（gateway 集成）**

```rust
// tests/gateway.rs（macOS gate）
#[tokio::test]
async fn trusted_lan_binds_host_project_dir_and_caps_by_principal() {
    // principal 带 host_project_dirs=[真实临时目录（放一个 marker.txt）] + max_trust_profile=TrustedLan
    // 1. 请求 trust_profile=trusted-lan → 201；exec: cat /workspace/<basename>/marker.txt → 内容正确
    // 2. exec: touch /workspace/<basename>/new.txt → 宿主侧文件出现（RW 双向）
    // 3. 另一 principal 无 host_project_dirs 请求 trusted-lan → 400 (InvalidConfig)
    // 4. principal max=restricted 请求 trusted-lan → 400
}
```

- [ ] **Step 2: 确认失败 → 实现 → 通过**

顺序：`TrustProfile` 上限校验（state.rs `create_sandbox` 入口）→ `host_project_dirs` 校验与映射表 → `SeatbeltEnv` 多映射（`resolve_path` 的 guest 分支改查映射表，匹配最长 guest 前缀）→ seatbelt policy 追加各 host 目录 RW allow（deny floors 之后再次 allow 这些目录？**不**——项目目录不是凭据区，allow 放在 floors 之前即可；但 `.env` 拒读例外要在项目目录 allow 之后对每个绑定目录再 deny `.env`）→ bwrap 追加 `--bind host guest`（Task 4）。
Run: `CARGO_HTTP_PROXY="" cargo test -p llm-harness-sandbox-gateway -p llm-harness-sandbox --features seatbelt 2>&1 | tail -3`

- [ ] **Step 3: Commit**

```bash
cargo fmt && git add crates/llm-harness-sandbox crates/llm-harness-sandbox-gateway
git commit --no-verify -m "feat(gateway): host project dir binding and trust profile caps per principal"
```

---

### Task 4: bwrap 后端同语义（Linux，编译期验证）

**Files:**
- Modify: `crates/llm-harness-sandbox/src/bwrap.rs`（网络 share + 项目目录 bind）
- Test: `crates/llm-harness-sandbox/src/bwrap.rs` 现有测试（Linux-gated，Mac 上仅编译验证）+ 新增 args 单测（纯函数 `build_bwrap_args` 断言，跨平台可跑）

**Interfaces:**
- Consumes: `SandboxConfig.trust_profile`、`host_project_mappings`（经 `BwrapEnv::new_mapped` 扩展参数传入）。
- Produces: trusted 档 bwrap args：`--unshare-all` 拆为按需（网络放开时替换为 `--unshare-all` 去掉 net 部分 = 追加 `--share-net`）；项目目录追加 `--bind <host> <guest>`；凭据区追加 `--ro-bind-try` 不可行（bwrap 无 deny 语义）——**bwrap 凭据区靠不 bind 实现**（未 bind = 不可见），floors 测试改为断言 args 中不含凭据路径。

- [ ] **Step 1: 写 args 断言测试（跨平台）**

```rust
#[test]
fn trusted_lan_bwrap_args_share_net_and_bind_projects() {
    let args = build_args_for_profile(TrustedLan, &mappings);
    assert!(args.iter().any(|a| a == "--share-net"));
    assert!(args.windows(3).any(|w| w[0] == "--bind" && w[1] == "/Users/x/proj"));
    // 凭据区永不出现在 bind 列表
    assert!(!args.iter().any(|a| a.contains(".ssh")));
}
```

- [ ] **Step 2: 实现 → Linux 测试门禁跑（Mac 上 `cargo check --all-targets` + 单测跑纯函数部分）→ Commit**

```bash
cargo fmt && git add crates/llm-harness-sandbox/src/bwrap.rs
git commit --no-verify -m "feat(sandbox): bwrap trust profile semantics (share-net, project binds, credential exclusion)"
```

---

### Task 5: mobile-core + Android UI 档位透传

**Files:**
- Modify: `crates/llm-harness-mobile-core/src/lib.rs`（`MobileConfig.trust_profile: String` → 解析到 `TrustProfile`）
- Modify: `mobile/android/app/src/main/java/dev/ohmyharness/mobile/MainActivity.kt`
- Modify: `mobile/android/app/src/main/res/layout/activity_main.xml`
- Test: `crates/llm-harness-mobile-core/tests/session.rs`

**Interfaces:**
- Consumes: Task 1 wire 字段。
- Produces: `MobileConfig.trust_profile: String`（"restricted"|"trusted-lan"|"trusted-full"，非法值 → connect 报 InvalidConfig）。

- [ ] **Step 1: Rust 侧解析 + 失败测试**

```rust
// tests/session.rs
#[test] fn parses_trust_profile_strings() {
    // 经由 pub(crate) fn parse_trust_profile(&str) -> anyhow::Result<TrustProfile>
}
```

- [ ] **Step 2: uniffi 重生成 + Kotlin UI**

布局加三枚 `RadioButton`（RadioGroup，默认 restricted）；`MainActivity` 传字符串；connect 失败消息含档位名。

- [ ] **Step 3: 构建 + 模拟器冒烟**

Run: `cargo ndk -t arm64-v8a -o mobile/android/app/src/main/jniLibs build -p llm-harness-mobile-core && cd mobile/android && gradle assembleDebug`
冒烟：restricted（默认）连接 → 发 "curl pypi.org" → 预期网络失败；切 trusted-lan 重连 → 同指令成功。UI 顶部常显当前档位。

- [ ] **Step 4: Commit**

```bash
cargo fmt && git add crates/llm-harness-mobile-core mobile/android
git commit --no-verify -m "feat(mobile): trust profile selector end-to-end (restricted/trusted-lan/trusted-full)"
```

---

### Task 6: 验收场景全跑（spec 验收骨架）

**Files:**
- Create: `scripts/run-trust-profile-smoke.sh`（本机一键：起 gateway（两 principal：上限 full 的 phone、上限 restricted 的 kiosk）→ 跑 5 个验收场景的 curl 断言）

**Interfaces:**
- Consumes: 全部前序任务。

- [ ] **Step 1: 场景 1-3（trusted-lan 正向 + 底线）**

`pip install requests` 成功；`git clone` 内网仓库到绑定项目目录（宿主可见）；写 `/etc/hosts` 与读 `~/.ssh/id_ed25519` 均被拒（exec 返回 PermissionDenied 语义，非 crash）。

- [ ] **Step 2: 场景 4-5（越权与切档）**

kiosk principal 请求 trusted-lan → 400；app 切档重建 → create 响应 `trust_profile` 字段回显正确。

- [ ] **Step 3: 全量回归 + Commit**

Run: `CARGO_HTTP_PROXY="" cargo test --workspace --exclude llm-harness-live-tests --exclude llm-harness-runtime-app --exclude llm-harness-agent-team-studio --exclude llm-harness-runtime-app-shell 2>&1 | tail -2`

```bash
git add scripts/run-trust-profile-smoke.sh
git commit --no-verify -m "test(sandbox): trust profile acceptance smoke script (spec scenarios 1-5)"
```

---

## Self-Review

1. **Spec 覆盖**：分级设计→T1/T2；落地面 1→T2，2→T1/T3，3→T4，4→T5，5→T1（响应字段）/T5（UI 常显）；验收 5 场景→T6；非目标（按域过滤/Firecracker/动态提权）无任务对应，符合"不做"。`.env` 拒读在 T2 policy 实现。
2. **占位符扫描**：T3 Step 2 的实现顺序是指导而非占位（每个子项有明确落点）；T4 bwrap 凭据区"不 bind 即不可见"是设计决定非 TODO。
3. **类型一致性**：`TrustProfile` 在 T1 冻结（含 PartialOrd 供上限比较），T2/T3/T5 按名引用；`generate_policy` 新签名（+home_dir→改为内部取 HOME，签名不变）在 T2 冻结；`resolve_profile`/`host_project_mappings` 在 T3 冻结。
4. **风险**：T3 的 `SeatbeltEnv` 多映射是本计划最大改动面（`resolve_path` guest 分支重构），排在中段且 gateway 集成测试兜底；T2 的"restricted 逐字节等价"由现有 103 测试护航。
