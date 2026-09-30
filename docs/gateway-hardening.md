# Sandbox Gateway 生产加固 Runbook

这份手册面向 Linux 服务器运维。目标是把 gateway 从“能在开发机上跑起来”推进到“有明确边界、可监控、可恢复、可审计”的最小生产形态。

## 1. 支持的拓扑

推荐的信任边界是：

```text
mobile client / control service
        |
        | HTTPS
        v
TLS terminator: Caddy / Nginx / sidecar / tunnel
        |
        | HTTP
        v
llm-harness-sandbox-gateway
        |
        v
Bwrap / Firecracker sandbox
```

约束：

- gateway 只监听 loopback 或专用内网接口；
- gateway 进程自身不做 TLS；
- 手机端不要直接持有 gateway bearer token；由 control service 代理；
- 非生产环境也不要把 gateway 直接暴露到公网。

## 2. TLS 终止

gateway 期望由反向代理或 tunnel 终止 TLS。

### Caddy 示例

```caddyfile
gateway.example.com {
    reverse_proxy 127.0.0.1:7080
}
```

### Nginx 示例

```nginx
server {
    listen 443 ssl;
    server_name gateway.example.com;

    ssl_certificate     /etc/letsencrypt/live/gateway.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/gateway.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:7080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Request-ID $request_id;
        proxy_read_timeout 900s;
    }
}
```

证书要求：

- 使用受信任 CA 签发的证书；
- 只允许 TLS 1.2 及以上版本；
- 不要启用 client certificate 作为唯一的 gateway 身份；gateway 仍要求 bearer token；
- reverse proxy 需要支持长连接和 SSE；示例中 `proxy_read_timeout` 应大于 gateway 的最大执行超时。

## 3. 文件系统

gateway 需要 0700 的 canonical work root。它用于存放：

- sandbox host work root；
- sandbox registry；
- gateway lock 文件。

推荐目录：

```bash
sudo useradd --system --home-dir /var/lib/llm-harness --shell /usr/sbin/nologin llm-harness
sudo install -d -m 0700 -o llm-harness -g llm-harness /var/lib/llm-harness/gateway/work
```

要求：

- work root 必须是绝对路径；
- 不能是 symlink；
- 权限必须是 `0700`；
- registry 和 lock 文件必须是 regular file；
- 不要把 work root 放在 NFS 或其他不适合 POSIX 锁的文件系统上；
- 单个 work root 只能属于一个 gateway 进程。

备份：

```bash
sudo systemctl stop llm-harness-gateway.service
sudo tar -C /var/lib/llm-harness/gateway/work -czf /backup/gateway-work-$(date +%F-%H%M).tar.gz .
sudo systemctl start llm-harness-gateway.service
```

只在 gateway 停止后备份 work root，避免 registry 和 sandbox 文件系统出现不一致快照。

## 3.1 文件批量上传

`POST /v1/sandboxes/{id}/fs/upload` 复用 guest path、allowlist/denylist、Base64 和 `max_request_bytes` 限制。单次请求最多 32 个路径唯一的文件；gateway 先解码并校验全部条目，再按顺序写入，已写入文件不会自动回滚。生产部署应继续把 `max_request_bytes` 设置为实际需要的最小值，并确认反向代理放行同样大小。

## 4. 最小权限部署

推荐使用 [`deploy/README.md`](../deploy/README.md) 中的 systemd 模板。关键项如下：

```ini
User=llm-harness
Group=llm-harness
StateDirectory=llm-harness/gateway/work
StateDirectoryMode=0700
UMask=0077

NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
RestrictSUIDSGID=true
LockPersonality=true
```

至少检查这些点：

- 配置目录 `/etc/llm-harness` 由 `root:llm-harness` 拥有，权限 `0750`；
- 配置文件权限 `0640`，环境文件权限 `0600`；
- 服务用户不能写 `/etc/llm-harness`；
- 只给 gateway 自己的 `StateDirectory` 写权限；
- 不把宿主项目目录默认挂进 sandbox。

## 5. 健康检查、日志与告警

### 健康检查

```bash
curl -fsS http://127.0.0.1:7080/healthz
curl -fsS http://127.0.0.1:7080/readyz
```

- `/healthz`：进程可访问；
- `/readyz`：进程 ready，并返回当前 backend；
- 两个端点都不需要 bearer token，但也不应暴露到公网。

### 日志

gateway 不记录 bearer token、API key 或原始 prompt。结构化请求日志包含：

- `x-request-id`
- method
- path
- principal
- normalized route
- status
- duration

常用命令：

```bash
journalctl -u llm-harness-gateway.service -f
journalctl -u llm-harness-gateway.service --since "1 hour ago"
journalctl -u llm-harness-gateway.service -p warning
```

### 指标

```bash
curl -fsS -H "Authorization: Bearer $SANDBOX_GATEWAY_TOKEN" \
  http://127.0.0.1:7080/metrics
```

