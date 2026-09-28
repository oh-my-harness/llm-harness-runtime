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
      "max_trust_profile": "restricted"
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

`work_root` 必须是绝对路径，并且权限必须是 `0700`：

```bash
sudo mkdir -p /var/lib/llm-harness-sandbox-gateway
sudo chown gateway:gateway /var/lib/llm-harness-sandbox-gateway
sudo chmod 700 /var/lib/llm-harness-sandbox-gateway
```

## 探活和日志

`/healthz` 和 `/readyz` 是公开端点，返回 backend 和状态，可用于 reverse proxy 或容器编排器探活。业务路由仍要求 bearer token。

日志级别通过 `RUST_LOG` 控制，例如：

```bash
RUST_LOG=info ./target/release/sandbox-gateway gateway.json
```

gateway 在收到 `SIGTERM` 或 Ctrl-C 后会停止服务，并清理当前进程拥有的沙箱记录。部署环境给服务足够的 stop timeout，让 graceful shutdown 完成。
