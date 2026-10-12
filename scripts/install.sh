#!/usr/bin/env bash
# Build Tidy in release mode, install it into Cargo's bin directory, and make
# sure the `tidy` your shell finds is this one. macOS ships an unrelated,
# much older HTML Tidy at /usr/bin/tidy; if that comes first on PATH,
# `tidy --help` prints "Config: unknown option: help".
#
#   scripts/install.sh              # tuned for this CPU (-C target-cpu=native)
#   scripts/install.sh --portable   # runs on any CPU of this architecture
set -euo pipefail
cd "$(dirname "$0")/.."

native=1
for a in "$@"; do
  case "$a" in
    --portable) native=0 ;;
    *) echo "usage: $0 [--portable]" >&2; exit 2 ;;
  esac
done

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found: install Rust with rustup first" >&2
  exit 1
fi
if [ "$native" = 1 ]; then
  export RUSTFLAGS="${RUSTFLAGS:-} -C target-cpu=native"
fi

cargo install --path crates/tidy-cli --locked --force

bindir="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin"
bin="$bindir/tidy"
"$bin" --version

hash -r 2>/dev/null || true
first="$(command -v tidy || true)"
if [ "$first" != "$bin" ]; then
  echo >&2
  echo "warning: \`tidy\` on your PATH is ${first:-not found}, not $bin." >&2
  if [ "$first" = "/usr/bin/tidy" ] && [ "$(uname -s)" = "Darwin" ]; then
    echo "  /usr/bin/tidy is Apple's HTML Tidy, an unrelated program." >&2
  fi
  echo "  Put $bindir first on your PATH, for example in ~/.zshrc:" >&2
  echo "    export PATH=\"$bindir:\$PATH\"" >&2
  echo "  then open a new terminal (or run \`hash -r\`) and check with \`which -a tidy\`." >&2
  exit 1
fi
echo "ok: \`tidy\` resolves to $bin"
