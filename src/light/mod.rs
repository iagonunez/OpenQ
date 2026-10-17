//! Lighting: a lighting design (patch, groups, sequences of lighting cues with tracking) kept
//! apart from the live show, and DMX playback of it.
//!
//! * [`model`] — the design data stored in the show file as `Workspace.lighting`.
//! * [`library`] — built-in generic fixture types.
//! * [`fixlib`] — fixture type files (`.oqfixture`) and the user's fixture library folder.
//! * [`import`] — fixture import: [`ofl`] (Open Fixture Library), [`gdtf`] (GDTF, read with
//!   [`zipread`]), [`qxf`] (QLC+); [`oflsite`] searches and downloads Open Fixture Library
//!   fixtures from its website.
//! * [`command`] — the desk-style command line parser.
//! * [`effects`] — effect waveforms run across fixtures (pool, runs in cues, evaluation).
//! * [`mvr`] — MVR export of the patch with positions (for visualisers).
//! * [`plot`] — light plot (top view) as SVG or PDF.
//! * [`place`] — fixture placement layouts (line, arc, grid) for the patch's Arrange tool.
//! * [`patch`] — design deltas ([`patch::LightingPatch`]) sent between remote devices.
//! * [`playback`] — the compiled show and the playback state machine (fades, HTP/LTP, masters).
//! * [`engine`] — the light thread ([`engine::LightEngine`]): playback at 44 Hz into the outputs.
//! * [`script`] — Python effect scripts in a WebAssembly sandbox (the `openq` module).
//! * [`fxfile`] — effect files (`.oqfx`) for sharing effects.
//! * [`dmx`] — DMX frames and output threads; protocols in [`artnet`], [`sacn`], [`enttec`].
//! * [`dmxin`] — Art-Net/sACN input: merged into the output or captured into the programmer.
//!
//! OpenQ Light cues in the main cue list target a designed lighting cue and GO it; the design
//! itself can only be edited in Edit mode.

pub mod artnet;
pub mod command;
pub mod dmx;
pub mod dmxin;
pub mod effects;
pub mod engine;
pub mod enttec;
pub mod fixlib;
pub mod colour;
pub mod folder;
pub mod gdtf;
pub mod gobo;
pub mod import;
pub mod library;
pub mod model;
pub mod mvr;
pub mod ofl;
pub mod oflsite;
pub mod qxf;
pub mod patch;
pub mod place;
pub mod plot;
pub mod playback;
pub mod sacn;
pub mod fxfile;
pub mod script;
pub mod zipread;

pub use model::{Attribute, Feature, Fixture, FixtureType, Lighting, Look, LightCue, Sequence};
