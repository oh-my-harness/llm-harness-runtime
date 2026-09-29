# Mobile Agent Remote Sandbox MVP 设计

## 概要

本文档定义让手机端客户端使用 Agent 的最小后端能力：Agent 的文件系统和 Shell 操作全部运行在远程 Linux 主机上。第一个切片刻意保持很窄：手机只做薄控制和观察客户端，Agent runtime 和沙箱仍然留在服务器或用户控制下的笔记本电脑上。把完整 Agent runtime 放进手机是后续产品决策，不属于本次 MVP。

本文档追踪两个 runtime 侧工作项：

- **HAR-001**：把现有的 remote sandbox gateway 变成可部署的 Linux 服务。
- **HAR-002**：增加一个可认证、适合移动端使用的 Agent control service，并把远程沙箱注入 Agent runtime。

## 当前能力

runtime 已经具备若干必要基础：

- `llm-harness-sandbox-remote` 提供 HTTPS/Bearer/SSE 的远程沙箱客户端，并适配到 `Sandbox` / `ExecutionEnv`。
- `llm-harness-sandbox-gateway` 提供可认证的 Linux gateway，支持 Bwrap 和 Firecracker 后端选择、持久化沙箱记录以及重启恢复。
- `HarnessBuilder::build(env)` 接受任意 `ExecutionEnv`，因此远程沙箱可以注入 Agent runtime。
- `AgentHarness` 暴露 prompt、steer、abort、state、usage 以及广播事件流。

目前仍缺少手机与 Agent runtime 之间的可部署服务边界：

- gateway 目前主要通过 library API 和测试使用，还不是一个对运维友好的 Linux 服务。
- 还没有可认证的控制面，用于创建 Agent session、路由 prompt 和事件，并把 Agent 绑定到远程沙箱。
- 现有 Inspector 是 localhost 调试界面，不是生产可用的移动端控制面。

## 目标

- 手机客户端可以认证并启动一个由远程 Linux 沙箱支撑的 Agent session。
- prompt 提交、事件流式输出、steering 和 abort 都能通过 HTTPS 工作。
- Agent runtime 可以直接使用 `RemoteSandbox` / `RemoteEnv`，不需要为移动端客户端做特殊分支。
- 沙箱创建、执行、取消和清理遵循现有的 timeout、output-limit、tenant 和 fail-closed 策略。
- gateway 可以部署在 Linux 主机上，并位于 reverse proxy 或 tunnel 之后，而不是内嵌在测试里。
- 在任何移动端 UI 开发之前，可以用 `curl` 或小型脚本测试这个设计。

## MVP 的非目标

- 原生移动应用 UI、语音输入、推送通知或应用商店打包。
- 在手机进程里运行完整 Agent runtime。
- Firecracker 的生产验证；Bwrap 是默认后端。
- 完整的生产审计、token rotation、网络 allowlist、磁盘配额或 cgroup 部署验证。
- 多节点调度、分布式 session 存储或水平扩展。
- 把现有 Inspector 替换为通用管理产品。

这些限制并不意味着后续不兼容，而是为了避免第一个实现同时变成移动端产品和生产控制面。

## MVP 拓扑

```text
phone client (HAR-003, 不在本文范围内)
   |
   | HTTPS + bearer + JSON/SSE
   v
agent control service (HAR-002)
   |- session registry
   |- agent runtime manager
   |- event fanout
   |- remote sandbox handle
   |
   | existing remote sandbox protocol
   v
remote sandbox gateway (HAR-001)
   |- BwrapSandbox (default)
   |- FirecrackerSandbox (explicit opt-in)
```

本地开发时，control service 和 sandbox gateway 可以在同一进程内运行；目标部署中它们是两个独立服务。保持分离有两个好处：

1. sandbox gateway 可以在不绑定 Agent session 语义的情况下执行租户隔离。
2. 移动端控制面可以独立于沙箱协议演进。

control service 是唯一代表手机访问 sandbox gateway 的组件。手机永远不会收到 gateway host path、gateway credential 或直接访问 gateway 的 URL。

