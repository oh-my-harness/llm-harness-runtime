# Agent Inspector 调试指南

> `llm-harness-inspector` 是开发者调试工具（非运维监控、非终端用户面板）：通过 REST + WebSocket 实时观察 agent 状态与事件流，并支持运行中交互（注入消息、中止、发起新任务）。
>
> 只依赖 `llm-harness-agent` 与 `llm-harness-types`；全部实现 feature-gated（`inspect`），不启用时零成本。

---

## 快速开始

### 零代码挂载（live-tests / 任何走 `make_harness` 工厂的场景）

```bash
source .env
SENZA_INSPECT=8080 cargo run -p llm-harness-live-tests --example 02_tool_calling
# 🔧 Agent Inspector listening on http://127.0.0.1:8080  (ws token: ...)
# 浏览器打开 http://127.0.0.1:8080 即可实时观察事件流
```

### 代码挂载

```rust
use llm_harness_inspector::Inspector;
use std::sync::Arc;

// 最小路径
let inspector = Inspector::builder(Arc::clone(&harness))
    .port(8080)
    .mount()
    .await?;

// 零代码：SENZA_INSPECT=8080 环境变量（无则返回 None）
if let Some(builder) = Inspector::from_env(Arc::clone(&harness)) {
    let inspector = builder.mount().await?;
}

// 显式关闭（Drop 也会触发优雅关闭）
inspector.shutdown().await;
```

`mount()` 绑定 `127.0.0.1`（仅本机访问），`tracing::info!` 打印地址与 WebSocket token。

---

## API 一览

### REST

| 端点 | 方法 | 说明 |
|------|------|------|
| `/` | GET | 内置 Web UI（单文件 HTML，自动重连、暗色主题） |
| `/api/agent/config` | GET | 配置快照：model、thinking_level、tools、phase、budget 等 |
| `/api/agent/session` | GET | 当前 session 消息历史与分支 |
| `/api/agent/usage` | GET | token 用量与成本 |
| `/api/agent/skills` | GET | skill 列表 |
| `/api/inject` | POST `{ "text": "..." }` | `harness.steer()`——运行中注入引导消息，空闲时留给下次 prompt |
| `/api/abort` | POST | `harness.abort()`——中止当前 turn |
| `/api/prompt` | POST `{ "text": "..." }` | 发起新任务（仅 harness 空闲时允许，否则 409） |

### WebSocket `WS /api/events`

- **连接初始化**：先下发一条 `Snapshot`（`harness.state()` + `harness.usage()` 当前快照），之后切换为增量事件透传
- **消息格式**：`{ "type": "<AgentHarnessEvent variant>", "inner_type": "<内层 AgentEvent variant 或 null>", "timestamp": "...", "data": {...} }`
- **前端路由**：`inner_type` 有值时按其渲染（如 `TextDelta`/`MessageUpdate`/`ToolExecutionStart`），否则按 `type`（`PhaseChange`/`CompactionStart` 等）
- **容错**：Lagged-tolerant（broadcast 溢出不断连）；非浏览器客户端（无 Origin 头）必须带 `?token=` 查询参数，防 WebSocket CSRF
- **事件投影**：不可序列化的进程内字段（如 `ToolResult.extensions`）在投影层跳过，事件语义不裁剪

---

## 交互式调试（human-in-the-loop）

典型工作流（example 25 演示）：

1. `SENZA_INSPECT=8080` 启动 agent，`harness.prompt()` 开始任务
2. 浏览器打开 `http://127.0.0.1:8080`，实时看 token 流、工具调用、成本
3. 任务方向跑偏 → `/api/inject` 注入 steer 消息，模型在下一 turn 消化
4. 死循环/烧钱 → `/api/abort` 中止当前 turn
5. 结束后 `/api/agent/usage` 核对成本

---

## 安全边界

- 仅绑定 `127.0.0.1`，默认不暴露局域网
- WebSocket 对非浏览器客户端要求 token（mount 时生成、日志打印）；浏览器客户端经 Origin 校验（拒绝非 localhost）
- 调试工具不应在生产环境启用——不设 `SENZA_INSPECT` 即完全零开销

---

## 设计文档

- 完整设计：`docs/design/2026-08-22-inspector-web-api-design.md`
- 交互示例：`crates/llm-harness-live-tests/examples/25_inspector_interact.rs`
