#!/usr/bin/env bash
# Benchmarks to compare builds (src/bench.rs, examples/bench.rs). Every complete run is compared
# with the previous one (and with the last run of the previous OpnQ version), added to
# bench/history.json, and docs/performance.html's results section (tables + charts) is
# regenerated from that history.
#
#   tools/bench.sh                       # full run (~1 min), compared with the previous run
#   tools/bench.sh --baseline            # compare with bench/baseline.json instead
#   tools/bench.sh --set-baseline        # full run, also saved as bench/baseline.json
#   tools/bench.sh --only mixer                     # some groups: compared, not kept
#   tools/bench.sh --quick --only engine,lookup     # quick look (not compared, not kept)
#   tools/bench.sh --label "before fade rewrite"    # extra flags go to the runner
#
# Every run is also kept in bench/results/<date>-<revision>.json (git-ignored). The history holds
# one run per revision (a re-run replaces it); numbers only compare on the same machine, so the
# page uses the runs from the latest run's machine. Exit code 2 = a metric got worse by more than
# the tolerance (--tolerance PCT, default 15). Close other heavy programs first; re-run before
# trusting a single regression.
set -euo pipefail
cd "$(dirname "$0")/.."
baseline=bench/baseline.json
set_baseline=0
use_baseline=0
args=()
for a in "$@"; do
  case $a in
    --set-baseline) set_baseline=1 ;;
    --baseline) use_baseline=1 ;;
    *) args+=("$a") ;;
  esac
done
mkdir -p bench/results
rev=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)
git diff --quiet HEAD -- src examples Cargo.toml Cargo.lock 2>/dev/null || rev="$rev-dirty"
out="bench/results/$(date +%Y%m%d-%H%M%S)-$rev.json"
cargo build --release --example bench -q
cmp=()
if [[ $use_baseline == 1 ]]; then
  [[ -f $baseline ]] || { echo "no baseline yet: run tools/bench.sh --set-baseline"; exit 1; }
  cmp=(--compare "$baseline")
fi
status=0
./target/release/examples/bench --revision "$rev" --out "$out" --history bench/history.json \
  --page docs/performance.html "${cmp[@]}" "${args[@]}" || status=$?
if [[ $set_baseline == 1 ]]; then
  cp "$out" "$baseline"
  echo "baseline: $baseline ($rev)"
fi
exit $status
