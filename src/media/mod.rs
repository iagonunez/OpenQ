//! Media backend: routes engine requests to the audio mixer and the video player.

pub mod audio;
pub mod decoder;
pub mod input;
pub mod tcio;
pub mod text;
#[cfg(feature = "video")]
pub mod video;

use crate::engine::{Backend, FadeRequest};
#[cfg(feature = "video")]
use crate::model::VideoCue;
use crate::model::{Cue, CueId, CueKind, NetworkCue};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// What the prober learned about a media file.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct MediaInfo {
    /// File duration in seconds.
    pub duration: Option<f64>,
    /// Human-readable summary (format, channels, resolution...).
    pub detail: String,
    /// Audio channels (0 if none / unknown).
    pub channels: usize,
    /// True for still images.
    pub still: bool,
}

/// Background file prober with a cache, shared by the UI and the engine.
pub struct Prober {
    cache: Mutex<HashMap<PathBuf, Option<Result<MediaInfo, String>>>>,
    tx: Mutex<std::sync::mpsc::Sender<PathBuf>>,
    /// Replica mode: the main device pushes media info, nothing is probed locally.
    remote: std::sync::atomic::AtomicBool,
}

fn is_video_path(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" | "mpg" | "mpeg" | "wmv" | "ogv" | "mxf" | "ivf" | "ts" | "m2ts")
    )
}

/// Extension looks like an audio-only file.
pub fn is_audio_path(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("wav" | "wave" | "aif" | "aiff" | "flac" | "mp3" | "ogg" | "oga" | "opus" | "m4a" | "aac" | "caf" | "alac")
    )
}

/// Extension looks like a still image.
pub fn is_image_path(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("png" | "jpg" | "jpeg" | "bmp" | "gif" | "webp" | "tif" | "tiff")
    )
}

/// Files that become video cues (moving images and stills).
pub fn is_video_file(p: &Path) -> bool {
    is_video_path(p) || is_image_path(p)
}

/// Background waveform builder with a cache (decoding a whole file can take a while).
type WaveformResult = Result<Arc<decoder::Waveform>, String>;

/// Where a cache gets results from when this instance is a remote replica: the files live on
/// the main device, so requests go over the link instead of to a local decoder.
pub type RemoteFetch = Box<dyn Fn(PathBuf) + Send + Sync>;

/// Background waveform builder with a cache (decoding a whole file can take a while).
pub struct Waveforms {
    cache: Mutex<HashMap<PathBuf, Option<WaveformResult>>>,
    tx: Mutex<std::sync::mpsc::Sender<PathBuf>>,
    remote: Mutex<Option<RemoteFetch>>,
}

impl Waveforms {
    /// Start the builder thread.
    pub fn new() -> Arc<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
        let w = Arc::new(Self { cache: Mutex::new(HashMap::new()), tx: Mutex::new(tx), remote: Mutex::new(None) });
        let weak = Arc::downgrade(&w);
        std::thread::Builder::new()
            .name("openq-waveform".into())
            .spawn(move || {
                while let Ok(path) = rx.recv() {
                    let res = decoder::waveform(&path).map(Arc::new).map_err(|e| format!("{e:#}"));
                    let Some(w) = weak.upgrade() else { break };
                    w.cache.lock().insert(path, Some(res));
                }
            })
            .expect("spawn waveform thread");
        w
    }

    /// The waveform if ready; queues it otherwise.
    pub fn get(&self, path: &Path) -> Option<WaveformResult> {
        if path.as_os_str().is_empty() {
            return None;
        }
        let mut c = self.cache.lock();
        match c.get(path) {
            Some(r) => r.clone(),
            None => {
                c.insert(path.to_path_buf(), None);
                match &*self.remote.lock() {
                    Some(fetch) => fetch(path.to_path_buf()),
                    None => {
                        let _ = self.tx.lock().send(path.to_path_buf());
                    }
                }
                None
            }
        }
    }

    /// Store a waveform (e.g. one received from a main device).
    pub fn insert(&self, path: PathBuf, result: WaveformResult) {
        self.cache.lock().insert(path, Some(result));
    }

    /// Switch between local decoding (`None`) and fetching from a remote main device. Clears
    /// the cache either way.
    pub fn set_remote(&self, fetch: Option<RemoteFetch>) {
        *self.remote.lock() = fetch;
        self.cache.lock().clear();
    }
}

