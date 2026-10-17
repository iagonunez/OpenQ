//! Video playback with FFmpeg (libavformat/libavcodec) plus still images.
//!
//! [`VideoOutput`] runs in the video output process ([`crate::vout`]): each movie has its own
//! decode thread, which publishes the decoded pictures into a shared slot at their presentation
//! time, as they come from the decoder ([`Picture`]: YUV planes, converted to RGB by the output's
//! shader). In OpenQ itself a [`VideoOutput::soundtracks`] decodes only the soundtracks: resampled
//! to the mixer's rate and fed to a mixer voice, so they get level, fades, EQ and routing like an
//! audio cue. Using FFmpeg directly means any format the system FFmpeg can play works, with no
//! codec plugins to install.

use super::audio::VoiceShared;
use super::text::{TextPlanes, TextRender};
use crate::animation::AnimState;
use crate::engine::FadeRequest;
use crate::effects::VideoEffect;
use crate::model::{BlendMode, CameraCue, CueId, FadeCurve, Geometry, MIN_DB, StageId, TextCue, VideoCue, VideoFit};
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
    /// Mixer sample rate.
    pub rate: u32,
    /// Mixer channel count.
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

/// The latest decoded picture of a layer.
#[derive(Default)]
pub struct Frame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The picture (empty for Text cues, which use `planes`).
    pub pixels: Pixels,
    /// Bumped on every new frame so the renderer only re-uploads when needed.
    pub generation: u64,
    /// Image pixels per stage pixel (rendered text is supersampled); 0 means 1.
    pub density: f32,
    /// Show the picture mirrored left to right (Camera cues).
    pub mirror: bool,
    /// Text cues: coverage planes coloured as they are drawn. Shared with the render cache, so
    /// firing a text cue again costs no copy.
    pub planes: Option<Arc<TextPlanes>>,
}

/// The pixels of a [`Frame`].
#[derive(Default)]
pub enum Pixels {
    /// Nothing to show (yet).
    #[default]
    None,
    /// Tightly packed RGBA (still images).
    Rgba {
        /// The pixels, row after row.
        data: Vec<u8>,
        /// `data` is premultiplied.
        premultiplied: bool,
    },
    /// A decoded movie or camera picture as FFmpeg made it (YUV planes, converted on the GPU).
    Picture(Picture),
}

/// A decoded picture held by reference: the decoder's own buffer, uploaded plane by plane and
/// converted to RGB by the output's shader, so no CPU pass touches its pixels.
pub struct Picture {
    /// The FFmpeg frame (a new reference to the decoder's buffer, or a converted copy).
    pub frame: ff::util::frame::video::Video,
    /// How its planes are laid out.
    pub layout: PlaneLayout,
    /// YUV → RGB: `rgb[i] = dot(rows[i].xyz, yuv) + rows[i].w` (gamma-encoded, 0..1).
    pub matrix: [[f32; 4]; 3],
    /// Made by our converter (its buffer can be reused for the next conversion).
    owned: bool,
}

impl Picture {
    /// A picture of a decoded frame in a format the shader reads (converted when it isn't).
    pub fn of(f: &ff::util::frame::video::Video) -> anyhow::Result<Self> {
        let mut c = Converter::default();
        c.convert(f)?;
        c.next.context("nothing converted")
    }

    /// The colour of one pixel, converted on the CPU (tests and snapshots only; the output does
    /// this in its shader).
    pub fn rgb_at(&self, x: u32, y: u32) -> [u8; 3] {
        let f = &self.frame;
        let at = |plane: usize, x: u32, y: u32, size: usize| -> &[u8] {
            let (pw, ph) = (f.plane_width(plane).max(1), f.plane_height(plane).max(1));
            let (px, py) = ((x * pw / f.width().max(1)).min(pw - 1), (y * ph / f.height().max(1)).min(ph - 1));
            let i = py as usize * f.stride(plane) + px as usize * size;
            &f.data(plane)[i..i + size]
        };
        let yuv = match self.layout {
            PlaneLayout::Rgba => {
                let p = at(0, x, y, 4);
                return [p[0], p[1], p[2]];
            }
            PlaneLayout::Planar { .. } => [at(0, x, y, 1)[0], at(1, x, y, 1)[0], at(2, x, y, 1)[0]],
            PlaneLayout::Nv12 => {
                let uv = at(1, x, y, 2);
                [at(0, x, y, 1)[0], uv[0], uv[1]]
            }
        };
        let [y, u, v] = yuv.map(|c| c as f32 / 255.0);
        self.matrix.map(|r| ((r[0] * y + r[1] * u + r[2] * v + r[3]).clamp(0.0, 1.0) * 255.0).round() as u8)
    }
}

/// Plane layouts the output's shader reads directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaneLayout {
    /// Separate Y, U and V planes (any chroma subsampling), plus an alpha plane when `alpha`.
    Planar {
        /// Fourth plane: straight alpha.
        alpha: bool,
    },
    /// Y plane plus one interleaved UV plane.
    Nv12,
    /// One RGBA plane with straight alpha.
    Rgba,
}

impl Frame {
    /// Size of the picture in stage pixels.
    pub fn stage_size(&self) -> [f32; 2] {
        let d = if self.density > 0.0 { self.density } else { 1.0 };
        [self.width as f32 / d, self.height as f32 / d]
    }

    /// True when there is something to draw.
    pub fn has_picture(&self) -> bool {
        self.width > 0 && (self.planes.is_some() || !matches!(self.pixels, Pixels::None))
    }
}

/// A frame shared between a decode thread and the stage windows.
pub type FrameSlot = Arc<Mutex<Frame>>;

/// What a stage window needs to draw one running video.
#[derive(Clone)]
pub struct StageLayer {
    /// Cue playing this layer.
    pub id: CueId,
    /// Stage it shows on (None = first stage).
    pub stage: Option<StageId>,
    /// Current picture.
    pub frame: FrameSlot,
    /// Opacity 0..1 (including fades).
    pub opacity: f32,
    /// Placement on the stage (including fades).
    pub geometry: Geometry,
    /// Stacking layer: higher draws on top.
    pub layer: i32,
    /// How the picture combines with the layers below.
    pub blend_mode: BlendMode,
    /// How the picture fits the stage.
    pub fit: VideoFit,
    /// Start order, to stack equal layers by age.
    pub order: u64,
    /// Colour multiplier, sRGB RGBA 0..1 (white = unchanged; from the animation).
    pub tint: [f32; 4],
    /// Text cues: colours of the background, shadow, outline and fill planes (sRGB RGBA 0..1).
    pub text_colors: [[f32; 4]; 4],
    /// True while an animation runs, so stage windows keep redrawing.
    pub animated: bool,
    /// Video effects (including fades in progress), drawn by the stage's layer shader.
    pub effects: Vec<VideoEffect>,
}

/// Progress shaping for non-level parameters.
fn shape(curve: FadeCurve, t: f32) -> f32 {
    curve.progress(t)
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

/// A Fade cue taking a layer's video effects from one stack to another.
struct FxRamp {
    from: Vec<VideoEffect>,
    to: Vec<VideoEffect>,
    start: Instant,
    secs: f64,
    curve: FadeCurve,
}

impl FxRamp {
    fn progress(&self, now: Instant) -> f32 {
        Ramp { from: (), to: (), start: self.start, secs: self.secs, curve: self.curve }.progress(now)
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
            join_briefly(t, "video decoder");
        }
    }
}

