#!/usr/bin/env bash
# Frame-time check of the real application (window, GPU and all), for UI slowdowns that the
# headless tests (src/ui/perf_tests.rs) can't see.
#
#   tools/ui_perf.sh show.opnq [cue number]
#
# Runs a scripted session on the show (select the cue, open the Animation tab and the geometry
# preview, scrub, drag keys, GO, stop, close the stages) and prints per-phase frame times:
# ui = time in App::ui, cpu = whole frame incl. tessellation/painting, gap = time between frames.
# Phases should all cost about what "idle" does; "playing" also pays for the open stage window.
set -euo pipefail
show=${1:?usage: tools/ui_perf.sh show.opnq [cue number]}
cue=${2:-1}
here=$(cd "$(dirname "$0")/.." && pwd)
cargo build --release --manifest-path "$here/Cargo.toml" -q
out=$(mktemp --suffix .csv)
OPNQ_DEBUG_SCRIPT="0.5:select:$cue,0.6:tab:animation,0.7:preview,1.5:scrub,4:noscrub,4.5:drag,7:nodrag,7.5:go,10:stop,12:close,15:quit" \
OPNQ_DEBUG_FRAMES="$out" "$here/target/release/opnq" "$show" >/dev/null 2>&1 || true
python3 -I - "$out" <<'EOF'
import csv, sys
rows = [tuple(map(float, r)) for r in csv.reader(open(sys.argv[1]))]
def q(sel, i, p):
    v = sorted(r[i] for r in sel)
    return v[min(len(v) - 1, int(len(v) * p))]
for name, a, b in [("idle", 0.8, 1.5), ("scrub", 1.6, 4), ("drag", 4.6, 7), ("playing", 7.6, 10), ("stopping", 10.1, 12), ("closed", 12.1, 15)]:
    sel = [r for r in rows if a <= r[0] < b]
    if not sel:
        print(f"{name:9} no frames"); continue
    print(f"{name:9} ui med {q(sel,1,.5):6.2f} p95 {q(sel,1,.95):6.2f} ms | cpu med {q(sel,3,.5):6.2f} max {q(sel,3,1):6.2f} ms | gap max {q(sel,2,1):6.1f} ms")
EOF
rm -f "$out"
