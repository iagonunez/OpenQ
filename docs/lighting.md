# Lighting — design plan

A lighting design and playback system inside OpenQ: patch, programmer, palettes, effects and
Python scripting for design, plus Light cues in the main cue list for playback. The 3D visualiser
is a separate program (future) that connects to OpenQ (see §H).
It should follow the workflow of large lighting desks (command line, programmer, pools, executors)
and be ready for a custom hardware console later.

Task ids (`A1`, `B2`…) are referenced from the phase plan at the end. Sizes: **S** ≈ a day,
**M** ≈ a few days, **L** ≈ a week or more.

---

## 0. Principles

1. **Show mode is sacred.** In Show mode, design edits are locked. Only playback works (GO, executors,
   grand master, blackout). The lighting engine never waits on the UI, the show lock, Python or a
   connected visualiser.
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
                                         │ (latest frame)
                                         ▼
                                   External visualiser (separate program: patch + positions from
                                   OpenQ, DMX over Art-Net/sACN or the link; drops frames freely)

 Python worker (child process: system Python 3 running the bundled openq_worker.py)
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

Status (2026-10-14, P1–P6): done. How fixture types work now:
- **Model.** `FixtureType` → `FixtureMode` (master `channels`, cells in `heads`, declared `slots`) →
  `ChannelDef` (attribute, offset, fine, default, highlight, named `ranges`). A `ChannelRange` is a
  coarse-byte DMX span with a name, an optional slot colour and a gobo image name; picking it sets the
  range's middle. New attributes: `ColorMacro`, `Gobo2`, `Gobo2Rotation`, `PrismRotation`.
