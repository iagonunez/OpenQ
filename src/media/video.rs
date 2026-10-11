//! Video playback via GStreamer `playbin`. Decoded RGBA frames land in a shared slot that the
//! stage window uploads as a texture; the video's own audio goes to the default audio sink.

use crate::model::{CueId, FadeCurve, VideoCue, VideoFit, db_to_gain};
use anyhow::{Context, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

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

/// What the stage window needs to draw one running video.
#[derive(Clone)]
pub struct StageLayer {
    pub id: CueId,
    pub frame: FrameSlot,
    pub opacity: f32,
    pub layer: i32,
    pub fit: VideoFit,
    pub order: u64,
}

#[derive(Clone, Copy)]
struct Ramp {
    from: f32,
    to: f32,
    start: Instant,
    secs: f64,
    curve: FadeCurve,
}

impl Ramp {
    fn value(&self, now: Instant) -> (f32, bool) {
        if now < self.start {
            return (self.from, false);
        }
        let t = if self.secs <= 0.0 { 1.0 } else { ((now - self.start).as_secs_f64() / self.secs) as f32 };
        (self.curve.interpolate(self.from, self.to, t), t >= 1.0)
    }
}

struct Player {
    pipeline: gst::Element,
    frame: FrameSlot,
    params: VideoCue,
    volume: f32,
    vol_ramp: Option<Ramp>,
    opacity: f32,
    op_ramp: Option<Ramp>,
    plays_done: u32,
    /// Waiting for the initial seek to the start time to complete.
    seeking: bool,
    prerolled: bool,
    play_at: Option<Instant>,
    playing: bool,
    paused_at: Option<Instant>,
    finished: bool,
    error: Option<String>,
    error_reported: bool,
    order: u64,
}

impl Player {
    fn new(params: &VideoCue, order: u64) -> anyhow::Result<Self> {
        let uri = gst::glib::filename_to_uri(&params.path, None).context("bad path")?;
        let frame: FrameSlot = Arc::new(Mutex::new(Frame::default()));
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
        pipeline.set_property("volume", db_to_gain(params.volume_db) as f64);
        pipeline.set_state(gst::State::Paused).map_err(|e| anyhow!("cannot preroll video: {e}"))?;
        Ok(Self {
            pipeline,
            frame,
            params: params.clone(),
            volume: db_to_gain(params.volume_db),
            vol_ramp: None,
            opacity: params.opacity.clamp(0.0, 1.0),
            op_ramp: None,
            plays_done: 0,
            seeking: false,
            prerolled: false,
            play_at: None,
            playing: false,
            paused_at: None,
            finished: false,
            error: None,
            error_reported: false,
            order,
        })
    }

    fn seek_to_start(&self) -> bool {
        let t = gst::ClockTime::from_nseconds((self.params.start_time.max(0.0) * 1e9) as u64);
        self.pipeline
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, t)
            .is_ok()
    }

    fn handle_bus(&mut self) {
        let Some(bus) = self.pipeline.bus() else { return };
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
                    self.plays_done += 1;
                    let more = self.params.infinite_loop || self.plays_done < self.params.play_count.max(1);
                    if more && self.seek_to_start() {
                        continue;
                    }
                    self.finished = true;
                    let _ = self.pipeline.set_state(gst::State::Paused);
                    if !self.params.hold_last_frame {
                        *self.frame.lock() = Frame::default();
                    }
                }
                MessageView::Error(e) => {
                    let msg = format!("{} ({:?})", e.error(), e.debug());
                    log::error!("video {}: {msg}", self.params.path.display());
                    self.error = Some(msg);
                    self.finished = true;
                    *self.frame.lock() = Frame::default();
                }
                _ => {}
            }
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
            let _ = self.pipeline.set_state(gst::State::Playing);
            self.playing = true;
        }
        if let Some(r) = self.vol_ramp {
            let (v, done) = r.value(now);
            self.volume = v;
            self.pipeline.set_property("volume", v.clamp(0.0, 10.0) as f64);
            if done {
                self.vol_ramp = None;
            }
        }
        if let Some(r) = self.op_ramp {
            let (v, done) = r.value(now);
            self.opacity = v.clamp(0.0, 1.0);
            if done {
                self.op_ramp = None;
            }
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
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
        gst::init().context("GStreamer init failed")?;
        Ok(Self { players: HashMap::new(), preloaded: HashMap::new(), order: 0 })
    }

    pub fn set_preloaded(&mut self, cues: &[(CueId, &VideoCue)]) {
        self.preloaded.retain(|id, p| cues.iter().any(|(c, v)| c == id && **v == p.params));
        for (id, v) in cues {
            if !self.preloaded.contains_key(id) && v.path.exists() {
                self.order += 1;
                match Player::new(v, self.order) {
                    Ok(p) => {
                        self.preloaded.insert(*id, p);
                    }
                    Err(e) => log::warn!("preload video failed: {e:#}"),
                }
            }
        }
    }

    pub fn start(&mut self, cue: CueId, params: &VideoCue, at: Instant) -> Result<(), String> {
        if params.path.as_os_str().is_empty() {
            return Err("no video file".into());
        }
        if !params.path.exists() {
            return Err(format!("file not found: {}", params.path.display()));
        }
        self.players.remove(&cue);
        self.order += 1;
        let mut p = match self.preloaded.remove(&cue) {
            Some(p) if p.params == *params && !p.finished => p,
            _ => Player::new(params, self.order).map_err(|e| format!("{e:#}"))?,
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

    pub fn pause(&mut self, cue: CueId) {
        if let Some(p) = self.players.get_mut(&cue)
            && p.paused_at.is_none()
        {
            p.paused_at = Some(Instant::now());
            let _ = p.pipeline.set_state(gst::State::Paused);
        }
    }

    pub fn resume(&mut self, cue: CueId, at: Instant) {
        if let Some(p) = self.players.get_mut(&cue)
            && let Some(paused) = p.paused_at.take()
        {
            let shift = at.saturating_duration_since(paused);
            for r in [&mut p.vol_ramp, &mut p.op_ramp].into_iter().flatten() {
                r.start += shift;
            }
            if let Some(t) = &mut p.play_at {
                *t += shift;
            }
            if p.playing {
                let _ = p.pipeline.set_state(gst::State::Playing);
            }
        }
    }

    pub fn fade(&mut self, cue: CueId, volume_db: Option<f32>, opacity: Option<f32>, secs: f64, curve: FadeCurve, at: Instant) {
        let Some(p) = self.players.get_mut(&cue) else { return };
        if let Some(db) = volume_db {
            p.vol_ramp = Some(Ramp { from: p.volume, to: db_to_gain(db), start: at, secs, curve });
        }
        if let Some(o) = opacity {
            p.op_ramp = Some(Ramp { from: p.opacity, to: o.clamp(0.0, 1.0), start: at, secs, curve });
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
        let p = self.players.get(&cue)?;
        p.pipeline.query_position::<gst::ClockTime>().map(|t| t.nseconds() as f64 / 1e9)
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
                frame: p.frame.clone(),
                opacity: p.opacity,
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
}

pub fn probe(path: &Path) -> anyhow::Result<VideoInfo> {
    use gstreamer_pbutils as pbutils;
    gst::init()?;
    let uri = gst::glib::filename_to_uri(path, None)?;
    let disc = pbutils::Discoverer::new(gst::ClockTime::from_seconds(10))?;
    let info = disc.discover_uri(&uri)?;
    let (width, height) = info
        .video_streams()
        .first()
        .map(|v| (v.width(), v.height()))
        .unwrap_or((0, 0));
    Ok(VideoInfo {
        duration: info.duration().map(|d| d.nseconds() as f64 / 1e9),
        width,
        height,
        has_audio: !info.audio_streams().is_empty(),
    })
}
