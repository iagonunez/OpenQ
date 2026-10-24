#!/usr/bin/env bash
# Stress, soak, fuzz and crash-recovery tests (tests/stress.rs). Slow and hard on the machine,
# so plain `cargo test` skips them: run this before a release and after big features.
#
#   tools/stress.sh            # full suite, about a minute or two
#   tools/stress.sh timing     # only tests whose name contains "timing"
#   OPNQ_STRESS_SCALE=5 tools/stress.sh   # five times the work (longer soak, more fuzzing)
#
# Extra cargo flags go in CARGO_FLAGS, e.g. CARGO_FLAGS=--no-default-features (as CI does).
set -euo pipefail
cd "$(dirname "$0")/.."
# One test at a time: several measure timing, threads and memory of the whole process.
# shellcheck disable=SC2086
exec cargo test --release ${CARGO_FLAGS:-} --features fault-injection --test stress -- \
  --ignored --test-threads=1 --nocapture "$@"