- **Cells (multi-instance).** A mode's `heads` are cells, controlled as units numbered `101.1`, `101.2`…
  Cell ids are derived (`model::cell_id`: `CELL_BASE + fixture × 1024 + cell − 1`), so they aren't
  stored in the patch and stay in order for `thru`; fixture numbers are capped at `MAX_FIXTURE_ID`.
  `Lighting::unit`/`unit_channels`/`unit_attributes`/`units` replace `mode_of` wherever a unit is meant;
  playback compiles one `FixtureOut` per unit at the fixture's address; presets share values per
  (type, cell); renumbering/removing a fixture moves/drops its cells' values. A cell with colour but no
  dimmer gets a virtual dimmer only when the master has no real one (a master dimmer already sets the
  cells' intensity: `FixtureMode::unit_attributes`).
- **Files.** Shows carry copies of the types they use (`Lighting.fixture_types`, pruned when unused).
  `.oqfixture` (`light::fixlib`): `{"format": "openq-fixture", "version": 1, "fixture": …}`. Every type
  read from outside goes through `fixlib::sanitize` (limits, offsets < 512, names).
- **Library.** `<config>/fixtures/*.oqfixture`, offered when patching in every show; `fixlib::Library`
  reads/writes it on a background thread (`openq-fixtures`).
- **Import** (`light::import`, one entry point by extension): OFL JSON (`ofl`: capabilities → ranges,
  wheels → slot names/colours, matrix/template channels → cells, switching-channel aliases, a repeated
  light source → a cell named after the channel's first word), GDTF (`gdtf` + `zipread`: channel sets →
  ranges, wheel slots with CIE→sRGB colours, geometry references → cells, first DMX break only),
  QLC+ `.qxf` (`qxf`: presets/groups, capabilities, heads → cells). XML is parsed without DTDs (a plain
  `<!DOCTYPE>` is stripped). Imports go into the show and the library. `examples/fixture_import.rs`
  prints what a file gives (checked on real OFL, QLC+ and GDTF files).
- **Editor** (`ui/fixture_types.rs`, Patch → Fixture types): show / library / built-in lists, header and
  physical data, modes, cells (add, repeat cell 1 N times), channel table, range editor (colours,
  equal slots). Save to library, add/update in the show (fixtures keep their mode by name), export.
- **Programmer / command line.** Range buttons under each encoder (by name, so each type gets its own
  value); `gobo star`, `gobo2 3`, `wheel "deep red"`, `shutter open`; `101.1 thru 101.12`. Highlight
  opens wheels/shutter at their "Open" range.
- Repeated channels (2026-10-15): extra attributes `ColorWheel2`, `Frost2`, `Prism2`, `Control2`,
  `Control3`, `Effect2`, `Effect2Speed`; `import::alternates` gives each repeated attribute its list
  (Control → Control2 → Control3…), GDTF `Color2`/`Frost2`/`Prism2`/`Effects2*` map to them, command
  line words `wheel2 prism2 frost2 control2 control3`. Beyond those (and for functions OpenQ doesn't
  know) a channel becomes `Attribute::Extra(n)` (Control family, LTP, saved as `"Extra3"`), named by
  `ChannelDef.name` (the file's channel name) in the programmer; nothing is left unused (2026-10-15).
- Show vs library (2026-10-15): a show type whose library copy differs is marked (amber) in Patch →
  Fixture types, the tooltip lists what differs (`fixlib::differences`: name, physical, modes added/
  missing, channels, cells/slots, order); "Update from library" in the row menu.
- Multi-break modes (2026-10-15): `ChannelDef.dmx_break` (0-based), `FixtureMode::breaks()`/
  `footprint_of(b)`, `Fixture.breaks` (addresses of breaks 2…; `address_of(b)`); one `FixtureOut` per
  unit and break; `Lighting::spans` for overlap checks and free addresses; patch sheet menu
  "5 · 2 breaks"; MVR writes one `Address break=n` each; GDTF reads `DMXBreak` up to 8 (and the
  references' per-break offsets for cells); fixture editor Break column.
- Gobo pictures (2026-10-15): `light::gobo` thumbnails (64 px PNG, base64) in `FixtureType.images`,
  keyed by `ChannelRange.image`; GDTF `wheels/<name>.png|svg`, OFL embedded resources (website
  export); QLC+ gobos are QLC+'s own files (names only). Shown on the programmer's range buttons.

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
  groups like other cues. Stop cues act on them; Fade cues don't (decision 4). Panic = fade to
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
- **F4 (M)** Audio-reactive input (envelope/beat from mixer meters). **Done** (size only, see
  "Next steps" below; no beat detection).

## G. Python scripting (effects only, sandboxed)

Decision (2026-10-14): Python only **generates effects**, and only inside a sandbox. A first
version ran system Python with full access to edit the whole design; it was replaced because a
script in a shared show (or effect file) could have run anything on the computer.

How it works
- A script makes **custom effects**: curves sampled over one cycle, one per attribute, optionally a
  different curve per pattern row (`EffectTable`; `Shape::Custom`). Playback treats them like any
  effect (speed, size, base, relative/absolute, spread, groups, wings; fades in/out with cues).
  The table is stored in the show; Python never runs during playback.
- **Sandbox:** CPython 3.15 built for WebAssembly (WASI) runs inside wasmtime (`light::script`):
  no files except the standard library mounted read-only at `/lib`, no network, no environment,
  no other programs, memory capped at 256 MB, run time capped (20 s, epoch interruption), stdin and
  stdout in memory. The reply is untrusted JSON: every effect goes through `fxfile::sanitize`.
- **Interpreter:** installed by oqman as `python/` next to the app (`python.wasm` + ~2.6 MB of the
  standard library: only computation modules), pinned by version and SHA-256. Development:
  `python3 -I tools/fetch_python_wasi.py` → `target/python-wasi`. `OPENQ_PYTHON_WASI` overrides.
  The compiled module is cached in `<config>/python-cache` (first load compiles, ~1 s here).
- **Effect files** (`.oqfx`, `light::fxfile`): one effect as JSON (parameters, table, script,
  author, description), imported/exported on the Effects page; plain data, so sharing is safe.
  Importing never runs the script.
- **Editor in OpenQ:** Effects page → "New script effect" (starts from an example) or any custom
  effect: code editor, Run (Ctrl+Enter) / Stop, printed output and errors with the line. Run
  replaces the effect's table (a new effect also takes the script's name and settings); extra
  effects a script makes are added to the pool.
- API (`assets/python/openq.py`, stub `openq.pyi`): `openq.effect(name, attributes, steps, width,
  smooth)` → `Effect` with `fill(fn(t, row, width))`, `set/get`, playback defaults (`speed`,
  `size`, `spread`…); helpers `hsv`, `lerp`, `clamp`, `smoothstep`, `sine`, `triangle`, `square`,
  `saw`. Limits match the Rust side (16 attributes, 4096 steps, 512 rows, 1 M values).

Tasks
- **G1 (M)** Sandbox host (wasmtime + WASI CPython, limits, cache). **Done.**
- **G2 (M)** `openq` effect API + `.pyi` stub. **Done.**
- **G3 (M)** Custom table effects in playback + `.oqfx` files. **Done.**
- **G4 (M)** Script editor on the Effects page. **Done.**
- **G6 (S)** ~~Trust prompt~~: not needed, scripts can't reach the system.
- (G5 live script effects: dropped; Python never runs during playback.)
- Script parameters (`openq.param`) and the effect library folder: done 2026-10-15 (below).

## H. Visualiser link (the visualiser itself is a separate program)

Decision (2026-10-14): no 3D renderer inside OpenQ. A first in-app wgpu visualiser was built in
Phase 2 and removed again; the visualiser will be its own program (future, separate repo) that
connects to OpenQ, so its GPU load, crashes and window management stay out of the show machine's
process and it can run on another computer.

What OpenQ provides for it
- Fixture positions/rotations in the patch (`Fixture.position`/`rotation`, metres/degrees) and
  physical data in the types (`Physical`: pan/tilt range, beam angles, moving head).
- DMX out over Art-Net/sACN (already there): any visualiser can listen to it.
- A link (later): patch, positions, fixture types and the selection over the network, so the
  visualiser follows edits live and can send "select fixture" / "focus here" back. Candidates: the
  remote protocol (a View-role replica that only reads the lighting design) or MVR export (+ GDTF)
  for standard visualisers.

Tasks
- **H4 (M)** Fixture placement in OpenQ: position/rotation columns in the patch sheet, array tools
  (line, arc, grid), a 2D top view for placing fixtures. **Done** (`light::place`, patch page
  Sheet/Plan, Arrange bar).
- **H6 (S)** 2D plot view + export (SVG/PDF). **Done** (`light::plot`, Patch → Export plot: PDF on A3
  at the largest standard scale that fits, or SVG).
- **H8 (M)** Visualiser link: lighting design + selection to a connected visualiser; MVR export.
  **Done**: remote protocol (see Phase 3 status, step 6) + `light::mvr`.
- (H1–H3, H5, H7 now belong to the separate visualiser program.)

## I. Lighting UI (console-style workspace)

Ideas
- A **Lighting workspace** (tab/mode beside the cue list) laid out like a desk: fixture sheet,
  pools (groups/presets/effects/sequences), command line, encoder bar, executor bar.
- **User layouts/views**: dockable panels (egui_dock or our own), saved per show and per screen,
  quick view buttons (like desk "views"). Touch-friendly sizes for a touch screen.
- Dark desk theme consistent with `ui/theme.rs`; feature colours (intensity yellow, position blue,
  colour…) consistent everywhere.
- Keyboard-first: number pad, `Thru` `+` `-` `@` `Full`, Enter; everything on the command line.
- Show mode: the workspace becomes **playback only** (executor bar, grand master,
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
- UI: the lighting workspace keeps < 4 ms per frame with 1000 fixtures.
- Edits compile `LightShow` on a worker and swap it in; the light thread never waits for it.
- No lock shared with the audio callback or the 1 kHz engine tick. Light cue GO crosses through
  an `rtrb` queue (same as audio commands).
- Stress and bench: add lighting to `tests/stress.rs` (fuzz cue stacks, output device loss) and to
  `tools/bench.sh` (frame time per fixture count).
- Recovery: the light thread is guarded like `guarded_tick` (panic → hold last frame, notify).

What keeps big rigs cheap (audit 2026-10-14; test `ui::perf_tests::big_lighting_rig_frames_stay_cheap`)
- Cue and preset values (and the programmer) are `model::Shared` (copy-on-write `Arc`): the UI's
  per-frame design snapshot, `Show::sync_lighting`, undo steps and compile jobs share them, and
  comparing skips unchanged ones. 2000 fixtures × 3000 cues: clone + compare 1 s → 0.7 ms.
- `Lighting::fixture` uses a verified id → index cache (the UI looks fixtures up per selected unit).
- One compile thread; designs queued while it works are coalesced (a drag compiles the newest
  only). A compile panic sends `Failed` so held GOs go on; held commands also time out per frame.
- Compiled cues keep their full tracked state as 12-byte `Entry`s with a per-cue timing table.
- Fixture tiles lay out visible rows only; command-line selection add/remove use sets.
- Fixture import parses on a worker thread (panics become errors); dropped script runs are
  cancelled and the Python host never blocks its owner's drop; light engine errors are capped.
- Undo counts the lighting patch in its memory budget.

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
- **K2 (M)** MIDI/OSC surface mapping (with learn mode). **Done** (2026-10-15, `crate::surface`).
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
A3 · B2 · D2 D4(presets) D5 D6 · E5 E6 · F1 F2 F3 · I3 I5
→ Command line, presets, effects. (H1–H3, a 3D visualiser, were built and later removed: see §H.)

**Phase 3 — "power tools"**
G1–G4 (Python effects, sandboxed) · H4 H6 H8 · B3 B4 B5 · C6 · F4 · J2 · L2 L3

**Phase 4 — "the console"**
K1–K4.

## Decisions
1. **Design is separate from the live show.** The lighting design (patch, groups, sequences of
   lighting cues with tracking) is its own document inside the show file (`Workspace.lighting`),
   edited only in Edit mode. OpenQ **Light cues** in the main cue list *target* a designed
   lighting cue (sequence + cue) and GO it; they hold no values of their own.
2. Python: only for generating effects, in a WebAssembly sandbox (CPython WASI in wasmtime),
   interpreter installed by oqman (2026-10-14; replaces "system Python in a child process").
3. The 3D visualiser is a separate program connecting to OpenQ, not part of the app (2026-10-14).
4. **Lighting fades only with Light cues** (2026-10-15): a lighting cue fades with its own timing and
   the next lighting cue or a release takes over. Fade cues never target Light cues (the inspector
   says so); executor levels are faders/surfaces, not Fade cues.

## Phase 1 order (one commit per step) — done
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

## Phase 1 status
Done on branch `Lighting`: every step above.
- `src/light/`: `model` (design), `library` (generics + `bench_rig`), `playback` (compiled show,
  fades, HTP/LTP, levels, programmer, grand master), `engine` (light thread at 44 Hz, off-thread
  compiles, ordered GOs, panic guard), `dmx` + `artnet`/`sacn`/`enttec` (one sender thread each).
- `CueKind::Light` (`LightingCue`: GO a designed cue / release sequences); `control::LightControl`
  for executors and masters (routes through replicas like other transport commands).
- UI `src/ui/lighting.rs`: Cues/Lighting switch; Patch, Programmer, Sequences, Outputs, DMX
  monitor pages (Edit mode only); executor bar in both modes; Show mode = executors + monitor.
- Measured (Ryzen 9 9950X3D): one frame with 2000 fixtures / 28 universes / 3 sequences fading
  ≈ 92 µs (budget 22.7 ms); compiling that design (3 × 100 full cues) ≈ 160 ms off-thread;
  no allocation per frame or GO (`tests/light_alloc.rs`).

Not yet (Phase 2+): command line, presets/palettes, effects, 3D visualiser, fixture importers,
per-attribute timing, move in black, Art-Net/sACN input, Python, MIDI/OSC surfaces, manual chapter.

## Phase 2 order (one commit per step) — done
1. D4 I3 — presets (Intensity/Colour/Position/Beam/All) referenced by cues and the programmer, so a
   preset edit updates every cue using it; preset pool on the Programmer page.
2. D5 D6 — programmer tools: fan/spread across the selection, blind, highlight.
3. E5 — cue timing per feature, cue only, block, move in black.
4. F1 F3 — effects engine (shapes, phase spread, groups/wings, BPM) in the light thread, tap tempo.
5. F2 — effects pool and editor; effects in cues and the programmer.
6. D2 I5 — command line (`1 thru 10 at 50`, `group 2 at full`, `record cue 5`, …) and keypad.
7. E6 — Stop/Fade/Panic cues and the lighting (Stop releases, Panic option).
8. H1 — 3D visualiser: wgpu renderer, orbit camera, stage floor, fixtures, picking.
9. H2 H3 — beams (cones, colour, zoom, haze), moving heads from pan/tilt, click to focus.
   (Steps 8–9 were removed again on 2026-10-14: the visualiser becomes a separate program.)
10. B2 — Open Fixture Library import.

## Phase 2 status
Done on branch `Lighting`:
- Presets (`Value::Preset` references; per-type fallback; deleting unlinks into levels), preset pool.
- Programmer: blind, highlight (step through the selection), spread tool.
- Cues: timing per feature, cue only, block, move in black (per sequence); a running fade carries on
  when the next cue keeps its target.
- Effects (`light::effects`): sine/triangle/square/ramps/random/circle, phase spread, groups, wings,
  relative/absolute, tempo sync + tap tempo; they track through cues and fade with them; effects page
  with live preview; effects from the programmer.
- Command line (`light::command`) with abbreviations and history, desk keypad window.
- Stop cues aimed at Light cues release their sequences; optional release on Stop all / Panic.
- (3D visualiser built, then removed: it will be a separate program, §H.)
- Open Fixture Library import (`light::ofl`).
- Measured: frame with 2000 fixtures ≈ 95 µs, with two effects on every fixture ≈ 110 µs.

Left for Phase 3+: Python, GDTF/QLC+ import, placement tools (fixture position/rotation are only
in the file so far), visualiser link, Art-Net/sACN input, audio-reactive effects, MIDI/OSC surfaces, remote mirroring of executor levels, Fade cues on
lighting, manual chapter.

## Progress overview (end of session, 2026-10-14)
| Area | Tasks | State |
|---|---|---|
| A Data model and patch | A1 A2 A3 A4 | done |
| B Fixture library | B1–B5 | done: generics, OFL/GDTF/QLC+ import, type editor, library folder, `.oqfixture`, ranges, cells |
| C DMX output and input | C1–C7 | done (C6 input: merge HTP / replace / capture) |
| D Programmer | D1–D6 | done |
| E Light cues and playback | E1–E6 | done (Fade cues on lighting dropped: decision 4) |
| F Effects | F1 F2 F3 | done · F4 audio-reactive open |
| G Python | G1–G4 | done: sandboxed effect scripts, `.oqfx` files (G5 dropped, G6 not needed) |
| H Visualiser link | H4 H6 H8 | in-app 3D view removed (separate program) · placement, SVG plot, link + MVR export done |
| I Lighting UI | I1–I5 | done (pools are rows on the Programmer page, no docking) |
| J Performance | J1 J3 J2 (light panic stress test) | done |
| K Hardware console | K1–K4 | K2 done (MIDI/OSC surfaces) · K1 K3 K4 open (Phase 4) |
| L Persistence, remote, docs | L1 L2 L4 | done · L3 USITT ASCII open |

## Phase 3 order (one commit per step)
1. G1–G4 — sandboxed Python effect scripts: custom table effects, `.oqfx` effect files, script
   editor on the Effects page, oqman installs the interpreter. **Done.**
2. H4 — fixture placement: position/rotation in the patch sheet, array tools, 2D top view. **Done.**
3. B3 — GDTF import (with P1–P6: ranges/wheel slots, library + `.oqfixture`, type editor, cells, QLC+). **Done.**
4. C6 — Art-Net/sACN input: merge or record from another desk. **Done.**
5. L2 — remote: mirror executor levels/masters to replicas, lighting design edits in the Edit role. **Done.**
   Today `remote::cue_patch` ignores `lighting`, so replicas keep a stale design (and an Edit replica
   sending its workspace back can overwrite the main device's lighting). Format isn't the problem
   (JSON vs a binary encoding is only 3–5×); sending the whole design is:
   - **L2a Lighting deltas.** A `ServerMsg` lighting patch with only the changed cues, presets,
     fixtures, types, groups, effects and outputs, like `cue_patch` for cues. `model::Shared` makes
     the diff cheap (an unchanged cue is the same allocation: `Arc::ptr_eq`, no value compare).
     A drag sends a few hundred bytes instead of the design; replica edits go back the same way.
   - **L2b Full snapshots that fit.** Only on connect/resync. A big rig is ~300 MB of JSON, over
     `secure::MAX_FRAME` (64 MB), so it can't reach a replica today: compress it (zstd; repeated
     JSON keys shrink 10–20×) and send it in chunks, or raise the limit for that one message.
   - A binary encoding (postcard/bincode over the serde types, not protobuf) only if measurements
     still call for it after L2a/L2b.
6. H8 — visualiser link (design + selection to a connected visualiser; MVR export). **Done** (+ H6 SVG plot).
7. L4 — manual chapter (en/es/gl) with screenshots (`OPENQ_SCREENSHOT_LIGHTING=…`). **Done.**

## Phase 3 status
- Step 1 done (see §G): `light::script` (sandbox host), `light::fxfile` (`.oqfx`),
  `effects::EffectTable` + `Shape::Custom` (playback, preview with colour bars), Effects page script
  editor + import/export, oqman `src/python.rs`. Tests: sandbox escapes (files, writes, sockets,
  processes, env), time/memory limits, cancel, forged replies, hostile effect files; no allocation
  per frame with a table effect (`tests/light_alloc.rs`); bench `light.frame_table_*`.
- Step 2 done: patch sheet has rotation and pan/tilt invert columns; Sheet/Plan switch; Plan =
  top view (x right, z to the audience at the bottom; 1 m grid; heading tick from rotation y):
  click/Ctrl-click selects, drag moves the selection (5 cm snap), drag empty space pans, scroll
  zooms, double-click recentres. Arrange bar (selection, in number order): line, arc, grid
  (`light::place::positions`). Screenshot hook `OPENQ_SCREENSHOT_LIGHTING=plan`.

- Step 3 done (fixture profiles, see §B status).
- Step 4 done: `light::dmxin` — `Lighting.inputs` (`InputConfig`: Art-Net or sACN, wire → logical
  universe routes, `MergeMode` Htp / Input (replaces) / Capture (not output), sACN multicast
  interface). One receiver thread each (SO_REUSEADDR, so a desk on the same machine can listen
  too); sources kept apart (sACN CID, Art-Net sender), highest sACN priority wins, equal ones HTP,
  2.5 s source timeout, sACN stream-terminated and preview data honoured. OpenQ's own packets are
  ignored (`artnet::is_own_port` + local address, `sacn::own_cid`: one CID per process), so a
  broadcast output can't loop back and latch. The light thread merges inputs after playback (no
  allocation per frame); the DMX monitor shows the merged output. Programmer → "Capture input"
  reads the inputs back into programmer values (`dmxin::capture`, inverse of `ChannelDef::write`
  incl. 16-bit and pan/tilt invert; selection, or every unit off its defaults). Outputs page has
  an Inputs section with live status (sources, packets). Replicas open no inputs.
  Art-Net inputs answer ArtPoll since 2026-10-15. Not done: sACN universe discovery, per-slot merge
  ownership in the monitor.

- Step 5 done: remote protocol v3.
  - `light::patch`: `diff`/`apply` of two designs → `LightingPatch` (fixture types, fixtures,
    groups, presets, effects as `ListPatch` by id: changed items, or new order + new/changed
    items, or whole on duplicate ids; sequences down to their cues; outputs/inputs whole;
    scalars). Applying to a drifted copy fails and leaves it untouched. A test fails if a new
    `Lighting` field isn't carried.
  - Main → replica: `ServerMsg::Lighting(patch)` next to `ServerMsg::Cues`; a whole
    `ServerMsg::Workspace` after a cue list reshape has `keep_lighting` (design left out, patch
    follows), so the design only goes whole on connect/resync. Replica → main:
    `ClientMsg::Patch { cues, lighting }` for edits that keep the cue tree's shape,
    `ClientMsg::Reshape { ws, lighting }` otherwise (main keeps its design, applies the patch).
    A patch that doesn't fit → resync. This fixes the stale design on replicas and an Edit
    replica overwriting the main device's design.
  - `LiveState.lighting` (`LightLive`: masters + sequence states) → `LightEngine::mirror` on the
    replica, so its executors/Sequences page show the main device's (cleared when it stops being
    a replica).
  - Secure channel: messages ≥ 64 KB are deflated (flate2, fast) and sent in ≤ 16 MB sealed
    parts (frame kind byte: plain / part / last); caps 512 MB packed, 1 GB inflated. Measured on
    `bench_rig`: 2000 fixtures × 10 sequences × 300 cues = 548 MB JSON → 144 MB (3.8×; the
    rig's values are random floats, real shows compress better), 1 s to serialise + 1 s to
    deflate. No binary encoding needed so far.
  - Not mirrored: the DMX monitor on a replica (it would need 512 bytes per universe ~20 times a
    second to every replica); it shows the replica's own (empty) frame.

- Step 6 done: the visualiser link is the remote protocol. A visualiser connects as a replica
  (it gets the design and its patches, L2) and additionally:
  - `ClientMsg::Subscribe { dmx }` → `ServerMsg::Dmx(changed universes)` at the state rate
    (~20 Hz, only universes that changed). A replica's own DMX monitor uses it too while the
    monitor is showing (`LightEngine::mirror_frame`).
  - `LightLive.selection`: the main device's programmer selection (`Show::light_selection`, kept
    in step with the UI each frame); `ClientMsg::Select(fixtures)` (Control role or above) sets
    it (`light_selection_gen` tells the UI to take it).
  - MVR export (`light::mvr`, Patch → Export MVR…): zip (`zipread::write`) with
    `GeneralSceneDescription.xml` (MVR 1.6): one layer, a Fixture per patched fixture with a stable
    name-based UUID (show name + fixture number), Matrix (OpenQ metres x right/y up/z audience →
    MVR mm x right/y upstage/z up; rotation = heading y · tilt x · roll z), `GDTFSpec`
    `Manufacturer@Name.gdtf` (no GDTF files are kept, so none are embedded), mode, FixtureID,
    absolute address. Checked with `unzip -t` and `xmllint`; not yet opened in a real visualiser.
  - H6: SVG light plot (`light::plot`, Patch → Export plot…): top view, 1 m grid, centre line,
    symbols turned by heading (circle = moving head), number above, `universe.address` below,
    legend per type.
  - Not done: MVR import, embedding GDTF files. (PDF plot added 2026-10-15.)

- Step 7 done: manual chapter 11 "Lighting" (en/es/gl; later chapters renumbered), Light cue in
  the cue types list, 10 screenshots (`docs/manual/img/light-cue.webp`, `lighting-*.webp`,
  `fixture-types.webp`, in `manual::FILES`) taken from a demo show with sequences, presets,
  effects, outputs and an input (English UI: `OPENQ_LANG=en`).

- Plot shapes (after Phase 3): `Lighting.shapes` (`model::PlotShape`: `Bar` a→b at a trim height,
  `Rect` corners a/b with a height; x/z metres like fixture positions; carried by `LightingPatch`).
  Plan view: Draw → Bar / Rectangle (drag to draw, one-shot tool, slips under 10 cm dropped),
  select a bar or a rectangle's outline (the inside still pans), drag to move, white handles for
  ends/corners (5 cm snap), Delete key, settings row (name, from/to, height, length), "Hang n
  fixtures" (`PlotShape::hang`: evenly along the bar, half a gap in from the ends, at its
  height). SVG plot draws rectangles and bars under the fixtures. MVR export: since 2026-10-15
  (see below). Also fixed: Delete/Backspace and Enter in the
  Lighting view acted on the selected cues of the hidden cue list.

