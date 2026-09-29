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
sudo install -d -m 0750 -o llm-harness -g llm-harness /etc/llm-harness
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

## 5. 反向代理

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