/// A still preview of a video or image file: RGBA plus its size.
#[derive(Clone)]
pub struct Poster {
    /// Width of `rgba` in pixels.
    pub width: u32,
    /// Height of `rgba` in pixels.
    pub height: u32,
    /// Tightly packed RGBA pixels.
    pub rgba: Arc<Vec<u8>>,
    /// Full size of the source picture (the poster itself may be scaled down).
    pub source_size: [u32; 2],
}

type PosterKey = (PathBuf, u32);
type PosterResult = Result<Poster, String>;

/// Background poster-frame loader with a cache, keyed by file and time (in 10 ms steps).
pub struct Posters {
    cache: Mutex<HashMap<PosterKey, Option<PosterResult>>>,
    tx: Mutex<std::sync::mpsc::Sender<PosterKey>>,
}

impl Posters {
    /// Longest side of a poster in pixels.
    pub const MAX_SIDE: u32 = 640;

    /// Start the loader thread.
    pub fn new() -> Arc<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<PosterKey>();
        let p = Arc::new(Self { cache: Mutex::new(HashMap::new()), tx: Mutex::new(tx) });
        let weak = Arc::downgrade(&p);
        std::thread::Builder::new()
            .name("openq-poster".into())
            .spawn(move || {
                while let Ok(key) = rx.recv() {
                    let res = load_poster(&key.0, key.1 as f64 / 100.0).map_err(|e| format!("{e:#}"));
                    let Some(p) = weak.upgrade() else { break };
                    p.cache.lock().insert(key, Some(res));
                }
            })
            .expect("spawn poster thread");
        p
    }

    /// The poster if ready; queues it otherwise.
    pub fn get(&self, path: &Path, at: f64) -> Option<PosterResult> {
        if path.as_os_str().is_empty() {
            return None;
        }
        let key = (path.to_path_buf(), (at.max(0.0) * 100.0).round() as u32);
        let mut c = self.cache.lock();
        match c.get(&key) {
            Some(r) => r.clone(),
            None => {
                c.insert(key.clone(), None);
                let _ = self.tx.lock().send(key);
                None
            }
        }
    }
}

fn load_poster(path: &Path, at: f64) -> anyhow::Result<Poster> {
    let (width, height, rgba, source_size) = if is_image_path(path) {
        let img = image::open(path)?;
        let size = [img.width(), img.height()];
        let img = img.thumbnail(Posters::MAX_SIDE, Posters::MAX_SIDE).to_rgba8();
        (img.width(), img.height(), img.into_raw(), size)
    } else {
        #[cfg(feature = "video")]
        {
            video::poster(path, at, Posters::MAX_SIDE)?
        }
        #[cfg(not(feature = "video"))]
        {
            let _ = at;
            anyhow::bail!("video support is not built in")
        }
    };
    Ok(Poster { width, height, rgba: Arc::new(rgba), source_size })
}

fn probe_now(path: &Path) -> Result<MediaInfo, String> {
    let audio = || {
        decoder::probe(path)
            .map(|i| MediaInfo {
                duration: i.duration,
                detail: format!("{} Hz, {} ch", i.sample_rate, i.channels),
                channels: i.channels,
                still: false,
            })
            .map_err(|e| format!("{e:#}"))
    };
    if is_audio_path(path) {
        return audio();
    }
    if is_image_path(path) {
        let (w, h) = image::image_dimensions(path).map_err(|e| format!("{e}"))?;
        return Ok(MediaInfo { duration: None, detail: format!("{w}×{h} still"), channels: 0, still: true });
    }
    // Anything else goes to FFmpeg, which knows far more containers.
    #[cfg(feature = "video")]
    {
        video::probe(path)
            .map(|i| MediaInfo {
                duration: i.duration,
                channels: i.audio_channels,
                still: false,
                detail: if i.width > 0 {
                    format!("{} {}×{}{}", i.codec, i.width, i.height, if i.has_audio { " + audio" } else { "" })
                } else {
                    "audio only".into()
                },
            })
            .map_err(|e| format!("{e:#}"))
    }
    #[cfg(not(feature = "video"))]
    audio()
}

