# OpenQ

An open-source show-control / cue playback system, written in Rust with egui.

```sh
cargo run --release                     # empty workspace
cargo run --release -- show.openq       # open a workspace
cargo test                              # engine + model tests
cargo run --release --example smoke     # headless end-to-end check of real audio/video (muted)
```

User manual: open `docs/manual/index.html` in a browser. API documentation: `cargo doc --no-deps --open`.

Video uses the system FFmpeg libraries (libavformat/libavcodec/libswscale/libswresample, plus
clang at build time), so anything FFmpeg can play works, with no codec plugins to install. Build
without video with `--no-default-features`.

## Cues

Audio, Video, Text, Group (folder), Wait, Memo, Fade, Start, Stop, Pause, Go To, Devamp, Network.

**Text cues** use any font installed on the system (plus the built-in Inter), with weight,
italic, letter spacing, line height, outline and drop shadow. They get proper shaping (kerning,
ligatures, right-to-left scripts) and fall back to another font for missing characters.

Every cue has a number, name, notes, color, armed flag, **pre-wait**, **post-wait** and a
**continue mode**:

- *Do not continue*: wait for the next GO.
- *Auto-continue*: fire the next cue once the post-wait has elapsed (the post-wait is timed from the
  start of the action).
- *Auto-follow*: fire the next cue when this cue's action ends.

Group modes: start first child and enter the group, playlist (start the first child and skip past
the group), start all children at once, start a random child.

Cues can also have a **hotkey** (Basics tab) that fires them directly.

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
- Level, pan, EQ and routing changes apply live to a playing cue. Pick the output device in
  Settings → Audio.

### Video cues

- Movies and still images (PNG/JPEG/…; stills can show for a set time or until stopped).
- Start/end trim, loops, hold last frame.
- **Geometry**: position, scale (proportional or not), rotation, opacity, layer, fit/fill/stretch.
- **Fade cues** can animate level, opacity *and* geometry.
- The **soundtrack** plays through the OpenQ mixer (stereo), so it has level, fades and a routing
  matrix like an audio cue.
- **Stages**: any number of outputs (Settings → Video stages), each with its own canvas size and
  window. *Edit mapping* shows a test pattern and four corner pins; drag them (or nudge with the
  arrow keys) for keystone correction / projection mapping. Double-click or F11 for fullscreen.

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
src/geometry.rs     layer placement + corner-pin homography
src/engine.rs       cue state machine (GO, waits, continues, groups, fades, pause, panic)
src/media/audio.rs  cpal output + lock-free real-time mixer (sample-accurate starts, gain ramps,
                    per-voice EQ and routing matrix)
src/media/decoder.rs  per-voice symphonia decode → trim/loop → rubato resample → ring buffer
src/media/video.rs  FFmpeg decode threads / still images → RGBA frames + soundtrack to the mixer
src/show.rs         shared state + 1 kHz engine thread
src/osc.rs          OSC remote-control listener
src/control.rs      show-control commands shared by UI, OSC and remote devices
src/remote/         paired remote devices: SPAKE2 handshake + encrypted link, server, replica
                    client, LAN discovery
src/ui/             egui front end (cue list, inspector tabs, waveform, EQ, stages + mapping)
```

The engine works on logical timestamps, so the timing error does not build up along a chain of
cues. It hands starts to the backend up to 25 ms early, together with the exact target time. The
mixer then starts the voice on the matching sample, so auto-continues stay sample-accurate even
when the UI or engine thread is busy.

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

