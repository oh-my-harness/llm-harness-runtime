#!/usr/bin/env bash
# agent-team-studio local development launcher: build → start → open panel.
#
# 用法:
#   ./scripts/studio.sh              # allocate an ephemeral port
#   PORT=9000 ./scripts/studio.sh    # fixed development port
#   NO_OPEN=1 ./scripts/studio.sh    # start without opening the browser
#
# 数据根目录: ~/.harness/agent-team-studio（STUDIO_DATA_ROOT 可覆盖）。
# 后端只输出 panel descriptor 路径；URL/token 保存在 data root 内的 0600 文件中。
set -euo pipefail

export STUDIO_DATA_ROOT="${STUDIO_DATA_ROOT:-$HOME/.harness/agent-team-studio}"

cd "$(dirname "$0")/.."

echo "building agent-team-studio (first run takes a while)…"
cargo build --release -p llm-harness-agent-team-studio

BIN="target/release/agent-studio"
LOG="$(mktemp "${TMPDIR:-/tmp}/agent-team-studio.XXXXXX")"
trap 'kill "$PID" 2>/dev/null || true; rm -f "$LOG"' EXIT

STUDIO_PORT="${PORT:-0}" "$BIN" >"$LOG" 2>&1 &
PID=$!

DESCRIPTOR=""
for _ in $(seq 1 300); do
  DESCRIPTOR="$(sed -n 's/^STUDIO_PANEL_DESCRIPTOR=//p' "$LOG" | tail -1)"
  [ -n "$DESCRIPTOR" ] && break
  kill -0 "$PID" 2>/dev/null || { cat "$LOG" >&2; exit 1; }
  sleep 0.1
done

if [ -z "$DESCRIPTOR" ]; then
  cat "$LOG" >&2
  exit 1
fi

URL="$(sed -n 's/.*"url":"\([^"]*\)".*/\1/p' "$DESCRIPTOR" | tail -1)"
if [ -z "$URL" ]; then
  cat "$LOG" >&2
  exit 1
fi

echo "agent-team-studio ready: $DESCRIPTOR"
[ "${NO_OPEN:-0}" = "1" ] || open "$URL"
wait "$PID"
