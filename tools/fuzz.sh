#!/usr/bin/env bash
# Fuzz tests (tests/fuzz.rs): damaged and hostile show files, media, fixture/effect files, config
# files, show packages, remote messages, network packets and devices. Slow, so plain `cargo test` skips them.
#
#   tools/fuzz.sh                       # every target, OPNQ_FUZZ_SECS (default 20) each
#   tools/fuzz.sh show                  # only targets whose name contains "show"
#   OPNQ_FUZZ_SECS=600 tools/fuzz.sh   # a long run
#   OPNQ_FUZZ_SEED=<n> tools/fuzz.sh show            # repeat a run (each run prints its seed)
#   OPNQ_FUZZ_REPLAY=<file> tools/fuzz.sh show_files # one saved input (target/tmp/fuzz-failures)
#
# `tools/demo_show.sh --no-run` first gives fuzz_show_files every cue feature and real media.
# Extra cargo flags go in CARGO_FLAGS, e.g. CARGO_FLAGS=--no-default-features.
set -euo pipefail
cd "$(dirname "$0")/.."
# One test at a time: they measure memory and use fixed ports.
# shellcheck disable=SC2086
exec cargo test --release ${CARGO_FLAGS:-} --test fuzz -- --ignored --test-threads=1 --nocapture "$@"
