//! Lighting: a lighting design (patch, groups, sequences of lighting cues with tracking) kept
//! apart from the live show, and DMX playback of it.
//!
//! * [`model`] — the design data stored in the show file as `Workspace.lighting`.
//! * [`library`] — built-in generic fixture types.
//!
//! OpenQ Light cues in the main cue list target a designed lighting cue and GO it; the design
//! itself can only be edited in Edit mode. See `docs/lighting.md` for the plan.

pub mod library;
pub mod model;

pub use model::{Attribute, Feature, Fixture, FixtureType, Lighting, Look, LightCue, Sequence};
