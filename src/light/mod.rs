//! Lighting: a lighting design (patch, groups, sequences of lighting cues with tracking) kept
//! apart from the live show, and DMX playback of it.
//!
//! * [`model`] — the design data stored in the show file as `Workspace.lighting`.
//! * [`library`] — built-in generic fixture types.
//! * [`command`] — the desk-style command line parser.
//! * [`effects`] — effect waveforms run across fixtures (pool, runs in cues, evaluation).
//! * [`playback`] — the compiled show and the playback state machine (fades, HTP/LTP, masters).
//! * [`engine`] — the light thread ([`engine::LightEngine`]): playback at 44 Hz into the outputs.
//! * [`visual`] — fixtures as the 3D visualiser shows them (from a DMX frame) and aiming maths.
//! * [`dmx`] — DMX frames and output threads; protocols in [`artnet`], [`sacn`], [`enttec`].
//!
//! OpenQ Light cues in the main cue list target a designed lighting cue and GO it; the design
//! itself can only be edited in Edit mode. See `docs/lighting.md` for the plan.

pub mod artnet;
pub mod command;
pub mod dmx;
pub mod effects;
pub mod engine;
pub mod enttec;
pub mod library;
pub mod model;
pub mod playback;
pub mod sacn;
pub mod visual;

pub use model::{Attribute, Feature, Fixture, FixtureType, Lighting, Look, LightCue, Sequence};