Phase 3 is complete. Left for later: L3 USITT ASCII import/export, F4 audio-reactive effects,
MVR import, Phase 4 (hardware console: K1 `Surface` trait, K3 console protocol, K4).

## After Phase 3 (2026-10-15)
- **Fade cues and lighting:** dropped (decision 4). The Fade inspector explains it for Light targets.
- **Control surfaces (K2):** `crate::surface`. `Mapping { input: SurfaceInput (MidiCc/MidiNote/Osc),
  action: SurfaceAction }` in `Settings.surfaces` (show file); actions: executor level/GO/release/
  flash (executor n = n-th sequence), grand master, blackout, release all, tap tempo, main GO/Stop.
  Values 0..1; buttons fire on the rising edge through 0.5 (notes, CC 127/0, OSC 1/0; OSC without
  arguments = press). `SurfaceHub` (shared by the OSC listener and `MidiSurface`): learn mode, last
  input, button state, and a queue a replica's UI routes to the main device (`App::sync_surfaces`).
  The MIDI port is per computer (`<config>/midi-surface`), its thread reconnects like MTC. OSC also
  knows `/light/executor/{n}/level|go|release|flash`, `/light/grandmaster|blackout|release|tap`
  without mappings. UI: Settings → Control surfaces (learn, executor bank CC 1–8 / notes 36–43).
  Not done: feedback to the controller (LEDs, motor faders), soft takeover/pickup for faders,
  pages/banks, MIDI clock in.
