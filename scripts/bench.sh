#!/usr/bin/env bash
# bench.sh — run rtrt-compress benches and print a savings table.
#
# Usage:
#   ./scripts/bench.sh                 # full criterion run + table
#   ./scripts/bench.sh --table-only    # skip criterion, print savings only
#
# The savings table is computed by piping each fixture through the compressor
# and comparing pre/post character counts. Criterion handles timing separately.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

print_savings_table() {
  cat <<'EOF'

== character-count savings ==
fixture   level  before   after    saved%
--------  -----  -------  -------  ------
EOF
  for fixture in short code mixed long; do
    for level in lite full ultra; do
      before=$(wc -c < "crates/rtrt-compress/benches/fixtures/${fixture}.md")
      after=$(cargo run --quiet --release --example compress_savings -- \
        --fixture "$fixture" --level "$level" 2>/dev/null || echo "$before")
      pct=$(( (before - after) * 100 / before ))
      printf "%-8s  %-5s  %7d  %7d  %5d%%\n" "$fixture" "$level" "$before" "$after" "$pct"
    done
  done
}

if [ "${1:-}" = "--table-only" ]; then
  print_savings_table
  exit 0
fi

echo "== running criterion benches =="
cargo bench -p rtrt-compress --bench compress_bench -- --quick 2>&1 | tail -40 || true

print_savings_table

echo
echo "Full HTML report: target/criterion/report/index.html"
