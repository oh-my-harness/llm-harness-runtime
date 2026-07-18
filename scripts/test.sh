#!/usr/bin/env bash
# 运行 llm-harness-runtime 全部测试（Rust workspace）
# 用法: ./scripts/test.sh

set -euo pipefail

cd "$(dirname "$0")/.."

echo "── cargo test (Rust workspace) ──"
CARGO_HTTP_PROXY="" cargo test --workspace
