# Sandbox Gateway 部署说明

这个说明覆盖移动 Agent MVP 中 `HAR-001` 的 Linux gateway 部署。gateway 只提供 tool 执行环境，不内嵌 Agent session 语义；手机也不会拿到 gateway credential。

## 构建和运行

在 Linux 主机上执行：

```bash
cargo build --release -p llm-harness-sandbox-gateway --bin sandbox-gateway
./target/release/sandbox-gateway gateway.json
```

gateway 默认绑定 `127.0.0.1:7080`。生产部署应继续绑定内网地址，由 Caddy、Nginx、Cloudflare Tunnel 或 Tailscale Funnel 终止 TLS。

## 配置

`gateway.json` 使用下面的结构：

```json
{
  "bind": "127.0.0.1",
  "port": 7080,
  "work_root": "/var/lib/llm-harness-sandbox-gateway",
  "backend": {"type": "bwrap"},
  "principals": [
    {
      "principal": "control-service",
      "token_env": "SANDBOX_GATEWAY_TOKEN",
      "fs_allowlist": ["/workspace"],
      "max_trust_profile": "restricted",
      "requests_per_minute": 120,
      "max_active_sandboxes": 8
    }
  ],
  "max_sandboxes": 128,
  "default_exec_timeout_ms": 60000,
  "max_exec_timeout_ms": 3600000,
  "max_output_bytes": 1048576,
  "max_request_bytes": 67108864,
  "audit": {
    "path": "/var/lib/llm-harness-sandbox-gateway/audit.jsonl",
    "failure_mode": "fail_closed"
  }
}
```

`token_env` 是环境变量名，不是 bearer token 本身。gateway 启动时读取并校验这个环境变量。不要把 token 写入配置文件或命令行。

`requests_per_minute` 是可选的每 principal 固定窗口限流。省略表示不限制；设置为 `0` 会被拒绝。公网或移动端入口建议显式设置。

`max_active_sandboxes` 是可选的每 principal 并发 sandbox 上限。省略表示不限制；设置为 `0` 会被拒绝。计数包括所有已注册且未删除的 sandbox；删除、清理或重启后恢复为不可用状态的记录仍会占用计数，直到被清理。

Firecracker 后端可增加 `max_disk_mib`：

```json
"backend": {
  "type": "firecracker",
  "max_disk_mib": 2048
}
```

省略该字段时，客户端请求 `max_disk_mb` 会被拒绝；配置后，请求中的 `max_disk_mb` 不能超过这个硬上限。分配的 Firecracker rootfs 是按沙箱复制的 ext4 镜像，`max_disk_mb` 表示 rootfs 文件系统容量（MiB），必须不小于基础镜像实际大小。扩容需要宿主机安装并允许执行 `resize2fs`。

`fs/upload` 允许移动端或控制服务一次预置最多 32 个文件，二进制仍使用 Base64；请求 body 也受 `max_request_bytes` 限制。失败时不回滚已写入文件，客户端可依赖错误响应中的 guest path 定位失败项。

`work_root` 必须是绝对路径，并且权限必须是 `0700`：

```bash
sudo mkdir -p /var/lib/llm-harness-sandbox-gateway
sudo chown gateway:gateway /var/lib/llm-harness-sandbox-gateway
sudo chmod 700 /var/lib/llm-harness-sandbox-gateway
```

## 审计日志

gateway 会在启动时初始化 JSONL hash-chain 审计日志。默认路径是 `work_root/audit.jsonl`，head 摘要文件是 `work_root/audit.jsonl.head`。可以用 `audit.path` 指定独立磁盘；该路径必须是绝对路径，父目录必须已存在。两类文件创建为 `0600`，重启时会拒绝权限不是 `0600` 的已有文件。

审计文件不是普通 tracing 日志。每条记录包含时间、事件、`request_id`、principal、method、route、原始 path、HTTP status、sandbox、耗时和 detail。请求前先写 `request_started`，完成后写一个语义事件，例如 `sandbox_created`、`sandbox_started`、`sandbox_deleted`、`execution_started`、`file_read` 或 `file_written`。认证失败、限流和策略拒绝也会留下状态和原因。客户端传入的 UUID 格式 `x-request-id` 会保留；缺失时自动生成。

`failure_mode` 定义审计写入失败后的行为：

