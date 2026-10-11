//! Video playback via GStreamer `playbin`, plus still images. Decoded RGBA frames land in a
//! shared slot that the stage windows upload as textures; a video's own audio goes to the
//! default audio sink.

use super::audio::VoiceShared;
use crate::engine::FadeRequest;
use crate::model::{CueId, FadeCurve, Geometry, StageId, VideoCue, VideoFit, db_to_gain};
use anyhow::{Context, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Where a video's soundtrack goes: into the OpenQ mixer at this rate/channel count (so it gets
/// level, fades, EQ and routing like an audio cue).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AudioTap {
    pub rate: u32,
    pub channels: usize,
}

type AudioFeed = Arc<Mutex<Option<rtrb::Producer<f32>>>>;

/// Plugins whose decoders use GPU APIs. NVIDIA's NVDEC/CUDA decoders in particular can hang or
/// crash when initialised inside a process that also renders with the GPU driver, so hardware
/// decoding is off unless the user turns it on.
const HW_DECODER_PLUGINS: &[&str] = &["nvcodec", "va", "vaapi", "v4l2codecs", "qsv", "amfcodec"];

static ORIGINAL_RANKS: std::sync::OnceLock<Mutex<HashMap<String, gst::Rank>>> = std::sync::OnceLock::new();

/// Initialise GStreamer (idempotent) and apply the default decoder policy (software only).
pub fn init() -> anyhow::Result<()> {
    static INIT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    INIT.get_or_init(|| {
        gst::init().map_err(|e| e.to_string())?;
        set_hardware_decoding(false);
        Ok(())
    })
    .clone()
    .map_err(|e| anyhow!("GStreamer init failed: {e}"))
}

/// Enable or disable GPU video decoders for pipelines created from now on.
pub fn set_hardware_decoding(enabled: bool) {
    let originals = ORIGINAL_RANKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut originals = originals.lock();
    let reg = gst::Registry::get();
    for plugin in HW_DECODER_PLUGINS {
        for f in reg.features_by_plugin(plugin) {
            let Some(factory) = f.downcast_ref::<gst::ElementFactory>() else { continue };
            if !factory.klass().contains("Decoder") {
                continue;
            }
            let name = f.name().to_string();
            let original = *originals.entry(name).or_insert_with(|| f.rank());
            f.set_rank(if enabled { original } else { gst::Rank::NONE });
        }
    }
}

#[derive(Default)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA.
    pub data: Vec<u8>,
    /// Bumped on every new frame so the UI only re-uploads when needed.
    pub generation: u64,
}

pub type FrameSlot = Arc<Mutex<Frame>>;

/// What a stage window needs to draw one running video.
#[derive(Clone)]
pub struct StageLayer {
    pub id: CueId,
    pub stage: Option<StageId>,
    pub frame: FrameSlot,
    pub opacity: f32,
    pub geometry: Geometry,
    pub layer: i32,
    pub fit: VideoFit,
    pub order: u64,
}

/// Progress shaping for non-level parameters.
fn shape(curve: FadeCurve, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    match curve {
        FadeCurve::Linear | FadeCurve::Logarithmic => t,
        FadeCurve::SCurve => t * t * (3.0 - 2.0 * t),
    }
}

#[derive(Clone, Copy)]
struct Ramp<T> {
    from: T,
    to: T,
    start: Instant,
    secs: f64,
    curve: FadeCurve,
}

impl<T: Copy> Ramp<T> {
    fn progress(&self, now: Instant) -> f32 {
        if now < self.start {
            return 0.0;
        }
        if self.secs <= 0.0 { 1.0 } else { ((now - self.start).as_secs_f64() / self.secs) as f32 }
    }
}

enum Source {
    Pipeline(gst::Element),
    Still,
}

struct Player {
    source: Source,
    frame: FrameSlot,
    params: VideoCue,
    volume: f32,
    vol_ramp: Option<Ramp<f32>>,
    opacity: f32,
    op_ramp: Option<Ramp<f32>>,
    geometry: Geometry,
    geo_ramp: Option<Ramp<Geometry>>,
    plays_done: u32,
    /// Waiting for the initial seek to the start time to complete.
    seeking: bool,
    prerolled: bool,
    play_at: Option<Instant>,
    playing: bool,
    /// When a still started showing (shifted by pauses).
    started_at: Option<Instant>,
    paused_at: Option<Instant>,
    finished: bool,
    error: Option<String>,
    error_reported: bool,
    last_pos_check: Instant,
    order: u64,
    tap: Option<AudioTap>,
    audio_feed: AudioFeed,
    audio_state: Option<Arc<VoiceShared>>,
}