impl Prober {
    /// Start the probe thread.
    pub fn new() -> Arc<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
        let p = Arc::new(Self { cache: Mutex::new(HashMap::new()), tx: Mutex::new(tx), remote: Default::default() });
        let weak = Arc::downgrade(&p);
        std::thread::Builder::new()
            .name("openq-probe".into())
            .spawn(move || {
                while let Ok(path) = rx.recv() {
                    let t = std::time::Instant::now();
                    log::debug!("probing {}", path.display());
                    let res = probe_now(&path);
                    log::debug!("probed {} in {:?}: {}", path.display(), t.elapsed(), res.is_ok());
                    let Some(p) = weak.upgrade() else { break };
                    p.cache.lock().insert(path, Some(res));
                }
            })
            .expect("spawn prober");
        p
    }

    /// Cached info, queueing a background probe if unknown. None = not known yet.
    pub fn get(&self, path: &Path) -> Option<Result<MediaInfo, String>> {
        if path.as_os_str().is_empty() {
            return None;
        }
        let mut c = self.cache.lock();
        match c.get(path) {
            Some(r) => r.clone(),
            None => {
                c.insert(path.to_path_buf(), None);
                if !self.remote.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = self.tx.lock().send(path.to_path_buf());
                }
                None
            }
        }
    }

    /// Store media info (e.g. received from a main device).
    pub fn insert(&self, path: PathBuf, result: Result<MediaInfo, String>) {
        self.cache.lock().insert(path, Some(result));
    }

    /// Replica mode on/off; clears the cache.
    pub fn set_remote(&self, remote: bool) {
        self.remote.store(remote, std::sync::atomic::Ordering::Relaxed);
        self.cache.lock().clear();
    }

    /// Like `get` but probes synchronously when unknown (used for audio at start time).
    pub fn get_blocking(&self, path: &Path) -> Result<MediaInfo, String> {
        if let Some(Some(r)) = self.cache.lock().get(path) {
            return r.clone();
        }
        let r = probe_now(path);
        self.cache.lock().insert(path.to_path_buf(), Some(r.clone()));
        r
    }
}

/// Total action time of a media cue given the file's duration. None = infinite/unknown.
pub fn effective_duration(cue: &Cue, file_duration: Option<f64>) -> Option<f64> {
    match &cue.kind {
        CueKind::Audio(a) => {
            if a.infinite_loop {
                return None;
            }
            let rate = if a.rate > 0.0 { a.rate as f64 } else { 1.0 };
            Some(a.file_time_total(file_duration?)? / rate)
        }
        CueKind::Video(v) => {
            if v.infinite_loop {
                return None;
            }
            if is_image_path(&v.path) {
                return (v.still_duration > 0.0).then_some(v.still_duration);
            }
            let file = file_duration?;
            let end = v.end_time.map_or(file, |e| e.min(file));
            Some((end - v.start_time).max(0.0) * v.play_count.max(1) as f64)
        }
        CueKind::Text(t) => (t.duration > 0.0).then_some(t.duration),
        CueKind::Mic(m) => (m.duration > 0.0).then_some(m.duration),
        CueKind::Camera(c) => (c.duration > 0.0).then_some(c.duration),
        _ => None,
    }
}

