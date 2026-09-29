# Sandbox Gateway 部署说明

这个说明覆盖移动 Agent MVP 中 `HAR-001` 的 Linux gateway 部署。gateway 只提供 tool 执行环境，不内嵌 Agent session 语义；手机也不会拿到 gateway credential。

## 构建和运行

在 Linux 主机上执行：

```bash
cargo build --release -p llm-harness-sandbox-gateway --bin sandbox-gateway
./target/release/sandbox-gateway gateway.json
```

可复制的生产模板和 systemd 单元见 [`deploy/README.md`](../deploy/README.md)。

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
      "requests_per_minute": 120
    }
  ],
  "max_sandboxes": 128,
  "default_exec_timeout_ms": 60000,
  "max_exec_timeout_ms": 3600000,
  "max_output_bytes": 1048576,
  "max_request_bytes": 67108864
}
```

`token_env` 是环境变量名，不是 bearer token 本身。gateway 启动时读取并校验这个环境变量。不要把 token 写入配置文件或命令行。

`requests_per_minute` 是可选的每 principal 固定窗口限流。省略表示不限制；设置为 `0` 会被拒绝。公网或移动端入口建议显式设置。

`fs/upload` 允许移动端或控制服务一次预置最多 32 个文件，二进制仍使用 Base64；请求 body 也受 `max_request_bytes` 限制。失败时不回滚已写入文件，客户端可依赖错误响应中的 guest path 定位失败项。

`work_root` 必须是绝对路径，并且权限必须是 `0700`：

```bash
sudo mkdir -p /var/lib/llm-harness-sandbox-gateway
sudo chown gateway:gateway /var/lib/llm-harness-sandbox-gateway
sudo chmod 700 /var/lib/llm-harness-sandbox-gateway
```

## 探活和日志

`/healthz` 和 `/readyz` 是公开端点，返回 backend 和状态，可用于 reverse proxy 或容器编排器探活。业务路由仍要求 bearer token。

`/metrics` 返回 Prometheus 文本格式，包含按 method/route/principal/status 统计的请求总量和耗时、活跃 sandbox 数。它需要 bearer token，不要暴露到公网；建议只让内网监控进程或本地 exporter 拉取。

## 反向代理和 TLS

gateway 本身不做 TLS 终止。下面是最小 Caddy 示例；把它放在 gateway 前面，只暴露 HTTPS 入口：

```caddyfile
gateway.example.com {
    reverse_proxy 127.0.0.1:7080
}
```

Nginx 的对应示例：

```nginx
server {
    listen 443 ssl;
    server_name gateway.example.com;

    location / {
        proxy_pass http://127.0.0.1:7080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_read_timeout 3600s;
    }
}
```

没有公网 IP 或不想开放端口时，可以用 Cloudflare Tunnel：

```bash
cloudflared tunnel --url http://127.0.0.1:7080
```

Tailscale Funnel 也适合个人或小团队部署：

```bash
tailscale funnel 7080
```

无论选择哪种入口，都不要把 gateway 直接暴露为公网 plain HTTP。Agent control service 连接非 loopback gateway 时也应使用 HTTPS；只有 loopback 的测试环境才允许 `allow_insecure_http`。

日志级别通过 `RUST_LOG` 控制，例如：

```bash
RUST_LOG=info ./target/release/sandbox-gateway gateway.json
```

gateway 在收到 `SIGTERM` 或 Ctrl-C 后会停止服务，并清理当前进程拥有的沙箱记录。部署环境给服务足够的 stop timeout，让 graceful shutdown 完成。
