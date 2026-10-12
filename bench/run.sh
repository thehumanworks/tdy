#!/usr/bin/env bash
# Benchmark `tdy scan` against du, dust, dua and gdu on a fixed fixture tree.
#
#   bench/run.sh                    # 1M-file fixture under $TMPDIR/tdy-bench
#   FILES=200000 bench/run.sh       # smaller tree
#   FIX=~/some/real/dir bench/run.sh
#
# Requires: hyperfine. Optional competitors are skipped when not on PATH.
set -euo pipefail
cd "$(dirname "$0")/.."

FILES=${FILES:-1000000}
FIX=${FIX:-${TMPDIR:-/tmp}/tdy-bench/f${FILES}}
RUNS=${RUNS:-10}
OUT=${OUT:-bench/results-$(uname -s | tr A-Z a-z)-$(uname -m).md}

RUSTFLAGS="${RUSTFLAGS:--C target-cpu=native}" cargo build --release -q -p tdy-cli -p tdy-bench
TIDY=$PWD/target/release/tdy

if [ ! -d "$FIX" ]; then
  echo "generating $FIX ($FILES files)…"
  ./target/release/tdy-fixture --out "$FIX" --files "$FILES"
fi

cmds=("$TIDY scan -q $FIX")
names=("tdy")
command -v du   >/dev/null && { cmds+=("du -s $FIX");            names+=("du"); }
command -v dust >/dev/null && { cmds+=("dust -P -d 0 $FIX");     names+=("dust"); }
command -v dua  >/dev/null && { cmds+=("dua aggregate $FIX");    names+=("dua"); }
command -v gdu  >/dev/null && { cmds+=("gdu -snp $FIX");         names+=("gdu"); }

args=()
for i in "${!cmds[@]}"; do args+=(-n "${names[$i]}" "${cmds[$i]}"); done

hyperfine -N --warmup 2 --runs "$RUNS" --export-markdown "$OUT" "${args[@]}"

echo
echo "peak RSS (KB):"
TIMECMD=$(command -v gtime || command -v /usr/bin/time || true)
for i in "${!cmds[@]}"; do
  if [ -n "$TIMECMD" ] && "$TIMECMD" --version >/dev/null 2>&1; then
    tmp=$(mktemp); "$TIMECMD" -f %M -o "$tmp" ${cmds[$i]} >/dev/null 2>&1; rss=$(tail -1 "$tmp"); rm -f "$tmp"
  else
    # BSD time (macOS): -l prints "maximum resident set size" in bytes
    rss=$( { /usr/bin/time -l ${cmds[$i]} >/dev/null; } 2>&1 | awk '/maximum resident/ {print int($1/1024)}')
  fi
  printf '  %-6s %s\n' "${names[$i]}" "$rss"
done | tee -a "$OUT"

echo "results: $OUT"
