# Lighting — design plan

A lighting design and playback system inside OpenQ: patch, programmer, palettes, effects, a 3D
visualiser and Python scripting for design, plus Light cues in the main cue list for playback.
It should follow the workflow of large lighting desks (command line, programmer, pools, executors)
and be ready for a custom hardware console later.

Task ids (`A1`, `B2`…) are referenced from the phase plan at the end. Sizes: **S** ≈ a day,
**M** ≈ a few days, **L** ≈ a week or more.

---

## 0. Principles

1. **Show mode is sacred.** In Show mode, design edits are locked. Only playback works (GO, executors,
   grand master, blackout). The lighting engine never waits on the UI, the show lock, Python or the
   visualiser.
2. **Design and playback are separate.** Design data (patch, palettes, cues, effects, scripts) is
   *compiled* into a flat, read-only playback representation (`LightShow`). The DMX thread only
   reads that representation, so it never allocates, locks or parses.
3. **Lighting is its own island.** It runs on its own thread (or process, see §1) with its own
   clock. Audio, video and engine tick budgets are not touched. A crash in lighting must not stop
   sound and video, and the reverse.
4. **One source of truth for time.** Light cues are fired by the OpenQ engine like any other cue,
   with the same logical timestamp. The lighting engine schedules fades from that timestamp, so
   sound, video and light stay in sync.
5. **Hardware-agnostic I/O.** Outputs and control surfaces sit behind traits, so the custom
   console is "just another surface + output".

---

## 1. Architecture

```
 UI thread (egui)                      Lighting engine (thread "openq-light", 44 Hz…)
 ─────────────────                     ─────────────────────────────────────────────
 Lighting workspace ── edits ──►  Design model (patch, palettes, cues, effects)
 (Edit mode only)                        │ compile (background, off the hot path)
                                         ▼
 OpenQ engine ── Light cue GO ──►  LightShow (Arc swap)  ◄── executors / faders / surface input
 (1 kHz tick)     (rtrb queue)           │
                                         ▼
                                   Playback state: cue stacks, fades, effects, programmer
                                         │ resolve HTP/LTP + priority + grand master
                                         ▼
                                   DMX frame [universes × 512] ─► outputs (Art-Net, sACN, USB…)
                                         │ (triple buffer, latest frame)
                                         ▼
                                   Visualiser (wgpu, reads latest frame; drops frames freely)

 Python worker (child process "openq --py-worker")
   design time: scripts → effect tables / generated cues / palettes (cached, compiled into LightShow)
   live (optional): per-frame values through shared memory; late frames → last value held
```

**Thread vs process.** Recommended: the lighting engine as an in-process thread (like the audio
mixer) with no locks on its hot path, and **Python as a child process**. Reasons:
- DMX at 44 Hz is ~23 ms per frame. Computing a few thousand channels takes microseconds, so a
  thread is enough, and it shares the engine clock with no IPC.
- Python is the risky part (GIL, slow user code, crashes, infinite loops). A child process keeps
  it fully isolated, so it can be killed and restarted. It also avoids linking libpython into the
  app, which would complicate oqman builds on three OSes.
- Option for later: `openq --light-engine` as a separate process (shared memory frame + command
  pipe) if crash isolation from audio/video is ever needed. The `LightShow`/command boundary is
  designed so this switch is mechanical.

---

## A. Data model and patch

Ideas
- Fixture *types* (definitions) vs fixture *instances* (patched). Instances have id/number, name,
  type + mode, universe/address, 3D position/rotation, invert/swap pan-tilt, DMX limits,
  layer/class.
- Attributes are normalised by *feature*: Intensity, Colour (RGB, CMY, colour wheel, CTO), Position
  (pan/tilt), Beam (zoom, focus, iris, gobo, prism, shutter/strobe), Control. Fine (16-bit)
  channels and virtual intensity for fixtures without a dimmer.
- Multi-instance fixtures (LED bars with cells): sub-fixtures addressable as `101.1`…`101.12`.
- Universes: logical universes mapped to outputs (one universe may go to several outputs).

Tasks
- **A1 (M)** `light/model.rs`: `FixtureType`, `Mode`, `Channel`, `Attribute`, `Feature`, `Fixture`,
  `Patch`, `Universe`. Serde with `#[serde(default)]`, stored in the `.openq` file (`Workspace.lighting`).