## 新的服务边界

### `llm-harness-agent-control`

新增一个小型 service crate 作为适合移动端的控制面。它应该负责：

- 可认证的 HTTP routes；
- Agent session 元数据；
- 从 session id 到 sandbox id 和 `AgentHarness` 的映射；
- 后台 prompt 执行；
- 面向 SSE 客户端的事件 fanout；
- abort 和 shutdown 协调。

它不应该负责：

- 沙箱隔离策略；
- Firecracker 生命周期细节；
- 远程协议解析，应通过 `RemoteSandboxClient` 使用；
- 移动端 UI 状态管理。

这个 crate 应该刻意保持小型。大部分 Agent 语义继续留在 `llm-harness-agent`；大部分沙箱语义继续留在 `llm-harness-sandbox-gateway` 和 `llm-harness-sandbox-remote`。

## Gateway 部署外壳

### 配置

HAR-001 应该增加一个可运行的 gateway binary，并提供显式配置项：

- bind address；
- work root；
- 一个或多个 principal 和 bearer-token hash；
- 后端选择：Bwrap 或 Firecracker；
- 执行和请求限制；
- logging level。

第一版 gateway 配置使用 JSON，方便运维复制和校验。bearer token 只保存引用名，实际值从环境变量读取。这样既保持配置文件可审查，也避免把 secret 写进磁盘上的明文配置或进程参数。后续如需要，可在不改变安全边界的前提下增加环境变量覆盖或 TOML 展开层。

### 运行行为

gateway 进程应该：

- 只绑定到显式配置的地址；
- 暴露 readiness 和 health endpoints；
- 初始化现有的持久化 registry；
- 在 SIGTERM/Ctrl-C 时优雅关闭；
- 根据现有 gateway recovery 语义停止或恢复沙箱；
- 输出结构化 lifecycle 事件。

`/healthz` 和 `/readyz` 保持无鉴权，方便反向代理和编排器探测。这两个端点只返回 backend 和状态，不暴露 principal、文件路径或内部配置。业务路由继续走原有 bearer-token 鉴权。

### TLS

MVP 不应该在 gateway 进程内实现 TLS。替代方案是：

- 本地开发可以使用 loopback HTTP；
- 生产部署用 Caddy、Nginx、sidecar 或 Cloudflare Tunnel、Tailscale Funnel 这类 tunnel 终止 TLS；
- control service 必须为非 loopback gateway endpoint 配置 HTTPS；现有 remote-client 策略已经会拒绝非 loopback 的 plain HTTP。

这样可以让服务专注于沙箱执行，把传输层策略交给部署环境。

## Agent Control API

MVP API 应该使用 JSON 执行控制操作，使用 SSE 做流式事件。

| 方法和路径 | 成功返回 | 语义 |
|---|---|---|
| `POST /v1/agent-sessions` | `201` | 创建 Agent session 和对应远程沙箱 |
| `GET /v1/agent-sessions/{id}` | `200` | 返回 session 状态、沙箱状态和元数据 |
| `POST /v1/agent-sessions/{id}/prompt` | `202` | 异步启动一个 Agent turn |
| `POST /v1/agent-sessions/{id}/steer` | `202` | 向当前或下一个 turn 注入指引 |
| `POST /v1/agent-sessions/{id}/abort` | `204` | 中止当前 turn |
| `POST /v1/agent-sessions/{id}/files/upload` | `200` | 批量上传文本或 Base64 文件到 session 沙箱 |
| `POST /v1/agent-sessions/{id}/files/read` | `200` | 按文本或 Base64 读取 session 沙箱文件 |
| `GET /v1/agent-sessions/{id}/events` | `200` + SSE | 流式输出 Agent 事件 |
| `DELETE /v1/agent-sessions/{id}` | `204` | 停止 Agent 并删除沙箱 |

### 创建 session 请求

最小的创建请求应包含：

```json
{
  "model": "claude-3-5-sonnet",
  "system_prompt": "optional",
  "sandbox": {
    "backend": "bwrap"
  }
}
```

