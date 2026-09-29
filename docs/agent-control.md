# Agent Control Service

`llm-harness-agent-control` 是手机端 Agent 的控制服务。手机应用不需要直接持有 gateway token，只需要访问这个服务；服务内部把 Agent harness 和 `RemoteSandbox` 组合起来，并提供经过身份认证的 REST/SSE 接口。

## 当前能力

- 创建一个由远程沙箱支撑的 Agent session；
- 以异步方式提交 prompt，并继续接收 SSE 事件；
- 支持 steer、abort、查询状态和删除 session；运行中 steer 直接注入当前 turn，idle steer 进入下一 turn 的有界队列；
- 按 principal 隔离 session，并限制每个 principal 可创建的 session 数量；
- 服务关闭时停止 Agent，并关闭所有仍在该进程内的沙箱。

这不是持久化服务：session 和 Agent 上下文只保存在当前进程内存里，进程重启后需要手机端重新创建 session。

## 启动

先启动 sandbox gateway，然后准备一份 JSON 配置：

```json
{
  "bind": "127.0.0.1",
  "port": 7090,
  "default_model": "claude-3-5-sonnet",
  "gateway": {
    "base_url": "http://127.0.0.1:7080",
    "token_env": "AGENT_CONTROL_GATEWAY_TOKEN",
    "allow_insecure_http": true
  },
  "llm": {
    "provider": "anthropic",
    "api_key_env": "ANTHROPIC_API_KEY"
  },
  "principals": [
    {
      "principal": "phone",
      "token_env": "AGENT_CONTROL_PHONE_TOKEN",
      "max_trust_profile": "trusted-lan",
      "max_sessions": 4
    }
  ]
}
```

然后在仓库根目录运行：

```sh
export AGENT_CONTROL_GATEWAY_TOKEN="<gateway token>"
export AGENT_CONTROL_PHONE_TOKEN="<phone access token>"
export ANTHROPIC_API_KEY="<LLM API key>"
cargo run -p llm-harness-agent-control --bin agent-control -- agent-control.json
```

`base_url` 是 gateway 地址。`allow_insecure_http` 只适合 loopback 或受信任的内网实验环境；对非 loopback 的普通 HTTP，remote sandbox client 仍会拒绝。生产环境应通过反向代理或 tunnel 终止 TLS，并把 control service 的公网入口也配置为 HTTPS。

## 接口

所有 `/v1/...` 接口都需要 `Authorization: Bearer <phone access token>`。`GET /healthz` 不需要鉴权，只用于探活。

| 方法 | 路径 | 语义 |
|---|---|---|
| `POST` | `/v1/agent-sessions` | 创建 Agent session 和远程沙箱 |
| `GET` | `/v1/agent-sessions/{id}` | 查询 session 状态 |
| `POST` | `/v1/agent-sessions/{id}/prompt` | 异步开始一个 Agent turn |
| `POST` | `/v1/agent-sessions/{id}/steer` | 向 Agent 注入一条指引 |
| `POST` | `/v1/agent-sessions/{id}/abort` | 中止当前 Agent turn |
| `GET` | `/v1/agent-sessions/{id}/events` | 订阅 SSE 事件 |
| `DELETE` | `/v1/agent-sessions/{id}` | 停止 Agent 并关闭沙箱 |

创建 session 的请求体是：

```json
{
  "model": "claude-3-5-sonnet",
  "system_prompt": "optional",
  "trust_profile": "restricted"
}
```

`trust_profile` 支持 `restricted`、`trusted-lan` 和 `trusted-full`。请求值不能超过配置里给该 principal 设置的 `max_trust_profile`。

提交 prompt：

```json
{
  "prompt": "查看当前目录，并总结有哪些文件"
}
```

服务返回 `202 Accepted` 和一个 `prompt_id`。SSE 事件的外层信封会携带这个 `prompt_id`；从 assistant message 可读取到底层 turn 标识后，`turn_id` 也会随之填充。早期 run 事件可能还没有 `turn_id`。

## SSE 事件

`/events` 返回的每个事件的 `data` 是一行 JSON，格式为设计文档中定义的信封结构：

```json
{
  "type": "text_delta",
  "session_id": "...",
  "prompt_id": "...",
  "turn_id": null,
  "timestamp": "...",
  "data": {
    "text": "..."
  }
}
```

事件类型包括：

- `text_delta`：模型输出的增量文本；
- `tool_started` / `tool_finished`：工具开始和结束；
- `run_started` / `turn_started` / `run_finished`：当前 run 开始、底层 turn 标识出现和完成；
- `failed`：Agent 或事件流出错；
- `aborted`：请求中止；
- `session_closed`：session 已删除或服务正在停止。

顶层 `data` 字段包含事件负载。为了避免中间断线后丢失所有内容，MVP 使用有界的 broadcast 队列；客户端连接较慢时可能收到 `failed` 事件，提示已经落后了多少条事件。

## 安全边界

- 手机端 token 只能访问 control API；gateway token 留在 control service 进程环境中。
- token 通过环境变量配置，服务启动后读取；不要把明文 token 写入仓库。
- session 只能被创建它的 principal 查询、prompt、steer、abort 或删除。
- 未知请求字段会被拒绝。
- 沙箱仍由 gateway 和 platform sandbox 策略约束；control service 不绕过这些限制。

## 重启与孤儿沙箱清理

在配置中设置 `persistence_path` 可以启用孤儿沙箱跟踪：

```json
{
  "persistence_path": "./agent-control-sessions.json"
}
```

启用后，control service 会在每次创建 session 时把 sandbox ID 写入该文件，删除 session 时移除对应条目。进程重启时，如果该文件中仍有 sandbox ID，control service 会向 gateway 发送 `DELETE /v1/sandboxes/{id}` 请求清理这些孤儿沙箱，然后清空文件。

如果部分清理失败（例如 gateway 暂时不可达），文件中会保留未清理的 sandbox ID，下次启动时重试。

## 测试

```sh
cargo test -p llm-harness-agent-control
```

测试覆盖 manager 层的创建、查询、prompt 事件流、session 数量限制和删除；也覆盖路由的 health 探活和未鉴权拒绝。另有 idle/active steer、SSE 关联字段、持久化，以及真实 gateway（Linux + Bwrap）完整流程 E2E 测试。