`/metrics` 使用 Prometheus 文本格式，输出：

- `sandbox_gateway_requests_total`：按 method、normalized route、principal 和 status 分组；
- `sandbox_gateway_request_duration_seconds_sum` / `_count`：同维度的累计耗时；
- `sandbox_gateway_active_sandboxes`：当前 registry 中的 sandbox 数。

这个端点是内部监控面，需要 bearer token；不要把它直接暴露到公网。当前指标足以判断请求量、错误率、延迟总量和活跃 sandbox 数，但不是完整的 Prometheus histogram，也不包含宿主机资源指标。

### 告警

最小告警集：

1. gateway 进程重启；
2. `systemd` unit 进入 failed 状态；
3. loopback `/readyz` 持续失败；
4. `systemctl is-failed llm-harness-gateway.service` 为 true；
5. TLS 证书剩余有效期低于阈值。

当前 gateway 还没有内置 Prometheus metrics 端点。不要假装已有业务 metrics；生产监控应先基于 systemd 状态、健康探测和结构化日志。

## 6. Token 管理

token 来自环境变量，通过 `token_env` 间接引用。不要写进 JSON、命令行或日志。

### 生成

```bash
openssl rand -hex 32
```

### 轮换

1. 生成新 token；
2. 更新 `/etc/llm-harness/gateway.env`；
3. 更新调用方的同一个 token；
4. 重启 gateway；
5. 确认 `/readyz` 和受保护接口正常；
6. 从受影响环境文件中删除旧 token。

如果同一 principal 配置了多个 token，可以先加入新 token，等调用方切换完成后再移除旧 token。

### 撤销

1. 从 `/etc/llm-harness/gateway.env` 删除旧 token；
2. 重启 gateway；
3. 用旧 token 发起请求，确认返回 `401`；
4. 检查日志中没有异常来源继续成功访问。

当前 gateway 没有 runtime token revocation API；这是已知边界，不要把它描述为在线撤销。

## 7. 故障恢复

### gateway 崩溃或重启

1. `systemctl status llm-harness-gateway.service`
2. `journalctl -u llm-harness-gateway.service -n 200`
3. 确认 work root 权限仍是 `0700`
4. `systemctl restart llm-harness-gateway.service`
5. `curl -fsS http://127.0.0.1:7080/readyz`

gateway 启动时会 fail-closed 地 reconcile registry 和 work root，删除孤儿目录和处于 `Deleting` 状态的记录，并恢复仍有效的 sandbox。不要手工删除 registry 或 sandbox 子目录。

### work root 异常

如果启动报错提到权限、symlink、非 regular file 或无法 reconcile：

1. 不要删除数据；
2. 先备份；
3. 根据日志保留异常条目；
4. 由人工确认后再清理。

### TLS 不可用

1. 确认反向代理进程正常；
2. 检查证书过期时间和证书链；
3. 检查反向代理是否能访问 loopback gateway；
4. 不要通过把 gateway 直接暴露到公网来临时恢复。

## 8. 生产安全检查清单

上线前逐项确认：

- [ ] gateway 只绑定 loopback 或专用内网接口；
- [ ] 所有外部流量都经 TLS 终止；
- [ ] bearer token 只存在于环境文件；
- [ ] 环境文件权限为 `0600`；
- [ ] 配置目录不是服务用户可写；
- [ ] work root 是 canonical 目录且权限为 `0700`；
- [ ] 每个 principal 有独立 token 和最小 `fs_allowlist`；
- [ ] `host_project_dirs` 为空，除非明确理解 bind 风险；
- [ ] `max_trust_profile` 不高于业务需要；
- [ ] 公网或移动端入口配置了 `requests_per_minute`；
- [ ] `max_sandboxes`、执行超时、输出大小和请求大小都已设置；
- [ ] 多租户部署为每个 principal 设置 `max_active_sandboxes`；
- [ ] sandbox 对外网络默认拒绝；
- [ ] 服务用户没有 shell；
- [ ] systemd 启用 `NoNewPrivileges`、`ProtectSystem=strict`、`ProtectHome=true`；
- [ ] `journalctl` 中没有 token、API key 或 prompt 内容；
- [ ] 告警覆盖进程失败、`/readyz` 失败、证书过期；
- [ ] 有 work root 停机备份；
- [ ] 每次变更前记录 commit、配置 diff 和回滚步骤。

## 9. 当前限制

这份 runbook 不应被解释为“远程沙箱整体生产可用”。以下仍属于已知缺口：

- token 轮换/撤销需要重启；
- 没有 runtime token revocation API；
- 没有不可变审计后端；
- 磁盘 quota 未实现；
- 网络 allowlist 未实现；
- cgroup v2 资源限额仍需要真实部署验证；
- Firecracker backend 仍未完成生产部署验证。

在以上缺口关闭前，生产术语应使用“可部署的最小控制面”，而不是“完全生产就绪”。
