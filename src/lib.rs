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
//!  OSC        ─┼─ control::Command ──▶ Show ── engine ──▶ Backend ─┼─ media::video   FFmpeg layers
//!  remote     ─┘   (Arc<Mutex<Show>>)  ▲ 1 kHz tick      └─ network       OSC out
//!                                      └─ show::spawn_engine_thread
//! ```
//!
//! * [`model`] — the data: cues, settings, stages; load/save as `.openq` JSON.
//! * [`engine`] — the cue state machine. Pure logic over a [`engine::Backend`] trait, so it is
//!   unit-tested with a mock backend. It works on logical timestamps and hands starts to the
//!   backend [`engine::LOOKAHEAD`] early, so chained cues start sample-accurately.
//! * [`show`] — [`show::Show`] bundles workspace, engine and media; shared as
//!   [`show::SharedShow`] by the UI, the engine thread, OSC and remote connections.
//! * [`control`] — every transport action (GO, stop, pause, playhead, single cues) as a
//!   [`control::Command`]; the one entry point used by the UI, OSC and remote devices.
//! * [`media`] — audio mixer and decoders, live inputs (Mic cues), video/image/text/camera
//!   layers, timecode in and out ([`media::tcio`]), media probing, waveform and poster caches.
//!   Audio devices come from cpal's PipeWire host when PipeWire runs (feature `pipewire`), ALSA
//!   otherwise; the pickers list one entry per real output/input.
//! * [`timecode`] — SMPTE labels, the LTC encoder/decoder and MIDI Time Code. Cue lists can
//!   follow LTC or MTC and fire cues at their timecode trigger.
//! * [`geometry`] — layer placement, crop, corner-pin homography, edge-blend curve.
//! * [`animation`] — keyframe animation of video, image and text layers, evaluated live as the
//!   stage windows draw (relative to the cue's own geometry, opacity and colours).
//! * [`dsp`] — EQ filter design and processing.
//! * [`osc`] — OSC input. [`remote`] — paired remote devices (encrypted, authenticated).
//! * [`displays`] — system monitors for stage outputs. [`ui`] — the egui application, including
//!   the stage windows (spanned surfaces, edge blending, layer [`model::BlendMode`]s).
//! * [`recovery`] — crash safety: panics in workers become errors, the engine thread recovers
//!   in place, autosave plus crash detection bring unsaved work back after a crash.
//! * [`mod@bench`] — benchmarks for comparing builds (`tools/bench.sh`); the mixer reports its
//!   [DSP load](media::audio::AudioOutput::dsp_load).
//!
//! # Threads
//!
//! The audio callback never locks: the UI/engine talk to it through lock-free ring buffers.
//! Decoding (audio and video) runs on one thread per playing file; live inputs, cameras and MIDI
//! connections each run on a thread that owns them. Everything else that touches
//! the show takes the [`show::SharedShow`] mutex briefly.
//!
//! # Failure handling
//!
//! See [`recovery`]: worker panics become per-cue errors, the engine thread recovers from its own
//! panics, a failing or lost audio device is reopened, unsaved work is autosaved and offered back
//! after a crash. `tests/stress.rs` (run with `tools/stress.sh`) exercises all of it.
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

pub mod animation;
pub mod bench;
pub mod control;
pub mod displays;
pub mod dsp;
pub mod engine;
pub mod geometry;
pub mod media;
pub mod model;
pub mod osc;
pub mod recovery;
pub mod remote;
pub mod show;
pub mod timecode;
pub mod ui;