- **A2 (S)** Patch validation: overlaps, address out of range, auto-address next free.
- **A3 (M)** Patch window: sheet with fixture id, name, type, mode, universe.address, position.
  Multi-patch ("patch 20 × fixture X from 1.1 with offset 16").
- **A4 (S)** Attribute value model: normalised `f32` 0–1 per attribute + physical units for UI
  (degrees, Kelvin, %), converted to DMX (8/16/24-bit) only at output.

## B. Fixture library

Ideas
- Built-in generics: dimmer, RGB, RGBW, RGBA, RGBWAUV, CMY, moving head spot/wash, LED bar, strobe,
  fog/haze.
- Importers: **Open Fixture Library** JSON (MIT data), **GDTF** (open standard, zip with XML and 3D
  models; gives real geometry for the visualiser), **QLC+ `.qxf`** (Apache-2.0 data).
- A fixture editor for custom/unknown fixtures (channels, ranges, wheel slots with colours, gobo images).
- User library folder in the config dir and fixtures embedded in the show file (shows are portable).

Tasks
- **B1 (S)** Built-in generic types.
- **B2 (M)** OFL JSON importer (+ bundled index of popular fixtures, or fetch on demand).
- **B3 (L)** GDTF importer (XML + wheels + 3D model references).
- **B4 (M)** Fixture type editor UI.
- **B5 (S)** QLC+ `.qxf` importer.

## C. DMX output and input

Note: Art-Net and sACN are network protocols (to an Ethernet node). USB interfaces are separate
drivers. Both should be supported:

Ideas
- `DmxOutput` trait: `send(universe, &[u8; 512])`, `status()`, `reconnect()`.
- **Art-Net 4** (unicast to discovered nodes via ArtPoll, or broadcast), **sACN/E1.31**
  (multicast, priority), **Enttec DMX USB Pro** and compatibles (serialport, widely cloned),
  **Enttec Open DMX / FTDI** (needs precise break timing; best effort), **uDMX**.
- Output thread at a configurable rate (default 44 Hz), only changed universes are sent plus a keep-alive.
- Input: Art-Net/sACN in, to record from another desk or to merge (HTP/LTP merge per universe).
- Monitor: DMX sheet (universe grid, per channel value + who owns it), output status in the status bar.
- Reconnect like audio (`AUDIO_RETRY` pattern): a lost USB device comes back on its own.

Tasks
- **C1 (S)** `DmxOutput` trait + output thread + frame triple buffer.
- **C2 (M)** Art-Net out + ArtPoll node discovery.
- **C3 (S)** sACN out.
- **C4 (M)** Enttec USB Pro (serialport) + reconnect; Open DMX best effort.
- **C5 (M)** DMX monitor window.
- **C6 (M)** Art-Net/sACN input + merge.
- **C7 (S)** Settings → Lighting: outputs, universe mapping, rate.

## D. Programmer and selection (the design core)

Ideas
- **Command line** in the style of big desks: `1 thru 10 at 50`, `group 3 at full`,
  `1 + 5 - 3`, `record cue 5`, `update`, `copy`, `delete`, `preset 2.1`, `time 3`, `off`. Typed
  or built from buttons; a parser with history and auto-complete.
- **Selection**: fixture sheet, groups, selection order (for fans/effects), odd/even/every n,
  highlight/lowlight (brings selected fixtures to open white, front), next/previous.
- **Programmer**: holds values set by the user (always highest priority, shown in red/active),
  "blind" mode (programming without touching the output), preview of a cue in the visualiser only.
