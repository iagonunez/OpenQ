# OpenQ

An open-source show-control / cue playback system, written in Rust with egui.
Free to use, including for commercial productions. See [License](#license).

## Install

OpenQ is built from source on your computer by **[openqup](https://github.com/iagonunez/openqup)**,
an installer in the style of rustup. It installs Rust and the build dependencies if they're missing
(on Windows and macOS an LGPL-only FFmpeg, on Linux your distribution's packages, asking first),
builds the newest tagged version and installs the app with a menu entry. `openqup update` rebuilds
when a new version is tagged; `openqup uninstall` removes it.

- **Linux and macOS:**

  ```sh
  curl --proto '=https' --tlsv1.2 -fsSL https://github.com/iagonunez/openqup/releases/latest/download/install.sh | sh
  ```

- **Windows 10/11 (64-bit)**, in PowerShell:

  ```powershell
  powershell -ExecutionPolicy Bypass -c "irm https://github.com/iagonunez/openqup/releases/latest/download/install.ps1 | iex"
  ```

The app is installed to `~/Applications/OpenQ.app` (macOS, code-signed ad hoc or with your
Developer ID: `--sign`), `%LOCALAPPDATA%\Programs\OpenQ` (Windows) or `~/.local/bin/openq` (Linux).
openqup's README lists every download, its source and its license.

**By hand** (Linux; this is what openqup does there):

```sh
# Debian/Ubuntu
sudo apt install build-essential clang pkg-config libasound2-dev libpipewire-0.3-dev libudev-dev \
  libavcodec-dev libavformat-dev libavutil-dev libavdevice-dev libavfilter-dev \
  libswscale-dev libswresample-dev
# Fedora:  sudo dnf install clang pkgconf alsa-lib-devel pipewire-devel systemd-devel ffmpeg-free-devel
# Arch:    sudo pacman -S --needed base-devel clang pkgconf alsa-lib libpipewire ffmpeg
curl https://sh.rustup.rs -sSf | sh          # Rust, if you don't have it
git clone https://github.com/iagonunez/OpenQ && cd OpenQ
cargo build --release && ./target/release/openq
```

Audio goes through PipeWire when it is running (each sink and source is listed by name), and
through ALSA otherwise. Without the PipeWire headers, build with
`--no-default-features --features video` (ALSA only).

## Development

```sh
cargo run --release                     # empty workspace
cargo run --release -- show.openq       # open a workspace
cargo test                              # engine + model tests
cargo run --release --example smoke     # headless end-to-end check of real audio/video (muted)
tools/stress.sh                         # stress, soak, fuzz and crash-recovery suite (slow)
tools/ui_perf.sh show.openq [cue]       # frame times of the real window through a scripted session
```

**UI performance:** `src/ui/perf_tests.rs` runs the whole window headless (part of `cargo test`):
editing and playing an animated text cue must cost about what an idle frame does and must not
re-render the text, and frame time must not grow badly with the number of cues. For the real
window and GPU, `tools/ui_perf.sh` drives the app with `OPENQ_DEBUG_SCRIPT` (timed actions such as
`go`, `stop`, `scrub`, `drag`, `close`, `quit`) and `OPENQ_DEBUG_FRAMES` (per-frame CSV).

**Stress tests** (`tests/stress.rs`) are `#[ignore]`d so everyday `cargo test` stays fast. Run
`tools/stress.sh` before tagging a version and after big features; `.github/workflows/ci.yml` runs it
on every `v*` tag (run it by hand on the commit first, since openqup offers a tag as soon as it exists). It needs no sound card (audio goes to a virtual output that records what
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

User manual: **Help → User manual** (F1) in the app, or open `docs/manual/index.html` in a browser. API documentation: `cargo doc --no-deps --open`.

Video uses the FFmpeg libraries (the system's on Linux; on Windows and macOS openqup installs an
LGPL-only build inside the app), plus clang at build time, so anything FFmpeg can play works, with no codec plugins to
install. Build without video with `--no-default-features`.

**Versions** are `vX.Y.Z` tags; there are no binary releases. openqup
(<https://github.com/iagonunez/openqup>, its own repository) builds the newest tag on the user's
computer: BtbN's LGPL-only FFmpeg on Windows, FFmpeg compiled from source without GPL parts on macOS,
the distribution's FFmpeg on Linux. Its "End to end" workflow checks a full install on all three.

## Cues

Audio, Mic (live input), Video, Camera (live video), Text, Group (folder), Wait, Memo, Fade, Start,
Stop, Pause, Go To, Devamp, Network.

A show can hold several **cue lists** (tabs above the cue list). GO works on the list in front;
cues in other lists keep playing and can be targeted from any list.

**Text cues** use any font installed on the system (plus the built-in Inter), with weight,
italic, letter spacing, line height, outline, drop shadow and a background box (colour, padding,
rounded corners). They get proper shaping (kerning, ligatures, right-to-left scripts) and fall back
to another font for missing characters.

Every cue has a number, name, notes, color, armed flag, **pre-wait**, **post-wait** and a
**continue mode**:

- *Do not continue*: wait for the next GO.
- *Auto-continue*: fire the next cue once the post-wait has elapsed (the post-wait is timed from the
  start of the action).
- *Auto-follow*: fire the next cue when this cue's action ends.

Group modes: start first child and enter the group, playlist (start the first child and skip past
the group), start all children at once, start a random child.

The **Triggers** tab gives a cue a **hotkey** that fires it directly, and a **timecode trigger**.

## Timecode

Each cue list can follow **LTC** (from an audio input) or **MTC** (from a MIDI input) at 24, 25,
29.97 drop-frame or 30 fps; cues fire as running timecode passes their trigger time.
Jumps and dropouts don't fire cues.

## Edit and show mode

- **Edit mode** (default): add, delete, reorder and edit cues. Double-click a row's number, name,
  pre-wait or post-wait to edit it in place (Enter commits, Esc cancels), or press Enter to
  rename the selected cue. Click the continue column to cycle *do not continue → auto-continue →
  auto-follow*. Undo/redo with Ctrl+Z / Ctrl+Shift+Z.
- **Show mode** (Ctrl+E or the EDIT/SHOW button): the cue list is locked, the inspector and
  toolbar are hidden and rows are bigger. GO, stop, pause, hotkeys and selecting the next cue all
  still work.

### Audio cues

- **Time & Loops**: waveform with draggable start/end trim markers (drag to pan, Ctrl+scroll to
  zoom, live playhead), play count / infinite loop, and **rate** (varispeed, pitch follows).
- **Slices**: double-click the waveform to add slice markers (drag to move, right-click to delete).
  Each slice plays N times, or loops until a **Devamp** cue (or `/cue/<n>/devamp`) lets it finish
  its pass and play on.
- **Levels & Routing**: level, pan, and a per-cue **routing matrix** (file channels → device
  outputs, dB per crosspoint).
- **EQ**: up to 8 bands (high/low-pass, shelves, peaks) with a response curve; drag handles to set
  frequency/gain, scroll on a handle for Q.
- **Audio FX**: a chain of up to 8 effects after the EQ (delay, reverb, compressor, limiter,
  gate, distortion, chorus, bitcrusher); reorder, bypass, edit live (tails carry on).
- **Object audio**: place each channel as an object in a room of speakers
  (Settings → Audio: stereo/quad/5.1/7.1/ring presets or your own layout). Distance-based panning
  at constant power, spread from point to whole room; Fade cues move the objects, click-free.
- Level, pan, EQ, effect and routing changes apply live to a playing cue. Pick the output device in
  Settings → Audio.
- **Mic cues** play a live input (device and channels on the I/O tab) through the same mixer,
  with levels, EQ, routing and fades; about 30 ms of latency.

### Video cues

- Movies and still images (PNG/JPEG/WebP/…, and SVG, rasterised sharp for the size it is shown
  at; stills can show for a set time or until stopped).
- Start/end trim, loops, hold last frame.
- **Geometry**: position, scale (proportional or not), rotation, opacity, layer, fit/fill/stretch.
- **Fade cues** can animate level, opacity, geometry, video effects and object-audio positions.
- **Video FX** (video, camera and text cues): colour controls, exposure, hue, colourise, sepia,
  invert, posterise, threshold, vignette, blur, pixellate, sharpen and chroma key, run in the
  stage's GPU shader, live and fadeable.
- **Keyframe animation** (Animation tab, video and text cues): position, scale, rotation, crop,
  opacity, colour tint and, for text, fill/outline/shadow/background colours, with hold, linear,
  ease or custom Bézier curves and once/loop/ping-pong. Values are relative to the cue's own
  geometry, so Fade cues still move the animated layer. It is evaluated live on the GPU as the
  stage draws (a few interpolations per layer), so it adds no decoding or rendering load.
- The **soundtrack** plays through the OpenQ mixer (stereo), so it has level, fades and a routing
  matrix like an audio cue.
- **Stages**: any number of outputs (Settings → Video stages), each with its own canvas size and
  window. *Edit mapping* shows a white test pattern with a grid, pixel rulers and circles, plus
  four corner pins; drag them (or nudge with the arrow keys, Shift for 10 px) for keystone
  correction / projection mapping. Double-click or F11 for fullscreen.
- **Wide surfaces and edge blending**: a stage can show part of another stage (*Span & edge
  blend* in its settings), so one picture is split across several projectors; soft-edge blend
  zones (with projector gamma) make the overlaps add up to even light.
- **Blend modes** per layer (Geometry tab): Normal, Add, Screen, Multiply, Lighten, Darken,
  Subtract.
- **Camera cues** show a live camera or capture card (v4l2 / DirectShow / AVFoundation) as a layer
  with the same geometry, opacity and fades.

## Remote control and network cues

Turn on **OSC** in Settings → General (UDP port 53000 by default):

| Address | Action |
|---|---|
| `/go`, `/stop`, `/panic`, `/pause`, `/resume` | transport |
| `/playhead/next`, `/playhead/previous`, `/playhead/<number>` | move the playhead |
| `/cue/<number>/start` `stop` `pause` `resume` `devamp` | act on one cue |

A **Network cue** sends one OSC message (host, port, address, arguments) to other gear, such as a
lighting desk.

## Remote devices

Another OpenQ on the same network (a stage manager's laptop, a tablet at the tech table) can
connect to the one running the show, mirror it live and control it.

1. On the main device: **Remote → Share this OpenQ**, tick *Let paired devices connect*, then
   **Pair a new device…**. A code such as `C9XR-E9HJ` appears.
2. On the other device: **Remote → Connect to another OpenQ**, search the network (or type the
   address), enter the code and connect. Later connections need no code.

The replica shows the main device's cue list, playhead, running cues and meters. GO, stop,
pause and the playhead act on the main device, which does all the playback. Each paired device
has a role on the main device: *View only*, *Control*, or *Control & edit*. Edits made on a
replica with the edit role are saved on the main device. **Remove** revokes a device at once.
The replica's own show comes back when it disconnects.

Security: the pairing code is checked with SPAKE2 (a password-authenticated key exchange), so a
code can't be cracked offline from captured traffic. A code is valid for 5 minutes and 5 tries.
Pairing gives the device its own random 256-bit key, and both sides prove they hold it on every
connection. All traffic is encrypted and authenticated with ChaCha20-Poly1305. Addresses that keep
failing are blocked for 5 minutes. Keys are stored in `~/.config/openq/remote.json` (mode 0600),
never in show files. Remote access uses TCP port 53200, and discovery uses UDP 53201.

## Keys

| Key | Action |
|---|---|
| Space | GO |
| Esc | Stop all (fades out; press again to stop instantly) |
| P / R | Pause all / resume all |
| Ctrl+E | Toggle edit / show mode |
| Ctrl+Z / Ctrl+Shift+Z | Undo / redo |
| Enter | Rename the selected cue in place |
| ↑ / ↓ | Move the playhead |
| ← / → | Collapse / expand a group |
| Ctrl+1…0 | New Audio, Video, Group, Wait, Memo, Fade, Start, Stop, Pause, Go To cue |
| Ctrl+D / Ctrl+G / Del | Duplicate / group the selection / delete |
| Ctrl+N / O / S / Shift+S | New / open / save / save as |

To reorder cues, drag them. Drop onto the middle of a group row to put them inside the group. You
can also drop media files from your file manager onto the window.

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

## Look and credits

The UI uses a dark theme (`src/ui/theme.rs`) with bundled fonts in `assets/fonts`:

- [Material Symbols Rounded](https://fonts.google.com/icons) icons (Apache-2.0)
- [Inter](https://fonts.google.com/specimen/Inter) for text and
  [JetBrains Mono](https://fonts.google.com/specimen/JetBrains+Mono) for times, and
  [Barlow Condensed](https://fonts.google.com/specimen/Barlow+Condensed) for the OpenQ title
  (SIL OFL 1.1)

They are subset to the glyphs in use. To add an icon, put its name in `tools/icons.txt` and
rebuild with `python3 -I tools/build_fonts.py <dir with the upstream font files>`, which also
regenerates `src/ui/icons.rs`.


## License

Copyright 2026 Iago Núñez. OpenQ is licensed under the **Apache License 2.0** ([LICENSE.md](LICENSE.md)):

- **Allowed:** any use, including commercial productions. You may run paid shows, concerts,
  events and installations, and be paid to program or operate them. You may copy, change and share
  OpenQ, keeping the copyright notice, `LICENSE.md` and `NOTICE.md` with every copy.
- **Also allowed:** selling copies or changed versions of OpenQ, and hosting, support or
  consulting services built on it. Changed files must say that they were changed.

Third-party fonts, crates and system libraries keep their own licenses. They are listed in
[NOTICE.md](NOTICE.md) and [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md), and are also shown
in the app under **Help → About OpenQ and licenses**. Two points to note when distributing binaries:

- Ship `LICENSE.md`, `NOTICE.md`, `THIRD_PARTY_LICENSES.md` and `assets/fonts/*.txt` with them.
  The app also contains these texts.
- Build against an **LGPL-only FFmpeg**, or use `--no-default-features`. A GPL build of FFmpeg
  (`--enable-gpl`, as many Linux distributions ship it) puts a distributed binary as a whole
  under GPL 3, with its complete source.

After changing dependencies, run `python3 -I tools/third_party_licenses.py`.

OpenQ is open source: the Apache License 2.0 is approved by the Open Source Initiative.
