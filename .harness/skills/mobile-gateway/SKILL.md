---
name: mobile-gateway
description: How to run and debug the mobile sandbox gateway (restart, tokens, healthz, work_root lock pitfalls). Use when mobile development tasks involve gateway operations.
---

# Mobile Gateway Operations

## Start
GATEWAY_TOKEN_PHONE_TEST='mvp-smoke-token-0123456789abcdef' SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk CARGO_HTTP_PROXY="" nohup cargo run -p llm-harness-sandbox-gateway --bin sandbox-gateway -- /tmp/ohmyharness-gateway/gateway.json > /tmp/gateway.log 2>&1 &

## Health
curl -s http://127.0.0.1:7080/healthz

## Pitfalls
- work_root lock: "already in use" means an old gateway process is alive; pkill and wait 3s.
- After gateway restart, phone-side sessions must create a new task (old sandbox ids are dead).
- Rust-side code changes require `bash scripts/build-android.sh` before assembleDebug, or the app crashes at startup (uniffi checksum mismatch).