/// The real media backends behind the engine's [`crate::engine::Backend`] trait.
pub struct Media {
    /// Audio mixer, if the device opened.
    pub audio: Option<audio::AudioOutput>,
    /// Why audio isn't available.
    pub audio_error: Option<String>,
    /// Video layers, if FFmpeg initialised.
    #[cfg(feature = "video")]
    pub video: Option<video::VideoOutput>,
    /// Why video isn't available.
    pub video_error: Option<String>,
    /// Media-info cache.
    pub prober: Arc<Prober>,
    net: Option<std::net::UdpSocket>,
    /// Open inputs of running Mic cues.
    mics: HashMap<CueId, input::InputStream>,
}

impl Media {
    /// Open the default audio device and the video system.
    pub fn new(prober: Arc<Prober>) -> Self {
        let (audio, audio_error) = match audio::AudioOutput::open(None) {
            Ok(a) => (Some(a), None),
            Err(e) => {
                log::error!("audio unavailable: {e:#}");
                (None, Some(format!("{e:#}")))
            }
        };
        #[cfg(feature = "video")]
        let (video, video_error) = match video::VideoOutput::new() {
            Ok(v) => (Some(v), None),
            Err(e) => {
                log::error!("video unavailable: {e:#}");
                (None, Some(format!("{e:#}")))
            }
        };
        #[cfg(not(feature = "video"))]
        let video_error = Some("built without the `video` feature".to_string());
        Self {
            audio,
            audio_error,
            #[cfg(feature = "video")]
            video,
            video_error,
            prober,
            net: None,
            mics: HashMap::new(),
        }
    }

    /// Playback position of a running media cue, as reported by the media itself.
    pub fn elapsed(&self, id: CueId) -> Option<f64> {
        #[cfg(feature = "video")]
        if let Some(v) = &self.video
            && v.has(id)
        {
            return v.elapsed(id);
        }
        if let Some(a) = &self.audio
            && a.has(id)
        {
            return a.elapsed(id);
        }
        None
    }

    /// Layers currently on stages, for the stage windows.
    #[cfg(feature = "video")]
    pub fn stage_layers(&self) -> Vec<video::StageLayer> {
        self.video.as_ref().map(|v| v.stage_layers()).unwrap_or_default()
    }

    /// Release every player and the audio stream.
    pub fn shutdown(&mut self) {
        #[cfg(feature = "video")]
        {
            self.video = None;
        }
        self.audio = None;
    }

    /// Switch audio output device. Running audio cues must be stopped first.
    pub fn reopen_audio(&mut self, device: Option<&str>, master_db: f32) {
        if self.audio.as_ref().is_some_and(|a| a.device_id.as_deref() == device) {
            return;
        }
        self.audio = None; // close the old stream first
        match audio::AudioOutput::open(device) {
            Ok(mut a) => {
                a.set_master(master_db);
                self.audio = Some(a);
                self.audio_error = None;
            }
            Err(e) => {
                log::error!("audio unavailable: {e:#}");
                self.audio_error = Some(format!("{e:#}"));
            }
        }
    }

    /// Source channel count for an audio file (probing it if needed).
    fn audio_channels(&self, path: &Path) -> usize {
        self.prober.get_blocking(path).map(|i| i.channels).unwrap_or(2).max(1)
    }

    /// How a video cue's soundtrack enters the mixer (None: no mixer, or the file has no audio).
    #[cfg(feature = "video")]
    fn video_tap(&self, v: &VideoCue) -> Option<video::AudioTap> {
        let a = self.audio.as_ref()?;
        if is_image_path(&v.path) {
            return None;
        }
        let channels = match self.prober.get(&v.path) {
            Some(Ok(i)) if i.channels == 0 => return None,
            Some(Ok(i)) => i.channels,
            _ => 2,
        };
        Some(video::AudioTap { rate: a.rate, channels: channels.clamp(1, crate::model::MAX_SRC_CHANNELS) })
    }