/// Join a worker that was asked to quit. Usually it ends at once; one stuck in a read (a hung
/// network share, an unplugged camera) would block the engine thread here, so after a short
/// wait it is left to end on its own.
fn join_briefly(t: std::thread::JoinHandle<()>, what: &str) {
    let deadline = Instant::now() + std::time::Duration::from_millis(50);
    while !t.is_finished() && Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    if t.is_finished() {
        let _ = t.join();
    } else {
        log::warn!("{what} still busy (stuck reading?); left to finish in the background");
    }
}

struct DecodeJob {
    params: VideoCue,
    /// Decode the pictures (false: only the soundtrack, for OpenQ's side of a movie whose
    /// pictures the video output process decodes itself).
    video: bool,
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
        // A panic inside FFmpeg handling must still mark the movie finished, or its cue would
        // never end.
        let res = crate::recovery::catch(|| self.decode());
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
            let due = origin + crate::engine::duration(t);
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
        let mut ictx = open_file(&p.path).with_context(|| format!("cannot open {}", p.path.display()))?;
        let vstream = if self.video { ictx.streams().best(ff::media::Type::Video) } else { None };
        let astream = if self.tap.is_some() { ictx.streams().best(ff::media::Type::Audio) } else { None };
        if !self.video && astream.is_none() {
            // Soundtrack only, and the file has none: nothing to play here.
            return Ok(());
        }
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
            if self.video && ictx.streams().best(ff::media::Type::Audio).is_some() {
                // A sound file on a stage: nothing to show (its sound plays in OpenQ's mixer).
                return Ok(());
            }
            return Err(anyhow!("no playable video or audio stream"));
        }
        let file_dur = (ictx.duration() > 0).then(|| ictx.duration() as f64 / f64::from(ff::ffi::AV_TIME_BASE));
        let start = p.start_time.max(0.0);
        let end = match (p.end_time, file_dur) {
            (Some(e), Some(d)) => Some(e.min(d)),
            (e, _) => e,
        };

        let mut conv = Converter::default();
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
                        match self.show_frame(&decoded, pts, start, end, pass_offset, &mut pass_len, &mut conv, &mut pending)? {
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
                    match self.show_frame(&decoded, pts, start, end, pass_offset, &mut pass_len, &mut conv, &mut pending)? {
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
        conv: &mut Converter,
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
        conv.convert(decoded)?;
        if let Flow::Quit = self.wait_until(t, pending) {
            return Ok(FrameResult::Quit);
        }
        conv.publish(&self.frame, false);
        self.ctl.state.lock().position = pts;
        Ok(FrameResult::Shown)
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

/// Hands decoded pictures to the output: formats the shader reads (planar YUV, NV12, RGBA) go as
/// a new reference to the decoder's buffer, so nothing is copied or converted on the CPU; others
/// (10-bit, packed YUV, BGR...) are converted once to the nearest format it reads.
#[derive(Default)]
struct Converter {
    /// The converter and the input it was made for.
    scaler: Option<(ff::software::scaling::Context, u32, u32, ff::format::Pixel)>,
    /// The picture to publish next.
    next: Option<Picture>,
    /// A converted frame no longer shown, reused as the next conversion's output.
    spare: Option<ff::util::frame::video::Video>,
}

/// Premultiply straight RGBA in place (gamma space, as egui's colours are).
fn premultiply(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a < 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * a + 127) / 255) as u8;
            }
        }
    }
}

/// The layout of a pixel format the shader reads as it is.
fn native_layout(fmt: ff::format::Pixel) -> Option<PlaneLayout> {
    use ff::format::Pixel as P;
    match fmt {
        P::YUV420P | P::YUVJ420P | P::YUV422P | P::YUVJ422P | P::YUV444P | P::YUVJ444P | P::YUV440P | P::YUVJ440P
        | P::YUV410P | P::YUV411P | P::YUVJ411P => Some(PlaneLayout::Planar { alpha: false }),
        P::YUVA420P | P::YUVA422P | P::YUVA444P => Some(PlaneLayout::Planar { alpha: true }),
        P::NV12 => Some(PlaneLayout::Nv12),
        P::RGBA => Some(PlaneLayout::Rgba),
        _ => None,
    }
}

/// What a format the shader can't read is converted to: RGBA for RGB formats, else 8-bit planar
/// YUV with the same chroma subsampling (and alpha when it has some).
fn conversion_target(fmt: ff::format::Pixel) -> ff::format::Pixel {
    use ff::format::Pixel as P;
    let Some(d) = fmt.descriptor() else { return P::RGBA };
    // SAFETY: the descriptor is a static FFmpeg table entry.
    let rgb = unsafe { (*d.as_ptr()).flags } & ff::ffi::AV_PIX_FMT_FLAG_RGB as u64 != 0;
    let alpha = matches!(d.nb_components(), 2 | 4);
    if rgb || fmt == P::PAL8 {
        return P::RGBA;
    }
    match (d.log2_chroma_w(), d.log2_chroma_h(), alpha) {
        (0, 0, false) => P::YUV444P,
        (0, 0, true) => P::YUVA444P,
        (1, 0, false) => P::YUV422P,
        (1, 0, true) => P::YUVA422P,
        (_, _, false) => P::YUV420P,
        (_, _, true) => P::YUVA420P,
    }
}

/// The YUV → RGB matrix for a frame: its colour space (BT.601, BT.709 or BT.2020; unknown ones
/// guessed from the picture height) and range (limited/TV or full/JPEG).
fn yuv_matrix(f: &ff::util::frame::video::Video) -> [[f32; 4]; 3] {
    use ff::util::color::{Range, Space};
    use ff::format::Pixel as P;
    let (kr, kb) = match f.color_space() {
        Space::BT709 => (0.2126, 0.0722),
        Space::BT470BG | Space::SMPTE170M | Space::FCC => (0.299, 0.114),
        Space::BT2020NCL | Space::BT2020CL => (0.2627, 0.0593),
        Space::SMPTE240M => (0.212, 0.087),
        _ if f.height() >= 720 => (0.2126, 0.0722),
        _ => (0.299, 0.114),
    };
    let full = f.color_range() == Range::JPEG
        || matches!(f.format(), P::YUVJ420P | P::YUVJ422P | P::YUVJ444P | P::YUVJ440P | P::YUVJ411P);
    yuv_matrix_for(kr, kb, full)
}

/// [`yuv_matrix`] for luma weights `kr`, `kb` and full or limited range.
fn yuv_matrix_for(kr: f32, kb: f32, full: bool) -> [[f32; 4]; 3] {
    let kg = 1.0 - kr - kb;
    let (ys, yo, cs) = if full { (1.0, 0.0, 1.0) } else { (255.0 / 219.0, 16.0 / 255.0, 255.0 / 224.0) };
    let co = 128.0 / 255.0;
    // Coefficients on (Y', Cb, Cr) after range expansion.
    let rows = [[1.0, 0.0, 2.0 * (1.0 - kr)], [1.0, -2.0 * kb * (1.0 - kb) / kg, -2.0 * kr * (1.0 - kr) / kg], [1.0, 2.0 * (1.0 - kb), 0.0]];
    rows.map(|[a, b, c]| [a * ys, b * cs, c * cs, -(a * ys * yo) - (b + c) * cs * co])
}