- **Encoders**: attribute bar per feature (Intensity, Position, Colour, Beam…), mouse wheel / drag,
  fine mode, colour picker (HSV, gel library for colour mixing per fixture's emitters), pan/tilt XY pad.
- **Fan/spread**: distribute values across the selection (linear, centre-out, ends).
- **Pools / presets (palettes)**: Groups, Position, Colour, Beam, Focus, All. Cues refer to presets,
  so editing a preset updates every cue (essential for touring/refocus).

Tasks
- **D1 (M)** Programmer state + selection model + highlight.
- **D2 (M)** Command line parser and executor (grammar documented in the manual).
- **D3 (M)** Encoder bar + colour picker + XY pad.
- **D4 (M)** Groups and presets pools (reference vs hard values).
- **D5 (S)** Blind / preview modes.
- **D6 (S)** Fan/spread tools.

## E. Light cues and playback

Ideas
- **New cue kind `Light`** in the OpenQ cue list (added at the end of `KindTag::ALL`). Two flavours:
  - *Light cue* = a look stored in a **lighting cue stack** (tracking), fired from the main list.
  - *Executor cue* = control an executor/sequence (go, back, release, set fader level).
- **Tracking**: only changes are stored; `cue only`, `block`, tracking previews ("where does this
  value come from").
- **Timing**: per-cue fade/delay, per-attribute and per-fixture timing (split times, fan delay),
  fade curves (reuse `FadeCurve`), "move in black" (MIB) to pre-position movers.
- **Executors**: sequences on faders/buttons with HTP/LTP masters, priorities, flash, temp, swap,
  rate/speed masters, grand master, blackout. They work in Show mode.
- **Resolve order**: playbacks by priority → HTP on intensity, LTP otherwise → programmer → grand
  master → DMX limits/inversion → output.
- **OpenQ integration**: Light cues use pre/post wait, continue, timecode triggers, hotkeys and
  groups like other cues. Stop/Pause/Fade cues act on them (Fade → executor level). Panic = fade to
  "panic look" (configurable: blackout or house lights).

Tasks
- **E1 (L)** Playback engine: cue stack, tracking state, fades with per-attribute timing, LTP/HTP
  resolve. Pure, no I/O, unit-tested.
- **E2 (M)** `LightShow` compiler + Arc swap to the light thread.
- **E3 (M)** `CueKind::Light` + inspector tabs (look summary, timing, executor target) + engine Backend glue.
- **E4 (M)** Executor bar UI (virtual faders/buttons) + grand master + blackout.
- **E5 (S)** Move in black, cue-only, block.
- **E6 (S)** Stop/Pause/Fade/Panic integration with existing control cues.

## F. Effects engine

Ideas
- Built-in **phasers/shapes**: sine, saw, square, ramp, random, chase steps, circle/figure-8 for
  position, rainbow for colour, intensity chases.
- Parameters: speed (BPM or Hz), size, base, phase spread across the selection (fan), groups/blocks/wings,
  direction, bounce. Tempo from a tap button, MIDI clock, OSC or **audio** (beat detection from a
  Mic cue or the master bus).
- Effects run on the light thread: evaluated per frame from precompiled tables, no allocation.
- Effects layered on top of cue values (relative) or absolute.

Tasks
- **F1 (M)** Effect model + evaluator (shape tables, phase per fixture).
- **F2 (M)** Effect editor UI with live preview (in the visualiser, blind if needed).
- **F3 (S)** Tap tempo, BPM masters, MIDI clock in.
- **F4 (M)** Audio-reactive input (envelope/beat from mixer meters).

## G. Python scripting

Ideas
- **Design time** (default): a script reads the patch/selection and *generates* things: effect
  curves (as tables of keys), cues, presets, fixture positions (arrays/circles in 3D), patching
  helpers. The output is ordinary data stored in the show, so playback never runs Python.
- **Live effects** (opt-in): a script exposes `def frame(t, fixtures) -> values`. It runs in the
  worker process at the DMX rate, writing to shared memory; the light thread reads the latest
  values and holds the last ones if the worker is late or dead (with a status warning).
- **Bake**: a live script can be baked to a table for the show (no Python on show night).
- API module `openq` (typed, documented): `patch`, `selection`, `fixture.attr`, `cue`, `preset`,
  `effect`, `time`, math helpers; numpy optional for heavy maths.
- Editor in the app (syntax highlight, run, console, errors pointing to lines); scripts stored in
  the show file; a script library folder.
- Sandbox: no imports beyond an allow-list by default (scripts in shared show files are untrusted),
  time/memory limits, kill on timeout.

Tasks
- **G1 (M)** Worker process: `openq --py-worker`, finds system Python 3 (or a bundled one via
  oqman), JSON/msgpack over stdin/stdout, restart on crash, timeouts.
- **G2 (M)** `openq` Python API + generated stub file (`.pyi`) for IDE completion.
- **G3 (M)** Design-time scripts: generate effects/cues/presets.
- **G4 (M)** Script editor window (code editor widget, console, run/stop).
- **G5 (L)** Live script effects through shared memory + bake.
- **G6 (S)** Sandbox / trust prompt when opening a show with scripts.

## H. 3D visualiser

Ideas
- wgpu renderer through the existing egui paint callback (`App.gpu`), in a dockable panel and a
  separate window (second monitor).
- Scene: stage set from **glTF/GLB** (or simple primitives: floor, truss, flats, a person for
  scale), fixtures with real bodies (GDTF models) or primitives, moving yokes/heads from pan/tilt.
- Beams: volumetric cones with additive blending + haze amount, colour, zoom, iris, gobo
  (projected texture), prism (multiple beams), strobe; floor/set projections (spot lighting with
  shadow maps on the set as an option, off by default for speed).
- Interaction: orbit/pan/zoom camera, saved camera views, click to select fixtures (syncs the
  programmer), drag to place fixtures, "focus" a mover by clicking a point on the stage (solves
  pan/tilt from the 3D position).
- Performance: instanced rendering of beams/fixtures, LOD by distance, quality slider, frame cap,
  reads the DMX frame (or programmer preview) only. Hundreds of beams at 60 fps on an iGPU.
- Plot view: 2D top view for patching/positions and printed plot export (SVG/PDF).

Tasks
- **H1 (M)** Renderer skeleton: camera, grid floor, primitives, fixtures as boxes, picking.
- **H2 (M)** Beams: cone shader, colour, intensity, zoom, haze, additive.
- **H3 (M)** Moving heads (pan/tilt kinematics) + click-to-focus.
- **H4 (M)** glTF stage import; fixture placement tools (array/line/circle, snap to truss).
- **H5 (M)** Gobos, prisms, strobe, floor projections.
- **H6 (S)** 2D plot view + export.
- **H7 (S)** Detached visualiser window on another display.

## I. Lighting UI (console-style workspace)

Ideas
- A **Lighting workspace** (tab/mode beside the cue list) laid out like a desk: fixture sheet,
  pools (groups/presets/effects/sequences), command line, encoder bar, executor bar, visualiser.
- **User layouts/views**: dockable panels (egui_dock or our own), saved per show and per screen,
  quick view buttons (like desk "views"). Touch-friendly sizes for a touch screen.
- Dark desk theme consistent with `ui/theme.rs`; feature colours (intensity yellow, position blue,
  colour…) consistent everywhere.
- Keyboard-first: number pad, `Thru` `+` `-` `@` `Full`, Enter; everything on the command line.
- Show mode: the workspace becomes **playback only** (executor bar, grand master, visualiser,
  monitor). Programmer/design panels are hidden or read-only.

Tasks
- **I1 (M)** Lighting workspace shell + docking + saved views.
- **I2 (M)** Fixture sheet (fast table: 1000+ fixtures, virtualised rows).
- **I3 (M)** Pool windows (grid of buttons, drag to record, colour swatches).
- **I4 (S)** Show-mode lock for all design actions (single check point in the command layer).
- **I5 (S)** Keyboard map + desk keypad panel.

## J. Performance and isolation

Ideas and budgets
- Light thread: < 0.5 ms per frame for 64 universes / 2000 fixtures with effects, no allocation in
  steady state (checked by a test with a counting allocator).
- UI: the lighting workspace keeps < 4 ms per frame with 1000 fixtures; the visualiser has its own budget.
- Edits compile `LightShow` on a worker and swap it in; the light thread never waits for it.
- No lock shared with the audio callback or the 1 kHz engine tick. Light cue GO crosses through
  an `rtrb` queue (same as audio commands).
- Stress and bench: add lighting to `tests/stress.rs` (fuzz cue stacks, output device loss) and to
  `tools/bench.sh` (frame time per fixture count).
- Recovery: the light thread is guarded like `guarded_tick` (panic → hold last frame, notify).

Tasks
- **J1 (S)** Bench + allocation test for the light thread.
- **J2 (S)** Stress tests (fuzz, device loss, Python worker killed).
- **J3 (S)** Panic guard: hold last frame, report, keep outputs alive.

## K. Custom hardware console (future)

Ideas
- `Surface` trait: inputs (faders, encoders, buttons, keypad, jog) and feedback (LEDs, motor
  faders, small displays, scribble strips).
- **OpenQ Console Protocol**: documented, versioned, over USB CDC serial and/or UDP; device
  announces its layout (number of faders/encoders/buttons) so OpenQ maps it automatically.
- The console can also be a DMX output (USB DMX ports inside the console), so the console is the
  only box needed.
- Until the hardware exists: map any MIDI controller (APC40, X-Touch, etc.) and OSC (TouchOSC) to
  the same `Surface` actions, so the mapping layer is tested early.
- Optional: the console runs OpenQ itself (embedded Linux) and the laptop is a remote replica,
  reusing the existing remote system.

Tasks
- **K1 (S)** `Surface` trait + action enum (shared with keyboard/command line).
- **K2 (M)** MIDI/OSC surface mapping (with learn mode).
- **K3 (M)** OpenQ Console Protocol spec (`docs/console-protocol.md`) + a simulator.
- **K4 (L)** Firmware/hardware side (separate repo).

## L. Persistence, remote and docs

- **L1 (S)** Lighting data in the `.openq` file (`Workspace.lighting`), versioned; fixture types embedded.
- **L2 (M)** Remote: replicas mirror the lighting show; executors from a replica in the Control role;
  design edits in the Edit role.
- **L3 (S)** Import/export: ASCII Light Cues (USITT ASCII) for cue/patch exchange with other desks.
- **L4 (S)** Manual chapter (en/es/gl) and translations for every new UI text.

---

## Phase plan (proposal)

**Phase 1 — "lights come on" (MVP, ~3–4 weeks)**
A1 A2 A4 · B1 · C1 C2 C3 C4 C5 C7 · D1 D3 D4(groups) · E1 E2 E3 E4 · I1 I2 I4 · J1 J3 · L1
→ Patch generic fixtures, program looks with encoders, record Light cues, play them from the main
cue list in sync with sound/video, out over Art-Net/sACN/USB, DMX monitor.

**Phase 2 — "designer's desk"**
A3 · B2 · D2 D4(presets) D5 D6 · E5 E6 · F1 F2 F3 · H1 H2 H3 · I3 I5
→ Command line, presets, effects, and a 3D visualiser with moving heads.

**Phase 3 — "power tools"**
G1–G6 (Python) · H4–H7 · B3 B4 B5 · C6 · F4 · J2 · L2 L3

**Phase 4 — "the console"**
K1–K4.

## Decisions
1. **Design is separate from the live show.** The lighting design (patch, groups, sequences of
   lighting cues with tracking) is its own document inside the show file (`Workspace.lighting`),
   edited only in Edit mode. OpenQ **Light cues** in the main cue list *target* a designed
   lighting cue (sequence + cue) and GO it; they hold no values of their own.
2. Python: system Python 3 in a child process first (decide on bundling with oqman later).
3. Visualiser placement and docking: decided in Phase 2.

## Phase 1 order (one commit per step)
1. A1 A2 A4 B1 L1 — `light` module: fixture types + built-in generics, patch, groups,
   sequences/cues with tracking, stored in `Workspace.lighting`.
2. C1 — DMX frame, `DmxOutput` trait, one sender thread per output (latest frame wins).
3. C2 C3 — Art-Net (+ ArtPoll discovery), sACN.
4. C4 — Enttec DMX USB Pro.
5. E1 — playback engine (pure): sequence playback, fades, HTP/LTP, grand master.
6. E2 J3 — `LightShow` compiler, light thread, panic guard.
7. E3 E6 — `CueKind::Light` + engine glue + inspector.
8. I1 I2 I4 — Lighting workspace (Edit mode only), patch/fixture sheet.
9. D1 D3 D4 — programmer, encoders, groups; record/update lighting cues.
10. E4 C5 C7 — executor bar, DMX monitor, Settings → Lighting outputs.
11. J1 — bench + allocation test; manual chapter at the end.
