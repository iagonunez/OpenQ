//! Video output in its own process.
//!
//! Stage windows, movie decoding and layer compositing run in a child process
//! (`opnq --video-output`, [`child`]) with its own wgpu renderer ([`render`]): decoded YUV
//! pictures go to the GPU as they are and are converted in the shader. OpnQ sends it cue
//! commands with start times on a shared clock ([`proto`]) through [`link::VideoLink`], which
//! also plays movie soundtracks in OpnQ's mixer. The video never waits on OpnQ's UI, and a
//! crash or hang in video decoding, a GPU driver or FFmpeg doesn't stop the show: the link starts
//! a new process and brings back the layers that were showing. When nothing plays, an output
//! shows black (never a hint or a desktop).

pub mod child;
pub mod link;
mod pattern;
pub mod proto;
pub mod render;

/// Command-line flag that makes the binary run as the video output process.
pub const FLAG: &str = "--video-output";
