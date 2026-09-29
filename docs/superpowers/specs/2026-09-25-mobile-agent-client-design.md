# MVP 优先（2026-09-25 修订）

完整形态之前先做 MVP 走通全流程，验证核心问题：**Rust runtime 能否在 Android 上运行，并通过现有 gateway 协议完成一次"LLM 对话 + 工具在远端沙箱执行"。**

MVP 范围：

1. gateway 接入 Seatbelt 后端（macOS，复用现有 `SeatbeltSandbox`，接口 `GatewayBackend`/`GatewaySandbox` 已留好）——使整条链可在 Mac 本机验证，不依赖 Linux 服务器。
2. `llm-harness-mobile-core`：aarch64-linux-android 交叉编译 + uniffi 绑定，最小接口 `send()/events()`。无持久化、无恢复。
3. Android 壳：单 Activity，消息列表 + 输入框，事件流直刷。亮屏测试，无前台服务。
4. 端到端冒烟：手机发"在沙箱里执行 `ls -la`"，屏幕显示沙箱真实输出。

MVP 砍掉：语音、前台服务、SQLite 持久化、通知、UI 打磨、公网穿透（gateway 跑 Mac loopback）。
MVP 中最大不确定性是 uniffi 工具链 + tokio 在 Android 的启动，实施计划排最前先排雷。

---

# Mobile Agent Client — 大方向设计

日期：2026-09-25
状态：方向已确认，详细设计后置

## 背景与目标

用手机驱动 agent 工作，不打开电脑。躺床上、用语音下指令、看 agent 干活、收结果。

现有资产：`llm-harness-runtime` agent loop、`llm-harness-sandbox-remote` gateway 客户端、`llm-harness-sandbox-gateway`（bwrap + Firecracker 后端，含 #253 崩溃恢复）。缺的只是手机端入口。

## 已确认的方向决策

| 决策点 | 结论 |
|---|---|
| agent loop 位置 | 手机上。电脑/服务器只跑 sandbox-gateway，不跑 agent |
| 平台 | Android 先行，iOS 后置 |
| 后台运行 | 任务活跃期间用前台服务（常驻通知），空闲降级。熄屏可用是核心承诺 |
| 技术栈 | Rust core（复用 runtime + sandbox-remote）编译为 Android 库，uniffi 绑定，Kotlin 壳 |
| 语音 | 系统语音转文字（SpeechRecognizer），文字进 agent，不做云端 ASR |
| 网络 | gateway 公网可达（内网穿透/公网服务器），手机一律 HTTPS |
| 会话数 | 一期单会话单沙箱；多沙箱后置（协议层已支持） |
| 详细设计 | 后置。本 spec 只定方向与边界 |

## 架构

```text
┌─ Android app ─────────────────────────────┐
│  Kotlin: UI + 前台服务 + 语音              │
│  ── uniffi 绑定 ──                        │
│  Rust core（新 crate llm-harness-mobile-core）
│    · AgentHarness 组装（复用，不 fork）    │
│    · RemoteSandboxClient（复用）           │
│    · SQLite 会话持久化（turn 级恢复点）    │
└──────────────┬────────────────────────────┘
               │ HTTPS + Bearer + SSE（现有协议）
               ▼
        gateway（bwrap / Firecracker，现有）
```

- `mobile-core` 是组装层：session 生命周期、事件桥接、持久化。不改 runtime、不改 gateway。
- 单会话 = 一个 `MobileSession` + 一个 sandbox；drop 走现有 cleanup 语义，恢复时查 `GET /v1/sandboxes/{id}` 复用或重建。
- 数据流：语音→文字→`prompt()`→`subscribe()` 收 `AgentEvent`→uniffi 回调→Compose 渲染。恢复粒度 turn 级（`TurnEnd` 后落盘；`MessageEnd{discarded:true}` 跳过）。
- 错误处理原则：mobile-core 不吞错、不重试、不兜底，透传给 UI；重试/超时语义全部沿用 runtime 与客户端现有配置。

## 硬约束：UI 质量

**UI 是这个项目成败的关键，不是附属品。** 用户之所以现在用 ToDesk 苦哈哈地远控，就是因为现有工具在手机上不好用。因此：

1. **详细设计阶段 UI 与 core 同等投入**，UI 单独走详细设计（交互流程、信息层级、状态呈现），不由 core 接口顺手决定。
2. **流式优先**：对话逐字渲染、工具执行状态实时可见（"正在执行 xx"）、等待期有进度感。躺在手机上盯着看是这个 app 的主要使用姿态，信息呈现密度和可读性按手机竖屏优化。
3. **语音一等公民**：一键说话、转写可编辑确认、语音/键盘输入平等共存，语音入口常驻可达。
4. **通知即入口**：前台服务通知显示任务进展，点击回到会话；agent 完成时用通知把用户拉回来（躺床上等回复的场景里，通知就是"叫你"）。
5. **技术选型**：Kotlin + Jetpack Compose，遵循 Material 3，不自造组件体系。

