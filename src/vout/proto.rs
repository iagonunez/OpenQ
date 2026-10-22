//! Messages between OpenQ and its video output process: one JSON object per line, OpenQ → video
//! process on its stdin ([`ToVideo`]), the other way on its stdout ([`FromVideo`]).
//!
//! Times are [`Ns`]: nanoseconds on OpenQ's clock since the link started. The video process
//! measures the offset to its own clock at the start ([`ToVideo::Ping`] / [`FromVideo::Pong`],
//! then [`ToVideo::Clock`]), so a cue told to start at a time starts then, in both processes.

use crate::effects::VideoEffect;
use crate::model::{CameraCue, CueId, FadeCurve, Geometry, Stage, StageId, TextCue, VideoCue};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::PathBuf;

/// Nanoseconds since the link's epoch, on OpenQ's monotonic clock.
pub type Ns = i64;

/// What a layer shows.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Layer {
    /// A movie or still image.
    Movie(VideoCue),
    /// A Text cue.
    Text(TextCue),
    /// A live camera.
    Camera(CameraCue),
}

/// The picture side of a Fade cue (the level is faded in OpenQ's mixer).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct VideoFade {
    /// Target opacity.
    pub opacity: Option<f32>,
    /// Target placement.
    pub geometry: Option<Geometry>,
    /// Target video effects.
    pub video_fx: Option<Vec<VideoEffect>>,
    /// Fade length in seconds.
    pub duration: f64,
    /// Shape of the fade.
    pub curve: FadeCurve,
}

/// Which stage windows should be open, and how.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Windows {
    /// Stages whose window is open.
    pub open: Vec<StageId>,
    /// Stages in mapping mode (test pattern and corner pins).
    pub mapping: Vec<StageId>,
    /// Stages without a display that are fullscreen on the screen they are on.
    pub fullscreen: Vec<StageId>,
}

/// OpenQ → video process.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ToVideo {
    /// Clock check: answer at once with [`FromVideo::Pong`].
    Ping(u32),
    /// The video process's clock minus OpenQ's, in nanoseconds.
    Clock(i64),
    /// The workspace's stages.
    Stages(Vec<Stage>),
    /// Stage windows to show.
    Windows(Windows),
    /// Open these movies and render these texts ahead of GO.
    Preload {
        /// Movies and stills.
        videos: Vec<(CueId, VideoCue)>,
        /// Text cues.
        texts: Vec<(CueId, TextCue)>,
    },
    /// Show a layer from `at` on.
    Start {
        /// The cue.
        cue: CueId,
        /// What it shows.
        layer: Layer,
        /// When it appears.
        at: Ns,
        /// Seconds of its animation already played (a layer brought back after a restart).
        anim_offset: f64,
        /// Movies: seconds of playback already done, loop passes included (a layer brought back
        /// after a restart).
        #[serde(default)]
        skip: f64,
    },
    /// Edits to a showing layer.
    Update {
        /// The cue.
        cue: CueId,
        /// Its new settings.
        layer: Layer,
    },
    /// Remove a layer.
    Stop(CueId),
    /// Remove every layer.
    StopAll,
    /// Freeze a layer.
    Pause {
        /// The cue.
        cue: CueId,
        /// When it froze.
        at: Ns,
    },
    /// Continue a frozen layer.
    Resume {
        /// The cue.
        cue: CueId,
        /// When it continues.
        at: Ns,
    },
    /// Freeze fades in progress at their current values.
    Hold(CueId),
    /// Freeze fades in progress at their values as of `at` (a hold replayed after a restart).
    HoldAt {
        /// The cue.
        cue: CueId,
        /// When it was held.
        at: Ns,
    },
    /// A Fade cue on a layer.
    Fade {
        /// The cue.
        cue: CueId,
        /// What to fade.
        fade: VideoFade,
        /// When the fade starts.
        at: Ns,
    },
    /// Decode movies started from now on with the graphics card's video engine where it takes
    /// them ([`crate::media::video::set_hardware_decoding`]).
    HardwareDecoding(bool),
    /// Send small copies of this layer's picture a few times a second ([`FromVideo::Thumb`]), or
    /// none (the geometry preview's live picture).
    Watch(Option<CueId>),
    /// Save a picture of a stage window's next frame as a PNG (screenshots, tests).
    Snapshot {
        /// The stage.
        stage: StageId,
        /// Where to save it.
        path: PathBuf,
    },
    /// Close every window and exit.
    Quit,
    /// Crash on purpose (tests of the restart).
    #[doc(hidden)]
    Crash,
}