- `fail_closed`（默认）：请求被拒绝为 `503 audit_unavailable`；审计不可用时继续处理会留下不可审计的安全动作。
- `fail_open`：请求继续执行，gateway 记录 warning。这个模式适合可用性优先、由外层监控补偿告警的环境。

第一次审计写入失败后，当前 gateway 进程的审计 sink 会进入 degraded 状态；`fail_open` 只保证后续请求不被这次故障阻塞，不会隐式丢弃已有日志。重启 gateway 前先排查磁盘、权限和文件系统错误。

不要依赖 gateway 自身来提供绝对不可篡改存储。hash chain 可以检测单条记录修改和 head 与链尾不一致；但运行 gateway 的服务账号必须有追加权限，root 仍可能改文件。更强的部署应把 `audit.jsonl` 和 `.head` 定期复制到外部 append-only 存储，或使用 WORM/快照备份，并监控备份失败。

启动时 gateway 会验证已有链和 head。停止 gateway 后执行：

```bash
./target/release/sandbox-gateway validate-audit /var/lib/llm-harness-sandbox-gateway/audit.jsonl
```

运行时 gateway 会持续追加并更新 head，所以不要一边运行一边用该命令判断生产文件；先拷贝两个文件再验证副本，或先停止 gateway。

gateway 没有内置自动 rotation。旋转要在 gateway 停止后成对移动主文件和 head 文件，例如：

```bash
systemctl stop llm-harness-sandbox-gateway
suffix="$(date -u +%Y%m%dT%H%M%SZ)"
mv /var/lib/llm-harness-sandbox-gateway/audit.jsonl "/var/archive/audit-${suffix}.jsonl"
mv /var/lib/llm-harness-sandbox-gateway/audit.jsonl.head "/var/archive/audit-${suffix}.jsonl.head"
systemctl start llm-harness-sandbox-gateway
```

不要设置自动删除。保留期限由组织合规要求决定；备份和归档必须先确认可读、可验证且至少有一个异地或不可变副本。

## Token rotation 和 revocation

gateway 将每个 principal 的当前 token 哈希保存在 `work_root/credentials.sqlite`，文件权限为 `0600`，使用 WAL 和 `synchronous=FULL`。启动时从这个存储加载哈希；如果文件损坏、schema 不支持或存储不可用，gateway 启动失败，不会退回明文或绕过验证。配置文件仍通过 `token_env` 提供 bootstrap token；SQLite 已有该 principal 的记录时，配置 token 不会覆盖已轮换状态。

认证成功后可以自服务轮换或撤销自己的 token：

```bash
curl -X POST https://gateway.example.com/v1/credentials/rotate \
  -H "Authorization: Bearer $SANDBOX_GATEWAY_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"new_token":"<at-least-32-byte-new-token>"}'

curl -X POST https://gateway.example.com/v1/credentials/revoke \
  -H "Authorization: Bearer $SANDBOX_GATEWAY_TOKEN"
```

成功时两个端点返回 `204`；gateway 不在响应、日志或审计中记录 token 明文。rotate 会立即使当前 token 失效，并激活新 token；其他 principal 的有效 token 不受影响。revoke 会立即使当前 principal 的当前 token 失效，适合收到“撤销使用中凭证”的指令时调用。已经发出的 HTTP handler 不重新认证；下一次请求按更新后的内存凭证判断。

credential store 写入失败时，gateway 保持内存凭证不变，并返回 `500`。启动时打开或校验 credential store 失败也会让 gateway 启动失败。撤销后的 principal 不能再自行认证。若要恢复到配置文件中的 bootstrap token，必须先停止 gateway，删除或重建 `credentials.sqlite`，确认外部备份仍可读后再启动；不要在 gateway 运行时删除数据库。

## 探活和日志

`/healthz` 和 `/readyz` 是公开端点，返回 backend 和状态，可用于 reverse proxy 或容器编排器探活。业务路由仍要求 bearer token。

日志级别通过 `RUST_LOG` 控制，例如：

```bash
RUST_LOG=info ./target/release/sandbox-gateway gateway.json
```

gateway 在收到 `SIGTERM` 或 Ctrl-C 后会停止服务，并清理当前进程拥有的沙箱记录。部署环境给服务足够的 stop timeout，让 graceful shutdown 完成。
