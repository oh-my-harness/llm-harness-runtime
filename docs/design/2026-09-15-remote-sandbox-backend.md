# Remote Sandbox Backend Design

## 目标与非目标

`llm-harness-sandbox-remote` 为 `Sandbox` / `ExecutionEnv` 提供远程网关客户端。它把 runtime 侧的文件和 shell 操作转换为 HTTP/SSE 协议，并保留超时、取消、输出上限和错误语义。

`llm-harness-sandbox-gateway` 提供 Linux Bwrap 后端实现，并已接入一个 Linux Firecracker 后端。Bwrap 是可嵌入的 container/namespace 级 gateway；Firecracker 是 gateway 内的 VM 级后端，但仍未完成生产部署验证。

非目标：

- 不在本 crate 内实现 Firecracker、Kata、gVisor 或容器隔离。
- 不承担网关侧多租户调度、镜像管理、网络策略执行和资源限额执行；registry 只提供单个 work root 内的持久化恢复，不是分布式控制面。
- 不提供 UI 或运维控制面。

因此，本 crate 的生产边界是“协议客户端 + runtime 适配器”。只有部署了符合本协议并真实执行隔离策略的 gateway 后，整个远程沙箱才是生产可用。

## 架构

```text
runtime process
  └─ RemoteSandbox / RemoteEnv
       └─ RemoteSandboxClient (HTTPS + Bearer + SSE)
            └─ remote sandbox gateway
                 └─ llm-harness-sandbox-gateway (Linux)
                      ├─ BwrapSandbox: namespace + capability isolation
                      └─ FirecrackerSandbox: VM isolation + guest agent
```

`RemoteSandbox::connect` 创建远端 sandbox 并保存 gateway 返回的 `sandbox_id`、`work_dir` 和 backend 标识。如果 create 响应未通过协议校验，客户端会用响应中的有效 sandbox id 尽力删除远端实例，再返回协议错误。远端资源存在与 `Running` 状态分开记录：`Creating` 时 `is_running()` 为 false。`RemoteEnv` 与 sandbox handle 共享远端资源所有权，因此仅 drop handle 不会清理实例；最后一个引用 drop 时会在独立 cleanup 线程中尽力删除远端实例，`shutdown()` 成功后只删除一次。gateway 崩溃后，Bwrap 活动命令通过 `--die-with-parent` 终止；重启时基于持久化 registry 恢复 sandbox id、owner、配置和 work root，并把 Bwrap 记录标记为 `Running`，但活动 execution id 不会恢复。Firecracker 记录重启时恢复为 `Creating`，客户端随后通过 start 端点显式完成启动。`RemoteEnv` 实现 `ExecutionEnv`，所有路径均使用 guest 坐标系，不泄漏 gateway 宿主机路径。

## HTTP 协议

所有请求携带：

- `Authorization: Bearer <token>`
- `Accept: application/json` 或 `text/event-stream`
- `Content-Type: application/json`（有请求体时）
- `x-request-id: <uuid>`，便于网关侧关联日志

HTTPS 客户端使用 rustls，并启用 native root store；因此企业私有 CA 可通过平台证书库（Unix 上包括 `SSL_CERT_FILE`）注入，而不要求把 CA 打包进应用。

JSON 响应必须是 `application/json`（忽略参数并按大小写不敏感比较 media type），且客户端会检查精确成功状态码：创建 sandbox 为 `201 Created`，其他 JSON 操作为 `200 OK`。无返回值的操作必须返回 `204 No Content` 且响应体为空；客户端会拒绝 `200` 加空 body、`204` 加 body、`202 Accepted` 或其他成功状态。

