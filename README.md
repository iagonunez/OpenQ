# OpenQ

An open-source show-control / cue playback system, written in Rust with egui.

```sh
cargo run --release                     # empty workspace
cargo run --release -- show.openq       # open a workspace
cargo test                              # engine + model tests
cargo run --release --example smoke     # headless end-to-end check of real audio/video (muted)
```

Video needs GStreamer (`gst-plugins-base`, plus `-good`/`-libav` for common containers and codecs).
Build without it with `--no-default-features`.

## Cues

Audio, Video, Group (folder), Wait, Memo, Fade, Start, Stop, Pause, Go To.

Every cue has a number, name, notes, color, armed flag, **pre-wait**, **post-wait** and a
**continue mode**:

- *Do not continue*: wait for the next GO.
- *Auto-continue*: fire the next cue once the post-wait has elapsed (the post-wait is timed from the
  start of the action).
- *Auto-follow*: fire the next cue when this cue's action ends.

Group modes: start first child and enter the group, playlist (start the first child and skip past
the group), start all children at once, start a random child.

## Keys

| Key | Action |
|---|---|
| Space | GO |
| Esc | Stop all (fades out; press again to stop instantly) |
| P / R | Pause all / resume all |
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
src/engine.rs       cue state machine (GO, waits, continues, groups, fades, pause, panic)
src/media/audio.rs  cpal output + lock-free real-time mixer (sample-accurate starts, gain ramps)
src/media/decoder.rs  per-voice symphonia decode → trim/loop → rubato resample → ring buffer
src/media/video.rs  GStreamer playbin → RGBA frames for the stage window
src/show.rs         shared state + 1 kHz engine thread
src/ui/             egui front end (cue list, inspector, transport, active cues, stage)
```

The engine works on logical timestamps, so the timing error does not build up along a chain of
cues. It hands starts to the backend up to 25 ms early, together with the exact target time. The
mixer then starts the voice on the matching sample, so auto-continues stay sample-accurate even
when the UI or engine thread is busy.
