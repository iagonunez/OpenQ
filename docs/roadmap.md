# OpenQ roadmap — detailed plans for upcoming work

Plans for upcoming work, in priority order.

## 1. Soft-edge blending (finish)
Already done:
- Model: `Stage.span_of: Option<StageId>`, `crop: [x0,y0,x1,y1]` (`Stage::FULL_CROP`),
  `blend: [l,t,r,b]` as fractions of the output, `blend_gamma` (default 2.2).
- Math in `geometry.rs`: `to_crop(p, crop)` and `blend_gain(t, gamma)`. The S-curve satisfies
  f(t) + f(1−t) = 1 once gamma is undone, and this is tested.

Rendering, in `ui/stage.rs::stages()`:
- **Source stage:** `span_of` if set, otherwise the stage itself. Use that stage's layers and its
  canvas size for `layer_quad`.
- **Layer points:** map each through `to_crop(p, stage.crop)`, then through the stage homography.
- **Masking and blend:** after the layers, paint black outside the mapped quad. For each non-zero
  blend edge, add a black overlay mesh with alpha = 1 − blend_gain(t).
- **Auto-open:** a layer's stage opens automatically, and so do the stages spanning it. A surface
  that only exists to be spanned doesn't open on its own.

Settings UI in `settings_stages`: a "span of" combo, crop, blend edges and gamma.

## 2. MIDI triggers
- Model: `Cue.midi_trigger: Option<MidiTrigger{kind: NoteOn|ProgramChange|ControlChange,
  channel: Option<u8>, number: u8}>`.
- Settings: a MIDI input port, plus an optional "GO on note/PC".
- A midir input thread maps incoming messages to `control::Command`, then calls
  `control::apply` (the same path as OSC).

## 3. Timecode triggers
- **MTC:** decode quarter-frame messages via midir.
- **LTC:** read a cpal input; biphase-mark decode, sync word 0x3FFD.
- Model: `Cue.timecode_trigger`. A cue fires when timecode crosses its time and re-arms after a
  backwards jump.
- Settings: timecode source and frame rate.

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

## Remote (possible follow-ups)
- Merge edits per cue instead of sending the whole workspace (removes last-writer-wins).
- Push the main device's display list so a replica can pick stage outputs.
- Directed broadcast per interface, or mDNS, for discovery.
- Rate-limit Edit messages on the server side.
