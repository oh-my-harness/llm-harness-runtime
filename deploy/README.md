# Linux systemd 部署

这份模板部署两个服务：

- `llm-harness-gateway.service`：Bwrap 沙箱 gateway，只监听 loopback；
- `llm-harness-agent-control.service`：手机访问的 Agent control service。

模板假设使用 restricted 沙箱策略，不挂载宿主机项目目录。如果需要 `trusted-lan` 的 host project bind，必须重新评估 `ProtectHome=true` 和 `ProtectSystem=strict` 的影响。

## 1. 构建

在 Linux 主机上执行：

```bash
cargo build --release \
  -p llm-harness-sandbox-gateway --bin sandbox-gateway \
  -p llm-harness-agent-control --bin agent-control
```

## 2. 安装文件

```bash
sudo useradd --system --home-dir /var/lib/llm-harness --shell /usr/sbin/nologin llm-harness
sudo install -d -m 0750 -o root -g llm-harness /etc/llm-harness
sudo install -m 0640 -o root -g llm-harness deploy/gateway.json.example /etc/llm-harness/gateway.json
sudo install -m 0640 -o root -g llm-harness deploy/agent-control.json.example /etc/llm-harness/agent-control.json
sudo install -m 0755 target/release/sandbox-gateway /usr/local/bin/llm-harness-sandbox-gateway
sudo install -m 0755 target/release/agent-control /usr/local/bin/llm-harness-agent-control
```

## 3. 配置 credential

先生成 token：

```bash
openssl rand -hex 32
```

复制 `.env.example`，填入真实值。`gateway.env` 只需要 gateway token；`agent-control.env` 需要同一个 gateway token、手机 token 和 model provider key。确保 gateway token 与两份配置中的 `LLM_HARNESS_GATEWAY_TOKEN` 一致：

```bash
sudo install -m 0600 -o root -g llm-harness deploy/gateway.env.example /etc/llm-harness/gateway.env
sudo install -m 0600 -o root -g llm-harness deploy/agent-control.env.example /etc/llm-harness/agent-control.env
sudo chmod 600 /etc/llm-harness/gateway.env /etc/llm-harness/agent-control.env
```

不要把真实 token 提交到仓库。

## 4. 启用服务

```bash
sudo cp deploy/llm-harness-gateway.service /etc/systemd/system/
sudo cp deploy/llm-harness-agent-control.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now llm-harness-gateway.service
sudo systemctl enable --now llm-harness-agent-control.service
```

检查状态：

```bash
systemctl status llm-harness-gateway.service
systemctl status llm-harness-agent-control.service
curl -fsS http://127.0.0.1:7080/readyz
curl -fsS http://127.0.0.1:7090/healthz
```

`/healthz` 和 `/readyz` 是公开探活端点；`/metrics` 仍要求 gateway bearer token，不要暴露到公网。建议至少告警：

- `readyz` 连续失败；
- `systemd` 进入 failed/stop-sigterm 状态；
- `/var/lib/llm-harness` 可用容量低于 20%；
- gateway 5xx 或 `audit_unavailable` 持续出现；
- active sandbox 数接近 `max_sandboxes`。

日志在 journald 中查看。不要在告警正文、值班平台或 incident 复制 bearer token、provider API key、guest path 以外的用户数据或完整 prompt。

## 5. TLS 终止和证书

gateway 本身只做 plain HTTP，必须由反向代理或隧道终止 TLS。推荐拓扑：

- 小团队公网入口：Caddy 或 Nginx 终止 TLS，只转发到 loopback；
- 无公网/家庭网络：Tailscale Funnel；
- 云上入口：Cloudflare Tunnel，但注意国内链路可用性；
- 企业内网：由 ingress/controller 终止 TLS，并限制 source network。

