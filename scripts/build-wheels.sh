#!/usr/bin/env bash
# Build llm-harness-py wheels and upload to GitHub Release.
#
# Usage:
#   ./scripts/build-wheels.sh <version>      # build + upload
#   ./scripts/build-wheels.sh <version> --build-only  # skip upload
#
# Prerequisites:
#   - Rust toolchain (stable)
#   - maturin (pip install maturin)
#   - Python 3.12 on PATH
#   - rustup targets: x86_64-apple-darwin (for Intel macOS cross-compile)
#   - gh (GitHub CLI) authenticated, with access to the runtime repo
#
# Linux x64 and Windows x64 wheels cannot be built on macOS.
# Build those on their respective platforms and upload separately:
#   gh release upload v<version> -R oh-my-harness/llm-harness-runtime <wheel-files>
set -euo pipefail

cd "$(dirname "$0")/.."

VERSION="${1:?usage: build-wheels.sh <version> [--build-only]}"
BUILD_ONLY="${2:-}"

CRATE_DIR="crates/llm-harness-py"
WHEEL_DIR="target/wheels"
MATURIN="${MATURIN:-maturin}"
PY="${PYTHON:-python3.12}"

if ! command -v "$MATURIN" &>/dev/null; then
    echo "✗ maturin not found. Install: pip install maturin"
    exit 1
fi

if ! command -v "$PY" &>/dev/null; then
    echo "✗ Python 3.12 not found: $PY"
    exit 1
fi

mkdir -p "$WHEEL_DIR"

# ── Build wheels ──────────────────────────────────────────────
TARGETS=(
    "aarch64-apple-darwin"
    "x86_64-apple-darwin"
)

for target in "${TARGETS[@]}"; do
    echo ""
    echo "▶ Building wheel for $target ..."
    "$MATURIN" build --release --target "$target" --interpreter "$PY" \
        --manifest-path "$CRATE_DIR/Cargo.toml"
done

echo ""
echo "✓ Built wheels:"
ls -lh "$WHEEL_DIR"/*.whl

if [ "$BUILD_ONLY" = "--build-only" ]; then
    echo ""
    echo "Skipping upload (--build-only)."
    echo "Wheels are in: $WHEEL_DIR/"
    exit 0
fi

# ── Upload to GitHub Release (public wheels repo) ────────────
TAG="v$VERSION"
WHEELS_REPO="oh-my-harness/llm-harness-py-wheels"
echo ""
echo "▶ Uploading wheels to release $TAG in $WHEELS_REPO ..."

if gh release view "$TAG" -R "$WHEELS_REPO" &>/dev/null; then
    gh release upload "$TAG" -R "$WHEELS_REPO" "$WHEEL_DIR"/*.whl --clobber
else
    gh release create "$TAG" -R "$WHEELS_REPO" \
        --title "$TAG" \
        --notes "Prebuilt PyO3 wheels for llm-harness-py (CPython 3.12). Source code is in the private llm-harness-runtime repo; this repo distributes compiled binaries only." \
        "$WHEEL_DIR"/*.whl
fi

echo ""
echo "✓ Done. Release: https://github.com/$WHEELS_REPO/releases/tag/$TAG"
