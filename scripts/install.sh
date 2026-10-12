#!/usr/bin/env bash
# Build Tidy in release mode, install it into Cargo's bin directory as
# `tdy`, and make sure the `tdy` your shell finds is this one. (The command
# was `tidy` until it clashed with macOS's HTML Tidy at /usr/bin/tidy.)
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

cargo install --path crates/tdy-cli --locked --force

bindir="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin"
bin="$bindir/tdy"
"$bin" --version

hash -r 2>/dev/null || true
first="$(command -v tdy || true)"
if [ "$first" != "$bin" ]; then
  echo >&2
  echo "warning: \`tdy\` on your PATH is ${first:-not found}, not $bin." >&2
  echo "  Put $bindir first on your PATH, for example in ~/.zshrc:" >&2
  echo "    export PATH=\"$bindir:\$PATH\"" >&2
  echo "  then open a new terminal (or run \`hash -r\`) and check with \`which -a tdy\`." >&2
  exit 1
fi
echo "ok: \`tdy\` resolves to $bin"
old="$bindir/tidy"
if [ -x "$old" ] && "$old" --version 2>/dev/null | grep -q '^tidy '; then
  echo "note: the old \`tidy\` binary is still at $old; remove it with \`cargo uninstall tidy-cli\`."
fi
