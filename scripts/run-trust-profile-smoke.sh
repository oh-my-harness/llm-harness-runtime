#!/usr/bin/env bash
# Trust-profile acceptance smoke: spec scenarios 1-5.
# Starts a Seatbelt gateway with two principals (phone: capped at
# trusted-full; kiosk: capped at restricted) and runs curl assertions.
#
# Usage: scripts/run-trust-profile-smoke.sh
# Requires: macOS, the workspace built (cargo build).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${PORT:-7081}"
TOKEN_PHONE="phone-trust-smoke-token-0123456789abcdef"
TOKEN_KIOSK="kiosk-readonly-token-0123456789abcdef"
WORK_ROOT="$(mktemp -d /tmp/trust-smoke.XXXXXX)"

PASS=0
FAIL=0
check() { # name expected actual
  if [[ "$3" == *"$2"* ]]; then
    echo "PASS: $1"; PASS=$((PASS+1))
  else
    echo "FAIL: $1 — expected [$2] in [${3:0:160}]"; FAIL=$((FAIL+1))
  fi
}

cd "$REPO_ROOT"
pkill -f seatbelt_demo 2>/dev/null || true
sleep 1
GATEWAY_TOKEN="$TOKEN_PHONE" GATEWAY_PORT="$PORT" GATEWAY_BIND=127.0.0.1 GATEWAY_WORK_ROOT="$WORK_ROOT" \
  ./target/debug/examples/seatbelt_demo > /tmp/trust-smoke-gw.log 2>&1 &
GW_PID=$!
trap 'kill $GW_PID 2>/dev/null || true' EXIT
sleep 2

BASE="http://127.0.0.1:$PORT"
AUTH_PHONE="Authorization: Bearer $TOKEN_PHONE"
AUTH_KIOSK="Authorization: Bearer $TOKEN_KIOSK"
CREATE_BODY='{"config":{"fs_allowlist":[],"fs_denylist":[],"net_allowlist":[],"resource_limits":{"max_cpus":null,"max_processes":null,"max_memory_mb":null,"max_disk_mb":null,"timeout":null},"work_dir":null,"trust_profile":"%s"}}'

echo "== scenario 4: principal cap (kiosk requests trusted-lan → 400) =="
RES=$(curl -s -m 10 "$BASE/v1/sandboxes" -X POST -H "$AUTH_KIOSK" -H "Content-Type: application/json" \
  -d "$(printf "$CREATE_BODY" trusted_lan)" -w "\n%{http_code}")
check "over-cap rejected 400" "400" "$RES"
check "over-cap error message" "exceeds principal cap" "$RES"

echo "== scenario 5: phone requests trusted-lan → 201 + echo =="
RES=$(curl -s -m 20 "$BASE/v1/sandboxes" -X POST -H "$AUTH_PHONE" -H "Content-Type: application/json" \
  -d "$(printf "$CREATE_BODY" trusted_lan)")
SID=$(echo "$RES" | python3 -c 'import json,sys; print(json.load(sys.stdin)["sandbox_id"])' 2>/dev/null || echo "")
check "trusted-lan created" '"state"' "$RES"
check "profile echoed" '"trust_profile":"trusted_lan"' "$RES"

echo "== scenario 3: floors (write /etc/hosts, read ~/.ssh) → denied =="
if [[ -n "$SID" ]]; then
  EXEC_URL="$BASE/v1/sandboxes/$SID/exec"
  AUTH="$AUTH_PHONE"
  run() { curl -s -m 30 "$EXEC_URL" -X POST -H "$AUTH" -H "Content-Type: application/json" \
    -H "Accept: text/event-stream" -d "{\"command\":\"$1\",\"cwd\":null,\"env\":{},\"timeout_ms\":15000,\"max_output_bytes\":65536}"; }
  # SSE assertions match on raw event lines (shell_failed / PermissionDenied
  # / HTTP 200 appear in stderr/stdout chunk payloads).
  RES=$(run "echo x >> /etc/hosts")
  check "write /etc/hosts denied" "shell_failed" "$RES"
  # Floors are only observable when the target exists: seed .env and .ssh
  # in the sandbox work dir first (host side can write the work root).
  WORK_DIR="$WORK_ROOT/sandboxes/$SID"
  echo "secret" > "$WORK_DIR/.env"
  mkdir -p "$WORK_DIR/.ssh" && echo "key" > "$WORK_DIR/.ssh/id_ed25519"
  RES=$(run "cat \$PWD/.env")
  check "read work .env denied" "Operation not permitted" "$RES"
  RES=$(run "cat \$PWD/.ssh/id_ed25519")
  check "read work .ssh denied" "Operation not permitted" "$RES"

  echo "== scenario 1: network (pypi reachable) =="
  RES=$(run "echo aW1wb3J0IHVybGxpYi5yZXF1ZXN0OyBwcmludCh1cmxsaWIucmVxdWVzdC51cmxvcGVuKCdodHRwczovL3B5cGkub3JnL3NpbXBsZS8nLCB0aW1lb3V0PTE1KS5zdGF0dXMp | base64 -d | python3")
  check "https reach pypi" "200" "$RES"

  curl -s -m 10 "$BASE/v1/sandboxes/$SID" -X DELETE -H "$AUTH" -o /dev/null
else
  echo "FAIL: no sandbox id"; FAIL=$((FAIL+1))
fi

echo
echo "== results: $PASS passed, $FAIL failed =="
[[ $FAIL -eq 0 ]]