fn load_still(path: &Path, slot: &FrameSlot) -> anyhow::Result<()> {
    let img = image::open(path).with_context(|| format!("cannot read image {}", path.display()))?.to_rgba8();
    let mut f = slot.lock();
    f.width = img.width();
    f.height = img.height();
    f.data = img.into_raw();
    f.generation = f.generation.wrapping_add(1);
    Ok(())
}

impl Player {
    fn new(params: &VideoCue, order: u64, tap: Option<AudioTap>) -> anyhow::Result<Self> {
        let frame: FrameSlot = Arc::new(Mutex::new(Frame::default()));
        let audio_feed: AudioFeed = Arc::new(Mutex::new(None));
        let source = if super::is_image_path(&params.path) {
            load_still(&params.path, &frame)?;
            Source::Still
        } else {
            Source::Pipeline(Self::pipeline(params, &frame, tap, &audio_feed)?)
        };
        let still = matches!(source, Source::Still);
        Ok(Self {
            source,
            frame,
            params: params.clone(),
            volume: db_to_gain(params.volume_db),
            vol_ramp: None,
            opacity: params.opacity.clamp(0.0, 1.0),
            op_ramp: None,
            geometry: params.geometry,
            geo_ramp: None,
            plays_done: 0,
            seeking: false,
            prerolled: still,
            play_at: None,
            playing: false,
            started_at: None,
            paused_at: None,
            finished: false,
            error: None,
            error_reported: false,
            last_pos_check: Instant::now(),
            order,
            tap,
            audio_feed,
            audio_state: None,
        })
    }

    fn pipeline(params: &VideoCue, frame: &FrameSlot, tap: Option<AudioTap>, feed: &AudioFeed) -> anyhow::Result<gst::Element> {
        let uri = gst::glib::filename_to_uri(&params.path, None).context("bad path")?;
        let caps = gst_video::VideoCapsBuilder::new().format(gst_video::VideoFormat::Rgba).build();
        let sink = gst_app::AppSink::builder().caps(&caps).max_buffers(2).drop(true).build();
        let slot = frame.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    copy_frame(&sample, &slot);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        let pipeline = gst::ElementFactory::make("playbin")
            .property("uri", uri.as_str())
            .property("video-sink", &sink)
            .build()
            .context("GStreamer 'playbin' element missing (install gst-plugins-base)")?;
        match tap {
            Some(tap) => {
                // Soundtrack goes to the mixer; level is applied there.
                let caps = gst::Caps::builder("audio/x-raw")
                    .field("format", "F32LE")
                    .field("layout", "interleaved")
                    .field("rate", tap.rate as i32)
                    .field("channels", tap.channels as i32)
                    .build();
                let asink = gst_app::AppSink::builder().caps(&caps).build();
                let feed = feed.clone();
                asink.set_callbacks(
                    gst_app::AppSinkCallbacks::builder()
                        .new_sample(move |s| {
                            let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                            if let Some(buf) = sample.buffer()
                                && let Ok(map) = buf.map_readable()
                                && let Some(tx) = feed.lock().as_mut()
                            {
                                for b in map.chunks_exact(4) {
                                    // A full ring means the mixer isn't consuming (paused/stopped):
                                    // dropping is the right thing.
                                    if tx.push(f32::from_le_bytes([b[0], b[1], b[2], b[3]])).is_err() {
                                        break;
                                    }
                                }
                            }
                            Ok(gst::FlowSuccess::Ok)
                        })
                        .build(),
                );
                pipeline.set_property("audio-sink", &asink);
                pipeline.set_property("volume", 1.0f64);
            }
            None => pipeline.set_property("volume", db_to_gain(params.volume_db) as f64),
        }
        pipeline.set_state(gst::State::Paused).map_err(|e| anyhow!("cannot preroll video: {e}"))?;
        Ok(pipeline)
    }

    fn set_state(&self, state: gst::State) {
        if let Source::Pipeline(p) = &self.source {
            let _ = p.set_state(state);
        }
    }

