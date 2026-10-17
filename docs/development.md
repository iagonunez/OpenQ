# Development

Notes for working on OpenQ itself. For users, see the [README](../README.md) and the user manual.

```sh
cargo run --release                     # empty workspace
cargo run --release -- show.openq       # open a workspace
cargo test                              # engine + model tests
cargo run --release --example smoke     # headless end-to-end check of real audio/video (muted)
tools/stress.sh                         # stress, soak, fuzz and crash-recovery suite (slow)
tools/fuzz.sh                           # hostile input from every way in (slow; OPENQ_FUZZ_SECS per target)
tools/ui_perf.sh show.openq [cue]       # frame times of the real window through a scripted session
tools/demo_show.sh                      # big demo show for manual testing, opened in the app
```

**Demo show:** `tools/demo_show.sh` downloads free media (music by Kevin MacLeod, Google sound
library effects, Blender open film clips; ~550 MB, cached in `target/demo-show/media`), writes
`target/demo-show/demo.openq` with `examples/demo_show.rs` and opens it. "The Lighthouse Keeper"
has five cue lists (two acts, preshow/interval, a timecode list, a hotkey pad), 400 media cues and a
68-fixture rig with 300 lighting cues, and uses every feature of the show model at least once.
`--fresh` writes the show again (your edits to it are kept otherwise), `--no-run` doesn't open it,
`--media` only downloads.

**UI performance:** `src/ui/perf_tests.rs` runs the whole window headless (part of `cargo test`):
editing and playing an animated text cue must cost about what an idle frame does and must not
re-render the text, and frame time must not grow badly with the number of cues. For the real
window and GPU, `tools/ui_perf.sh` drives the app with `OPENQ_DEBUG_SCRIPT` (timed actions such as
`go`, `stop`, `scrub`, `drag`, `close`, `quit`) and `OPENQ_DEBUG_FRAMES` (per-frame CSV).

**Stress tests** (`tests/stress.rs`) are `#[ignore]`d so everyday `cargo test` stays fast. Run
`tools/stress.sh` before tagging a version and after big features; `.github/workflows/ci.yml` runs it
on every `v*` tag (run it by hand on the commit first, since oqman offers a tag as soon as it exists). It needs no sound card (audio goes to a virtual output that records what
it plays) and covers:

- engine fuzzing (hundreds of random shows under random transport and edits) and scale (thousands
  of cues running at once);
- sample accuracy of chained cues while other threads hog the show lock and every core;
- soak tests for leaked voices, threads and memory, and the mixer's voice limit;
- damaged show files, garbage media, timecode noise, OSC floods and junk remote connections;
- crash recovery: a process that aborts leaves a recoverable autosave and a crash report, and,
  with the `fault-injection` feature, panics injected into the engine, the audio callback, a
  decoder and the prober are recovered from.

`OPENQ_STRESS_SCALE=<n>` multiplies the work for longer runs.

**Fuzz tests** (`tests/fuzz.rs`, also `#[ignore]`d) throw damaged and deliberately malicious data at
every way into OpenQ and require an error or a sane result, never a crash, a hang or runaway memory.
Any panic counts, also one OpenQ catches on a worker thread; a call that doesn't answer in time
counts as a hang. One target per way in:

- `fuzz_show_files`: show files with valid JSON but hostile values (huge, negative and non-finite
  numbers, duplicate ids, cues aimed at the wrong things, paths to devices, other users' files and
  network shares, deep nesting) and broken bytes; what loads is saved, vetted and played;
- `fuzz_media_files`: damaged audio, video, image and SVG files of 20-odd formats through every
  reader, FIFOs, endless devices, huge sparse files, and SVGs that reach for other files, nest
  without end or ask for filters that would take days;
- `fuzz_lighting_files`, `fuzz_config_files`: effect/fixture files, zips, command lines, gobo images;
  garbage (or a FIFO, or a folder) in every file of the config folder, and saving where it can't;
- `fuzz_remote_messages`: hostile handshakes, every message an Edit-role replica can send over the
  real encrypted link, every message a main device can send to a replica;
- `fuzz_network_packets`, `fuzz_devices`: Art-Net, sACN, OSC, MIDI, MTC and LTC input; sound cards,
  inputs, MIDI and serial ports and network outputs that don't exist or aren't what they claim.