/// A new reference to `f`'s buffers (no pixel copy; FFmpeg's clone copies).
fn frame_ref(f: &ff::util::frame::video::Video) -> anyhow::Result<ff::util::frame::video::Video> {
    let mut out = ff::util::frame::video::Video::empty();
    // SAFETY: both pointers are valid frames; `out` is empty, as av_frame_ref requires.
    let r = unsafe { ff::ffi::av_frame_ref(out.as_mut_ptr(), f.as_ptr()) };
    anyhow::ensure!(r >= 0, "cannot reference a decoded frame ({r})");
    Ok(out)
}

impl Converter {
    /// Prepare `f` for publishing. Done before waiting for the frame's presentation time, so
    /// publishing it on time costs only a swap.
    fn convert(&mut self, f: &ff::util::frame::video::Video) -> anyhow::Result<()> {
        let (w, h, fmt) = (f.width(), f.height(), f.format());
        let matrix = yuv_matrix(f);
        if let Some(layout) = native_layout(fmt) {
            self.next = Some(Picture { frame: frame_ref(f)?, layout, matrix, owned: false });
            return Ok(());
        }
        let target = conversion_target(fmt);
        if self.scaler.as_ref().is_none_or(|s| s.1 != w || s.2 != h || s.3 != fmt) {
            let ctx = ff::software::scaling::Context::get(fmt, w, h, target, w, h, ff::software::scaling::Flags::BILINEAR)?;
            self.scaler = Some((ctx, w, h, fmt));
        }
        let mut out = self
            .spare
            .take()
            .filter(|o| o.format() == target && o.width() == w && o.height() == h)
            .unwrap_or_else(ff::util::frame::video::Video::empty);
        self.scaler.as_mut().expect("scaler").0.run(f, &mut out)?;
        let layout = native_layout(target).expect("conversion targets are native");
        self.next = Some(Picture { frame: out, layout, matrix, owned: true });
        Ok(())
    }

    /// Show the last prepared picture in `slot`.
    fn publish(&mut self, slot: &FrameSlot, mirror: bool) {
        let Some(p) = self.next.take() else { return };
        let mut s = slot.lock();
        (s.width, s.height) = (p.frame.width(), p.frame.height());
        let old = std::mem::replace(&mut s.pixels, Pixels::Picture(p));
        s.mirror = mirror;
        s.generation = s.generation.wrapping_add(1);
        drop(s);
        if let Pixels::Picture(o) = old
            && o.owned
        {
            self.spare = Some(o.frame);
        }
    }
}

/// Per-frame cost of what a movie's decode thread does to each picture: prepare a `w`×`h`
/// picture in `format` for the output and publish it to a frame slot (benchmarks). One entry per
/// frame.
pub(crate) fn conversion_times(w: u32, h: u32, format: ff::format::Pixel, frames: usize) -> anyhow::Result<Vec<std::time::Duration>> {
    init()?;
    let mut src = ff::util::frame::video::Video::new(format, w, h);
    for plane in 0..src.planes() {
        for (i, b) in src.data_mut(plane).iter_mut().enumerate() {
            *b = (i * 7 + plane * 31) as u8;
        }
    }
    let slot = FrameSlot::default();
    let mut conv = Converter::default();
    let mut out = Vec::with_capacity(frames);
    for _ in 0..frames {
        let t = std::time::Instant::now();
        conv.convert(&src)?;
        conv.publish(&slot, false);
        out.push(t.elapsed());
    }
    Ok(out)
}

/// Open a media file with FFmpeg, reading local files only. A path (from a show file or a
/// replica) that names a protocol (`http:`, `tcp:`, `concat:`, `subfile:`...), or a playlist
/// inside the file that points at the network, is refused instead of reaching out.
fn open_file(path: &Path) -> anyhow::Result<ff::format::context::Input> {
    super::check_local(path)?;
    let mut opts = ff::Dictionary::new();
    opts.set("protocol_whitelist", "file");
    Ok(ff::format::input_with_dictionary(path, opts)?)
}

// ---------------------------------------------------------------- live cameras

/// `lavfi` sources and filters a Camera cue may use: test patterns and simple layout. Other
/// filters can read or write files (`movie`, `metadata`...), so a show file (or a replica) can't
/// use them through a Camera cue.
const LAVFI_ALLOWED: &[&str] = &[
    "testsrc", "testsrc2", "smptebars", "smptehdbars", "pal75bars", "pal100bars", "rgbtestsrc", "yuvtestsrc", "colorspectrum",
    "colorchart", "color", "mandelbrot", "allrgb", "allyuv", "gradients", "zoneplate", "nullsrc", "hstack", "vstack", "scale",
    "format", "hflip", "vflip", "pad", "crop", "fps", "setsar",
];

/// Refuse capture sources that aren't cameras or plain test patterns: only this system's camera
/// driver, and `lavfi` graphs made of [`LAVFI_ALLOWED`] (FFmpeg's other input devices grab the
/// screen or the like).
fn check_camera_source(format: &str, device: &str) -> anyhow::Result<()> {
    if format == camera_format_name() {
        if cfg!(target_os = "linux") && !device.starts_with("/dev/") {
            anyhow::bail!("{device} is not a camera device");
        }
        return Ok(());
    }
    if format != "lavfi" {
        anyhow::bail!("FFmpeg input {format} is not a camera");
    }
    // No quoting or escapes: every filter name is plain to see.
    let plain = |c: char| c.is_ascii_alphanumeric() || "=:._#-/[];,+*".contains(c);
    if let Some(c) = device.chars().find(|&c| !plain(c)) {
        anyhow::bail!("test pattern with {c:?} is not allowed");
    }
    for filter in device.split([';', ',']) {
        // `[a][b]hstack=inputs=2[out]`: drop the pad labels, keep the name before its options.
        let mut name = String::new();
        let mut depth = 0;
        for c in filter.chars() {
            match c {
                '[' => depth += 1,
                ']' => depth -= 1,
                _ if depth == 0 => name.push(c),
                _ => {}
            }
        }
        let name = name.split('=').next().unwrap_or("").trim();
        if !LAVFI_ALLOWED.contains(&name) {
            anyhow::bail!("test pattern filter {name:?} is not allowed");
        }
    }
    Ok(())
}

/// This system's camera driver in FFmpeg.
fn camera_format_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "dshow"
    } else if cfg!(target_os = "macos") {
        "avfoundation"
    } else {
        "v4l2"
    }
}

fn find_input_format(name: &str) -> Option<ff::format::format::Input> {
    ff::device::register_all();
    ff::device::input::video().find_map(|f| match f {
        ff::format::format::Format::Input(i) if i.name() == name => Some(i),
        _ => None,
    })
}