/// Video process → OpenQ.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum FromVideo {
    /// Answer to [`ToVideo::Ping`]: the video process's clock, in nanoseconds since its start.
    Pong(u32, i64),
    /// The process is alive and drawing (sent a few times a second).
    Heartbeat,
    /// The process can't show video (no display, no GPU) and is exiting.
    Fatal(String),
    /// A layer ended on its own (or failed) and is gone from the stage.
    Finished(CueId),
    /// A showing layer's progress: media position in seconds (movies), and how many pictures it
    /// has shown so far.
    Progress(CueId, Option<f64>, u64),
    /// Something went wrong with a layer, for the engine log.
    Error(String),
    /// The user moved a stage's corner pins in its mapping window.
    Corners(StageId, [[f32; 2]; 4]),
    /// A stage window opened by itself (a layer appeared on it).
    Opened(StageId),
    /// The user closed a stage window.
    Closed(StageId),
    /// A stage window that closes when nothing plays closed (it opens again with the next layer;
    /// fullscreen and mapping choices stay).
    IdleClosed(StageId),
    /// The user turned a stage window's fullscreen on or off.
    Fullscreen(StageId, bool),
    /// A snapshot was saved (or failed).
    Snapshot(Result<PathBuf, String>),
    /// A small copy of the watched layer's picture ([`ToVideo::Watch`]).
    Thumb {
        /// The layer.
        cue: CueId,
        /// Width and height in pixels.
        size: [u32; 2],
        /// The picture's size on stage.
        stage_size: [f32; 2],
        /// Opaque RGBA, base64.
        rgba: String,
    },
}

/// Largest side of a [`FromVideo::Thumb`].
pub const THUMB_SIDE: u32 = 320;

/// The longest line either side accepts (a show with many stages or a long text stays far below).
pub const MAX_LINE: usize = 8 << 20;

/// Write one message as a line.
pub fn write<T: Serialize>(out: &mut impl Write, msg: &T) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
    line.push(b'\n');
    out.write_all(&line)?;
    out.flush()
}

/// Read the next message. `Ok(None)` at the end of the stream; lines that aren't a message (or
/// are too long) are skipped.
pub fn read<T: for<'de> Deserialize<'de>>(input: &mut impl BufRead, buf: &mut Vec<u8>) -> std::io::Result<Option<T>> {
    loop {
        buf.clear();
        let n = std::io::Read::take(&mut *input, MAX_LINE as u64 + 1).read_until(b'\n', buf)?;
        if n == 0 {
            return Ok(None);
        }
        // (A last line without a newline is the end of the stream: read as it is.)
        if buf.last() != Some(&b'\n') && n > MAX_LINE {
            // Skip the rest of an oversized line.
            let mut sink = Vec::new();
            input.read_until(b'\n', &mut sink)?;
            log::warn!("video link: dropped a {}+ byte line", MAX_LINE);
            continue;
        }
        match serde_json::from_slice(buf) {
            Ok(m) => return Ok(Some(m)),
            Err(e) => log::debug!("video link: unreadable line ({e}): {}", String::from_utf8_lossy(&buf[..buf.len().min(200)])),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_as_lines() {
        let cue = CueId::new_v4();
        let msgs = vec![
            ToVideo::Ping(3),
            ToVideo::Start { cue, layer: Layer::Text(TextCue { text: "Hi\nthere".into(), ..Default::default() }), at: -5, anim_offset: 0.5, skip: 0.0 },
            ToVideo::Fade {
                cue,
                fade: VideoFade { opacity: Some(0.5), geometry: None, video_fx: None, duration: 2.0, curve: FadeCurve::default() },
                at: 1_000_000_000,
            },
            ToVideo::Quit,
        ];
        let mut bytes = Vec::new();
        for m in &msgs {
            write(&mut bytes, m).unwrap();
        }
        // Junk between messages is skipped.
        bytes.extend_from_slice(b"not json\n");
        write(&mut bytes, &ToVideo::StopAll).unwrap();
        let mut r = std::io::Cursor::new(bytes);
        let mut buf = Vec::new();
        let mut back = Vec::new();
        while let Some(m) = read::<ToVideo>(&mut r, &mut buf).unwrap() {
            back.push(m);
        }
        assert_eq!(back.len(), msgs.len() + 1);
        assert_eq!(&back[..msgs.len()], &msgs[..]);
    }
}