    /// Push inspector edits to a cue that is playing right now.
    pub fn update_live(&mut self, cue: &Cue) {
        match &cue.kind {
            CueKind::Audio(p) => {
                if let Some(a) = &mut self.audio {
                    a.update_live(cue.id, &audio::MixParams::from(p));
                }
            }
            CueKind::Mic(m) => {
                if let Some(a) = &mut self.audio {
                    a.update_live(cue.id, &audio::MixParams::from(m));
                }
            }
            #[cfg(feature = "video")]
            CueKind::Text(t) => {
                if let Some(v) = &mut self.video {
                    v.update_text(cue.id, t);
                }
            }
            #[cfg(feature = "video")]
            CueKind::Camera(c) => {
                if let Some(v) = &mut self.video {
                    v.update_live(cue.id, &video::camera_layer_params(c));
                }
            }
            #[cfg(feature = "video")]
            CueKind::Video(p) => {
                if let Some(a) = &mut self.audio {
                    a.update_live(cue.id, &audio::MixParams::from(p));
                }
                if let Some(v) = &mut self.video {
                    v.update_live(cue.id, p);
                }
            }
            _ => {}
        }
    }

    /// Input peak of a running Mic cue since the last call (linear).
    pub fn mic_peak(&self, id: CueId) -> Option<f32> {
        self.mics.get(&id).map(|m| m.take_peak())
    }

    /// Open a Mic cue's input and feed it into a new mixer voice.
    fn start_mic(&mut self, id: CueId, m: &crate::model::MicCue, at: Instant) -> Result<(), String> {
        let Some(a) = &mut self.audio else {
            return Err(format!("audio unavailable: {}", self.audio_error.as_deref().unwrap_or("?")));
        };
        let ch = (m.channels as usize).clamp(1, crate::model::MAX_SRC_CHANNELS);
        // Start silent and ramp up when there's a fade-in.
        let mut mix = audio::MixParams::from(m);
        if m.fade_in > 0.0 {
            mix.volume_db = crate::model::MIN_DB;
        }
        let label = format!("input {}", m.device.as_deref().unwrap_or("default"));
        let (tx, shared) = a.add_external(id, &mix, ch, &label, 0.25);
        let out_rate = a.rate;
        // The input rate is only known once the stream opens, so the feeder is built on the first chunk.
        let mut feeder: Option<input::MicFeeder> = None;
        let mut parts = Some((tx, shared.clone()));
        let (first, duration) = (m.first_channel as usize, m.duration);
        let rate_cell = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let rate_in = rate_cell.clone();
        let stream = input::open_input(m.device.as_deref(), move |data, in_ch| {
            if feeder.is_none() {
                let in_rate = rate_in.load(std::sync::atomic::Ordering::Acquire);
                if in_rate == 0 {
                    return;
                }
                let (tx, shared) = parts.take().expect("feeder parts");
                feeder = Some(input::MicFeeder::new(tx, shared, first, ch, in_rate, out_rate, duration));
            }
            if let Some(f) = &mut feeder {
                f.push(data, in_ch);
            }
        });
        match stream {
            Ok(s) => {
                if (s.channels as u16) < m.first_channel + m.channels {
                    log::warn!("{}: has {} input channels; missing ones are silent", s.device_name, s.channels);
                }
                rate_cell.store(s.rate, std::sync::atomic::Ordering::Release);
                if m.fade_in > 0.0 {
                    a.fade(id, m.volume_db, m.fade_in, crate::model::FadeCurve::SCurve, at);
                }
                self.mics.insert(id, s);
                Ok(())
            }
            Err(e) => {
                shared.eof.store(true, std::sync::atomic::Ordering::Release);
                a.stop(id);
                Err(format!("audio input: {e:#}"))
            }
        }
    }

    /// Output peaks since the last call by this `reader`.
    pub fn take_meters(&self, reader: audio::MeterReader) -> Vec<f32> {
        self.audio.as_ref().map(|a| a.take_meters(reader)).unwrap_or_default()
    }
}