    fn seek_to_start(&self) -> bool {
        let Source::Pipeline(p) = &self.source else { return false };
        let t = gst::ClockTime::from_nseconds((self.params.start_time.max(0.0) * 1e9) as u64);
        p.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, t).is_ok()
    }

    /// Rewind a stream that can't seek by resetting the pipeline (the start-time seek, if any,
    /// happens again after the new preroll).
    fn restart(&mut self) -> bool {
        let Source::Pipeline(p) = &self.source else { return false };
        log::debug!("video {}: not seekable, restarting pipeline to loop", self.params.path.display());
        if p.set_state(gst::State::Ready).is_err() {
            return false;
        }
        self.prerolled = false;
        self.seeking = false;
        p.set_state(gst::State::Playing).is_ok()
    }

    /// Reached the end of the file or the end trim point.
    fn on_end(&mut self) {
        self.plays_done += 1;
        let more = self.params.infinite_loop || self.plays_done < self.params.play_count.max(1);
        if more && (self.seek_to_start() || self.restart()) {
            return;
        }
        self.finish();
    }

    fn end_audio(&self) {
        if let Some(st) = &self.audio_state {
            st.eof.store(true, Ordering::Release);
        }
        *self.audio_feed.lock() = None;
    }

    fn finish(&mut self) {
        self.end_audio();
        self.finished = true;
        self.set_state(gst::State::Paused);
        if !self.params.hold_last_frame {
            *self.frame.lock() = Frame::default();
        }
    }

    fn handle_bus(&mut self) {
        let Source::Pipeline(p) = &self.source else { return };
        let Some(bus) = p.bus() else { return };
        while let Some(msg) = bus.pop() {
            use gst::MessageView;
            match msg.view() {
                MessageView::AsyncDone(_) => {
                    if self.seeking {
                        self.seeking = false;
                        self.prerolled = true;
                    } else if !self.prerolled {
                        if self.params.start_time > 0.0 && self.seek_to_start() {
                            self.seeking = true;
                        } else {
                            self.prerolled = true;
                        }
                    }
                }
                MessageView::Eos(_) => {
                    if !self.finished {
                        self.on_end();
                    }
                }
                MessageView::Error(e) => {
                    let msg = format!("{} ({:?})", e.error(), e.debug());
                    log::error!("video {}: {msg}", self.params.path.display());
                    self.error = Some(msg);
                    self.finished = true;
                    self.end_audio();
                    *self.frame.lock() = Frame::default();
                }
                _ => {}
            }
        }
    }

    fn position(&self) -> Option<f64> {
        match &self.source {
            Source::Pipeline(p) => p.query_position::<gst::ClockTime>().map(|t| t.nseconds() as f64 / 1e9),
            Source::Still => self.started_at.map(|s| (self.paused_at.unwrap_or_else(Instant::now) - s).as_secs_f64()),
        }
    }

    fn update(&mut self, now: Instant) {
        self.handle_bus();
        if self.paused_at.is_some() || self.finished {
            return;
        }
        if !self.playing
            && self.prerolled
            && let Some(at) = self.play_at
            && now >= at
        {
            self.set_state(gst::State::Playing);
            self.playing = true;
            self.started_at = Some(at);
        }
        if self.playing {
            match &self.source {
                Source::Still => {
                    let d = self.params.still_duration;
                    if d > 0.0 && self.started_at.is_some_and(|s| now.saturating_duration_since(s).as_secs_f64() >= d) {
                        self.finish();
                    }
                }
                Source::Pipeline(_) => {
                    // End trim point: poll position every 10 ms.
                    if let Some(end) = self.params.end_time
                        && now.duration_since(self.last_pos_check) >= Duration::from_millis(10)
                    {
                        self.last_pos_check = now;
                        if self.position().is_some_and(|p| p >= end) {
                            self.on_end();
                        }
                    }
                }
            }
        }
        if let Some(r) = self.vol_ramp {
            let t = r.progress(now);
            self.volume = r.curve.interpolate(r.from, r.to, t);
            if let Source::Pipeline(p) = &self.source {
                p.set_property("volume", self.volume.clamp(0.0, 10.0) as f64);
            }
            if t >= 1.0 {
                self.vol_ramp = None;
            }
        }
        if let Some(r) = self.op_ramp {
            let t = r.progress(now);
            self.opacity = (r.from + (r.to - r.from) * shape(r.curve, t)).clamp(0.0, 1.0);
            if t >= 1.0 {
                self.op_ramp = None;
            }
        }
        if let Some(r) = self.geo_ramp {
            let t = r.progress(now);
            self.geometry = r.from.lerp(&r.to, shape(r.curve, t));
            if t >= 1.0 {
                self.geo_ramp = None;
            }
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.end_audio();
        self.set_state(gst::State::Null);
    }
}