| 方法与路径 | 成功响应 | 语义 |
|---|---|---|
| `POST /v1/sandboxes` | `201` + `CreateSandboxResponse` | 创建 sandbox |
| `GET /v1/sandboxes/{id}` | `200` + `RemoteSandboxStatus` | 查询状态 |
| `DELETE /v1/sandboxes/{id}` | `204` | 删除实例 |
| `POST /v1/sandboxes/{id}/start` | `204` | 启动重启后处于 `Creating` 状态的 sandbox |
| `POST /v1/sandboxes/{id}/reset` | `204` | 重置到初始状态 |
| `POST /v1/sandboxes/{id}/cleanup` | `204` | 清理临时资源 |
| `POST /v1/sandboxes/{id}/fs/read` | `200` + `ReadFileResponse` | 读文本或二进制文件 |
| `POST /v1/sandboxes/{id}/fs/write` | `204` | 写入或追加文件 |
| `POST /v1/sandboxes/{id}/fs/info` | `200` + file info | 查询元数据 |
| `POST /v1/sandboxes/{id}/fs/list` | `200` + entries | 列目录 |
| `POST /v1/sandboxes/{id}/fs/exists` | `200` + boolean | 判断路径存在 |
| `POST /v1/sandboxes/{id}/fs/create-dir` | `204` | 创建目录 |
| `POST /v1/sandboxes/{id}/fs/remove` | `204` | 删除文件或目录 |
| `POST /v1/sandboxes/{id}/fs/create-temp-dir` | `200` + guest path | 在 work_dir 下创建临时目录 |
| `POST /v1/sandboxes/{id}/exec` | `200` + SSE | 执行 shell |
| `POST /v1/sandboxes/{id}/exec/{execution_id}/kill` | `204` | 终止执行 |

`sandbox_id` 和 `execution_id` 是 opaque identifier，但必须由 ASCII 字母、数字、`-`、`_` 组成。该限制避免路径穿越、query 注入和 URL 片段歧义。

## 认证与传输

- 非 loopback endpoint 必须使用 HTTPS；loopback HTTP 仅用于本机测试和开发。
- endpoint 不允许携带 URL credentials、query 或 fragment。
- HTTP client 禁用 redirect 和环境代理，避免 gateway 响应或宿主环境把认证请求转发到未预期位置。
- bearer token 不允许为空、控制字符或空白字符。
- `RemoteSandboxClientConfig` 的 `Debug` 输出会隐藏 token。
- token 只标识调用方，不代表 sandbox 归属。gateway 必须把 token 映射到租户/调用方，并确保只能操作其拥有的 sandbox。

## 文件语义

- guest path 必须是合法 UTF-8，并以 `/` 开头；不允许包含 `.`、`..` 组件或 NUL。
- 所有协议路径和 `fs_allowlist` / `fs_denylist` 均使用 guest 坐标系。合法路径必须是 `/workspace` 或其子路径，且不得包含反斜杠、NUL、`.` 或 `..` 路径段。`fs_allowlist` 是允许访问的前缀集合；`fs_denylist` 是其例外，deny 优先。
- 文本内容使用 UTF-8 JSON；二进制内容使用 standard Base64。
- `create_temp_dir` 必须把结果放在稳定 guest `work_dir`（`/workspace`）下，不允许映射到 gateway 宿主机 `/tmp`。

客户端会验证 read 返回的内容类型与请求的 `binary` 匹配，并拒绝非法 Base64。
文件元数据和目录列表中的 guest path 必须通过同一规则校验；`file_info` 返回的路径必须与请求一致，目录列表条目必须是请求目录的直接子路径，`create_temp_dir` 返回的路径必须位于 sandbox `work_dir` 下。