- **Art-Net:** optional ArtSync after each batch (`OutputProtocol::ArtNet.sync`); inputs answer
  ArtPoll with ArtPollReply (`artnet::poll_replies`: 4 ports per bind index, grouped by net/sub-net,
  sent to the poller's 6454 and its own port); discovery merges multi-bind replies per IP and skips
  OpenQ's own; Shift-click a node takes its universes; warning when a unicast output sends a
  universe the chosen node doesn't output. Still untested on real nodes.
- **PDF plot:** `plot::pdf` (one A3 landscape page, scale 1:20…1:1000, base-14 Helvetica, WinAnsi
  text, deflated stream) from the same drawing routine as the SVG (`Canvas` trait).
- **Fixture profiles:** repeated-channel attributes and the show/library difference marker (§B).

Known issues to look at:
- Untested on real hardware: Art-Net nodes, sACN receivers, Enttec USB Pro. Importers were checked
  on a few real files only (OFL: MAC Aura, Robin 600E, COLORband PiX; QLC+: Sharpy, ColorBand Pix IP;
  GDTF: four BlenderDMX profiles).

## Review (2026-10-15, after merging master 0.2.3)
Measured with `library::bench_rig` (release build): 500 fixtures × 5 sequences × 100 cues compiles in
47 ms (1.6 M entries, 18 MB); 2000 × 10 × 300 in 1.7 s (38 M entries, 455 MB, twice that at the
swap). The per-frame design clone + compare in `lighting_view` costs 0.1 / 0.6 ms: fine.

Done the same day: R1–R7, P1–P3 (P4 left). Results on the 2000 × 10 × 300 rig: a cue edit
recompiles in 170 ms (one sequence) instead of 1.7 s; the compiled show takes 303 MB instead of
455 MB (that rig changes every value in every cue, so only the per-entry timing goes; a tracking
show that changes a few fixtures per cue keeps little more than one snapshot per 32 cues); the
full compile is unchanged. How each was fixed:
- R1: Grand master, blackout, tempo and levels skip the queue (levels are re-applied when a new
  show arrives); held commands carry the compile generation announced before them.
- R2: `LightShow::same_input` in `Show::sync_lighting`; otherwise `LightEngine::load_io` only.
- R3: released LTP tracks resolve after the others and fade from their start to that value.
- R4: the programmer is resent after every edit (`edit_gen`), like programmer effects.
- R5: up to four earlier cues' effect lists fade out at once (`OLD_EFFECTS`); an effect a later
  list also has is left to that one.
- R6: `Playback.base` (sequence values before effects/programmer) is where new fades start.
- R7: `Shared.quit` ends the light thread on drop without waiting for the compile thread.
- P1: `LightShow::compile_from` (the compile thread keeps the last design + show); sequences are
  `Arc<CompiledSequence>`, kept when the patch, presets, effects and the sequence are unchanged.
- P2: `CompiledSequence` keeps a full snapshot every `SNAPSHOT` (32) cues, cues keep `changes`
  (NaN = released) and per-feature `timings`; moves in black are `CompiledCue.moves`. GO rebuilds
  the state into reused buffers (`state_into`).
- P3: a replica loads an empty design once and never compiles.

Bugs:
- **R1 GOs and masters wait for compiles that never settle.** Every design edit sends
  `LightCommand::Compiling(g)`, and the light thread holds *every* later command (GO, release,
  grand master, blackout, levels, tempo) until a show with generation ≥ the newest announced one
  arrives. A continuous edit (dragging fixtures in the Plan view, a slider in the patch sheet) bumps
  the generation every UI frame; once a compile takes longer than a frame (≥ ~500 fixtures with
  cues), the newest show is always behind, so commands sit in the queue until `HOLD_LIMIT` (5 s),
  then again for the next edit. Blackout and the grand master should never wait. Fix: masters/
  tempo/levels go straight through; a held GO records the generation announced when it arrived and
  is released once that one (not the newest) is applied, so it waits one compile at most.
- **R2 Recompiles for edits playback doesn't use.** Moving fixtures in the plan, plot shapes,
  names, groups, notes all recompile the whole design (1.7 s on the big rig) and trigger R1. Fix: a
  "playback key" compare (types, fixture id/type/mode/address/universe/invert, sequences, presets,
  effects) in `Show::sync_lighting`; other edits skip `load`.
- **R3 LTP release snaps.** Releasing a sequence that owns an LTP slot (pan, colour) fades it to its
  *home* value, then drops the track, and the slot jumps to whatever an older sequence holds there.
  Should fade to the value underneath (the next-highest stamp) or home if none.
- **R4 Programmer follows presets only when the programmer changes.** `lighting_frame` resolves
  preset references when the programmer key changes; editing a preset that the programmer uses
  leaves the stage on the old value until the next programmer edit. Fix: include the edit
  generation in the key (or resolve on every recompile, like effects).
- **R5 Quick GOs cut effect fades.** `Playback::go`/`release` overwrite `old_effects`: a second GO
  while the previous cue's effects are still fading out stops them instantly.
- **R6 Fades start from the programmer.** A GO starts an LTP fade from the slot's last resolved
  value, which includes programmer overrides and effect offsets, so after clearing the programmer
  a long fade still begins at the programmer's value. Minor (only while designing).
- **R7 Shutdown can wait for a compile.** The compile thread holds a sender to the light thread,
  so `LightEngine::drop`'s join waits until a running compile ends (1.7 s on the big rig; bounded
  by `EXIT_GRACE`).