fn copy_frame(sample: &gst::Sample, slot: &FrameSlot) {
    let (Some(buffer), Some(caps)) = (sample.buffer(), sample.caps()) else { return };
    let Ok(info) = gst_video::VideoInfo::from_caps(caps) else { return };
    let Ok(map) = buffer.map_readable() else { return };
    let (w, h) = (info.width(), info.height());
    let stride = info.stride()[0] as usize;
    let row = w as usize * 4;
    let mut f = slot.lock();
    f.data.resize(row * h as usize, 0);
    for y in 0..h as usize {
        let src = &map[y * stride..y * stride + row];
        f.data[y * row..(y + 1) * row].copy_from_slice(src);
    }
    f.width = w;
    f.height = h;
    f.generation = f.generation.wrapping_add(1);
}

pub struct VideoOutput {
    players: HashMap<CueId, Player>,
    preloaded: HashMap<CueId, Player>,
    order: u64,
}

impl VideoOutput {
    pub fn new() -> anyhow::Result<Self> {
        init()?;
        Ok(Self { players: HashMap::new(), preloaded: HashMap::new(), order: 0 })
    }

    pub fn set_preloaded(&mut self, cues: &[(CueId, &VideoCue, Option<AudioTap>)]) {
        self.preloaded.retain(|id, p| cues.iter().any(|(c, v, t)| c == id && **v == p.params && *t == p.tap));
        for (id, v, tap) in cues {
            if !self.preloaded.contains_key(id) && v.path.exists() {
                self.order += 1;
                match Player::new(v, self.order, *tap) {
                    Ok(p) => {
                        self.preloaded.insert(*id, p);
                    }
                    Err(e) => log::warn!("preload video failed: {e:#}"),
                }
            }
        }
    }

    pub fn start(&mut self, cue: CueId, params: &VideoCue, at: Instant, tap: Option<AudioTap>) -> Result<(), String> {
        if params.path.as_os_str().is_empty() {
            return Err("no video file".into());
        }
        if !params.path.exists() {
            return Err(format!("file not found: {}", params.path.display()));
        }
        self.players.remove(&cue);
        self.order += 1;
        let mut p = match self.preloaded.remove(&cue) {
            Some(p) if p.params == *params && p.tap == tap && !p.finished => p,
            _ => Player::new(params, self.order, tap).map_err(|e| format!("{e:#}"))?,
        };
        p.order = self.order;
        p.play_at = Some(at);
        p.update(Instant::now());
        self.players.insert(cue, p);
        Ok(())
    }

    pub fn stop(&mut self, cue: CueId) {
        self.players.remove(&cue);
    }

    /// Connect a started cue's soundtrack to its mixer voice.
    pub fn attach_audio(&mut self, cue: CueId, tx: rtrb::Producer<f32>, state: Arc<VoiceShared>) {
        if let Some(p) = self.players.get_mut(&cue) {
            *p.audio_feed.lock() = Some(tx);
            if p.finished {
                state.eof.store(true, Ordering::Release);
            }
            p.audio_state = Some(state);
        }
    }

    /// Freeze fades in progress on this cue at their current values.
    pub fn hold(&mut self, cue: CueId) {
        if let Some(p) = self.players.get_mut(&cue) {
            p.update(Instant::now());
            p.vol_ramp = None;
            p.op_ramp = None;
            p.geo_ramp = None;
        }
    }

    pub fn pause(&mut self, cue: CueId) {
        if let Some(p) = self.players.get_mut(&cue)
            && p.paused_at.is_none()
        {
            p.paused_at = Some(Instant::now());
            p.set_state(gst::State::Paused);
        }
    }

    pub fn resume(&mut self, cue: CueId, at: Instant) {
        if let Some(p) = self.players.get_mut(&cue)
            && let Some(paused) = p.paused_at.take()
        {
            let shift = at.saturating_duration_since(paused);
            if let Some(r) = &mut p.vol_ramp {
                r.start += shift;
            }
            if let Some(r) = &mut p.op_ramp {
                r.start += shift;
            }
            if let Some(r) = &mut p.geo_ramp {
                r.start += shift;
            }
            for t in [&mut p.play_at, &mut p.started_at].into_iter().flatten() {
                *t += shift;
            }
            if p.playing {
                p.set_state(gst::State::Playing);
            }
        }
    }