具体配置 schema 可以在实现时继续细化，但第一版应支持：

- model 选择；
- 可选 system prompt；
- sandbox 后端选择；
- 可选 execution timeout；
- 可选 output limit。

control service 应拒绝未知或不安全字段，而不是静默降级。

### Prompt 语义

`prompt` 应立即返回 `202 Accepted`。服务随后在后台调用 `AgentHarness::prompt`，避免 HTTP 请求在 Agent 运行期间一直保持打开。

响应应包含稳定的 prompt id 或 turn id，客户端可以用它关联后续事件。

### 事件流

事件流应把现有 `AgentHarnessEvent` 包装成稳定的 JSON envelope：

```json
{
  "type": "text_delta",
  "session_id": "...",
  "turn_id": "...",
  "timestamp": "...",
  "data": {}
}
```

第一版应转发：

- text delta；
- tool-call 生命周期事件；
- phase 变化；
- 错误；
- turn 完成。

事件流应能被浏览器 `EventSource`、原生 HTTP 客户端或 `curl` 读取。第一版的重连可以基于 session 状态和当前事件缓冲区；MVP 不要求 durable replay。

### Abort 语义

`abort` 应取消当前 Agent turn，但不一定要删除沙箱。control service 应该：

- 调用 `AgentHarness::abort`；
- 发出 abort 事件；
- 保留沙箱供下一次 prompt 使用；
- 通过现有远程协议清理孤儿执行。

## Session 生命周期

### 状态

control service 至少应跟踪：

| 状态 | 含义 |
|---|---|
| `Creating` | 沙箱或 Agent runtime 正在启动 |
| `Ready` | Agent 已准备好接受 prompt |
| `Running` | 一个 prompt 正在执行 |
| `Stopping` | abort、shutdown 或 cleanup 正在进行 |
| `Failed` | session 无法继续 |
| `Deleted` | session 和沙箱都已清理 |

移动端客户端只需要看到这些 control-plane 状态，不需要理解 gateway 内部的沙箱状态。

### 归属

每个 session 属于一个 principal。control service 应保证：

- 只有 owning principal 可以读取、prompt、steer、abort、stream 或删除该 session；
- 每个 session 映射到一个 sandbox record；
- sandbox credential 永远不返回给手机；
- principal 级别的 session 上限可配置。

### 清理

session 删除应先停止 Agent，然后通过远程协议删除沙箱。清理操作应是幂等的。

如果清理的任何一步失败，服务应保留足够的元数据用于重试或报告孤儿资源。不能静默忘记一个仍在运行的沙箱。

### 重启行为

MVP 不需要恢复内存中的 `AgentHarness` 状态。control-service 重启时：

1. 加载持久化的 session 元数据；
2. 找出无法恢复的 session；
3. 将它们标记为 `Failed`；
4. 尽可能删除对应的远程沙箱；
5. 报告无法恢复的孤儿资源。

持久化对话回放和 live agent migration 属于后续工作。

## 安全模型

### 认证

所有移动端控制端点都要求 bearer token。MVP 中：

- principal 可以由配置的用户 token 表示；
- control service 校验 principal；
- control service 使用独立的 service credential 访问 sandbox gateway；
- 手机永远不接收 sandbox gateway token；
- model-provider credential 只保存在服务端，不进入沙箱工作区，也不出现在手机可见响应中。

control service 应复用 sandbox gateway 的通用 bearer-token 纪律：不允许空白字符或控制字符、不记录 credential、也不在错误信息中泄漏 credential。

### 授权

MVP 至少支持一个 principal，但不能意外省略授权：

- session 按 principal 隔离；
- 沙箱以 principal 的身份创建；
- admin 或跨 principal 访问不属于 MVP；
- 应强制执行 principal 级别的 session 和请求限制。

### 传输

- loopback HTTP 允许用于开发；
- 非 loopback 客户端必须使用 HTTPS；
- TLS 终止交给 reverse proxy 或 tunnel；
- control service 不应跟随携带认证信息的重定向；
- SSE 和控制响应不应缓存 credential 或 host path。