Performance:
- **P1 Incremental compile.** Keep `Arc<CompiledSequence>` per sequence and recompile only
  sequences whose cues/presets/effects or the patch changed (cue values are already `Shared`, so
  the check is pointer compares). A cue edit on the big rig would go from 1.7 s to ~170 ms, and the
  swap no longer doubles memory.
- **P2 Smaller tracked state.** Each compiled cue stores every slot ever set in its sequence
  (slots × cues). Storing only the changes per cue plus a full snapshot every N cues (GO to a cue
  = nearest snapshot + replay) would cut the 455 MB by ~10× for typical tracking shows.
- **P3 Replicas don't need to compile.** A replica mirrors the main device's states and frame but
  still compiles the whole design (455 MB, 1.7 s per edit on the big rig) for a light thread that
  has no outputs. Compile lazily, only when the replica stops mirroring.
- **P4 Status without allocation.** The light thread builds a new `Vec<SequenceStatus>` 11 times a
  second (`*s = playback.status()`); fill the shared one in place (`status_into`) so the thread is
  allocation-free after loading, like `tick`.

## Next steps (2026-10-15, after merging master 0.2.4)
Bugs:
- P4 done: `Playback::status_into` fills the shared status in place (checked by `tests/light_alloc.rs`).
- `sync_lighting` compare: `Lighting.fixtures` and `Sequence.cues` are `Shared<Vec<…>>` (copy on
  write), so the kept copy shares them and an untouched design compares by pointer (was 0.4 ms
  compare + 0.2 ms clone on the 2000 × 10 × 300 rig). Chosen over a change counter: no edit site
  can forget to bump it. Pages that edit through `&mut` every frame (patch sheet, cue table)
  unshare their list once per frame, as the old full clone did.