/// Cameras this system offers: (device string for [`CameraCue::device`], readable name).
pub fn list_cameras() -> Vec<(String, String)> {
    if init().is_err() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(fmt) = find_input_format(camera_format_name()) {
        // SAFETY: FFmpeg fills a list we free with the matching call; strings are copied out.
        unsafe {
            let mut list: *mut ff::ffi::AVDeviceInfoList = std::ptr::null_mut();
            if ff::ffi::avdevice_list_input_sources(fmt.as_ptr(), std::ptr::null(), std::ptr::null_mut(), &mut list) >= 0
                && !list.is_null()
            {
                for i in 0..(*list).nb_devices as usize {
                    let d = *(*list).devices.add(i);
                    let cstr = |p: *mut std::ffi::c_char| {
                        if p.is_null() { String::new() } else { std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned() }
                    };
                    let (name, desc) = (cstr((*d).device_name), cstr((*d).device_description));
                    let id = if cfg!(target_os = "windows") { format!("video={desc}") } else { name.clone() };
                    out.push((id, if desc.is_empty() { name } else { desc }));
                }
            }
            if !list.is_null() {
                ff::ffi::avdevice_free_list_devices(&mut list);
            }
        }
    }
    if out.is_empty() && cfg!(target_os = "linux") {
        // Not every driver lists its devices; video nodes are the cameras.
        let mut nodes: Vec<String> = std::fs::read_dir("/dev")
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path().display().to_string())
            .filter(|p| p.starts_with("/dev/video"))
            .collect();
        nodes.sort();
        out.extend(nodes.into_iter().map(|n| (n.clone(), n)));
    }
    out
}

/// The default device string when a Camera cue names none.
fn default_camera() -> String {
    list_cameras().into_iter().next().map(|c| c.0).unwrap_or_else(|| {
        if cfg!(target_os = "macos") { "0".into() } else if cfg!(target_os = "windows") { "video=0".into() } else { "/dev/video0".into() }
    })
}

/// The capture thread of a Camera cue.
struct Capture {
    quit: Arc<std::sync::atomic::AtomicBool>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    finished: Arc<std::sync::atomic::AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            join_briefly(t, "camera capture");
        }
    }
}

impl Capture {
    fn start(c: &CameraCue, frame: FrameSlot) -> Self {
        let quit = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let c = c.clone();
        let (q, p, f, e) = (quit.clone(), paused.clone(), finished.clone(), error.clone());
        let thread = std::thread::Builder::new()
            .name("openq-camera".into())
            .spawn(move || {
                if let Err(err) = crate::recovery::catch(|| capture(&c, &frame, &q, &p)) {
                    log::error!("camera: {err:#}");
                    *e.lock() = Some(format!("{err:#}"));
                }
                *frame.lock() = Frame::default();
                f.store(true, Ordering::Release);
            })
            .ok();
        Self { quit, paused, finished, error, thread }
    }
}