## 非目标（一期）

- iOS
- 多会话/多沙箱 UI（协议层已支持，纯 UI 问题）
- 云端 ASR、多模态音频输入
- Doze/厂商省电策略的自动化适配（记录为真机人工验证项；国产 ROM 可能需用户手动加白名单）
- gateway 侧任何"移动端专用"改动（手机只是又一个持 token 的调用方）

## 风险登记

| 风险 | 对策 |
|---|---|
| Android 进程冻结 | 前台服务（已决策）；真机验证熄屏长任务 |
| 手机网络切换断 SSE | 沿用 runtime 重试 + discarded 语义；真机验证 WiFi↔5G 切换 |
| uniffi/交叉编译工具链 | 构建期问题，先跑通再开发 |
| UI 做不好导致弃用 | UI 详细设计单独立项，按上述硬约束执行 |

## MVP 实测发现（2026-09-26 补录）

MVP 端到端已在模拟器验证通过（手机 → gateway(Seatbelt) → GLM-5.2(SiliconFlow 公网) → 沙箱内 python 解析 PDF → 流式回屏）。实测暴露的后续项：

1. **沙箱预装工具/文件注入 API**：沙箱内无网络（设计使然），pip 不可用；宿主预装的库按 python 路径而定（CLT python3 自带 PyMuPDF，但 `/usr/bin/python3` shim 曾被 denylist 拦截，已修复）。文件注入目前只能宿主侧手工 cp 进 `sandboxes/<id>/`。**需要**：gateway 增加"预置文件上传"协议端点（如 `POST /v1/sandboxes/{id}/fs/upload`，二进制走现有 Base64 语义），或预构建带常用工具的沙箱镜像。
2. ** Seatbelt denylist 与开发工具链的边界**：`/Library/Developer/CommandLineTools` 已加 read-only allow（0f45924）。后续如需 swift/homebrew 等工具链，逐项评估放行，勿放宽 `/Library` 整体。
3. **agent 对受限环境的韧性**：实测中 GLM-5.2 在网络/文件系统受限时能降级实现（纯 Python 解析 CID 字体 PDF）且不编造结果——策略层的硬约束 + agent 自适应是有效的组合，保持"deny 优先"设计。
4. **LAN 明文 HTTP 端到端**：代码就绪（`new_insecure` + UI 开关 + `GATEWAY_BIND`），因测试机 UniVPN 全局路由劫持未完成端到端验证；需无 VPN 环境或分流规则排除 `192.168.0.0/16` 后复测。
5. **真机验证项**（未做）：熄屏长任务、WiFi↔5G 切换、国产 ROM 后台策略。

## 真机端到端实测（2026-09-29 补录）

真机（小米 2407FRK8EC / 澎湃OS / Android 16，arm64）端到端验证通过：手机 → gateway(Seatbelt, Mac LAN IP) → GLM-5.2(SiliconFlow) → 沙箱内 `ls -LA` 执行 → 工具状态与最终回答流式回屏。同时闭环了上一轮的遗留项：

1. **LAN 明文 HTTP 跨网段可用**：手机 10.0.12.x → Mac 10.0.13.x（跨 /24 子网、宿主疑似 VPN 分流、RTT ~145ms）下 `allow_insecure_http` 链路正常，功能不受影响；RTT 偏大提示流量可能仍绕 VPN，SSE 长流体验待复测。
2. **国产 ROM 安装限制**：MIUI/澎湃"USB 安装"要求插 SIM 卡（无 SIM 无法开启）；绕过方式为 `adb push` APK 到 Download 后文件管理器手动安装（需允许"安装未知应用"+"仍然安装"）。
3. **MIUI 禁止 adb 注入输入事件**（INJECT_EVENTS 拒绝）：无法 adb 远程操作 UI，界面配置需手填；uiautomator dump 可用（读 UI 状态 OK）。
4. **gateway work_root 必须 canonical 绝对路径**：`/tmp` 是 `/private/tmp` 的符号链接，配置须写 `/private/tmp/...`（macOS）。
5. **FFI 错误链必须完整透传**：MobileError 只取 anyhow 最外层 context 时，真机上只见 "connect to gateway sandbox" 而看不到底层原因（超时/401/422），排查全靠盲猜；已修为 `format!("{error:#}")` 全链输出（8b7ee6d）。教训：FFI 错误文案是唯一可观测面，必须自带完整原因链。
6. **gateway 独立进程配置**（服务外壳 b353776 首次实战）：token 走环境变量引用（配置文件不落明文）；principal `max_trust_profile` 上限生效；`/healthz` 无鉴权供探测。
7. **待验证**（承接上轮）：熄屏长任务、WiFi↔5G 切换、restricted 沙箱内工具空输出在 UI 上易被误读（"(no output)" + 空目录场景），可考虑 UI 文案优化。

