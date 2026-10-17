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
  - 3D visualiser: a separate program (future) that connects to OpenQ; not part of the app.
- **Console (future; wait until its requirements and hardware are clearer):**
  - Two phases, never at the same time:
    - *Design:* every lighting tool can be on screen (patch, programmer, keypad, effects, sequences…).
    - *Running:* light cues are recorded and played by the Cues system. The lighting designer only sees the
      executors and the DMX preview, and only the executor window takes input.
  - The console is a hybrid with a Design mode and a Running mode. Each display is assigned one tool (keypad,
    programmer, executors, DMX monitor…).
  - Usual setup: one PC for Cues, another for lighting design and executors during the show. If everything runs on
    one PC, the other displays serve the Cues mode or the executors while the show runs.
  - Input: key presses and executor taps go straight to the show core (ordered, never dropped, never waiting for a
    redraw). Screens redraw only on change, at a rate each tool picks; continuous controls (faders) may merge values.
    Physical controls skip the UI (as `crate::surface` does).
  - Each tool draws from a snapshot of the state it needs and sends commands, so it can be a deferred viewport now
    and its own process later (core + one UI process per display, reusing the remote protocol).
  - Related: the Playback window is still an immediate viewport (it slows to the main window's rate when that one is
    hidden). Stage video moved to its own process (done 2026-10-17, see "Video output process").

## Lighting effects without Python (done; follow-ups)
- Done: custom effects come from effect formulas (`light::formula`): one expression per attribute
  over the cycle position and pattern row, a fixed list of functions, sliders from `param` lines,
  several effects per formula, playback settings kept outside it. OpenQ works them out itself (no
  interpreter or sandbox, no loops, cost capped). Replaced the Python scripts and the `scripting`
  feature (wasmtime, ~84 crates, the interpreter oqman installed).
- Formulas can't set the pattern width or step count from a slider (both are fixed in the
  `effect` line), nor carry state between steps.
- Done: the user manual ("Effects made in Python", en/es/gl) shows how to write effects in plain
  Python 3 on your own computer and save them as `.oqfx` files, which OpenQ imports as data.
- Possible: a small importable Python module (instead of the copy-paste `save_effect`), and an
  "Open in a text editor"/reload loop for it.

## Video output process (done: `openq --video-output`, own wgpu renderer, YUV in the shader; follow-ups)
- Live pictures in the geometry preview (small frames from the video process); it shows posters now.
- Keep fades in progress (and exact loop positions) when a restarted video process brings layers back.
- Mipmaps for the canvas and for text, so pictures drawn much smaller than their size don't alias.
- Hardware decoding (VAAPI / VideoToolbox / D3D11VA) with zero-copy upload where the platform allows it.
- Optional "close outputs when nothing plays" per stage (now: black while open).
- GO/Stop keys pressed in an output window forwarded to OpenQ.
- The same protocol over the network for a separate media-server machine.

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