## 沙箱策略

Bwrap 是 MVP 的默认后端。它提供 namespace 和 capability 隔离，并且已被 gateway 支持。

默认沙箱应保持受限：

- 文件系统访问锚定在 guest workspace 内；
- 默认拒绝网络；
- 不支持的策略字段 fail closed；
- Shell 执行有 timeout 和 output limit；
- 每个沙箱都有显式 owner。

文件访问是移动端与 Agent 共享工作区产物的补充通道。上传和读取都只作用于 owning principal 的 session；路径仍由 sandbox gateway 的 allowlist/denylist 校验。批量上传默认一次最多 32 个文件，控制服务请求体默认上限为 8 MiB；单个读取文件默认上限也是 8 MiB。二进制内容使用 standard Base64，服务端在解析失败或路径越界时返回明确错误。

Firecracker 可以作为显式 opt-in 后端保留。在真实 Linux/KVM 部署验证之前，MVP 不应宣称其具备生产级 VM 隔离能力。

## 可观测性

第一版应提供：

- session create、prompt、steer、abort、delete 和 cleanup 的结构化日志；
- `session_id`、`principal`、`sandbox_id` 和 `request_id` 字段；
- control-service 和 gateway 启动/关闭的 lifecycle 日志；
- 日志中不包含 bearer token、model credential 或原始 prompt 内容；
- 活跃 session、活跃 prompt 和 cleanup failure 的基础计数器。

OpenTelemetry 和 immutable audit log 很有价值，但不是移动端 MVP 的必备项。

## 测试计划

### 单元测试

- 配置解析和校验；
- session 状态转换；
- principal ownership 检查；
- cleanup 和重启 reconciliation。

### 服务集成测试

使用 in-process gateway 或 fake remote sandbox 测试：

- create session；
- submit prompt；
- 接收流式事件；
- 在运行中 steer；
- abort 运行；
- delete session 并确认沙箱删除；
- 拒绝其他 principal 的访问；
- 拒绝不支持的沙箱策略。

### Gateway 服务测试

- 从 TOML/env 配置启动；
- 暴露 readiness；
- 接受现有远程协议；
- 重启后恢复 registry 状态；
- 关闭时不泄漏子进程。

### 端到端 smoke

在开发移动端 UI 前，应先通过脚本化 smoke test 验证：

```bash
create session -> prompt -> stream events -> steer -> abort -> delete session
```

使用 `curl` 实现即可，足以验证后端契约。

## 验收标准

HAR-001 完成的条件是：

- [ ] sandbox gateway 可以通过显式配置启动；
- [ ] 已实现 health/readiness 和优雅关闭；
- [ ] Bwrap 是默认且经过测试的后端；
- [x] 部署文档展示 reverse-proxy 或 tunnel 配置；
- [ ] 正常关闭后没有泄漏沙箱资源。

HAR-002 完成的条件是：

- [x] control service 可以创建由 `RemoteSandbox` 支撑的 Agent session；
- [x] prompt、event streaming、steer 和 abort 的控制契约有集成测试；真实 gateway E2E 已覆盖完整流程；
- [x] session 按已认证 principal 隔离；
- [x] session 删除会停止 Agent 并删除沙箱；
- [x] 重启 reconciliation 不会留下不可达但仍在运行的沙箱；
- [x] 集成测试覆盖移动端控制契约。

当脚本化客户端能完成完整流程时，组合 MVP 才算可用：

```text
authenticate -> create session -> prompt -> receive events -> abort -> delete
```

## 后续工作

在后端 MVP 之后：

- 开发移动端客户端或移动端 web UI；
- 增加持久化对话历史和事件回放；
- 为长时间运行的 turn 增加推送通知；
- 集成 immutable audit logging；
- 增加 token rotation 和 revocation；
- 实现网络 allowlist 和磁盘配额；
- 在真实 Linux/KVM 主机上验证 Firecracker；
- 增加生产 metrics、alerting 和 runbooks。