/// Parse a Network cue's argument string into OSC values.
pub fn parse_osc_args(s: &str) -> Vec<rosc::OscType> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let tok: String = if c == '"' {
            chars.next();
            let t: String = chars.by_ref().take_while(|c| *c != '"').collect();
            out.push(rosc::OscType::String(t));
            continue;
        } else {
            let mut t = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                t.push(c);
                chars.next();
            }
            t
        };
        out.push(if let Ok(i) = tok.parse::<i32>() {
            rosc::OscType::Int(i)
        } else if let Ok(f) = tok.parse::<f32>() {
            rosc::OscType::Float(f)
        } else {
            rosc::OscType::String(tok)
        });
    }
    out
}

impl Backend for Media {
    fn set_preloaded(&mut self, cues: &[&Cue]) {
        let list: Vec<_> = cues
            .iter()
            .filter(|c| c.armed)
            .filter_map(|c| match &c.kind {
                CueKind::Audio(p) if p.path.exists() => Some((c.id, p, self.audio_channels(&p.path))),
                _ => None,
            })
            .collect();
        if let Some(a) = &mut self.audio {
            a.set_preloaded(&list);
        }
        #[cfg(feature = "video")]
        {
            let list: Vec<_> = cues
                .iter()
                .filter(|c| c.armed)
                .filter_map(|c| match &c.kind {
                    CueKind::Video(p) => Some((c.id, p, self.video_tap(p))),
                    _ => None,
                })
                .collect();
            if let Some(v) = &mut self.video {
                v.set_preloaded(&list);
            }
        }
    }

    fn start(&mut self, cue: &Cue, at: Instant) -> Result<(), String> {
        match &cue.kind {
            CueKind::Audio(p) => match self.audio.is_some() {
                true => {
                    let ch = self.audio_channels(&p.path);
                    self.audio.as_mut().unwrap().start(cue.id, p, ch, at)
                }
                false => Err(format!("audio unavailable: {}", self.audio_error.as_deref().unwrap_or("?"))),
            },
            #[cfg(feature = "video")]
            CueKind::Video(p) => {
                let tap = self.video_tap(p);
                let Some(v) = &mut self.video else {
                    return Err(format!("video unavailable: {}", self.video_error.as_deref().unwrap_or("?")));
                };
                v.start(cue.id, p, at, tap)?;
                if let (Some(tap), Some(a)) = (tap, &mut self.audio) {
                    let label = p.path.display().to_string();
                    let (tx, state) = a.add_external(cue.id, &audio::MixParams::from(p), tap.channels, &label, 1.0);
                    v.attach_audio(cue.id, tx, state);
                }
                Ok(())
            }
            CueKind::Mic(m) => self.start_mic(cue.id, m, at),
            #[cfg(feature = "video")]
            CueKind::Camera(c) => match &mut self.video {
                Some(v) => {
                    v.start_camera(cue.id, c, at);
                    Ok(())
                }
                None => Err(format!("video unavailable: {}", self.video_error.as_deref().unwrap_or("?"))),
            },
            #[cfg(feature = "video")]
            CueKind::Text(t) => match &mut self.video {
                Some(v) => {
                    v.start_text(cue.id, t, at);
                    Ok(())
                }
                None => Err("video output unavailable".into()),
            },
            #[cfg(not(feature = "video"))]
            CueKind::Video(_) | CueKind::Text(_) | CueKind::Camera(_) => Err("built without video support".into()),
            _ => Ok(()),
        }
    }

    fn stop(&mut self, id: CueId) {
        if let Some(a) = &mut self.audio {
            a.stop(id);
        }
        self.mics.remove(&id);
        #[cfg(feature = "video")]
        if let Some(v) = &mut self.video {
            v.stop(id);
        }
    }

    fn pause(&mut self, id: CueId) {
        if let Some(a) = &mut self.audio {
            a.pause(id);
        }
        #[cfg(feature = "video")]
        if let Some(v) = &mut self.video {
            v.pause(id);
        }
    }