fn capture(c: &CameraCue, slot: &FrameSlot, quit: &std::sync::atomic::AtomicBool, paused: &std::sync::atomic::AtomicBool) -> anyhow::Result<()> {
    let fmt_name = c.format.clone().unwrap_or_else(|| camera_format_name().to_string());
    let fmt = find_input_format(&fmt_name).with_context(|| format!("FFmpeg has no {fmt_name} input"))?;
    let url = c.device.clone().unwrap_or_else(default_camera);
    check_camera_source(&fmt_name, &url)?;
    let mut opts = ff::Dictionary::new();
    if let Some([w, h]) = c.size {
        opts.set("video_size", &format!("{w}x{h}"));
    }
    if fmt_name == "avfoundation" {
        // avfoundation refuses to open without a frame rate it supports; 30 is near universal.
        opts.set("framerate", "30");
    }
    let mut ictx = match ff::format::open_with(&url, &ff::format::format::Format::Input(fmt), opts)
        .with_context(|| format!("cannot open camera {url}"))?
    {
        ff::format::context::Context::Input(i) => i,
        _ => return Err(anyhow!("{url} is not an input")),
    };
    let stream = ictx.streams().best(ff::media::Type::Video).context("the camera sends no video")?;
    let idx = stream.index();
    let mut dec = ff::codec::context::Context::from_parameters(stream.parameters())?.decoder().video()?;
    let mut conv = Converter::default();
    let mut decoded = ff::util::frame::video::Video::empty();
    for (s, packet) in ictx.packets() {
        if quit.load(Ordering::Acquire) {
            break;
        }
        if s.index() != idx {
            continue;
        }
        dec.send_packet(&packet)?;
        while dec.receive_frame(&mut decoded).is_ok() {
            // Paused: keep capturing (so the camera doesn't lag on resume) but hold the picture.
            if !paused.load(Ordering::Acquire) {
                conv.convert(&decoded)?;
                conv.publish(slot, c.mirror);
            }
        }
        // Test sources run as fast as they can; real cameras pace themselves.
        if fmt_name == "lavfi" {
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    Ok(())
}

/// The video-layer settings a Camera cue plays with.
pub fn camera_layer_params(c: &CameraCue) -> VideoCue {
    VideoCue {
        volume_db: MIN_DB,
        opacity: c.opacity,
        layer: c.layer,
        blend_mode: c.blend_mode,
        fit: c.fit,
        stage: c.stage,
        geometry: c.geometry,
        still_duration: c.duration,
        video_fx: c.video_fx.clone(),
        ..Default::default()
    }
}

enum Source {
    Movie(Movie),
    Still,
    Camera(Capture),
}

/// How long a playing movie may show the same picture before the watchdog reports it (movies
/// slower than one frame per this long would be reported too).
#[cfg(not(test))]
const MOVIE_STALL: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const MOVIE_STALL: std::time::Duration = std::time::Duration::from_secs(1);

struct Player {
    source: Source,
    frame: FrameSlot,
    params: VideoCue,
    opacity: f32,
    op_ramp: Option<Ramp<f32>>,
    geometry: Geometry,
    geo_ramp: Option<Ramp<Geometry>>,
    /// Video effects now (the cue's, or where a fade has taken them).
    effects: Vec<VideoEffect>,
    fx_ramp: Option<FxRamp>,
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
    /// Text cues: background, shadow, outline and fill colours.
    text_colors: [[u8; 4]; 4],
    /// A still image's load error, set by its loader thread.
    load_error: Option<Arc<Mutex<Option<String>>>>,
    /// Text cues: planes being rendered, shown once ready (the previous ones stay up meanwhile).
    text_job: Option<TextRender>,
    /// Text cues: the look last handed to this layer (shown, or rendering).
    text_look: Option<TextCue>,
    /// Stall watchdog of a playing movie: the last frame generation seen and since when.
    stall_watch: Option<(u64, Instant)>,
    stall_reported: bool,
    /// Pictures are decoded here (false for a soundtrack-only movie: no stall watchdog).
    shows_video: bool,
    /// Seconds already played before this start (a layer brought back after a restart): its
    /// animation and still duration continue from there.
    clock_offset: f64,
}

/// SVG rasterising scale for a layer: sharp at the largest size its geometry and animation
/// reach (2× like text when shown at its own size; about 2560 px across when fitted).
fn svg_scale(params: &VideoCue, longest: f32) -> f32 {
    let g = &params.geometry;
    let zoom = g.scale_x.abs().max(g.scale_y.abs()).max(1.0) * params.animation.max_scale();
    match params.fit {
        VideoFit::Native => 2.0 * zoom,
        _ => 2560.0 * zoom / longest.max(1.0),
    }
}

/// Decode a still on its own thread (big images and SVGs take a while; the engine must not wait),
/// publishing it into `slot` when ready.
fn load_still(params: &VideoCue, slot: FrameSlot) -> Arc<Mutex<Option<String>>> {
    let error = Arc::new(Mutex::new(None));
    let (err, params) = (error.clone(), params.clone());
    let spawned = std::thread::Builder::new().name("openq-still".into()).spawn(move || {
        let path = &params.path;
        let res = crate::recovery::catch(|| {
            let (w, h) = if super::still::is_svg(path) { super::still::dimensions(path)? } else { (1, 1) };
            super::still::load(path, svg_scale(&params, w.max(h) as f32))
        });
        match res {
            Ok(mut img) => {
                // Premultiply here, so the stage uploads it as is (no per-pixel pass on the UI thread).
                premultiply(&mut img.rgba);
                let mut f = slot.lock();
                f.width = img.width;
                f.height = img.height;
                f.pixels = Pixels::Rgba { data: img.rgba, premultiplied: true };
                f.density = img.density;
                f.generation = f.generation.wrapping_add(1);
            }
            Err(e) => *err.lock() = Some(format!("{e:#}")),
        }
    });
    if let Err(e) = spawned {
        *error.lock() = Some(format!("cannot start image loader: {e}"));
    }
    error
}

/// The video-layer settings a Text cue plays with.
pub fn text_layer_params(t: &TextCue) -> VideoCue {
    VideoCue {
        volume_db: MIN_DB,
        opacity: t.opacity,
        layer: t.layer,
        blend_mode: t.blend_mode,
        fit: VideoFit::Native,
        stage: t.stage,
        geometry: t.geometry,
        still_duration: t.duration,
        animation: t.animation.clone(),
        video_fx: t.video_fx.clone(),
        ..Default::default()
    }
}


/// A Text cue's colours in plane order: background, shadow, outline, fill.
fn text_colors(t: &TextCue) -> [[u8; 4]; 4] {
    [t.bg_color, t.shadow_color, t.outline_color, t.color]
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
            effects: params.video_fx.clone(),
            fx_ramp: None,
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
            text_colors: [[255; 4]; 4],
            load_error: None,
            text_job: None,
            text_look: None,
            stall_watch: None,
            stall_reported: false,
            shows_video: true,
            clock_offset: 0.0,
        }
    }

    fn new(params: &VideoCue, order: u64, tap: Option<AudioTap>, video: bool) -> anyhow::Result<Self> {
        let frame: FrameSlot = Arc::new(Mutex::new(Frame::default()));
        let audio_feed: AudioFeed = Arc::new(Mutex::new(None));
        let audio_state = Arc::new(Mutex::new(None));
        let mut load_error = None;
        let source = if super::is_image_path(&params.path) {
            load_error = Some(load_still(params, frame.clone()));
            Source::Still
        } else {
            let ctl = Arc::new(MovieCtl { state: Mutex::new(MovieState::default()), wake: Condvar::new() });
            let job = DecodeJob {
                params: params.clone(),
                video,
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
            effects: params.video_fx.clone(),
            fx_ramp: None,
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
            text_colors: [[255; 4]; 4],
            load_error,
            text_job: None,
            text_look: None,
            stall_watch: None,
            stall_reported: false,
            shows_video: video,
            clock_offset: 0.0,
        })
    }

    /// Seconds the layer has been playing (its animation clock; stops while paused).
    fn anim_time(&self, now: Instant) -> f64 {
        self.clock_offset + self.started_at.map_or(0.0, |s| self.paused_at.unwrap_or(now).saturating_duration_since(s).as_secs_f64())
    }

    fn position(&self) -> Option<f64> {
        match &self.source {
            Source::Movie(m) => Some(m.ctl.state.lock().position),
            Source::Still | Source::Camera(_) => {
                self.started_at.map(|s| (self.paused_at.unwrap_or_else(Instant::now) - s).as_secs_f64())
            }
        }
    }

    /// Show a Text cue's planes once its background render has finished.
    fn publish_text(&mut self) {
        let Some(planes) = self.text_job.as_ref().and_then(|j| j.ready()) else { return };
        self.text_job = None;
        let mut f = self.frame.lock();
        f.width = planes.width;
        f.height = planes.height;
        f.density = planes.density;
        f.planes = Some(planes);
        f.generation = f.generation.wrapping_add(1);
    }

    fn update(&mut self, now: Instant) {
        self.publish_text();
        if let Some(e) = self.load_error.as_ref().and_then(|e| e.lock().take()) {
            self.finished = true;
            self.error = Some(e);
        }
        if let Source::Movie(m) = &self.source {
            let mut st = m.ctl.state.lock();
            if st.finished && !self.finished {
                self.finished = true;
                if let Some(e) = st.error.take() {
                    self.error = Some(e);
                }
            }
        }
        if let Source::Camera(c) = &self.source
            && c.finished.load(Ordering::Acquire)
            && !self.finished
        {
            self.finished = true;
            self.error = c.error.lock().take();
        }
        if self.paused_at.is_some() || self.finished {
            self.stall_watch = None;
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
            && matches!(self.source, Source::Still | Source::Camera(_))
        {
            let d = self.params.still_duration;
            if d > 0.0 && self.started_at.is_some_and(|s| self.clock_offset + now.saturating_duration_since(s).as_secs_f64() >= d) {
                self.finished = true;
                if !self.params.hold_last_frame {
                    *self.frame.lock() = Frame::default();
                }
            }
        }
        if self.playing
            && matches!(self.source, Source::Movie(_))
            && self.shows_video
            && !self.stall_reported
        {
            // Watchdog: a movie whose picture stops changing without an error or its end (a
            // hung network share, a stuck decoder) is reported once; it plays on if it recovers.
            let generation = self.frame.lock().generation;
            match self.stall_watch {
                Some((g, since)) if g == generation => {
                    if now.saturating_duration_since(since) >= MOVIE_STALL {
                        self.stall_reported = true;
                        self.error = Some(format!(
                            "no new picture for {} s (slow or disconnected drive, or a broken file?)",
                            MOVIE_STALL.as_secs()
                        ));
                    }
                }
                _ => self.stall_watch = Some((generation, now)),
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
        if let Some(r) = &self.fx_ramp {
            let t = r.progress(now);
            self.effects = crate::effects::lerp_video_fx(&r.from, &r.to, if t >= 1.0 { 1.0 } else { shape(r.curve, t) });
            if t >= 1.0 {
                self.fx_ramp = None;
            }
        }
    }

    fn pause(&mut self, now: Instant) {
        self.stall_watch = None;
        if self.paused_at.is_some() {
            return;
        }
        self.paused_at = Some(now);
        if let Source::Movie(m) = &self.source {
            m.ctl.state.lock().paused_at = Some(now);
        }
        if let Source::Camera(c) = &self.source {
            c.paused.store(true, Ordering::Release);
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
        if let Some(r) = &mut self.fx_ramp {
            r.start += shift;
        }
        for t in [&mut self.play_at, &mut self.started_at].into_iter().flatten() {
            *t += shift;
        }
        if let Source::Camera(c) = &self.source {
            c.paused.store(false, Ordering::Release);
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

/// All playing video, image and text layers (in the video output process), or the soundtracks of
/// playing movies (in OpenQ itself, see [`VideoOutput::soundtracks`]).
pub struct VideoOutput {
    /// Decode pictures (false: soundtracks only).
    video: bool,
    players: HashMap<CueId, Player>,
    preloaded: HashMap<CueId, Player>,
    /// Latest render of each Text cue that is preloaded or showing (rendered in the background
    /// ahead of GO, so starting a text cue renders nothing).
    text_renders: HashMap<CueId, TextRender>,
    order: u64,
}

impl VideoOutput {
    /// Initialise FFmpeg.
    pub fn new() -> anyhow::Result<Self> {
        init()?;
        Ok(Self { video: true, players: HashMap::new(), preloaded: HashMap::new(), text_renders: HashMap::new(), order: 0 })
    }

    /// Movie soundtracks only: each started movie decodes just its audio, into the mixer (the
    /// pictures play in the video output process). Stills, text and cameras are not used here.
    pub fn soundtracks() -> anyhow::Result<Self> {
        Ok(Self { video: false, ..Self::new()? })
    }

    /// Open these cues ahead of time so they start without delay.
    pub fn set_preloaded(&mut self, cues: &[(CueId, &VideoCue, Option<AudioTap>)]) {
        self.preloaded.retain(|id, p| cues.iter().any(|(c, v, t)| c == id && **v == p.params && *t == p.tap));
        for (id, v, tap) in cues {
            if !self.preloaded.contains_key(id) {
                self.order += 1;
                match Player::new(v, self.order, *tap, self.video) {
                    Ok(p) => {
                        self.preloaded.insert(*id, p);
                    }
                    Err(e) => log::warn!("preload video failed: {e:#}"),
                }
            }
        }
    }

    /// Start a movie or image so it appears at `at`; `tap` routes its soundtrack into the mixer.
    pub fn start(&mut self, cue: CueId, params: &VideoCue, at: Instant, tap: Option<AudioTap>) -> Result<(), String> {
        if params.path.as_os_str().is_empty() {
            return Err("no video file".into());
        }
        // The caller checks the file from the prober's cache (never stat on the engine thread).
        self.players.remove(&cue);
        self.order += 1;
        let mut p = match self.preloaded.remove(&cue) {
            Some(p) if p.params == *params && p.tap == tap && !p.finished => p,
            _ => Player::new(params, self.order, tap, self.video).map_err(|e| format!("{e:#}"))?,
        };
        p.order = self.order;
        p.play_at = Some(at);
        p.update(Instant::now());
        self.players.insert(cue, p);
        Ok(())
    }

    /// Remove a layer.
    pub fn stop(&mut self, cue: CueId) {
        self.players.remove(&cue);
    }

    /// Remove every layer.
    pub fn stop_all(&mut self) {
        self.players.clear();
    }

    /// Show a live camera as a layer.
    pub fn start_camera(&mut self, cue: CueId, c: &CameraCue, at: Instant) {
        self.order += 1;
        let frame: FrameSlot = Arc::new(Mutex::new(Frame::default()));
        let mut p = Player::from_frame(camera_layer_params(c), self.order, frame.clone());
        p.source = Source::Camera(Capture::start(c, frame));
        p.play_at = Some(at);
        p.update(Instant::now());
        self.players.insert(cue, p);
    }

    /// The render of `t`'s look: the cached one if it still matches, else a new background render.
    fn text_render(&mut self, cue: CueId, t: &TextCue) -> TextRender {
        let look = super::text::look(t);
        match self.text_renders.get(&cue) {
            Some(r) if r.look == look => r.clone(),
            _ => {
                let r = TextRender::spawn(t);
                self.text_renders.insert(cue, r.clone());
                r
            }
        }
    }

    /// Render these Text cues in the background ahead of GO; drop renders no longer needed.
    pub fn set_preloaded_text(&mut self, cues: &[(CueId, &TextCue)]) {
        let players = &self.players;
        self.text_renders.retain(|id, _| players.contains_key(id) || cues.iter().any(|(c, _)| c == id));
        for (id, t) in cues {
            self.text_render(*id, t);
        }
    }

    /// Show a Text cue as a layer. Its planes come from the preload render (or a new background
    /// render, shown a few milliseconds late); nothing is rendered on the calling thread.
    pub fn start_text(&mut self, cue: CueId, t: &TextCue, at: Instant) {
        self.order += 1;
        let job = self.text_render(cue, t);
        let mut p = Player::from_frame(text_layer_params(t), self.order, Arc::new(Mutex::new(Frame::default())));
        p.text_colors = text_colors(t);
        p.text_look = Some(job.look.clone());
        p.text_job = Some(job);
        p.play_at = Some(at);
        p.update(Instant::now());
        self.players.insert(cue, p);
    }

    /// Live edits to a showing Text cue: colours and placement apply at once; a changed look is
    /// re-rendered in the background and replaces the picture when ready.
    pub fn update_text(&mut self, cue: CueId, t: &TextCue) {
        if !self.players.contains_key(&cue) {
            return;
        }
        let job = self.text_render(cue, t);
        if let Some(p) = self.players.get_mut(&cue) {
            p.text_colors = text_colors(t);
            if p.text_look.as_ref() != Some(&job.look) {
                p.text_look = Some(job.look.clone());
                p.text_job = Some(job);
                p.publish_text();
            }
        }
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
            p.fx_ramp = None;
        }
    }

    /// Freeze a layer (and its soundtrack).
    pub fn pause(&mut self, cue: CueId) {
        self.pause_at(cue, Instant::now());
    }

    /// Freeze a layer as of `at` (a moment just past: the pause the engine asked for).
    pub fn pause_at(&mut self, cue: CueId, at: Instant) {
        if let Some(p) = self.players.get_mut(&cue) {
            p.pause(at.min(Instant::now()));
        }
    }

    /// Continue a paused layer at `at`.
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
        if let Some(fx) = &f.video_fx {
            p.fx_ramp = Some(FxRamp { from: p.effects.clone(), to: fx.clone(), start: at, secs, curve });
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
        if old.video_fx != params.video_fx {
            p.fx_ramp = None;
            p.effects = params.video_fx.clone();
        }
    }

    /// Continue a just-started layer's clock (animation, still duration) from `secs`.
    pub fn set_clock_offset(&mut self, cue: CueId, secs: f64) {
        if let Some(p) = self.players.get_mut(&cue) {
            p.clock_offset = secs.max(0.0);
        }
    }

    /// How many pictures each layer has received so far (0 until it shows).
    pub fn frame_counts(&self) -> Vec<(CueId, u64)> {
        self.players.iter().map(|(id, p)| (*id, if p.playing || p.finished { p.frame.lock().generation } else { 0 })).collect()
    }

    /// The cues that have a layer.
    pub fn cues(&self) -> Vec<CueId> {
        self.players.keys().copied().collect()
    }

    /// True if the cue has a layer.
    pub fn has(&self, cue: CueId) -> bool {
        self.players.contains_key(&cue)
    }

    /// True once a movie ended on its own (or failed).
    pub fn is_finished(&self, cue: CueId) -> bool {
        self.players
            .get(&cue)
            .is_none_or(|p| p.finished && (p.error.is_some() || p.error_reported || !p.params.hold_last_frame))
    }

    /// Seconds of media played.
    pub fn elapsed(&self, cue: CueId) -> Option<f64> {
        self.players.get(&cue)?.position()
    }

    /// Advance fades.
    pub fn update(&mut self, now: Instant) {
        for p in self.players.values_mut() {
            p.update(now);
        }
    }

    /// Errors from decode threads since the last call.
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

    /// Layers to draw, bottom to top, with their animations evaluated for this moment.
    pub fn stage_layers(&self) -> Vec<StageLayer> {
        let now = Instant::now();
        let mut v: Vec<StageLayer> = self
            .players
            .iter()
            .filter(|(_, p)| p.playing || p.finished)
            .map(|(id, p)| {
                let anim = &p.params.animation;
                let st = if anim.is_empty() { AnimState::default() } else { anim.sample(p.anim_time(now)) };
                let text_colors = std::array::from_fn(|i| st.text[i].unwrap_or(p.text_colors[i].map(|c| c as f32 / 255.0)));
                StageLayer {
                    id: *id,
                    stage: p.params.stage,
                    frame: p.frame.clone(),
                    opacity: p.opacity * st.opacity,
                    geometry: st.geometry(&p.geometry),
                    layer: p.params.layer,
                    blend_mode: p.params.blend_mode,
                    fit: p.params.fit,
                    order: p.order,
                    tint: st.tint,
                    text_colors,
                    animated: !anim.is_empty() && p.paused_at.is_none(),
                    effects: p.effects.iter().filter(|e| e.enabled).copied().collect(),
                }
            })
            .collect();
        v.sort_by_key(|l| (l.layer, l.order));
        v
    }
}

/// What FFmpeg reports about a media file.
#[derive(Clone, Debug, Default)]
pub struct VideoInfo {
    /// Duration in seconds, if known.
    pub duration: Option<f64>,
    /// Picture width (0 = no video stream).
    pub width: u32,
    /// Picture height.
    pub height: u32,
    /// Has a soundtrack.
    pub has_audio: bool,
    /// Soundtrack channel count.
    pub audio_channels: usize,
    /// Video codec name.
    pub codec: String,
}

/// Read a file's streams with FFmpeg.
pub fn probe(path: &Path) -> anyhow::Result<VideoInfo> {
    init()?;
    let ictx = open_file(path).with_context(|| format!("cannot open {}", path.display()))?;
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
    let mut ictx = open_file(path).with_context(|| format!("cannot open {}", path.display()))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animated_text_layer() {
        use crate::animation::AnimProp;
        let mut t = TextCue { text: "Anim".into(), color: [255, 0, 0, 255], ..Default::default() };
        t.geometry.x = 0.1;
        t.animation.set_key(AnimProp::X, 0.0, [0.0; 4]);
        t.animation.set_key(AnimProp::X, 0.2, [0.5, 0.0, 0.0, 0.0]);
        t.animation.set_key(AnimProp::Opacity, 0.0, [0.5, 0.0, 0.0, 0.0]);
        t.animation.set_key(AnimProp::TextColor, 0.0, [0.0, 1.0, 0.0, 1.0]);
        let mut out = VideoOutput::new().unwrap();
        let id = CueId::new_v4();
        out.start_text(id, &t, Instant::now());
        std::thread::sleep(Duration::from_millis(250));
        eventually(|| {
            out.update(Instant::now());
            out.stage_layers()[0].frame.lock().planes.is_some()
        });
        let l = out.stage_layers().pop().unwrap();
        assert!((l.geometry.x - 0.6).abs() < 1e-4, "{}", l.geometry.x);
        assert_eq!(l.opacity, 0.5);
        assert_eq!(l.text_colors[3], [0.0, 1.0, 0.0, 1.0]);
        assert!(l.animated);
        assert!(l.frame.lock().planes.as_ref().is_some_and(|p| p.masks.len() == 1));
        // Paused: the clock stops.
        out.pause(id);
        let a = out.stage_layers()[0].geometry.x;
        assert!(!out.stage_layers()[0].animated);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(out.stage_layers()[0].geometry.x, a);
    }

    /// Wait (up to 10 s) until `f` holds.
    fn eventually(mut f: impl FnMut() -> bool) {
        let t0 = Instant::now();
        while !f() {
            assert!(t0.elapsed() < Duration::from_secs(10), "timed out");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Text is rendered ahead of GO on the worker; starting and editing a showing text cue does
    /// no rendering on the calling (engine/UI) thread, and colour, placement and animation edits
    /// neither re-render nor re-upload the picture.
    #[test]
    fn text_cues_never_render_on_the_calling_thread() {
        use crate::animation::AnimProp;
        use crate::media::text::render_count;
        let text = "no render on the engine thread";
        let mut t = TextCue { text: text.into(), outline_width: 4.0, shadow_color: [0, 0, 0, 200], ..Default::default() };
        let mut out = VideoOutput::new().unwrap();
        let id = CueId::new_v4();
        out.set_preloaded_text(&[(id, &t)]);
        out.text_renders[&id].wait();
        assert_eq!(render_count(text), 1);

        let t0 = Instant::now();
        out.start_text(id, &t, Instant::now());
        out.update(Instant::now());
        let took = t0.elapsed();
        assert!(took < Duration::from_millis(5), "start_text took {took:?}");
        let generation = out.stage_layers()[0].frame.lock().generation;
        assert!(out.stage_layers()[0].frame.lock().planes.is_some(), "preloaded planes shown at once");

        // 300 live edits that don't change the look: colours, placement, animation.
        let t0 = Instant::now();
        for i in 0..300 {
            let f = i as f32 / 300.0;
            t.color = [255, (f * 255.0) as u8, 0, 255];
            t.shadow_color[3] = 100 + (i % 100) as u8;
            t.geometry.x = f;
            t.opacity = 1.0 - f / 2.0;
            t.animation.set_key(AnimProp::Rotation, f as f64, [f * 90.0, 0.0, 0.0, 0.0]);
            out.update_text(id, &t);
            out.update(Instant::now());
            let _ = out.stage_layers();
        }
        let took = t0.elapsed();
        assert!(took < Duration::from_millis(150), "300 edits took {took:?}");
        assert_eq!(render_count(text), 1, "a colour/placement/animation edit re-rendered the text");
        assert_eq!(out.stage_layers()[0].frame.lock().generation, generation, "the picture was re-uploaded");

        // A new look renders in the background; the old picture stays up until it is ready.
        t.size = 50.0;
        let t0 = Instant::now();
        out.update_text(id, &t);
        assert!(t0.elapsed() < Duration::from_millis(5), "update_text blocked");
        eventually(|| {
            out.update(Instant::now());
            out.stage_layers()[0].frame.lock().generation != generation
        });
        assert_eq!(render_count(text), 2);

        // Without a preload, starting still doesn't block: the text appears when rendered.
        let other = TextCue { text: "started cold".into(), outline_width: 6.0, ..Default::default() };
        let id2 = CueId::new_v4();
        let t0 = Instant::now();
        out.start_text(id2, &other, Instant::now());
        assert!(t0.elapsed() < Duration::from_millis(5), "cold start_text blocked");
        eventually(|| {
            out.update(Instant::now());
            out.stage_layers().iter().any(|l| l.id == id2 && l.frame.lock().planes.is_some())
        });
    }

    /// Movie pictures with alpha are premultiplied on the decode thread (gamma space, matching
    /// egui's `Color32::from_rgba_unmultiplied`), so the stage can upload them as they are.
    #[test]
    fn premultiply_matches_egui() {
        let mut px = vec![200, 100, 50, 128, 10, 20, 30, 255, 255, 255, 255, 0];
        let want: Vec<u8> = px
            .chunks_exact(4)
            .flat_map(|p| egui::Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3]).to_array())
            .collect();
        premultiply(&mut px);
        for (a, b) in px.iter().zip(&want) {
            assert!(a.abs_diff(*b) <= 1, "{px:?} vs {want:?}");
        }
    }

    /// Camera cues open cameras and test patterns only: no screen grabbing, and no lavfi filter
    /// that reads or writes files.
    #[test]
    fn camera_sources_are_checked() {
        assert!(check_camera_source("lavfi", "testsrc2=size=1280x720:rate=30000/1001").is_ok());
        assert!(check_camera_source("lavfi", "color=red:size=64x16[a];color=blue:size=64x16[b];[a][b]hstack").is_ok());
        for bad in ["movie=/etc/passwd", "testsrc,metadata=mode=print:file=x", "life=f=secret", "color='red'", "testsrc;[0]sendcmd=f=x"] {
            assert!(check_camera_source("lavfi", bad).is_err(), "{bad}");
        }
        assert!(check_camera_source("x11grab", ":0.0").is_err());
        assert!(check_camera_source("fbdev", "/dev/fb0").is_err());
        assert!(check_camera_source(camera_format_name(), if cfg!(target_os = "linux") { "/dev/video0" } else { "0" }).is_ok());
    }

    /// FFmpeg only reads local files for media cues: a URL is refused, not fetched.
    #[test]
    fn media_files_are_local_only() {
        init().unwrap();
        let dir = std::env::temp_dir().join(format!("openq-local-only-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("a.wav");
        crate::media::decoder::tests::write_wav(&wav, 48000, 0.2);
        assert!(open_file(&wav).is_ok());
        // FFmpeg on its own would open these (two files joined, or a URL).
        let joined = format!("concat:{}|{}", wav.display(), wav.display());
        assert!(ff::format::input(&joined).is_ok(), "FFmpeg takes {joined}");
        for url in [joined.as_str(), &format!("file:{}", wav.display()).replace("file:", "subfile:,start,0,end,0,:"), "http://127.0.0.1:9/x.wav"] {
            assert!(open_file(Path::new(url)).is_err(), "{url}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn camera_capture_publishes_mirrored_frames() {
        init().unwrap();
        if find_input_format("lavfi").is_none() {
            return; // FFmpeg built without libavfilter devices
        }
        // A test pattern stands in for a camera: left half red, right half blue.
        let c = CameraCue {
            format: Some("lavfi".into()),
            device: Some("color=red:size=64x16[a];color=blue:size=64x16[b];[a][b]hstack".into()),
            mirror: true,
            ..Default::default()
        };
        let mut out = VideoOutput::new().unwrap();
        let id = CueId::new_v4();
        out.start_camera(id, &c, Instant::now());
        let t0 = Instant::now();
        let slot = loop {
            out.update(Instant::now());
            if let Some(l) = out.stage_layers().first()
                && l.frame.lock().width > 0
            {
                break l.frame.clone();
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "no camera frame");
            std::thread::sleep(Duration::from_millis(10));
        };
        let check = |f: &Frame| {
            assert_eq!((f.width, f.height), (128, 16));
            // The picture is passed on as decoded; the output mirrors it.
            assert!(f.mirror);
            let Pixels::Picture(p) = &f.pixels else { panic!("not a decoded picture") };
            let (left, right) = (p.rgb_at(2, 8), p.rgb_at(125, 8));
            assert!(left[0] > 200 && left[2] < 60, "{left:?}");
            assert!(right[2] > 200 && right[0] < 60, "{right:?}");
        };
        let generation = {
            let f = slot.lock();
            check(&f);
            f.generation
        };
        // Later frames are published too.
        let t0 = Instant::now();
        while slot.lock().generation < generation + 3 {
            assert!(t0.elapsed() < Duration::from_secs(5), "no further camera frames");
            std::thread::sleep(Duration::from_millis(10));
        }
        check(&slot.lock());
        out.stop(id);
        assert!(!out.has(id));
    }

    /// Formats the shader can't read are converted to the nearest one it can; the colour
    /// matrices turn reference YUV values into the right RGB.
    #[test]
    fn pictures_convert_to_shader_formats() {
        use ff::format::Pixel as P;
        init().unwrap();
        assert_eq!(native_layout(P::YUV420P), Some(PlaneLayout::Planar { alpha: false }));
        assert_eq!(native_layout(P::NV12), Some(PlaneLayout::Nv12));
        assert_eq!(conversion_target(P::YUV420P10LE), P::YUV420P);
        assert_eq!(conversion_target(P::YUVA444P10LE), P::YUVA444P);
        assert_eq!(conversion_target(P::YUYV422), P::YUV422P);
        assert_eq!(conversion_target(P::BGRA), P::RGBA);
        assert_eq!(conversion_target(P::RGB24), P::RGBA);
        // BT.709 limited range: black, white and pure red.
        let m = yuv_matrix_for(0.2126, 0.0722, false);
        let rgb = |y: f32, u: f32, v: f32| m.map(|r| r[0] * y / 255.0 + r[1] * u / 255.0 + r[2] * v / 255.0 + r[3]);
        let close = |a: [f32; 3], b: [f32; 3]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.01);
        assert!(close(rgb(16.0, 128.0, 128.0), [0.0; 3]), "{:?}", rgb(16.0, 128.0, 128.0));
        assert!(close(rgb(235.0, 128.0, 128.0), [1.0; 3]));
        assert!(close(rgb(63.0, 102.0, 240.0), [1.0, 0.0, 0.0]), "{:?}", rgb(63.0, 102.0, 240.0));
        // A 10-bit picture goes out as 8-bit planar YUV.
        let mut src = ff::util::frame::video::Video::new(P::YUV420P10LE, 64, 32);
        for plane in 0..3 {
            let v: u16 = if plane == 0 { 940 } else { 512 }; // white
            for px in src.data_mut(plane).as_chunks_mut::<2>().0 {
                *px = v.to_le_bytes();
            }
        }
        let mut conv = Converter::default();
        conv.convert(&src).unwrap();
        let slot = FrameSlot::default();
        conv.publish(&slot, false);
        let f = slot.lock();
        let Pixels::Picture(p) = &f.pixels else { panic!() };
        assert_eq!(p.frame.format(), P::YUV420P);
        assert!(p.rgb_at(10, 10).iter().all(|c| *c > 245), "{:?}", p.rgb_at(10, 10));
    }

    /// A movie that stops delivering pictures without failing (a hung share) is reported once.
    #[test]
    fn a_stalled_movie_is_reported() {
        let dir = std::env::temp_dir().join(format!("openq-movie-stall-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("hung.mp4");
        let _ = std::fs::remove_file(&fifo);
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let mut out = VideoOutput::new().unwrap();
        let cue = CueId::new_v4();
        let params = VideoCue { path: fifo.clone(), ..Default::default() };
        out.start(cue, &params, Instant::now(), None).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let mut reports = Vec::new();
        while reports.is_empty() {
            assert!(Instant::now() < deadline, "stall never reported");
            std::thread::sleep(std::time::Duration::from_millis(50));
            out.update(Instant::now());
            reports = out.drain_errors();
        }
        assert!(reports[0].contains("no new picture"), "{reports:?}");
        std::thread::sleep(std::time::Duration::from_millis(1200));
        out.update(Instant::now());
        assert!(out.drain_errors().is_empty(), "reported once");
        // Stopping doesn't wait for the decode thread stuck opening the FIFO.
        let t0 = Instant::now();
        out.stop(cue);
        assert!(t0.elapsed() < std::time::Duration::from_millis(500), "stop blocked for {:?}", t0.elapsed());
        let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
