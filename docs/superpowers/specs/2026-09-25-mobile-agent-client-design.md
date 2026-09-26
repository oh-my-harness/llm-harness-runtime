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

## 后续流程

1. 本 spec 评审通过后，进 writing-plans 出一期实施计划（core 骨架 → gateway 链路 → 最小 UI → 前台服务 → 语音 → 真机验证）。
2. UI 详细设计在 core 骨架跑通后、正式 UI 开发前，单独立项。
