#!/usr/bin/env bash
# Compare the most recent criterion run against a saved baseline.
#
# Usage: perf-gate.sh <threshold>
#   threshold — fractional regression budget (e.g. 0.10 = 10%).
#
# For every benchmark under target/criterion/, the script reads:
#   <bench>/base/estimates.json           — saved earlier via --save-baseline base
#   <bench>/new/estimates.json            — current run
# and compares the mean point estimates. If any benchmark's new p50 is more
# than `threshold` above the baseline, the script prints the offending row
# and exits 1.

set -euo pipefail

THRESHOLD="${1:-0.10}"
ROOT="target/criterion"

if [ ! -d "$ROOT" ]; then
    echo "perf-gate: no criterion output at $ROOT — did the bench step run?" >&2
    exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
    echo "perf-gate: jq is required to parse criterion estimates.json" >&2
    exit 1
fi

failures=0
overall_status=0

while IFS= read -r -d '' new_file; do
    bench_dir="$(dirname "$(dirname "$new_file")")"
    bench_name="${bench_dir#"$ROOT/"}"
    base_file="$bench_dir/base/estimates.json"
    [ -f "$base_file" ] || continue
    base_ns=$(jq -r '.mean.point_estimate' "$base_file")
    new_ns=$(jq -r '.mean.point_estimate' "$new_file")
    if ! [[ "$base_ns" =~ ^[0-9.eE+-]+$ && "$new_ns" =~ ^[0-9.eE+-]+$ ]]; then
        echo "perf-gate: skipping $bench_name — non-numeric estimates"
        continue
    fi
    delta=$(awk -v b="$base_ns" -v n="$new_ns" 'BEGIN { if (b <= 0) print 0; else print (n - b) / b }')
    over=$(awk -v d="$delta" -v t="$THRESHOLD" 'BEGIN { print (d > t) ? 1 : 0 }')
    printf '%-50s base=%-12.2fns  new=%-12.2fns  delta=%+.2f%%\n' \
        "$bench_name" "$base_ns" "$new_ns" "$(awk -v d="$delta" 'BEGIN { print d * 100 }')"
    if [ "$over" = "1" ]; then
        failures=$((failures + 1))
        overall_status=1
    fi
done < <(find "$ROOT" -path '*/new/estimates.json' -print0)

if [ "$overall_status" -ne 0 ]; then
    echo
    echo "perf-gate: $failures benchmark(s) regressed beyond $(awk -v t="$THRESHOLD" 'BEGIN { print t * 100 }')%." >&2
    echo "perf-gate: see docs/PERF.md for the regression policy." >&2
fi

exit "$overall_status"