- Duplicate attributes: `Attribute::Extra(n)` (§B), so no channel is dropped.
Features:
- MIDI clock in (`surface::MidiClock`): the surface MIDI input no longer ignores timing messages;
  24 PPQN measured over two beats, one tempo per beat when it moves ≥ 0.2 BPM, Start/Stop/Continue
  or a 1 s gap restart the measurement. `SurfaceSettings.midi_clock` (show file) makes
  `SurfaceHub::clock_tempo` set the effects tempo (replicas queue it); Settings → Control surfaces
  shows the measured BPM.
- Audio-reactive effects (F4): `Effect.audio: Option<AudioReact{gain, floor}>` scales the size by
  the output level. The mixer has a third meter reader (`MeterReader::Light`, `MeterTap`), handed to
  the light thread by `Media::update` whenever the output (re)opens; `Playback::set_audio` keeps an
  envelope (instant attack, `AUDIO_RELEASE` 0.25 s). Effect editor: "Size follows the sound", gain,
  floor, live meter. Not done: beat detection, a choice of input (it is everything OpenQ plays).
- MVR plot shapes: bars → `Truss`, rectangles → `SceneObject` in a "Plot" layer, each with a box mesh
  `shape-<id>.glb` (glTF, metres, y up) in the zip.
- Multi-break GDTF and gobo pictures: §B.
- Script parameters: `openq.param(name, default, min, max, integer=False)` returns the value OpenQ
  passes (`Request.params`), declared ones come back in the reply → `Effect.params`
  (`effects::ScriptParam`, `sanitize_params`); sliders under the script re-run it when let go.
  Scripts may also use `extra1…extra255` attributes.
- Effect library: `<config>/effects` (`fxfile::EffectLibrary`, on `light::folder::Library`, the generic
  background folder worker the fixture library now uses too). Effects page: Save to library, library
  list (click adds to the show), remove, open folder.
- Fixed on the way: "Open the fixture library folder" called `App::open_path` (opens a show); both
  folder buttons now use the system file manager.
