# OpenQ roadmap — detailed plans for upcoming work

Plans for upcoming work, in priority order.

## 1. Stage outputs: extras after edge blending
Done: span/crop/edge blend rendering (`ui/stage.rs`), the Settings → Video stages "Span & edge
blend" section, black outside the warped canvas, and per-layer blend modes (`ui/blend.rs`: a
wgpu paint callback with fixed-function blend states).

Still open:
- An "auto split" helper: N outputs across a surface with a given overlap fills in crop + blend.
- Blend modes that need the destination in the shader (Overlay, Difference, Colour dodge): render
  the stage into an offscreen texture and composite there.
- Geometry preview: show the spanning outputs' parts and blend zones on the surface.
- Blend-zone black level lift (projectors don't reach true black, so the overlap looks grey).

## 2. MIDI triggers
- Model: `Cue.midi_trigger: Option<MidiTrigger{kind: NoteOn|ProgramChange|ControlChange,
  channel: Option<u8>, number: u8}>`.
- Settings: a MIDI input port, plus an optional "GO on note/PC".
- A midir input thread maps incoming messages to `control::Command`, then calls
  `control::apply` (the same path as OSC).

## 3. Timecode (done: LTC/MTC in per cue list, cue timecode triggers on the Triggers tab)
Still open:
- Chase mode: on a jump, start the cues that would be running at the new time, at the right offset.
- Per-list timecode offset, and an optional "fire cues skipped by a jump".
- Read the rate from LTC itself (bit 10 and frame counting) instead of the configured rate.

## 4. Lighting / DMX mode (large; target is the user's own console hardware)
- **Mode:** a new `Mode::Lighting`.
- **Patch:** fixtures with type, mode, universe, address and a 3D position.
- **Fixture library:**
  - Built-in generics: dimmer, RGB(W), moving head.
  - Open Fixture Library JSON import.
- **Programmer:** intensity, colour, pan/tilt and other attributes; groups.
- **Light cue kind:** tracking with LTP fades run by the engine, plus HTP intensity.
- **DMX engine:** about 44 Hz, behind a `DmxOutput` trait with these outputs:
  - Art-Net
  - sACN/E1.31
  - Enttec DMX USB Pro (serialport)
  - A documented OpenQ serial protocol for the user's custom hardware
- **Monitoring and preview:**
  - DMX monitor grid.
  - 3D visualiser drawn with egui: orbit camera, beam cones, floor spots.

## Animation (done: live keyframes for video/text layers; follow-ups)
- **Bake to file**: render heavy effects (animated blur, typewriter/reflowing text, morphing SVG)
  to a movie with alpha (ProRes 4444 / VP9 alpha / PNG sequence through FFmpeg) and play it as a
  normal Movie layer. Transforms, opacity and colour don't need it: they're evaluated live.
- Record mode: geometry edits in the preview write keys at the playhead.
- Copy/paste keys, multi-select and box-select on the timeline; snapping to other keys.
- Animate Camera cues; animate output faders and audio level.
- Re-rasterise an SVG when a live edit raises its scale (now only on start).
- Brightness/colourise beyond multiply tint (needs a custom paint callback / shader).

## Effects and object audio (done: audio FX chains, video FX in the layer shader, object audio; follow-ups)
- Fade cues for audio effect parameters (wet/dry, filter sweeps); only video effects and object
  positions fade now.
- Effects on outputs/master, not only per cue.
- Group objects: one object for several channels (stereo pair moving together) and object level.
- Video effects that need the destination or several passes (glow, edge detect, separable
  large-radius blur); blur is a single-pass 48-tap spiral, fine up to ~30 px.
- Room in 3D (height layers) and a VBAP option alongside DBAP.

## Remote (possible follow-ups)
- Merge edits per cue instead of sending the whole workspace (removes last-writer-wins).
- Push the main device's display list so a replica can pick stage outputs.
- Directed broadcast per interface, or mDNS, for discovery.
- Rate-limit Edit messages on the server side.

## Reliability (done: stress suite, crash recovery; follow-ups)
- Done: `tests/stress.rs` + `tools/stress.sh` (run by the CI workflow on version tags), autosave + crash
  recovery window, crash reports, engine panic recovery, audio device reconnect, panic-safe
  workers, DLL-smoothed mixer clock, preloading of continue successors, cue-loop detection,
  voice cap (`MAX_VOICES`), bounded poster cache, atomic saves.
- Measure the clock filter on real hardware (only the virtual output was measured: worst chained
  start error 9 → 2 samples).
- Video: reconnect lost stage displays; a stalled decoder watchdog (decoder alive but no frames).
- Mic/LTC inputs: reopen a lost input device like the output.
- A "show is running" lock: refuse risky actions (device change, open) while cues play.
- Run the stress suite on Windows and macOS CI runners too (Linux only now).