`tools/fuzz.sh` runs them (`OPENQ_FUZZ_SECS` per target, default 20; the CI workflow gives each a
minute). Each run prints its seed (`OPENQ_FUZZ_SEED` repeats it); a failing input is saved under
`target/tmp/fuzz-failures/` and `OPENQ_FUZZ_REPLAY=<file>` runs just that one. With the demo show in
`target/demo-show` the show fuzzer also draws on every cue feature and real media, and with the
`ffmpeg` program installed the media fuzzer starts from many more formats.

User manual: **Help → User manual** (F1) in the app, or open `docs/manual/index.html` in a browser. API documentation: `cargo doc --no-deps --open`.

Video uses the FFmpeg libraries (the system's on Linux; on Windows and macOS oqman installs an
LGPL-only build inside the app), plus clang at build time, so anything FFmpeg can play works, with no codec plugins to
install. Build without video with `--no-default-features`.

**Lighting** is the cargo feature `lighting` (on by default; it turns on `fixture-download`). Without it OpenQ is cues only: no Cues/Lighting switch, playback window or Light
cue type; lighting designs in a show still load, are saved back and play (`oqman install --no-lighting`
builds that way).

**Lighting effect formulas** (`light::formula`) are a small arithmetic language OpenQ works out into
effect tables itself: no interpreter or sandbox, no loops, and a cost cap checked before a formula runs.
They are data, kept in shows and `.oqfx` files.

**Fixture downloads** from the Open Fixture Library website use ureq with rustls (cargo feature
`fixture-download`, on by default; ring needs a C compiler for the target, so the cross-compile checks
run with `--no-default-features`). `cargo test oflsite -- --ignored` talks to the real site.

**Versions** are `vX.Y.Z` tags; there are no binary releases. oqman
(<https://github.com/iagonunez/oqman>, its own repository) builds the newest tag on the user's
computer: BtbN's LGPL-only FFmpeg on Windows, FFmpeg compiled from source without GPL parts on macOS,
the distribution's FFmpeg on Linux. Its "End to end" workflow checks a full install on all three.

## Architecture

```
src/model.rs        cue tree, workspace file (.openq JSON, media paths stored relative)
src/dsp.rs          biquad EQ (design, response, processing)
src/effects.rs      audio/video effect settings (parameter tables, fades, shader uniform)
src/audio_fx.rs     real-time audio effect chains (delay, reverb, dynamics, ...)
src/spatial.rs      object audio: speakers, objects, DBAP panning
src/geometry.rs     layer placement + corner-pin homography
src/animation.rs    keyframe tracks, easing, sampling (evaluated as the stages draw)
src/engine.rs       cue state machine (GO, waits, continues, groups, fades, pause, panic)
src/media/audio.rs  cpal output + lock-free real-time mixer (sample-accurate starts, gain ramps,
                    per-voice EQ, effects and ramped routing matrix)
src/media/decoder.rs  per-voice symphonia decode → trim/loop → rubato resample → ring buffer
src/media/video.rs  FFmpeg decode threads / still images / cameras → RGBA frames + soundtrack to
                    the mixer
src/media/input.rs  live audio inputs (Mic cues)
src/media/still.rs  still images (image crate) and SVG (resvg)
src/media/text.rs   Text cues → coverage planes (background, shadow, outline, fill)
src/media/tcio.rs   timecode input (LTC/MTC readers that cue lists follow)
src/timecode.rs     SMPTE labels, LTC encoder/decoder, MIDI Time Code
src/show.rs         shared state + 1 kHz engine thread (recovers from internal errors)
src/recovery.rs     crash safety: panics → errors, autosave + crash detection, crash reports
src/osc.rs          OSC remote-control listener
src/control.rs      show-control commands shared by UI, OSC and remote devices
src/remote/         paired remote devices: SPAKE2 handshake + encrypted link, server, replica
                    client, LAN discovery
src/ui/             egui front end (cue list, inspector tabs, waveform, EQ, stages + mapping)
```

The engine works on logical timestamps, so the timing error does not build up along a chain of
cues. It hands starts to the backend up to 25 ms early, together with the exact target time. The
mixer then starts the voice on the matching sample, so auto-continues stay sample-accurate even
when the UI or engine thread is busy. The mixer clock is tied to wall-clock time through a
delay-locked loop, which smooths out the audio thread's scheduling jitter. Cues that running cues
will continue into are preloaded, like the next GO.

**Crash safety.** Unsaved changes are autosaved every few seconds, and a show left behind by a
crash is offered back at the next start. Saves are atomic. A panic in a decoder, prober or worker
thread becomes an error for that one cue. A panic in the engine stops all cues and the show carries
on. A failing audio callback goes silent instead of crashing, and a lost audio device is reopened
automatically.
