#!/usr/bin/env bash
# Build the mobile-core Android library and Kotlin bindings.
#
# Prerequisites:
#   rustup target add aarch64-linux-android
#   cargo install cargo-ndk
#   Android NDK installed (ANDROID_NDK_HOME or ~/Library/Android/sdk/ndk/<ver>)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CRATE="llm-harness-mobile-core"
JAVA_OUT="$REPO_ROOT/mobile/android/app/src/main/java"
JNILIBS="$REPO_ROOT/mobile/android/app/src/main/jniLibs"

cd "$REPO_ROOT"

echo "== cross-compiling $CRATE (arm64-v8a) =="
cargo ndk -t arm64-v8a -o "$JNILIBS" build -p "$CRATE"

echo "== building host dylib for binding generation =="
cargo build -p "$CRATE"

echo "== generating Kotlin bindings =="
DYLIB="$(find target/debug -maxdepth 1 -name 'libllm_harness_mobile_core.dylib' | head -1)"
cargo run -p "$CRATE" --bin uniffi-bindgen -- generate \
    --library "$DYLIB" \
    --language kotlin \
    --out-dir "$JAVA_OUT" \
    --no-format

echo "== done: $JNILIBS/arm64-v8a/libllm_harness_mobile_core.so + bindings in $JAVA_OUT/uniffi =="