    fn resume(&mut self, id: CueId, at: Instant) {
        if let Some(a) = &mut self.audio {
            a.resume(id, at);
        }
        #[cfg(feature = "video")]
        if let Some(v) = &mut self.video {
            v.resume(id, at);
        }
    }

    fn fade(&mut self, id: CueId, f: &FadeRequest, at: Instant) {
        if let Some(a) = &mut self.audio
            && let Some(db) = f.volume_db
        {
            a.fade(id, db, f.duration, f.curve, at);
        }
        #[cfg(feature = "video")]
        if let Some(v) = &mut self.video {
            v.fade(id, f, at);
        }
    }

    fn hold_fade(&mut self, id: CueId) {
        if let Some(a) = &mut self.audio {
            a.hold(id);
        }
        #[cfg(feature = "video")]
        if let Some(v) = &mut self.video {
            v.hold(id);
        }
    }

    fn devamp(&mut self, id: CueId) -> bool {
        self.audio.as_mut().is_some_and(|a| a.devamp(id))
    }

    fn send_network(&mut self, n: &NetworkCue) -> Result<(), String> {
        use std::net::ToSocketAddrs;
        if self.net.is_none() {
            self.net = Some(std::net::UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?);
        }
        let addr = (n.host.as_str(), n.port)
            .to_socket_addrs()
            .map_err(|e| format!("{}: {e}", n.host))?
            .next()
            .ok_or_else(|| format!("cannot resolve {}", n.host))?;
        let msg = rosc::OscPacket::Message(rosc::OscMessage { addr: n.address.clone(), args: parse_osc_args(&n.args) });
        let bytes = rosc::encoder::encode(&msg).map_err(|e| e.to_string())?;
        self.net.as_ref().unwrap().send_to(&bytes, addr).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn is_finished(&self, id: CueId) -> bool {
        let audio_done = self.audio.as_ref().is_none_or(|a| a.is_finished(id));
        #[cfg(feature = "video")]
        let video_done = self.video.as_ref().is_none_or(|v| v.is_finished(id));
        #[cfg(not(feature = "video"))]
        let video_done = true;
        audio_done && video_done
    }

    fn duration(&self, cue: &Cue) -> Option<f64> {
        let file = match &cue.kind {
            CueKind::Audio(a) => self.prober.get_blocking(&a.path).ok()?.duration,
            CueKind::Video(v) if is_image_path(&v.path) => None,
            CueKind::Text(t) => return (t.duration > 0.0).then_some(t.duration),
            CueKind::Mic(m) => return (m.duration > 0.0).then_some(m.duration),
            CueKind::Camera(c) => return (c.duration > 0.0).then_some(c.duration),
            CueKind::Video(v) => self.prober.get(&v.path)?.ok()?.duration,
            _ => return None,
        };
        effective_duration(cue, file)
    }

    fn drain_errors(&mut self) -> Vec<String> {
        #[cfg_attr(not(feature = "video"), allow(unused_mut))]
        let mut out = self.audio.as_mut().map(|a| a.drain_errors()).unwrap_or_default();
        #[cfg(feature = "video")]
        if let Some(v) = &mut self.video {
            out.extend(v.drain_errors());
        }
        out
    }

    fn update(&mut self, now: Instant) {
        if let Some(a) = &mut self.audio {
            a.update();
            // Close the inputs of Mic cues whose voice has ended.
            self.mics.retain(|id, _| !a.is_finished(*id));
        }
        #[cfg(feature = "video")]
        if let Some(v) = &mut self.video {
            v.update(now);
        }
        let _ = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rosc::OscType;

    #[test]
    fn osc_args_parse_numbers_and_quoted_strings() {
        let a = parse_osc_args(r#"1 2.5 hello "two words" -3"#);
        assert_eq!(
            a,
            vec![
                OscType::Int(1),
                OscType::Float(2.5),
                OscType::String("hello".into()),
                OscType::String("two words".into()),
                OscType::Int(-3)
            ]
        );
    }
}
