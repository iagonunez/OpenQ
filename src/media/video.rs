//! Video playback with FFmpeg (libavformat/libavcodec) plus still images.
//!
//! Each movie runs on its own decode thread: video frames are converted to RGBA and published
//! into a shared slot at their presentation time (the stage windows upload them as textures);
//! the soundtrack is resampled to the mixer's rate and fed to an OpenQ mixer voice, so it gets
//! level, fades, EQ and routing like an audio cue. Using FFmpeg directly means any format the
//! system FFmpeg can play works, with no codec plugins to install.

use super::audio::VoiceShared;
use crate::engine::FadeRequest;
use crate::model::{CueId, FadeCurve, Geometry, MIN_DB, StageId, TextCue, VideoCue, VideoFit};
use anyhow::{Context, anyhow};
use ffmpeg_next as ff;
use parking_lot::{Condvar, Mutex};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Where a video's soundtrack goes: into the OpenQ mixer at this rate/channel count.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AudioTap {
    pub rate: u32,
    pub channels: usize,
}

type AudioFeed = Arc<Mutex<Option<rtrb::Producer<f32>>>>;

/// Initialise FFmpeg (idempotent).
pub fn init() -> anyhow::Result<()> {
    static INIT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    INIT.get_or_init(|| {
        ff::init().map_err(|e| e.to_string())?;
        ff::util::log::set_level(ff::util::log::Level::Error);
        Ok(())
    })
    .clone()
    .map_err(|e| anyhow!("FFmpeg init failed: {e}"))
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

// ---------------------------------------------------------------- movie decode thread

#[derive(Default)]
struct MovieState {
    /// Media-clock origin while playing: frame with timestamp `t` (seconds since the pass start)
    /// shows at `origin + t`.
    origin: Option<Instant>,
    paused_at: Option<Instant>,
    quit: bool,
    prerolled: bool,
    finished: bool,
    error: Option<String>,
    /// Last presented file position, seconds.
    position: f64,
}

struct MovieCtl {
    state: Mutex<MovieState>,
    wake: Condvar,
}

impl MovieCtl {
    fn wait(&self, st: &mut parking_lot::MutexGuard<'_, MovieState>, timeout: Duration) {
        self.wake.wait_for(st, timeout);
    }
}

struct Movie {
    ctl: Arc<MovieCtl>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Movie {
    fn drop(&mut self) {
        self.ctl.state.lock().quit = true;
        self.ctl.wake.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct DecodeJob {
    params: VideoCue,
    frame: FrameSlot,
    ctl: Arc<MovieCtl>,
    tap: Option<AudioTap>,
    feed: AudioFeed,
    audio_state: Arc<Mutex<Option<Arc<VoiceShared>>>>,
}

enum Flow {
    Continue,
    Quit,
}

enum FrameResult {
    Shown,
    End,
    Quit,
}

impl DecodeJob {
    fn run(self) {
        let res = self.decode();
        let mut st = self.ctl.state.lock();
        if let Err(e) = res {
            log::error!("video {}: {e:#}", self.params.path.display());
            st.error = Some(format!("{e:#}"));
            *self.frame.lock() = Frame::default();
        } else if !self.params.hold_last_frame {
            *self.frame.lock() = Frame::default();
        }
        st.finished = true;
        drop(st);
        if let Some(s) = self.audio_state.lock().as_ref() {
            s.eof.store(true, Ordering::Release);
        }
        *self.feed.lock() = None;
    }

    /// Sleep until the media clock reaches `t` (seconds into the pass), honouring pause/quit.
    fn wait_until(&self, t: f64, pending: &mut VecDeque<f32>) -> Flow {
        loop {
            self.flush_audio(pending);
            let mut st = self.ctl.state.lock();
            if st.quit {
                return Flow::Quit;
            }
            let Some(origin) = st.origin.filter(|_| st.paused_at.is_none()) else {
                self.ctl.wait(&mut st, Duration::from_millis(20));
                continue;
            };
            let due = origin + Duration::from_secs_f64(t.max(0.0));
            let now = Instant::now();
            if now >= due {
                return Flow::Continue;
            }
            // Wake at least every 10 ms to keep feeding audio.
            self.ctl.wait(&mut st, (due - now).min(Duration::from_millis(10)));
        }
    }

    /// Push pending soundtrack samples into the mixer ring (whatever fits).
    fn flush_audio(&self, pending: &mut VecDeque<f32>) {
        if pending.is_empty() {
            return;
        }
        let mut feed = self.feed.lock();
        let Some(tx) = feed.as_mut() else {
            // Not attached yet (preroll): keep at most a second.
            let cap = self.tap.map_or(0, |t| t.rate as usize * t.channels);
            while pending.len() > cap {
                pending.pop_front();
            }
            return;
        };
        let n = tx.slots().min(pending.len());
        if n == 0 {
            return;
        }
        if let Ok(mut chunk) = tx.write_chunk(n) {
            let (a, b) = chunk.as_mut_slices();
            for s in a.iter_mut().chain(b.iter_mut()) {
                *s = pending.pop_front().unwrap_or(0.0);
            }
            chunk.commit_all();
        }
    }

    fn decode(&self) -> anyhow::Result<()> {
        let p = &self.params;
        let mut ictx = ff::format::input(&p.path).with_context(|| format!("cannot open {}", p.path.display()))?;
        let vstream = ictx.streams().best(ff::media::Type::Video);
        let astream = if self.tap.is_some() { ictx.streams().best(ff::media::Type::Audio) } else { None };
        let (vidx, vtb) = match &vstream {
            Some(s) => (Some(s.index()), f64::from(s.time_base())),
            None => (None, 0.0),
        };
        let mut vdec = match &vstream {
            Some(s) => {
                let mut ctx = ff::codec::context::Context::from_parameters(s.parameters())?;
                ctx.set_threading(ff::codec::threading::Config { kind: ff::codec::threading::Type::Frame, count: 0 });
                Some(ctx.decoder().video().context("unsupported video codec")?)
            }
            None => None,
        };
        let (aidx, atb) = match &astream {
            Some(s) => (Some(s.index()), f64::from(s.time_base())),
            None => (None, 0.0),
        };
        let mut adec = match &astream {
            Some(s) => Some(ff::codec::context::Context::from_parameters(s.parameters())?.decoder().audio()?),
            None => None,
        };
        if vdec.is_none() && adec.is_none() {
            return Err(anyhow!("no playable video or audio stream"));
        }
        let file_dur = (ictx.duration() > 0).then(|| ictx.duration() as f64 / f64::from(ff::ffi::AV_TIME_BASE));
        let start = p.start_time.max(0.0);
        let end = match (p.end_time, file_dur) {
            (Some(e), Some(d)) => Some(e.min(d)),
            (e, _) => e,
        };

        let mut scaler: Option<(ff::software::scaling::Context, u32, u32, ff::format::Pixel)> = None;
        let mut resampler: Option<ff::software::resampling::Context> = None;
        let mut pending: VecDeque<f32> = VecDeque::new();
        let mut plays = 0u32;
        let mut pass_offset = 0.0f64; // media-clock seconds before the current pass

        loop {
            // Position at the start point.
            if start > 0.0 || plays > 0 {
                let ts = (start * f64::from(ff::ffi::AV_TIME_BASE)) as i64;
                ictx.seek(ts, ..ts + 1).context("seek failed")?;
                if let Some(d) = vdec.as_mut() {
                    d.flush();
                }
                if let Some(d) = adec.as_mut() {
                    d.flush();
                }
            }
            let mut pass_len = 0.0f64;
            let mut reached_end = false;
            let mut decoded = ff::util::frame::video::Video::empty();
            let mut adecoded = ff::util::frame::audio::Audio::empty();

            for (stream, packet) in ictx.packets() {
                let idx = stream.index();
                if Some(idx) == vidx {
                    let dec = vdec.as_mut().unwrap();
                    if dec.send_packet(&packet).is_err() {
                        continue;
                    }
                    while dec.receive_frame(&mut decoded).is_ok() {
                        let pts = decoded.timestamp().or(decoded.pts()).unwrap_or(0) as f64 * vtb;
                        match self.show_frame(&decoded, pts, start, end, pass_offset, &mut pass_len, &mut scaler, &mut pending)? {
                            FrameResult::Quit => return Ok(()),
                            FrameResult::End => {
                                reached_end = true;
                                break;
                            }
                            FrameResult::Shown => {}
                        }
                    }
                    if reached_end {
                        break;
                    }
                } else if Some(idx) == aidx {
                    let dec = adec.as_mut().unwrap();
                    if dec.send_packet(&packet).is_err() {
                        continue;
                    }
                    while dec.receive_frame(&mut adecoded).is_ok() {
                        let pts = adecoded.timestamp().or(adecoded.pts()).unwrap_or(0) as f64 * atb;
                        let frame_secs = adecoded.samples() as f64 / adecoded.rate().max(1) as f64;
                        if pts + frame_secs <= start {
                            continue;
                        }
                        if end.is_some_and(|e| pts >= e) {
                            continue;
                        }
                        pass_len = pass_len.max(pts - start + frame_secs);
                        self.resample(&mut adecoded, &mut resampler, &mut pending, (start - pts).max(0.0))?;
                        if vdec.is_none() {
                            // Audio-only file: pace on the media clock.
                            if !self.ctl.state.lock().prerolled {
                                self.ctl.state.lock().prerolled = true;
                            }
                            if let Flow::Quit = self.wait_until(pass_offset + (pts - start) - 0.5, &mut pending) {
                                return Ok(());
                            }
                            self.ctl.state.lock().position = pts;
                        }
                    }
                }
                self.flush_audio(&mut pending);
                if self.ctl.state.lock().quit {
                    return Ok(());
                }
            }
            // Drain frames still inside the decoder (frame threading holds several back).
            if !reached_end && let Some(dec) = vdec.as_mut() {
                let _ = dec.send_eof();
                while dec.receive_frame(&mut decoded).is_ok() {
                    let pts = decoded.timestamp().or(decoded.pts()).unwrap_or(0) as f64 * vtb;
                    match self.show_frame(&decoded, pts, start, end, pass_offset, &mut pass_len, &mut scaler, &mut pending)? {
                        FrameResult::Quit => return Ok(()),
                        FrameResult::End => break,
                        FrameResult::Shown => {}
                    }
                }
            }
            if let Some(dec) = adec.as_mut() {
                let _ = dec.send_eof();
                while dec.receive_frame(&mut adecoded).is_ok() {
                    let pts = adecoded.timestamp().or(adecoded.pts()).unwrap_or(0) as f64 * atb;
                    if end.is_none_or(|e| pts < e) && pts >= start {
                        self.resample(&mut adecoded, &mut resampler, &mut pending, 0.0)?;
                    }
                }
            }
            plays += 1;
            pass_offset += pass_len.max(0.04);
            let more = p.infinite_loop || plays < p.play_count.max(1);
            if !more {
                break;
            }
        }
        // Drain the soundtrack, then wait for the last frame's display time to pass.
        while !pending.is_empty() {
            if let Flow::Quit = self.wait_until(0.0, &mut pending) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
            if self.feed.lock().is_none() {
                break;
            }
        }
        let _ = self.wait_until(pass_offset, &mut pending);
        Ok(())
    }

    /// Wait for a frame's presentation time and show it.
    #[allow(clippy::too_many_arguments)]
    fn show_frame(
        &self,
        decoded: &ff::util::frame::video::Video,
        pts: f64,
        start: f64,
        end: Option<f64>,
        pass_offset: f64,
        pass_len: &mut f64,
        scaler: &mut Option<(ff::software::scaling::Context, u32, u32, ff::format::Pixel)>,
        pending: &mut VecDeque<f32>,
    ) -> anyhow::Result<FrameResult> {
        if pts + 1e-6 < start {
            return Ok(FrameResult::Shown);
        }
        if end.is_some_and(|e| pts >= e) {
            return Ok(FrameResult::End);
        }
        let t = pass_offset + (pts - start);
        *pass_len = pass_len.max(pts - start + 1.0 / 25.0);
        if !self.ctl.state.lock().prerolled {
            self.ctl.state.lock().prerolled = true;
            self.ctl.wake.notify_all();
        }
        if let Flow::Quit = self.wait_until(t, pending) {
            return Ok(FrameResult::Quit);
        }
        self.publish(decoded, scaler)?;
        self.ctl.state.lock().position = pts;
        Ok(FrameResult::Shown)
    }

    fn publish(
        &self,
        f: &ff::util::frame::video::Video,
        scaler: &mut Option<(ff::software::scaling::Context, u32, u32, ff::format::Pixel)>,
    ) -> anyhow::Result<()> {
        let (w, h, fmt) = (f.width(), f.height(), f.format());
        if scaler.as_ref().is_none_or(|s| s.1 != w || s.2 != h || s.3 != fmt) {
            let ctx = ff::software::scaling::Context::get(
                fmt,
                w,
                h,
                ff::format::Pixel::RGBA,
                w,
                h,
                ff::software::scaling::Flags::BILINEAR,
            )?;
            *scaler = Some((ctx, w, h, fmt));
        }
        let mut rgba = ff::util::frame::video::Video::empty();
        scaler.as_mut().unwrap().0.run(f, &mut rgba)?;
        let stride = rgba.stride(0);
        let row = w as usize * 4;
        let src = rgba.data(0);
        let mut slot = self.frame.lock();
        slot.data.resize(row * h as usize, 0);
        for y in 0..h as usize {
            slot.data[y * row..(y + 1) * row].copy_from_slice(&src[y * stride..y * stride + row]);
        }
        slot.width = w;
        slot.height = h;
        slot.generation = slot.generation.wrapping_add(1);
        Ok(())
    }

    /// Convert a decoded audio frame to interleaved f32 at the mixer rate; `skip` seconds at the
    /// front are dropped (start-time trim).
    fn resample(
        &self,
        f: &mut ff::util::frame::audio::Audio,
        resampler: &mut Option<ff::software::resampling::Context>,
        pending: &mut VecDeque<f32>,
        skip: f64,
    ) -> anyhow::Result<()> {
        let Some(tap) = self.tap else { return Ok(()) };
        if f.channel_layout().is_empty() {
            f.set_channel_layout(ff::ChannelLayout::default(f.channels() as i32));
        }
        if resampler.is_none() {
            *resampler = Some(ff::software::resampling::Context::get(
                f.format(),
                f.channel_layout(),
                f.rate(),
                ff::format::Sample::F32(ff::format::sample::Type::Packed),
                ff::ChannelLayout::default(tap.channels as i32),
                tap.rate,
            )?);
        }
        let mut out = ff::util::frame::audio::Audio::empty();
        resampler.as_mut().unwrap().run(f, &mut out)?;
        let n = out.samples() * tap.channels;
        let bytes = &out.data(0)[..n * 4];
        let skip_samples = ((skip * tap.rate as f64) as usize * tap.channels).min(n);
        pending.extend(
            bytes.chunks_exact(4).skip(skip_samples).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        );
        Ok(())
    }
}

// ---------------------------------------------------------------- players

enum Source {
    Movie(Movie),
    Still,
}

struct Player {
    source: Source,
    frame: FrameSlot,
    params: VideoCue,
    opacity: f32,
    op_ramp: Option<Ramp<f32>>,
    geometry: Geometry,
    geo_ramp: Option<Ramp<Geometry>>,
    play_at: Option<Instant>,
    playing: bool,
    /// When a still started showing (shifted by pauses).
    started_at: Option<Instant>,
    paused_at: Option<Instant>,
    finished: bool,
    error: Option<String>,
    error_reported: bool,
    order: u64,
    tap: Option<AudioTap>,
    audio_feed: AudioFeed,
    audio_state: Arc<Mutex<Option<Arc<VoiceShared>>>>,
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

/// The video-layer settings a Text cue plays with.
pub fn text_layer_params(t: &TextCue) -> VideoCue {
    VideoCue {
        volume_db: MIN_DB,
        opacity: t.opacity,
        layer: t.layer,
        fit: VideoFit::Native,
        stage: t.stage,
        geometry: t.geometry,
        still_duration: t.duration,
        ..Default::default()
    }
}

fn text_frame(t: &TextCue) -> FrameSlot {
    let (w, h, data) = super::text::render(t);
    Arc::new(Mutex::new(Frame { width: w, height: h, data, generation: 1 }))
}

impl Player {
    /// A still layer from an already-rendered frame (Text cues).
    fn from_frame(params: VideoCue, order: u64, frame: FrameSlot) -> Self {
        Self {
            source: Source::Still,
            frame,
            opacity: params.opacity.clamp(0.0, 1.0),
            op_ramp: None,
            geometry: params.geometry,
            geo_ramp: None,
            params,
            play_at: None,
            playing: false,
            started_at: None,
            paused_at: None,
            finished: false,
            error: None,
            error_reported: false,
            order,
            tap: None,
            audio_feed: Arc::new(Mutex::new(None)),
            audio_state: Arc::new(Mutex::new(None)),
        }
    }

    fn new(params: &VideoCue, order: u64, tap: Option<AudioTap>) -> anyhow::Result<Self> {
        let frame: FrameSlot = Arc::new(Mutex::new(Frame::default()));
        let audio_feed: AudioFeed = Arc::new(Mutex::new(None));
        let audio_state = Arc::new(Mutex::new(None));
        let source = if super::is_image_path(&params.path) {
            load_still(&params.path, &frame)?;
            Source::Still
        } else {
            let ctl = Arc::new(MovieCtl { state: Mutex::new(MovieState::default()), wake: Condvar::new() });
            let job = DecodeJob {
                params: params.clone(),
                frame: frame.clone(),
                ctl: ctl.clone(),
                tap,
                feed: audio_feed.clone(),
                audio_state: audio_state.clone(),
            };
            let name = format!("openq-video-{}", params.path.file_name().map(|f| f.to_string_lossy()).unwrap_or_default());
            let thread = std::thread::Builder::new().name(name).spawn(move || job.run())?;
            Source::Movie(Movie { ctl, thread: Some(thread) })
        };
        Ok(Self {
            source,
            frame,
            params: params.clone(),
            opacity: params.opacity.clamp(0.0, 1.0),
            op_ramp: None,
            geometry: params.geometry,
            geo_ramp: None,
            play_at: None,
            playing: false,
            started_at: None,
            paused_at: None,
            finished: false,
            error: None,
            error_reported: false,
            order,
            tap,
            audio_feed,
            audio_state,
        })
    }

    fn position(&self) -> Option<f64> {
        match &self.source {
            Source::Movie(m) => Some(m.ctl.state.lock().position),
            Source::Still => self.started_at.map(|s| (self.paused_at.unwrap_or_else(Instant::now) - s).as_secs_f64()),
        }
    }

    fn update(&mut self, now: Instant) {
        if let Source::Movie(m) = &self.source {
            let mut st = m.ctl.state.lock();
            if st.finished && !self.finished {
                self.finished = true;
                if let Some(e) = st.error.take() {
                    self.error = Some(e);
                }
            }
        }
        if self.paused_at.is_some() || self.finished {
            return;
        }
        if !self.playing
            && let Some(at) = self.play_at
            && now >= at
        {
            self.playing = true;
            self.started_at = Some(at);
            if let Source::Movie(m) = &self.source {
                m.ctl.state.lock().origin = Some(at);
                m.ctl.wake.notify_all();
            }
        }
        if self.playing
            && let Source::Still = self.source
        {
            let d = self.params.still_duration;
            if d > 0.0 && self.started_at.is_some_and(|s| now.saturating_duration_since(s).as_secs_f64() >= d) {
                self.finished = true;
                if !self.params.hold_last_frame {
                    *self.frame.lock() = Frame::default();
                }
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

    fn pause(&mut self, now: Instant) {
        if self.paused_at.is_some() {
            return;
        }
        self.paused_at = Some(now);
        if let Source::Movie(m) = &self.source {
            m.ctl.state.lock().paused_at = Some(now);
        }
    }

    fn resume(&mut self, at: Instant) {
        let Some(paused) = self.paused_at.take() else { return };
        let shift = at.saturating_duration_since(paused);
        for r in [&mut self.op_ramp].into_iter().flatten() {
            r.start += shift;
        }
        if let Some(r) = &mut self.geo_ramp {
            r.start += shift;
        }
        for t in [&mut self.play_at, &mut self.started_at].into_iter().flatten() {
            *t += shift;
        }
        if let Source::Movie(m) = &self.source {
            let mut st = m.ctl.state.lock();
            st.paused_at = None;
            if let Some(o) = &mut st.origin {
                *o += shift;
            }
            m.ctl.wake.notify_all();
        }
    }
}

pub struct VideoOutput {
    players: HashMap<CueId, Player>,
    preloaded: HashMap<CueId, Player>,
    /// Text cues currently showing (to detect content edits).
    texts: HashMap<CueId, TextCue>,
    order: u64,
}

impl VideoOutput {
    pub fn new() -> anyhow::Result<Self> {
        init()?;
        Ok(Self { players: HashMap::new(), preloaded: HashMap::new(), texts: HashMap::new(), order: 0 })
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

    pub fn start_text(&mut self, cue: CueId, t: &TextCue, at: Instant) {
        self.order += 1;
        let mut p = Player::from_frame(text_layer_params(t), self.order, text_frame(t));
        p.play_at = Some(at);
        p.update(Instant::now());
        self.players.insert(cue, p);
        self.texts.insert(cue, t.clone());
    }

    /// Live edits to a showing Text cue: re-render if the text itself changed.
    pub fn update_text(&mut self, cue: CueId, t: &TextCue) {
        let Some(old) = self.texts.get(&cue).cloned() else { return };
        let looks = |x: &TextCue| (x.text.clone(), x.size.to_bits(), x.color, x.bold, x.align);
        if looks(&old) != looks(t)
            && let Some(p) = self.players.get_mut(&cue)
        {
            let (w, h, data) = super::text::render(t);
            let mut f = p.frame.lock();
            f.width = w;
            f.height = h;
            f.data = data;
            f.generation = f.generation.wrapping_add(1);
        }
        self.texts.insert(cue, t.clone());
        self.update_live(cue, &text_layer_params(t));
    }

    /// Connect a started cue's soundtrack to its mixer voice.
    pub fn attach_audio(&mut self, cue: CueId, tx: rtrb::Producer<f32>, state: Arc<VoiceShared>) {
        if let Some(p) = self.players.get_mut(&cue) {
            *p.audio_feed.lock() = Some(tx);
            if p.finished {
                state.eof.store(true, Ordering::Release);
            }
            *p.audio_state.lock() = Some(state);
        }
    }

    /// Freeze fades in progress on this cue at their current values.
    pub fn hold(&mut self, cue: CueId) {
        if let Some(p) = self.players.get_mut(&cue) {
            p.update(Instant::now());
            p.op_ramp = None;
            p.geo_ramp = None;
        }
    }

    pub fn pause(&mut self, cue: CueId) {
        if let Some(p) = self.players.get_mut(&cue) {
            p.pause(Instant::now());
        }
    }

    pub fn resume(&mut self, cue: CueId, at: Instant) {
        if let Some(p) = self.players.get_mut(&cue) {
            p.resume(at);
        }
    }

    /// Opacity and geometry fades (the soundtrack level is faded in the mixer).
    pub fn fade(&mut self, cue: CueId, f: &FadeRequest, at: Instant) {
        let Some(p) = self.players.get_mut(&cue) else { return };
        let (secs, curve) = (f.duration, f.curve);
        if let Some(o) = f.opacity {
            p.op_ramp = Some(Ramp { from: p.opacity, to: o.clamp(0.0, 1.0), start: at, secs, curve });
        }
        if let Some(g) = f.geometry {
            p.geo_ramp = Some(Ramp { from: p.geometry, to: g, start: at, secs, curve });
        }
    }

    /// Apply inspector edits to a playing cue (opacity, geometry, layer, fit, stage).
    pub fn update_live(&mut self, cue: CueId, params: &VideoCue) {
        let Some(p) = self.players.get_mut(&cue) else { return };
        let old = std::mem::replace(&mut p.params, params.clone());
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
    pub codec: String,
}

pub fn probe(path: &Path) -> anyhow::Result<VideoInfo> {
    init()?;
    let ictx = ff::format::input(&path).with_context(|| format!("cannot open {}", path.display()))?;
    let duration = (ictx.duration() > 0).then(|| ictx.duration() as f64 / f64::from(ff::ffi::AV_TIME_BASE));
    let mut info = VideoInfo { duration, ..Default::default() };
    if let Some(s) = ictx.streams().best(ff::media::Type::Video) {
        let dec = ff::codec::context::Context::from_parameters(s.parameters())?.decoder().video()?;
        info.width = dec.width();
        info.height = dec.height();
        info.codec = dec.codec().map(|c| c.name().to_string()).unwrap_or_default();
    }
    if let Some(s) = ictx.streams().best(ff::media::Type::Audio) {
        let dec = ff::codec::context::Context::from_parameters(s.parameters())?.decoder().audio()?;
        info.has_audio = true;
        info.audio_channels = dec.channels() as usize;
    }
    if info.width == 0 && !info.has_audio {
        return Err(anyhow!("no video or audio stream"));
    }
    Ok(info)
}

/// One RGBA frame of a movie at `at` seconds (or the first frame after it), scaled so neither
/// side exceeds `max_side`. Used for previews.
/// Returns the scaled size, the pixels and the movie's own size.
pub fn poster(path: &Path, at: f64, max_side: u32) -> anyhow::Result<(u32, u32, Vec<u8>, [u32; 2])> {
    let mut ictx = ff::format::input(&path).with_context(|| format!("cannot open {}", path.display()))?;
    let stream = ictx.streams().best(ff::media::Type::Video).context("no video stream")?;
    let (idx, tb) = (stream.index(), f64::from(stream.time_base()));
    let mut dec = ff::codec::context::Context::from_parameters(stream.parameters())?.decoder().video()?;
    if at > 0.0 {
        let ts = (at * f64::from(ff::ffi::AV_TIME_BASE)) as i64;
        ictx.seek(ts, ..ts + 1).context("seek failed")?;
    }
    let mut frame = ff::util::frame::video::Video::empty();
    let mut got = false;
    'read: for (s, packet) in ictx.packets() {
        if s.index() != idx || dec.send_packet(&packet).is_err() {
            continue;
        }
        while dec.receive_frame(&mut frame).is_ok() {
            let pts = frame.timestamp().or(frame.pts()).unwrap_or(0) as f64 * tb;
            if pts + 0.05 >= at {
                got = true;
                break 'read;
            }
        }
    }
    if !got {
        dec.send_eof()?;
        got = dec.receive_frame(&mut frame).is_ok();
    }
    anyhow::ensure!(got, "no frame at {at:.2}s");
    let (w, h) = (frame.width(), frame.height());
    let k = (max_side as f32 / w.max(h) as f32).min(1.0);
    let (tw, th) = (((w as f32 * k) as u32).max(2), ((h as f32 * k) as u32).max(2));
    let mut scaler =
        ff::software::scaling::Context::get(frame.format(), w, h, ff::format::Pixel::RGBA, tw, th, ff::software::scaling::Flags::BILINEAR)?;
    let mut rgba = ff::util::frame::video::Video::empty();
    scaler.run(&frame, &mut rgba)?;
    let (stride, row) = (rgba.stride(0), tw as usize * 4);
    let src = rgba.data(0);
    let mut data = Vec::with_capacity(row * th as usize);
    for y in 0..th as usize {
        data.extend_from_slice(&src[y * stride..y * stride + row]);
    }
    Ok((tw, th, data, [w, h]))
}