执行请求中的 `cwd` 使用同一 guest path 规则；环境变量 key 必须非空且不包含 `=` 或 NUL，环境变量值不允许包含 NUL。`create_temp_dir` 的 prefix 必须是单个路径组件，不能包含 `/`、`\`、NUL、`.` 或 `..`。

## Shell 与 SSE

执行请求包含 command、cwd、env、timeout、max output bytes。成功响应必须是 `text/event-stream`，并按以下顺序发送事件：

1. `started`：携带 `execution_id`
2. 零个或多个 `stdout` / `stderr`
3. `exit`、`timeout` 或 `error` 终止事件

响应可同时通过 `x-execution-id` header 提前返回执行 ID；如果 header 与 `started` 不一致，客户端会报协议错误。收到任何输出或终止事件前必须已知 execution id，否则客户端拒绝该流，避免取消时无法 kill 远端进程。

SSE 行结束符支持 `LF`、`CRLF` 和 `CR`，且 `CRLF` 允许跨网络 chunk 分割。每个事件的 `event` 字段必须与 JSON payload 的 `type` 一致，否则客户端拒绝该流。SSE 事件大小受 `max_output_bytes` 推导出的上限约束，最终 `stdout` / `stderr` 也不得超过请求上限。gateway 仍必须自己执行输出截断；客户端限制只是防御协议错误，不是替代。

## 超时与取消

- `ShellOptions.timeout` 会被限制在客户端配置的 `max_exec_timeout` 内；未请求超时时使用该上限。协议超时精度为毫秒，低于 1ms 的请求会被拒绝。gateway 收到有效超时后 kill 进程并发送 `timeout` 事件。
- 客户端额外设置有效 shell timeout + 5s 的 HTTP 请求超时，防止 gateway 永不终止 SSE。
- `ExecutionEnv` 的文件和元数据请求会响应取消 token；请求仍在途时取消会返回 `EnvError::Aborted`。
- 取消 token 触发后，一旦知道 execution id 就调用 kill endpoint；如果取消发生在 `started` 前，会在收到 execution id 后补发 kill。
- 若命令已经退出，kill 可能返回 `execution_not_found`。abort 路径会把该错误视为幂等结果并继续等待退出事件；显式调用 `kill_execution` 时仍会返回该 gateway 错误。

## 错误映射

错误响应使用：

```json
{
  "code": "machine_readable_code",
  "message": "human readable message",
  "path": "/optional/guest/path"
}
```

客户端把：

- `404 + path` 映射为 `EnvError::NotFound`
- `403 + path` 映射为 `EnvError::PermissionDenied`
- `401`、`408`、`504` 映射为带上下文的 `EnvError::Other`
- SSE `timeout` / abort 分别映射为 `EnvError::ShellTimeout` / `ShellAborted`
- 其他 gateway 错误保留 status、code、message 和可选 path

错误 body 最多读取 64 KiB；JSON 控制响应默认最多 64 MiB，可通过 `RemoteSandboxClientConfig::with_max_response_bytes` 调整。

## Gateway 生产要求

一个生产 gateway 至少必须：

1. 在独立 VM/container/microVM 中执行命令，而不是直接 fork gateway 进程。
2. 强制 CPU、进程数、内存、磁盘和执行时长限额。
3. 强制文件 allowlist/denylist，deny 优先。
4. 强制网络 allowlist，默认拒绝。
5. 为每个 sandbox 提供独立文件系统根和临时目录。
6. 将 sandbox 生命周期与租户绑定，并拒绝跨租户访问。
7. 对孤儿实例、崩溃实例和超时执行做后台回收。
8. 提供结构化生命周期、审计和诊断日志，并保留 `x-request-id`。
9. 支持 TLS、token 轮换和最小权限服务账号。
10. 明确定义 reset 与 delete 的资源释放语义。

本 crate 的 `tests/common/mod.rs` 是本地 `sh` 测试网关，只用于验证协议和客户端行为。它没有实现真实隔离、网络策略或资源限额，不能作为生产 gateway。

## Linux gateway backends

VM-level backend selection and threat model are defined separately in
[`2026-09-18-vm-sandbox-backend.md`](2026-09-18-vm-sandbox-backend.md).

`llm-harness-sandbox-gateway` 使用 `BwrapSandbox` 实现远程协议：

- 使用 SHA-256 token digest 和常数时间比较做 Bearer 认证；同一 principal 可配置多个 token，供轮换使用。
- gateway 使用绝对、canonical、0700 且非 symlink 的 work root（默认 `/tmp/llm-harness-sandbox-gateway`），并在其下创建 0700 `sandboxes` 目录与 0600 regular-file `.gateway.lock`。
- gateway 启动时用 `flock(LOCK_EX | LOCK_NB)` 独占 work root，拒绝第二个 gateway 共用同一 root；随后只删除 `sandboxes` 下名字可解析为 UUID 的直接子目录。
- 每个 sandbox 由 gateway 生成独立 0700 host work root，并将其映射为稳定 guest 坐标 `/workspace`；create 请求中的 `work_dir` 只能为空或 `/workspace`，远程调用方不能选择宿主机路径，invalid create 请求不会遗留 work root。
- sandbox 记录绑定创建 principal；其他有效 token 访问时返回 `404`，避免跨租户枚举。
- 文件操作通过 `cap-std` capability root 执行，并在协议层统一校验 guest 路径、`fs_allowlist` 与 `fs_denylist`；principal 的 `fs_allowlist` 使用 guest `/workspace` 前缀，作为请求 allowlist 的默认值和上限。shell 通过 `bwrap --unshare-all` 执行：allowlist 先用空目录覆盖 `/workspace`，再挂载允许的 host 子路径；denylist 在最终挂载点用空目录或 `/dev/null` 覆盖。默认网络为 deny。
- shell 执行有 gateway 级 timeout/output 上限，SSE 通道使用有界队列，JSON 请求体有上限。
- reset 清空 owned work root，delete 取消执行并释放 work root；删除时若有活跃请求会返回 `409` 而不是半途移除状态。
- 启动恢复对 `sandboxes` 下的非 UUID 名称、symlink 或非目录条目失败关闭，并保留异常条目供运维检查。
- registry 使用 work root 下 0600 regular-file SQLite WAL 数据库持久化 sandbox id、owner、配置和状态；重启时 fail-closed 地 reconcile registry 与 work root，删除孤儿和 `Deleting` 记录，并恢复仍有效的 sandbox。
- `fs_allowlist` / `fs_denylist` 在协议层、`BwrapEnv` 文件 API 和 shell mount namespace 三处使用组件级路径匹配保持一致；非空 `net_allowlist`、`max_disk_mb` 以及本机无法执行的 cgroup v2 限额都会失败关闭。
- 请求日志保留 `x-request-id`、方法、路径、状态和耗时，不记录 bearer token。

该 gateway 的 `serve` 只提供 HTTP listener；生产部署必须由 sidecar、ingress 或服务网格终止 TLS，并以最小权限 OS 用户运行。

### 仍不满足的完整生产要求

- 本机是 cgroup v1，`max_cpus` / `max_processes` / `max_memory_mb` 的 cgroup v2 执行路径无法在本环境完成部署级验证。
- 磁盘 quota 和网络 allowlist 尚未实现，当前按失败关闭处理。
- registry 只覆盖单 gateway work root，不提供跨节点高可用或调度。
- token 轮换通过重建配置支持，尚无动态撤销 API。
- 审计输出目前是结构化 request/lifecycle 日志，还不是不可变审计后端。
- Bwrap 是 namespace/container 级隔离；如果威胁模型要求 VM 级隔离，可使用 gateway 的 Linux Firecracker 后端，但该后端仍未完成生产部署验证。

## 当前状态

- 已实现 `Sandbox` / `ExecutionEnv` 适配、文件操作、流式 shell、abort、timeout、删除和协议防护。
- 已实现 Linux Bwrap gateway 第一阶段：认证、租户绑定、独立 work root、稳定 guest `/workspace`、协议级文件 allow/deny、默认断网、timeout/output 上限、reset/delete、失败关闭策略、持久化 registry，以及 work root 独占锁与启动恢复。
- 已实现 gateway 的 Linux Firecracker 后端：guest-agent readiness、guest cleanup、生命周期 start/reset/shutdown、持久化 registry、重启后恢复为 `Creating`、显式 `/start` 完成，以及 stale PID/artifact 清理。
- mapped `fs_allowlist` / `fs_denylist` 路径会逐组件拒绝 symlink，并在 canonicalize 后强制落在 host work root 内，避免 Bwrap bind 跟随策略路径造成宿主逃逸。
- 单元测试覆盖 SSE 解码（含跨 chunk CRLF）、事件大小限制、配置校验、IPv6 loopback HTTP 和 gateway 返回路径校验。
- 集成测试通过本地 HTTP/SSE 网关覆盖文件系统、allowlist、denylist、流式输出、最终输出上限、timeout、客户端 timeout 上限、shell abort、文件请求取消、错误成功状态码拒绝、无效 create 响应后的远端清理、env 生命周期和删除。
- Firecracker 仍缺网络策略和真实部署验证；尚未提供 Kata/gVisor gateway。cgroup v2 部署验证和不可变审计仍未完成。

结论：协议客户端、Linux Bwrap gateway 第一阶段和 gateway 内的 Linux Firecracker 后端可以合入并被集成测试使用；不能宣称“远程沙箱整体生产可用”，直到上述生产要求全部满足并通过部署级验证。