    pub fn fade(&mut self, cue: CueId, f: &FadeRequest, at: Instant) {
        let Some(p) = self.players.get_mut(&cue) else { return };
        let (secs, curve) = (f.duration, f.curve);
        if let Some(db) = f.volume_db
            && p.tap.is_none()
        {
            p.vol_ramp = Some(Ramp { from: p.volume, to: db_to_gain(db), start: at, secs, curve });
        }
        if let Some(o) = f.opacity {
            p.op_ramp = Some(Ramp { from: p.opacity, to: o.clamp(0.0, 1.0), start: at, secs, curve });
        }
        if let Some(g) = f.geometry {
            p.geo_ramp = Some(Ramp { from: p.geometry, to: g, start: at, secs, curve });
        }
    }

    /// Apply inspector edits to a playing cue (level, opacity, geometry, layer, fit, stage).
    pub fn update_live(&mut self, cue: CueId, params: &VideoCue) {
        let Some(p) = self.players.get_mut(&cue) else { return };
        let old = std::mem::replace(&mut p.params, params.clone());
        if old.volume_db != params.volume_db && p.tap.is_none() {
            p.vol_ramp = None;
            p.volume = db_to_gain(params.volume_db);
            if let Source::Pipeline(pl) = &p.source {
                pl.set_property("volume", p.volume.clamp(0.0, 10.0) as f64);
            }
        }
        if old.opacity != params.opacity {
            p.op_ramp = None;
            p.opacity = params.opacity.clamp(0.0, 1.0);
        }
        if old.geometry != params.geometry {
            p.geo_ramp = None;
            p.geometry = params.geometry;
        }
    }

    pub fn has(&self, cue: CueId) -> bool {
        self.players.contains_key(&cue)
    }

    pub fn is_finished(&self, cue: CueId) -> bool {
        self.players
            .get(&cue)
            .is_none_or(|p| p.finished && (p.error.is_some() || p.error_reported || !p.params.hold_last_frame))
    }

    pub fn elapsed(&self, cue: CueId) -> Option<f64> {
        self.players.get(&cue)?.position()
    }

    pub fn update(&mut self, now: Instant) {
        for p in self.players.values_mut() {
            p.update(now);
        }
        for p in self.preloaded.values_mut() {
            p.handle_bus();
        }
    }

    pub fn drain_errors(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        for p in self.players.values_mut() {
            if let Some(e) = p.error.take() {
                out.push(format!("{}: {e}", p.params.path.display()));
                p.error_reported = true;
            }
        }
        out
    }

    pub fn stage_layers(&self) -> Vec<StageLayer> {
        let mut v: Vec<StageLayer> = self
            .players
            .iter()
            .filter(|(_, p)| p.playing || p.finished)
            .map(|(id, p)| StageLayer {
                id: *id,
                stage: p.params.stage,
                frame: p.frame.clone(),
                opacity: p.opacity,
                geometry: p.geometry,
                layer: p.params.layer,
                fit: p.params.fit,
                order: p.order,
            })
            .collect();
        v.sort_by_key(|l| (l.layer, l.order));
        v
    }
}

#[derive(Clone, Debug, Default)]
pub struct VideoInfo {
    pub duration: Option<f64>,
    pub width: u32,
    pub height: u32,
    pub has_audio: bool,
    pub audio_channels: usize,
}

pub fn probe(path: &Path) -> anyhow::Result<VideoInfo> {
    use gstreamer_pbutils as pbutils;
    init()?;
    let uri = gst::glib::filename_to_uri(path, None)?;
    let disc = pbutils::Discoverer::new(gst::ClockTime::from_seconds(10))?;
    let info = disc.discover_uri(&uri)?;
    let (width, height) = info.video_streams().first().map(|v| (v.width(), v.height())).unwrap_or((0, 0));
    let audio = info.audio_streams();
    Ok(VideoInfo {
        duration: info.duration().map(|d| d.nseconds() as f64 / 1e9),
        width,
        height,
        has_audio: !audio.is_empty(),
        audio_channels: audio.first().map(|a| a.channels() as usize).unwrap_or(0),
    })
}
