#!/usr/bin/env bash
# Benchmarks to compare builds (src/bench.rs, examples/bench.rs). Run one before a big feature or
# fix, save it as the baseline, then run again afterwards and read the comparison.
#
#   tools/bench.sh                       # full run (~1 min), compared with bench/baseline.json
#   tools/bench.sh --set-baseline        # full run, saved as the new baseline
#   tools/bench.sh --quick --only engine,lookup     # quick look at some groups
#   tools/bench.sh --label "before fade rewrite"    # extra flags go to the runner
#
# Every run is kept in bench/results/<date>-<revision>.json (git-ignored: numbers are only
# comparable on the same machine). Exit code 2 = a metric got worse by more than the tolerance
# (--tolerance PCT, default 15). Close other heavy programs first; re-run before trusting a
# single regression.
set -euo pipefail
cd "$(dirname "$0")/.."
baseline=bench/baseline.json
set_baseline=0
args=()
for a in "$@"; do
  if [[ $a == --set-baseline ]]; then set_baseline=1; else args+=("$a"); fi
done
mkdir -p bench/results
rev=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)
git diff --quiet 2>/dev/null || rev="$rev-dirty"
out="bench/results/$(date +%Y%m%d-%H%M%S)-$rev.json"
cargo build --release --example bench -q
cmp=()
if [[ $set_baseline == 0 && -f $baseline ]]; then cmp=(--compare "$baseline"); fi
status=0
./target/release/examples/bench --revision "$rev" --out "$out" "${cmp[@]}" "${args[@]}" || status=$?
if [[ $set_baseline == 1 ]]; then
  cp "$out" "$baseline"
  echo "baseline: $baseline ($rev)"
elif [[ ! -f $baseline ]]; then
  echo "no baseline yet: run tools/bench.sh --set-baseline"
fi
exit $status
