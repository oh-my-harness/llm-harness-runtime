#!/usr/bin/env bash
# 运行 llm-harness-runtime 全部测试（Rust workspace + PyO3 Python 测试）
# 用法: ./scripts/test.sh

set -euo pipefail

cd "$(dirname "$0")/.."

CRATE="crates/llm-harness-py"
VENV_DIR="$CRATE/.venv"
VENV="$VENV_DIR/bin"

# 自动创建 venv（含 maturin + pytest）如果不存在
if [ ! -d "$VENV_DIR" ]; then
  echo "── 创建 Python venv ($VENV_DIR) ──────────"
  python3 -m venv "$VENV_DIR"
  "$VENV/pip" install --upgrade pip maturin pytest
fi

echo "── cargo test (Rust workspace, 排除 py crate) ──"
CARGO_HTTP_PROXY="" cargo test --workspace --exclude llm-harness-py

echo ""
echo "── maturin develop ─────────────────────────"
VIRTUAL_ENV="$(pwd)/$CRATE/.venv" "$VENV/maturin" develop --manifest-path "$CRATE/Cargo.toml" --features test-utils

echo ""
echo "── pytest (Python 测试) ────────────────────"
"$VENV/pytest" "$CRATE/tests/" -v

echo ""
echo "── cargo test (py crate, 含 --ignored) ──────"
PYO3_PYTHON="$(pwd)/$VENV/python3" \
  CARGO_HTTP_PROXY="" \
  cargo test -p llm-harness-py --no-default-features --features test-utils -- --ignored
