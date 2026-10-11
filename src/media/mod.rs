//! Media backend: routes engine requests to the audio mixer and the video player.

pub mod audio;
pub mod decoder;
#[cfg(feature = "video")]
pub mod video;

use crate::engine::{Backend, FadeRequest};
use crate::model::{Cue, CueId, CueKind};
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

pub fn is_video_file(p: &Path) -> bool {
    is_video_path(p)
}

fn probe_now(path: &Path) -> Result<MediaInfo, String> {
    let audio = || {
        decoder::probe(path)
            .map(|i| MediaInfo { duration: i.duration, detail: format!("{} Hz, {} ch", i.sample_rate, i.channels) })
            .map_err(|e| format!("{e:#}"))
    };
    if is_audio_path(path) {
        return audio();
    }
    // Anything else goes to GStreamer, which knows far more containers.
    #[cfg(feature = "video")]
    {
        video::probe(path)
            .map(|i| MediaInfo {
                duration: i.duration,
                detail: if i.width > 0 {
                    format!("{}×{}{}", i.width, i.height, if i.has_audio { " + audio" } else { "" })
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
                    let res = probe_now(&path);
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
            let file = file_duration?;
            let end = a.end_time.map_or(file, |e| e.min(file));
            Some((end - a.start_time).max(0.0) * a.play_count.max(1) as f64)
        }
        CueKind::Video(v) => {
            if v.infinite_loop {
                return None;
            }
            let file = file_duration?;
            Some((file - v.start_time).max(0.0) * v.play_count.max(1) as f64)
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
}

impl Media {
    pub fn new(prober: Arc<Prober>) -> Self {
        let (audio, audio_error) = match audio::AudioOutput::open() {
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
        }
    }

    /// Playback position of a running media cue, as reported by the media itself.
    pub fn elapsed(&self, id: CueId) -> Option<f64> {
        if let Some(a) = &self.audio
            && a.has(id)
        {
            return a.elapsed(id);
        }
        #[cfg(feature = "video")]
        if let Some(v) = &self.video
            && v.has(id)
        {
            return v.elapsed(id);
        }
        None
    }

    #[cfg(feature = "video")]
    pub fn stage_layers(&self) -> Vec<video::StageLayer> {
        self.video.as_ref().map(|v| v.stage_layers()).unwrap_or_default()
    }

    pub fn take_meters(&self) -> Vec<f32> {
        self.audio.as_ref().map(|a| a.take_meters()).unwrap_or_default()
    }
}

impl Backend for Media {
    fn set_preloaded(&mut self, cues: &[&Cue]) {
        if let Some(a) = &mut self.audio {
            let list: Vec<_> = cues
                .iter()
                .filter(|c| c.armed)
                .filter_map(|c| match &c.kind {
                    CueKind::Audio(p) => Some((c.id, p)),
                    _ => None,
                })
                .collect();
            a.set_preloaded(&list);
        }
        #[cfg(feature = "video")]
        if let Some(v) = &mut self.video {
            let list: Vec<_> = cues
                .iter()
                .filter(|c| c.armed)
                .filter_map(|c| match &c.kind {
                    CueKind::Video(p) => Some((c.id, p)),
                    _ => None,
                })
                .collect();
            v.set_preloaded(&list);
        }
    }

    fn start(&mut self, cue: &Cue, at: Instant) -> Result<(), String> {
        match &cue.kind {
            CueKind::Audio(p) => match &mut self.audio {
                Some(a) => a.start(cue.id, p, at),
                None => Err(format!("audio unavailable: {}", self.audio_error.as_deref().unwrap_or("?"))),
            },
            #[cfg(feature = "video")]
            CueKind::Video(p) => match &mut self.video {
                Some(v) => v.start(cue.id, p, at),
                None => Err(format!("video unavailable: {}", self.video_error.as_deref().unwrap_or("?"))),
            },
            #[cfg(not(feature = "video"))]
            CueKind::Video(_) => Err("built without video support".into()),
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
            v.fade(id, f.volume_db, f.opacity, f.duration, f.curve, at);
        }
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
            CueKind::Video(v) => self.prober.get(&v.path)?.ok()?.duration,
            _ => return None,
        };
        effective_duration(cue, file)
    }

    fn drain_errors(&mut self) -> Vec<String> {
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