## 后续流程

1. 本 spec 评审通过后，进 writing-plans 出一期实施计划（core 骨架 → gateway 链路 → 最小 UI → 前台服务 → 语音 → 真机验证）。
2. UI 详细设计在 core 骨架跑通后、正式 UI 开发前，单独立项。

---

# 信任分级沙箱（2026-09-26 修订，用户确认）

## 威胁模型变更

原设计假设沙箱内运行**不可信代码**（防失控、防数据外泄），因此默认全断网络、全拒宿主路径。MVP 实测证明该假设与产品定位错位：本产品的 agent 是**用户全权授权的个人代理**，跑的是用户自己发起的任务（装依赖、克隆仓库、访问内网服务），而非第三方提交的代码。继续沿用"防失控代码"策略会让产品在每个真实任务上碰壁（实测：pip 不可用、git clone 不可用、内网 API 不可达）。

新威胁模型：**防事故而非防恶意**。要防的是 agent 的错误命令（误删、误写系统目录、泄漏凭据文件）与 prompt injection 的放大效应，不再防"agent 本身不可信"。用户对宿主机拥有完全所有权，且以自己的账号授权 agent。

## 分级设计

`SandboxConfig` 增加 `trust_profile` 字段（wire + 内部），三档：

| profile | 网络 | 宿主文件系统 | 适用 |
|---|---|---|---|
| `restricted`（默认，现状） | 全断 | 仅 work_dir + allowlist | 跑第三方代码、敏感数据场景 |
| `trusted-lan` | 全放开（allow network*） | work_dir RW + 指定项目目录 RW + 系统区拒写 | 家庭/内网环境日常办公 |
| `trusted-full` | 全放开 | 除系统区与凭据区外全可读写（近宿主） | 完全信任的个人设备 |

**所有档位共同的硬底线**（deny 优先，profile 不能覆盖）：
- 拒写 `/`、`/System`、`/usr`、`/bin`、`/sbin`、`/private/etc`
- 拒读凭据区：`~/.ssh`、`~/Library/Keychains`、Keychain 数据库、浏览器 Cookie/密码库（`~/Library/Application Support/*/Cookies`、`Login Data`）
- 拒读 `.env`（包括 work_dir 内的，防 agent 任务中意外外泄；确需读取时由用户显式逐文件加入 allowlist）

**`trusted-lan` 的额外细则**：
- 项目目录由 principal 的 `fs_allowlist` 注入（现有机制复用，gateway 侧 principal 配置已有该字段），guest 坐标映射到 `/workspace/<name>`
- 网络放开含 DNS；如需再收紧，后置按域 allowlist（bwrap/seatbelt 都只支持 all-or-nothing 或不设限，按域过滤后置）

## 落地面

1. **`llm-harness-sandbox`（seatbelt.rs）**：`generate_policy` 按 profile 参数化——restricted 保持现状；trusted-lan 去掉 `(deny network*)`、追加项目目录 allow；trusted-full 追加 `$HOME` 读写 allow（凭据区 deny 保留在最后，last-match-wins）。
2. **wire 协议**（`RemoteSandboxConfig`）+ **gateway**（`RemoteSandboxConfig → SandboxConfig` 转换、`GatewayPrincipalConfig` 加 profile 上限——principal 声明它最高愿意授予的档位，请求档位超出即拒绝）。
3. **bwrap 后端**：同 profile 语义（网络放开 = 去掉 `--unshare-net`；项目目录 = 追加 bind）。Firecracker 不动（VM 边界本就更强）。
4. **mobile-core / Android UI**：`MobileConfig.trust_profile`，UI 一个三选开关（默认 restricted）。
5. **审计**：create 响应已含 backend 字段，追加 `trust_profile`，UI 常显当前档位（用户应始终知道 agent 此刻的权限级别）。

## 非目标

- 不做按域网络过滤（后置）
- 不做 Firecracker profile 分级（VM 隔离下默认全放，另一条产品线）
- 不做 agent 运行中动态提权（提权 = 新建沙箱，简洁且可审计）

## 验收场景（实施计划的骨架）

1. trusted-lan 沙箱内 `pip install requests` 成功（网络通）
2. `git clone` 内网仓库到挂载的项目目录成功，宿主侧文件可见
3. 尝试写 `/etc/hosts`、读 `~/.ssh/id_ed25519` 被拒（底线生效）
4. principal 上限 `restricted` 时请求 `trusted-lan` 被拒（档位不越权）
5. 手机 UI 档位切换 → 重建沙箱 → 新档位生效
