//! Media backend: routes engine requests to the audio mixer and the video player.

pub mod audio;
pub mod decoder;
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

#[derive(Clone, Debug, Default)]
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
}

fn is_video_path(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" | "mpg" | "mpeg" | "wmv" | "ogv" | "mxf" | "ivf" | "ts" | "m2ts")
    )
}

pub fn is_audio_path(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("wav" | "wave" | "aif" | "aiff" | "flac" | "mp3" | "ogg" | "oga" | "opus" | "m4a" | "aac" | "caf" | "alac")
    )
}

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

pub struct Waveforms {
    cache: Mutex<HashMap<PathBuf, Option<WaveformResult>>>,
    tx: Mutex<std::sync::mpsc::Sender<PathBuf>>,
}

impl Waveforms {
    pub fn new() -> Arc<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
        let w = Arc::new(Self { cache: Mutex::new(HashMap::new()), tx: Mutex::new(tx) });
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
                let _ = self.tx.lock().send(path.to_path_buf());
                None
            }
        }
    }
}

/// A still preview of a video or image file: RGBA plus its size.
#[derive(Clone)]
pub struct Poster {
    pub width: u32,
    pub height: u32,
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
    pub const MAX_SIDE: u32 = 640;

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
    pub fn new() -> Arc<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
        let p = Arc::new(Self { cache: Mutex::new(HashMap::new()), tx: Mutex::new(tx) });
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
                let _ = self.tx.lock().send(path.to_path_buf());
                None
            }
        }
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
        _ => None,
    }
}

pub struct Media {
    pub audio: Option<audio::AudioOutput>,
    pub audio_error: Option<String>,
    #[cfg(feature = "video")]
    pub video: Option<video::VideoOutput>,
    pub video_error: Option<String>,
    pub prober: Arc<Prober>,
    net: Option<std::net::UdpSocket>,
}

impl Media {
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
            #[cfg(feature = "video")]
            CueKind::Text(t) => {
                if let Some(v) = &mut self.video {
                    v.update_text(cue.id, t);
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

    pub fn take_meters(&self) -> Vec<f32> {
        self.audio.as_ref().map(|a| a.take_meters()).unwrap_or_default()
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
                    let (tx, state) = a.add_external(cue.id, &audio::MixParams::from(p), tap.channels, &label);
                    v.attach_audio(cue.id, tx, state);
                }
                Ok(())
            }
            #[cfg(feature = "video")]
            CueKind::Text(t) => match &mut self.video {
                Some(v) => {
                    v.start_text(cue.id, t, at);
                    Ok(())
                }
                None => Err("video output unavailable".into()),
            },
            #[cfg(not(feature = "video"))]
            CueKind::Video(_) | CueKind::Text(_) => Err("built without video support".into()),
            _ => Ok(()),
        }
    }

    fn stop(&mut self, id: CueId) {
        if let Some(a) = &mut self.audio {
            a.stop(id);
        }
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