证书要求与普通 HTTPS 服务一致：私钥权限 `0600`，证书覆盖客户端访问的 hostname，自动续期不能依赖手工操作，续期失败要有告警。反向代理只允许访问 gateway 的 `7080` 和 agent control 的 `7090`；不要把 gateway credential 或 model provider key 放入 URL、proxy header 或访问日志。

最小 Caddy 示例：

```caddyfile
control.example.com {
    reverse_proxy 127.0.0.1:7090
}
```

如果手机只访问 agent control，不要为 gateway 增加公网 hostname。gateway credential 只应保存在 agent control 主机内。

`agent-control` 也默认只监听 loopback。移动端入口应由 Caddy、Nginx、Cloudflare Tunnel 或 Tailscale Funnel 终止 HTTPS 后再转发到 `127.0.0.1:7090`。不要把 gateway 的 `7080` 直接暴露到公网。

## 6. 更新

```bash
cargo build --release \
  -p llm-harness-sandbox-gateway --bin sandbox-gateway \
  -p llm-harness-agent-control --bin agent-control
sudo install -m 0755 target/release/sandbox-gateway /usr/local/bin/llm-harness-sandbox-gateway
sudo install -m 0755 target/release/agent-control /usr/local/bin/llm-harness-agent-control
sudo systemctl restart llm-harness-gateway.service llm-harness-agent-control.service
```

## 7. 日志

```bash
journalctl -u llm-harness-gateway.service -f
journalctl -u llm-harness-agent-control.service -f
```

服务默认继承 JSON 配置中的 token 环境变量名。日志不要包含 bearer token、API key 或原始 prompt 内容。

## 8. 容量、备份和恢复

`work_root` 保存 sandbox work 目录、SQLite registry、audit JSONL/head 和 credential SQLite。备份前先停止 gateway，避免复制不一致的 WAL 或 audit head：

```bash
sudo systemctl stop llm-harness-gateway.service
sudo tar -C /var/lib/llm-harness/gateway -czf \
  /var/backups/llm-harness-gateway-$(date -u +%Y%m%dT%H%M%SZ).tar.gz work
sudo systemctl start llm-harness-gateway.service
```

备份应至少保留一个异地或不可变副本；恢复演练要验证 registry integrity、audit hash chain 和 credential store schema。容量监控要同时看宿主机磁盘和 Firecracker `max_disk_mib` 总量。不要自动删除 audit；保留期限由合规策略决定。

credential store 损坏或误撤销后的恢复顺序：

1. 停止 gateway 和 agent control；
2. 从已验证备份恢复 `/var/lib/llm-harness/gateway/work/credentials.sqlite`，或在确认可以回到 bootstrap token 时删除该文件；
3. 启动 gateway，确认配置 token 可认证；
4. 更新 agent control token 并启动 agent control；
5. 验证 `/metrics`、一次真实 sandbox 创建和 audit validation。

## 9. 生产安全检查清单

- [ ] 只监听 loopback 或内网接口，公网入口只暴露 HTTPS。
- [ ] TLS 证书自动续期、失败告警和私钥权限已验证。
- [ ] `gateway.env`、`agent-control.env`、SQLite 和 audit 文件权限为 `0600`，目录为 `0700`。
- [ ] 使用独立的 `llm-harness` 系统账号；不要共享登录账号或复用 shell 凭证。
- [ ] token 不出现在仓库、镜像、CI 日志、URL 或 shell history。
- [ ] rotation/revoke 流程演练过；旧 token 失效，新 token 可用。
- [ ] audit 和 credential store 有异地/不可变备份，并能离线验证。
- [ ] work_root 与审计日志所在磁盘有容量告警。
- [ ] `restricted` principal 不挂载 host project directory；如需 `trusted-lan`，单独评审 bind mount、宿主目录和 systemd 沙箱。
- [ ] Firecracker 使用独立、最小权限的服务边界；不要与 Bwrap 示例混用同一套假设。
- [ ] 失败演练覆盖：gateway 不可用、registry 损坏、audit 写失败、credential store 不可用、代理证书过期。
