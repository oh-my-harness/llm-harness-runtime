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

## 后续流程

1. 本 spec 评审通过后，进 writing-plans 出一期实施计划（core 骨架 → gateway 链路 → 最小 UI → 前台服务 → 语音 → 真机验证）。
2. UI 详细设计在 core 骨架跑通后、正式 UI 开发前，单独立项。
