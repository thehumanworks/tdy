#!/usr/bin/env bash
# Type-check the macOS code paths from Linux (no linking, no running).
# C dependencies (zstd, blake3 asm) are "compiled" by a stub that only
# produces empty objects: enough for `cargo check`, which never links.
set -euo pipefail
cd "$(dirname "$0")/.."
stub=$(mktemp -d)
cat > "$stub/cc" <<'SH'
#!/bin/sh
out=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift ;;
    -o*) out="${1#-o}" ;;
    -Fo*) out="${1#-Fo}" ;;
  esac
  shift
done
if [ -n "$out" ]; then : > "$out"; fi
exit 0
SH
chmod +x "$stub/cc"
rustup target add aarch64-apple-darwin >/dev/null 2>&1 || true
CC_aarch64_apple_darwin="$stub/cc" AR_aarch64_apple_darwin=ar \
  cargo check --workspace --all-targets --target aarch64-apple-darwin "$@"
rm -rf "$stub"
