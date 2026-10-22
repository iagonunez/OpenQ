//! OpenQ — an open-source show control / cue playback system.
//!
//! A show is a [`model::Workspace`]: one or more cue lists ([`model::CueList`]), each a tree of
//! [`model::Cue`]s, plus workspace [`model::Settings`] and video [`model::Stage`]s. Pressing GO fires the cue at the
//! playhead; the [`engine::Engine`] then runs pre-waits, actions, post-waits and
//! auto-continues, and drives the media backends.
//!
//! # Architecture
//!
//! ```text
//!  UI (egui)  ─┐                         ┌─ media::audio   cpal mixer (EQ, routing, fades)
//!  OSC        ─┼─ control::Command ──▶ Show ── engine ──▶ Backend ─┼─ vout::link     ══▶ openq --video-output
//!  remote     ─┘   (Arc<Mutex<Show>>)  ▲ 1 kHz tick      └─ network       OSC out      (stage windows, wgpu,
//!                                      └─ show::spawn_engine_thread                    FFmpeg layers)
//! ```
//!
//! * [`model`] — the data: cues, settings, stages; load/save as `.openq` JSON.
//! * [`engine`] — the cue state machine. Pure logic over a [`engine::Backend`] trait, so it is
//!   unit-tested with a mock backend. It works on logical timestamps and hands starts to the
//!   backend [`engine::LOOKAHEAD`] early, so chained cues start sample-accurately.
//! * [`show`] — [`show::Show`] bundles workspace, engine and media; shared as
//!   [`show::SharedShow`] by the UI, the engine thread, OSC and remote connections. Edits say
//!   which cues they changed ([`changes`]), so replicas and the autosave keep their copies in
//!   step without cloning the whole show under the lock.
//! * [`control`] — every transport action (GO, stop, pause, playhead, single cues) as a
//!   [`control::Command`]; the one entry point used by the UI, OSC and remote devices.
//! * [`media`] — audio mixer and decoders, live inputs (Mic cues), video/image/text/camera
//!   layers, timecode in and out ([`media::tcio`]), media probing, waveform and poster caches,
//!   Network cue messages ([`media::netsend`]: host lookups never on the engine thread).
//!   Audio devices come from cpal's PipeWire host when PipeWire runs (feature `pipewire`), ALSA
//!   otherwise; the pickers list one entry per real output/input.
//! * [`timecode`] — SMPTE labels, the LTC encoder/decoder and MIDI Time Code. Cue lists can
//!   follow LTC or MTC and fire cues at their timecode trigger.
//! * [`bundle`] — copy a show and every media file it plays into one folder.
//! * [`wallclock`] — wall-clock triggers: cues that fire at a time of day (and chosen weekdays).
//! * [`geometry`] — layer placement, crop, corner-pin homography, edge-blend curve.
//! * [`animation`] — keyframe animation of video, image and text layers, evaluated live as the
//!   stage windows draw (relative to the cue's own geometry, opacity and colours).
//! * [`loudness`] — integrated loudness (LUFS) of audio files, for levelling cues.
//! * [`dsp`] — EQ filter design and processing. [`audio_fx`] — real-time audio effect chains.
//! * [`effects`] — audio and video effect settings (effect stacks on cues); video
//!   effects run in the stage windows' layer shader.
//! * [`spatial`] — object audio: speakers in a room, objects placed by distance-based panning.
//! * [`light`] — the lighting design (patch, groups, lighting cue sequences with tracking), kept
//!   apart from the live show; Light cues target its cues.
//! * [`surface`] — MIDI/OSC control surfaces mapped onto executors, playback buttons, masters and GO
//!   (learn mode, pages, soft takeover, feedback to LEDs and motor faders); the same MIDI input fires
//!   cues by their MIDI trigger (note on, program change, control change). [`msc`] — MIDI Show Control
//!   commands (GO, STOP, RESUME, LOAD… by cue number and list) read on that input.
//! * [`overrides`] — global switches cutting MIDI, OSC, timecode, wall-clock triggers or DMX output for
//!   the running show.
//! * [`multiedit`] — an inspector edit carried to every selected cue; pasting chosen properties.
//! * [`warnings`] — problems found in a show before playing it (missing files and targets, messages
//!   that can't be sent, shared hotkeys).
//! * [`osc`] — OSC input. [`remote`] — paired remote devices (encrypted, authenticated).
//! * [`displays`] — system monitors for stage outputs. [`ui`] — the egui application.
//! * `vout` (feature `video`) — video output in its own process (`openq --video-output`): stage
//!   windows with their own wgpu renderer (YUV converted in the shader, corner pins, spanned
//!   surfaces, edge blending, layer [`model::BlendMode`]s), driven by cue commands on a shared
//!   clock; a crash or hang there never stops the show (the process is restarted and its layers
//!   brought back).
//! * [`i18n`] — UI translations (English, Spanish, Galician): texts are looked up by their
//!   English wording in `assets/i18n/*.json`; the language is chosen on the first run.
//! * [`recovery`] — crash safety: panics in workers become errors, the engine thread recovers
//!   in place, autosave plus crash detection bring unsaved work back after a crash.
//! * [`errlog`] — warnings, errors and crashes are also appended to a local log file
//!   (Help → Error log).
//! * [`mod@bench`] — benchmarks for comparing builds (`tools/bench.sh`); the mixer reports its
//!   [DSP load](media::audio::AudioOutput::dsp_load).
//!
//! # Threads
//!
//! The audio callback never locks: the UI/engine talk to it through lock-free ring buffers.
//! Decoding (audio and movie soundtracks) runs on one thread per playing file; live inputs and
//! MIDI connections each run on a thread that owns them. Everything else that touches
//! the show takes the [`show::SharedShow`] mutex briefly. Pictures (movies, stills, text,
//! cameras) are decoded and drawn by the video output process, so they never wait on the UI.
//!
//! # Failure handling
//!
//! See [`recovery`]: worker panics become per-cue errors, the engine thread recovers from its own
//! panics, a failing or lost audio device is reopened, unsaved work is autosaved and offered back
//! after a crash. Every warning and error lands in the [`errlog`] file. `tests/stress.rs` (run with `tools/stress.sh`) exercises all of it.
//! Input from outside (show, media, fixture and config files, network messages, devices) is
//! treated as hostile: files are read only when they are regular files of a bounded size
//! ([`recovery::read_file`], [`media::check_local`]), every value is brought into a plausible
//! range ([`limits`]: no fixture 10¹² m away, no fade of a thousand years), and `tests/fuzz.rs`
//! (`tools/fuzz.sh`) checks every way in with damaged and malicious data.
//!
//! # Example: drive a show without a UI
//!
//! ```no_run
//! use openq::{control::{apply, Command}, media::Prober, model::Workspace, show::Show};
//! use std::time::Instant;
//!
//! let mut show = Show::new(Prober::new());
//! show.replace_workspace(Workspace::load("show.openq".as_ref())?, None);
//! apply(&mut show, Command::Go, Instant::now()).map_err(anyhow::Error::msg)?;
//! loop {
//!     show.tick(Instant::now());
//!     if !show.engine.is_active() { break; }
//!     std::thread::sleep(std::time::Duration::from_millis(1));
//! }
//! # Ok::<(), anyhow::Error>(())
//! ```

#![warn(missing_docs)]

#[macro_use]
pub mod i18n;
pub mod limits;
pub mod loudness;
pub mod animation;
pub mod audio_fx;
pub mod bench;
pub mod bundle;
pub mod changes;
pub mod control;
pub mod displays;
pub mod dsp;
pub mod effects;
pub mod engine;
pub mod errlog;
pub mod geometry;
pub mod light;
pub mod media;
pub mod model;
pub mod msc;
pub mod multiedit;
pub mod osc;
pub mod overrides;
pub mod recovery;
pub mod remote;
pub mod show;
pub mod spatial;
pub mod surface;
pub mod timecode;
pub mod ui;
#[cfg(feature = "video")]
pub mod vout;
pub mod wallclock;
pub mod warnings;
