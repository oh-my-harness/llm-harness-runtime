#!/usr/bin/env bash
# agent-team-studio 一键启动：构建 → 起服务 → 打开面板。
#
# 用法:
#   ./scripts/studio.sh              # 默认端口 8080
#   PORT=9000 ./scripts/studio.sh    # 自定义端口
#   NO_OPEN=1 ./scripts/studio.sh    # 只启动不开浏览器
#
# 数据根目录: ~/.harness/agent-team-studio（STUDIO_DATA_ROOT 可覆盖）
set -euo pipefail

PORT="${PORT:-8080}"
export STUDIO_PORT="$PORT"
export STUDIO_DATA_ROOT="${STUDIO_DATA_ROOT:-$HOME/.harness/agent-team-studio}"

cd "$(dirname "$0")/.."

# 已有实例在跑 → 直接打开面板
if curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 \
   || curl -sf "http://127.0.0.1:$PORT/" >/dev/null 2>&1; then
  echo "agent-team-studio already running at http://127.0.0.1:$PORT/app/team.html"
  [ "${NO_OPEN:-0}" = "1" ] || open "http://127.0.0.1:$PORT/app/team.html"
  exit 0
fi

echo "building agent-team-studio (first run takes a while)…"
cargo build --release -p llm-harness-agent-team-studio

BIN="target/release/agent-studio"

# 起服务；等就绪后开浏览器
( sleep 2
  [ "${NO_OPEN:-0}" = "1" ] || open "http://127.0.0.1:$PORT/app/team.html" ) &

exec "$BIN"
