//! Media backend: routes engine requests to the audio mixer and the video player.

pub mod audio;
pub mod decoder;
pub mod input;
pub mod pool;
pub mod still;
pub mod tcio;
pub mod text;
#[cfg(feature = "video")]
pub mod video;

use crate::engine::{Backend, FadeRequest};
#[cfg(feature = "video")]
use crate::model::VideoCue;
use crate::model::{Cue, CueId, CueKind, NetworkCue};
use anyhow::Context;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// Largest texture side the GPU takes (set by the UI from its renderer; 8192 until then).
static MAX_TEXTURE_SIDE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(8192);

/// Tell the media code the renderer's largest texture side, so pictures are made to fit it.
pub fn set_max_texture_side(px: usize) {
    MAX_TEXTURE_SIDE.store(px.clamp(256, 1 << 15) as u32, std::sync::atomic::Ordering::Relaxed);
}

/// True for a Windows network path (`\\server\share\...`, `//server/share`, `\\?\UNC\...`,
/// device paths other than drive letters). Opening one, even just to check that it exists, makes
/// Windows sign in to that server with the user's credentials, which hands a password hash to
/// whoever runs it; a show file or a replica could name any server. Always false elsewhere (other
/// systems only reach shares the user mounted).
pub fn is_network_path(path: &Path) -> bool {
    cfg!(windows) && windows_network_path(&path.to_string_lossy())
}

/// [`is_network_path`] for a path written the Windows way (on any system, for tests).
fn windows_network_path(s: &str) -> bool {
    let s = s.replace('/', "\\");
    // `\\?\C:\...`, `\\.\C:\...` and `\??\C:\...` are local; any other device or verbatim
    // path (`\\?\UNC\`, `\\?\GLOBALROOT\...`) may reach the network.
    for prefix in ["\\\\?\\", "\\\\.\\", "\\??\\"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            let b = rest.as_bytes();
            return !(b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':');
        }
    }
    s.starts_with("\\\\")
}

/// Refuse a [network path](is_network_path) and anything that isn't a regular file: media are
/// opened from local disks (or shares mapped to a drive letter) only, and a show file can't point
/// a reader at a FIFO (opening it blocks until someone writes), a device such as `/dev/zero`
/// (read forever) or a folder. Symbolic links to regular files are fine. Worker threads only:
/// this looks at the disk.
pub fn check_local(path: &Path) -> anyhow::Result<()> {
    if is_network_path(path) {
        anyhow::bail!(
            "{} is on a network share: OpenQ doesn't open \\\\server paths (Windows would send your login to that server). Map the share to a drive letter.",
            path.display()
        );
    }
    let meta = std::fs::metadata(path).with_context(|| format!("cannot open {}", path.display()))?;
    // Unit tests stand in a FIFO named `hung-drive*` for a network drive that stops answering.
    let stand_in = cfg!(test) && path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("hung-drive"));
    if !meta.is_file() && !stand_in {
        anyhow::bail!("{} is not a file", path.display());
    }
    Ok(())
}

/// Largest texture side the renderer accepts, in pixels.
pub fn max_texture_side() -> u32 {
    MAX_TEXTURE_SIDE.load(std::sync::atomic::Ordering::Relaxed)
}

/// What the prober learned about a media file.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct MediaInfo {
    /// File duration in seconds.
    pub duration: Option<f64>,
    /// Human-readable summary (format, channels, resolution...). Shared, so the per-frame
    /// lookups of the cue list copy no text.
    pub detail: Arc<str>,
    /// Audio channels (0 if none / unknown).
    pub channels: usize,
    /// True for still images.
    pub still: bool,
}

/// Background file prober with a cache, shared by the UI and the engine.
pub struct Prober {
    cache: Mutex<HashMap<PathBuf, Option<Result<MediaInfo, String>>>>,
    probes: pool::Pool<PathBuf>,
    /// Replica mode: the main device pushes media info, nothing is probed locally.
    remote: std::sync::atomic::AtomicBool,
    /// File sizes (`Some(None)`: missing, None: first check pending) and when they were
    /// checked; see [`Prober::stat`].
    stats: Mutex<HashMap<PathBuf, StatEntry>>,
    stat_jobs: pool::Pool<PathBuf>,
}

/// A cached [`Prober::stat`] answer and when it was (last asked to be) checked.
type StatEntry = (Option<Option<u64>>, std::time::Instant);

/// How long a [`Prober::stat`] answer is trusted before it is checked again.
pub const STAT_TTL: std::time::Duration = std::time::Duration::from_secs(2);

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
        Some("png" | "jpg" | "jpeg" | "bmp" | "gif" | "webp" | "tif" | "tiff" | "svg" | "svgz")
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

/// Waveform cache: results (None while being built) and when each was last asked for.
#[derive(Default)]
struct WaveformCache {
    map: HashMap<PathBuf, (Option<WaveformResult>, u64)>,
    clock: u64,
}

impl WaveformCache {
    /// Keep a result, then drop the least recently used ones over [`Waveforms::MAX_BYTES`].
    fn insert(&mut self, path: PathBuf, result: WaveformResult) {
        self.clock += 1;
        self.map.insert(path, (Some(result), self.clock));
        let size = |r: &Option<WaveformResult>| match r {
            Some(Ok(w)) => w.peaks.len() * 16, // the peaks plus their coarser levels
            _ => 0,
        };
        let mut total: usize = self.map.values().map(|(r, _)| size(r)).sum();
        while total > Waveforms::MAX_BYTES {
            let Some(old) = self.map.iter().filter(|(_, (r, _))| size(r) > 0).min_by_key(|(_, (_, used))| *used).map(|(p, _)| p.clone())
            else {
                break;
            };
            if self.map.len() == 1 {
                break; // the one just made, however big
            }
            total -= self.map.remove(&old).map_or(0, |(r, _)| size(&r));
        }
    }
}

/// Background waveform builder with a cache (decoding a whole file can take a while).
pub struct Waveforms {
    cache: Mutex<WaveformCache>,
    jobs: pool::Pool<PathBuf>,
    remote: Mutex<Option<RemoteFetch>>,
}

impl Waveforms {
    /// Set up the builders (threads start on first use).
    pub fn new() -> Arc<Self> {
        Arc::new_cyclic(|weak: &std::sync::Weak<Self>| {
            let weak = weak.clone();
            let jobs = pool::Pool::new("openq-waveform", 2, move |path: PathBuf| {
                let res = crate::recovery::catch(|| decoder::waveform(&path)).map(Arc::new).map_err(|e| format!("{e:#}"));
                if let Some(w) = weak.upgrade() {
                    w.cache.lock().insert(path, res);
                }
            });
            Self { cache: Mutex::new(WaveformCache::default()), jobs, remote: Mutex::new(None) }
        })
    }

    /// The waveform if ready; queues it otherwise.
    pub fn get(&self, path: &Path) -> Option<WaveformResult> {
        if path.as_os_str().is_empty() {
            return None;
        }
        let mut c = self.cache.lock();
        c.clock += 1;
        let now = c.clock;
        match c.map.get_mut(path) {
            Some((r, used)) => {
                *used = now;
                r.clone()
            }
            None => {
                c.map.insert(path.to_path_buf(), (None, now));
                match &*self.remote.lock() {
                    Some(fetch) => fetch(path.to_path_buf()),
                    None => self.jobs.push(path.to_path_buf()),
                }
                None
            }
        }
    }

    /// Store a waveform (e.g. one received from a main device).
    pub fn insert(&self, path: PathBuf, result: WaveformResult) {
        self.cache.lock().insert(path, result);
    }

    /// Memory kept for waveforms (~0.8 KB per second of audio, twice that with the levels):
    /// about 2 hours of audio files; older views are rebuilt when needed again.
    pub const MAX_BYTES: usize = 64 << 20;

    /// Paths with a cached result (tests).
    pub fn cached(&self) -> usize {
        self.cache.lock().map.values().filter(|(r, _)| r.is_some()).count()
    }

    /// Switch between local decoding (`None`) and fetching from a remote main device. Clears
    /// the cache either way.
    pub fn set_remote(&self, fetch: Option<RemoteFetch>) {
        *self.remote.lock() = fetch;
        self.cache.lock().map.clear();
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

/// Poster cache contents: entries plus a use counter for least-recently-used eviction.
#[derive(Default)]
struct PosterCache {
    map: HashMap<PosterKey, (Option<PosterResult>, u64)>,
    clock: u64,
    bytes: usize,
}

impl PosterCache {
    fn size(r: &Option<PosterResult>) -> usize {
        match r {
            Some(Ok(p)) => p.rgba.len(),
            _ => 0,
        }
    }

    fn get(&mut self, key: &PosterKey) -> Option<&Option<PosterResult>> {
        self.clock += 1;
        let c = self.clock;
        self.map.get_mut(key).map(|e| {
            e.1 = c;
            &e.0
        })
    }

    fn insert(&mut self, key: PosterKey, r: Option<PosterResult>) {
        self.clock += 1;
        self.bytes += Self::size(&r);
        if let Some(old) = self.map.insert(key, (r, self.clock)) {
            self.bytes -= Self::size(&old.0);
        }
        // Scrubbing through a long movie asks for a new frame every 10 ms of media: keep
        // only the most recently used ones.
        while (self.bytes > Posters::MAX_BYTES || self.map.len() > Posters::MAX_ENTRIES) && self.map.len() > 1 {
            let Some(oldest) = self.map.iter().filter(|(_, v)| v.0.is_some()).min_by_key(|(_, v)| v.1).map(|(k, _)| k.clone()) else {
                break;
            };
            if let Some(old) = self.map.remove(&oldest) {
                self.bytes -= Self::size(&old.0);
            }
        }
    }
}

/// Background poster-frame loader with a cache, keyed by file and time (in 10 ms steps).
/// The cache is bounded ([`Posters::MAX_BYTES`]); the least recently used posters go first.
pub struct Posters {
    cache: Mutex<PosterCache>,
    jobs: pool::Pool<PosterKey>,
}

impl Posters {
    /// Longest side of a poster in pixels.
    pub const MAX_SIDE: u32 = 640;
    /// Most pixel memory the cache keeps.
    pub const MAX_BYTES: usize = 128 << 20;
    /// Most posters (and pending requests) the cache keeps.
    pub const MAX_ENTRIES: usize = 2048;

    /// Pixel memory held by cached posters.
    pub fn cached_bytes(&self) -> usize {
        self.cache.lock().bytes
    }

    /// Set up the loaders (threads start on first use).
    pub fn new() -> Arc<Self> {
        Arc::new_cyclic(|weak: &std::sync::Weak<Self>| {
            let weak = weak.clone();
            let jobs = pool::Pool::new("openq-poster", 2, move |key: PosterKey| {
                let res = crate::recovery::catch(|| load_poster(&key.0, key.1 as f64 / 100.0)).map_err(|e| format!("{e:#}"));
                if let Some(p) = weak.upgrade() {
                    p.cache.lock().insert(key, Some(res));
                }
            });
            Self { cache: Mutex::new(PosterCache::default()), jobs }
        })
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
                self.jobs.push(key);
                None
            }
        }
    }
}

fn load_poster(path: &Path, at: f64) -> anyhow::Result<Poster> {
    let (width, height, rgba, source_size) = if is_image_path(path) {
        let s = still::thumbnail(path, Posters::MAX_SIDE)?;
        (s.width, s.height, s.rgba, s.source_size)
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
    check_local(path).map_err(|e| format!("{e:#}"))?;
    let audio = || {
        decoder::probe(path)
            .map(|i| MediaInfo {
                duration: i.duration,
                detail: format!("{} Hz, {} ch", i.sample_rate, i.channels).into(),
                channels: i.channels,
                still: false,
            })
            .map_err(|e| format!("{e:#}"))
    };
    if is_audio_path(path) {
        return audio();
    }
    if is_image_path(path) {
        let (w, h) = still::dimensions(path).map_err(|e| format!("{e:#}"))?;
        let kind = if still::is_svg(path) { "SVG" } else { "still" };
        return Ok(MediaInfo { duration: None, detail: format!("{w}×{h} {kind}").into(), channels: 0, still: true });
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
                    format!("{} {}×{}{}", i.codec, i.width, i.height, if i.has_audio { " + audio" } else { "" }).into()
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
    /// Start the probe and stat workers (threads start on first use).
    pub fn new() -> Arc<Self> {
        Arc::new_cyclic(|weak: &std::sync::Weak<Self>| {
            let w = weak.clone();
            let stat_jobs = pool::Pool::new("openq-stat", 8, move |path: PathBuf| {
                // A network path isn't even looked at (see `is_network_path`): it reads as missing.
                let size = if is_network_path(&path) { None } else { std::fs::metadata(&path).ok().filter(|m| m.is_file()).map(|m| m.len()) };
                if let Some(p) = w.upgrade() {
                    p.stats.lock().insert(path, (Some(size), std::time::Instant::now()));
                }
            });
            let w = weak.clone();
            let probes = pool::Pool::new("openq-probe", 4, move |path: PathBuf| {
                let t = std::time::Instant::now();
                log::debug!("probing {}", path.display());
                let res = crate::recovery::catch(|| {
                    crate::recovery::fault_point(crate::recovery::Fault::Prober);
                    probe_now(&path).map_err(anyhow::Error::msg)
                })
                .map_err(|e| format!("{e:#}"));
                log::debug!("probed {} in {:?}: {}", path.display(), t.elapsed(), res.is_ok());
                if let Some(p) = w.upgrade() {
                    p.cache.lock().insert(path, Some(res));
                }
            });
            Self { cache: Mutex::new(HashMap::new()), probes, remote: Default::default(), stats: Mutex::new(HashMap::new()), stat_jobs }
        })
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
                    self.probes.push(path.to_path_buf());
                }
                None
            }
        }
    }

    /// A file's size, or `Some(None)` if it doesn't exist; None until first checked. Never
    /// touches the disk: the UI asks for every visible cue each frame while holding the show
    /// lock, and a stat on a network share or a sleeping drive can take seconds. Checks run on
    /// a background thread and are repeated after [`STAT_TTL`], so files that come and go
    /// (a drive plugged back in) are noticed.
    pub fn stat(&self, path: &Path) -> Option<Option<u64>> {
        if path.as_os_str().is_empty() {
            return Some(None);
        }
        let mut c = self.stats.lock();
        let now = std::time::Instant::now();
        match c.get_mut(path) {
            Some((size, checked)) => {
                if now.duration_since(*checked) > STAT_TTL {
                    // Mark it fresh so it's queued once, not every frame until the answer.
                    *checked = now;
                    self.stat_jobs.push(path.to_path_buf());
                }
                *size
            }
            None => {
                // A pending entry, so the first check is queued once.
                c.insert(path.to_path_buf(), (None, now));
                self.stat_jobs.push(path.to_path_buf());
                None
            }
        }
    }

    /// Whether a file exists, as far as [`Prober::stat`] knows (unknown counts as present, so
    /// nothing flashes red before the first check).
    pub fn exists(&self, path: &Path) -> bool {
        !matches!(self.stat(path), Some(None))
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
        let r = crate::recovery::catch(|| probe_now(path).map_err(anyhow::Error::msg)).map_err(|e| format!("{e:#}"));
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
    /// Video layers (shown by the video output process), if FFmpeg initialised.
    #[cfg(feature = "video")]
    pub video: Option<crate::vout::link::VideoLink>,
    /// Why video isn't available.
    pub video_error: Option<String>,
    /// Media-info cache.
    pub prober: Arc<Prober>,
    net: Option<std::net::UdpSocket>,
    /// Open inputs of running Mic cues.
    mics: HashMap<CueId, MicInput>,
    /// Set while a lost audio output is being reopened.
    reconnect: Option<Reconnect>,
    /// Messages for the engine log.
    notes: Vec<String>,
    /// Preloading skipped files still being probed: offer the preload set again at this time.
    preload_retry: Option<Instant>,
    /// Audio cues started before their file was probed: they start once the background probe
    /// answers (the engine thread never probes).
    pending_audio: HashMap<CueId, PendingAudio>,
    /// Object-audio speakers from the workspace (empty = the device's default layout); applied
    /// to every audio output this opens.
    room: Vec<crate::spatial::Speaker>,
    /// The light thread (lighting playback and DMX outputs).
    pub light: crate::light::engine::LightEngine,
    /// The output whose meters the light thread reads ([`audio::MeterTap::id`]).
    light_audio: Option<usize>,
}

/// A Mic cue's live input. Opening a device can take a while, so it opens on its own thread;
/// a device that goes away is reopened every [`AUDIO_RETRY`] while the cue runs.
struct MicInput {
    device: Option<String>,
    sink: Arc<Mutex<input::MicSink>>,
    stream: Option<input::InputStream>,
    opening: Option<std::sync::mpsc::Receiver<anyhow::Result<input::InputStream>>>,
    next_try: Instant,
    /// The device was lost (so its return is announced).
    lost: bool,
    /// Its fade-in, applied when the input first opens.
    fade_in: Option<(f32, f64, Instant)>,
    first: u16,
    channels: u16,
}

impl MicInput {
    /// Open the input in the background.
    fn open(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        let (device, sink) = (self.device.clone(), self.sink.clone());
        let spawned = std::thread::Builder::new().name("openq-mic-open".into()).spawn(move || {
            let r = input::open_input(device.as_deref(), move |data, in_ch| {
                // A reopen swaps the rate under the lock; skipping one chunk then is fine.
                if let Some(mut s) = sink.try_lock() {
                    s.push(data, in_ch);
                }
            });
            let _ = tx.send(r);
        });
        self.opening = spawned.is_ok().then_some(rx);
        self.next_try = Instant::now() + AUDIO_RETRY;
    }
}

/// An audio start waiting for its file's channel count.
struct PendingAudio {
    params: crate::model::AudioCue,
    at: Instant,
    paused: bool,
}

/// How often to try reopening a lost audio device.
pub const AUDIO_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// Reopening a lost audio output in the background (opening a device can take a while, and the
/// engine thread must keep ticking meanwhile).
struct Reconnect {
    endpoint: audio::Endpoint,
    master_db: f32,
    next_try: Instant,
    opening: Option<std::sync::mpsc::Receiver<anyhow::Result<audio::AudioOutput>>>,
    tries: u32,
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
        let (video, video_error) = match crate::vout::link::VideoLink::new() {
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
            light_audio: None,
            #[cfg(feature = "video")]
            video,
            video_error,
            prober,
            net: None,
            mics: HashMap::new(),
            reconnect: None,
            notes: Vec::new(),
            preload_retry: None,
            pending_audio: HashMap::new(),
            room: Vec::new(),
            light: crate::light::engine::LightEngine::start(),
        }
    }

    /// Set the object-audio room (the workspace's speakers).
    pub fn set_room(&mut self, speakers: &[crate::spatial::Speaker]) {
        if self.room != speakers {
            self.room = speakers.to_vec();
        }
        if let Some(a) = &mut self.audio {
            a.set_room(&self.room);
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
        if self.audio.as_ref().is_some_and(|a| a.device_id.as_deref() == device && !a.is_lost()) {
            return;
        }
        self.reconnect = None;
        self.audio = None; // close the old stream first
        match audio::AudioOutput::open(device) {
            Ok(mut a) => {
                a.set_master(master_db);
                a.set_room(&self.room);
                self.audio = Some(a);
                self.audio_error = None;
            }
            Err(e) => {
                log::error!("audio unavailable: {e:#}");
                self.audio_error = Some(format!("{e:#}"));
            }
        }
    }

    /// Start audio cues whose files have been probed since their GO (late by the probe time).
    fn start_pending_audio(&mut self, now: Instant) {
        if self.pending_audio.is_empty() {
            return;
        }
        let ready: Vec<(CueId, Result<usize, String>)> = self
            .pending_audio
            .iter()
            .filter(|(_, p)| !p.paused)
            .filter_map(|(id, p)| Some((*id, self.prober.get(&p.params.path)?.map(|i| i.channels.max(1)))))
            .collect();
        for (id, r) in ready {
            let p = self.pending_audio.remove(&id).expect("pending");
            let r = r.and_then(|ch| match &mut self.audio {
                Some(a) => a.start(id, &p.params, ch, p.at.max(now)),
                None => Err("audio unavailable".into()),
            });
            if let Err(e) = r {
                self.notes.push(format!("{}: {e}", p.params.path.display()));
            }
        }
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

    /// True while a lost audio device is being reconnected.
    pub fn audio_reconnecting(&self) -> bool {
        self.reconnect.is_some()
    }

    /// Notice a lost audio output and keep trying to reopen it, every [`AUDIO_RETRY`].
    fn watch_audio(&mut self, now: Instant) {
        if self.audio.as_ref().is_some_and(|a| a.is_lost()) {
            let a = self.audio.take().expect("checked");
            self.notes.push(format!("Audio output lost ({}); its sounds stopped. Reconnecting…", a.device_name));
            self.audio_error = Some("audio device lost; reconnecting".into());
            self.mics.clear();
            self.reconnect =
                Some(Reconnect { endpoint: a.endpoint.clone(), master_db: a.master_db, next_try: now, opening: None, tries: 0 });
        }
        let Some(r) = &mut self.reconnect else { return };
        match &r.opening {
            Some(rx) => match rx.try_recv() {
                Ok(Ok(mut a)) => {
                    a.set_master(r.master_db);
                    a.set_room(&self.room);
                    self.notes.push(format!("Audio output back: {}", a.device_name));
                    self.audio = Some(a);
                    self.audio_error = None;
                    self.reconnect = None;
                }
                Ok(Err(e)) => {
                    if r.tries == 0 {
                        log::warn!("audio reconnect: {e:#}");
                    }
                    r.tries += 1;
                    r.opening = None;
                    r.next_try = now + AUDIO_RETRY;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    r.opening = None;
                    r.next_try = now + AUDIO_RETRY;
                }
            },
            None if now >= r.next_try => {
                let (tx, rx) = std::sync::mpsc::channel();
                let ep = r.endpoint.clone();
                let spawned = std::thread::Builder::new()
                    .name("openq-audio-reopen".into())
                    .spawn(move || {
                        let _ = tx.send(crate::recovery::catch(|| audio::AudioOutput::open_endpoint(&ep, None)));
                    });
                match spawned {
                    Ok(_) => r.opening = Some(rx),
                    Err(_) => r.next_try = now + AUDIO_RETRY,
                }
            }
            None => {}
        }
    }

    /// Silence everything at once: every voice, layer and live input (last-resort recovery).
    pub fn stop_everything(&mut self) {
        self.pending_audio.clear();
        if let Some(a) = &mut self.audio {
            a.stop_all();
        }
        self.mics.clear();
        #[cfg(feature = "video")]
        if let Some(v) = &mut self.video {
            v.stop_all();
        }
    }

    /// Push inspector edits to a cue that is playing right now.
    pub fn update_live(&mut self, cue: &Cue) {
        match &cue.kind {
            CueKind::Audio(p) => {
                if let Some(pending) = self.pending_audio.get_mut(&cue.id) {
                    pending.params = p.clone();
                }
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
                    v.update_camera(cue.id, c);
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
        self.mics.get(&id).and_then(|m| m.stream.as_ref()).map(|s| s.take_peak())
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
        let sink = input::MicSink::new(tx, shared, m.first_channel as usize, ch, a.rate, m.duration);
        let mut mic = MicInput {
            device: m.device.clone(),
            sink: Arc::new(Mutex::new(sink)),
            stream: None,
            opening: None,
            next_try: at,
            lost: false,
            fade_in: (m.fade_in > 0.0).then_some((m.volume_db, m.fade_in, at)),
            first: m.first_channel,
            channels: m.channels,
        };
        // Never wait for the device on the engine thread: it opens in the background and the
        // voice plays silence until then (errors reach the log through `notes`).
        mic.open();
        self.mics.insert(id, mic);
        Ok(())
    }

    /// Mic inputs: take streams that finished opening, reopen lost ones.
    fn watch_mics(&mut self, now: Instant) {
        let Some(a) = &mut self.audio else { return };
        let mut failed = Vec::new();
        for (id, m) in self.mics.iter_mut() {
            if let Some(rx) = &m.opening {
                match rx.try_recv() {
                    Ok(Ok(s)) => {
                        m.opening = None;
                        if (s.channels as u16) < m.first + m.channels {
                            log::warn!("{}: has {} input channels; missing ones are silent", s.device_name, s.channels);
                        }
                        m.sink.lock().set_input_rate(s.rate);
                        if let Some((db, secs, _)) = m.fade_in.take() {
                            a.fade(*id, db, secs, crate::model::FadeCurve::SCurve, now);
                        }
                        if m.lost {
                            m.lost = false;
                            self.notes.push(format!("Audio input back: {}", s.device_name));
                        }
                        m.stream = Some(s);
                    }
                    Ok(Err(e)) => {
                        m.opening = None;
                        if m.lost {
                            log::debug!("audio input still away: {e:#}");
                        } else {
                            // The first open failed: the cue can't run.
                            failed.push((*id, format!("audio input: {e:#}")));
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => m.opening = None,
                }
            }
            if m.stream.as_ref().is_some_and(|s| s.is_lost()) {
                let name = m.stream.take().map(|s| s.device_name.clone()).unwrap_or_default();
                m.lost = true;
                m.next_try = now + AUDIO_RETRY;
                self.notes.push(format!("Audio input lost ({name}); reconnecting…"));
            }
            if m.lost && m.stream.is_none() && m.opening.is_none() && now >= m.next_try {
                m.open();
            }
        }
        for (id, e) in failed {
            self.mics.remove(&id);
            a.stop(id);
            self.notes.push(e);
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
        // This runs on the engine thread with the show locked: never probe a file here (that can
        // take long, or hang on a stalled network share). Files still being probed in the
        // background are preloaded on a later retry.
        let mut waiting = false;
        let list: Vec<_> = cues
            .iter()
            .filter(|c| c.armed)
            .filter_map(|c| match &c.kind {
                CueKind::Audio(p) if !p.path.as_os_str().is_empty() => match self.prober.get(&p.path) {
                    Some(Ok(i)) => Some((c.id, p, i.channels.max(1))),
                    Some(Err(_)) => None,
                    None => {
                        waiting = true;
                        None
                    }
                },
                _ => None,
            })
            .collect();
        self.preload_retry = waiting.then(|| Instant::now() + std::time::Duration::from_millis(50));
        if let Some(a) = &mut self.audio {
            a.set_preloaded(&list);
        }
        #[cfg(feature = "video")]
        {
            let list: Vec<_> = cues
                .iter()
                .filter(|c| c.armed)
                .filter_map(|c| match &c.kind {
                    CueKind::Video(p) if self.prober.exists(&p.path) => Some((c.id, p, self.video_tap(p))),
                    _ => None,
                })
                .collect();
            if let Some(v) = &mut self.video {
                v.set_preloaded(&list);
                // Text is rendered in the background now, so GO renders nothing.
                let texts: Vec<_> = cues
                    .iter()
                    .filter(|c| c.armed)
                    .filter_map(|c| match &c.kind {
                        CueKind::Text(t) => Some((c.id, t)),
                        _ => None,
                    })
                    .collect();
                v.set_preloaded_text(&texts);
            }
        }
    }

    fn preload_retry(&mut self, now: Instant) -> bool {
        self.preload_retry.is_some_and(|t| now >= t)
    }

    fn start(&mut self, cue: &Cue, at: Instant) -> Result<(), String> {
        match &cue.kind {
            CueKind::Audio(p) => {
                let Some(a) = &mut self.audio else {
                    return Err(format!("audio unavailable: {}", self.audio_error.as_deref().unwrap_or("?")));
                };
                if p.path.as_os_str().is_empty() {
                    return Err("no audio file".into());
                }
                if !self.prober.exists(&p.path) {
                    return Err(format!("file not found: {}", p.path.display()));
                }
                self.pending_audio.remove(&cue.id);
                match self.prober.get(&p.path) {
                    Some(Ok(i)) => a.start(cue.id, p, i.channels.max(1), at),
                    Some(Err(e)) => Err(e),
                    None => {
                        // Normally preloading has probed it. Never probe here (engine thread,
                        // show locked): start when the background probe answers.
                        a.stop(cue.id);
                        self.pending_audio.insert(cue.id, PendingAudio { params: p.clone(), at, paused: false });
                        Ok(())
                    }
                }
            }
            #[cfg(feature = "video")]
            CueKind::Video(p) => {
                if !p.path.as_os_str().is_empty() && !self.prober.exists(&p.path) {
                    return Err(format!("file not found: {}", p.path.display()));
                }
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
        self.pending_audio.remove(&id);
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
        if let Some(p) = self.pending_audio.get_mut(&id) {
            p.paused = true;
        }
        if let Some(a) = &mut self.audio {
            a.pause(id);
        }
        #[cfg(feature = "video")]
        if let Some(v) = &mut self.video {
            v.pause(id);
        }
    }

    fn resume(&mut self, id: CueId, at: Instant) {
        if let Some(p) = self.pending_audio.get_mut(&id) {
            p.paused = false;
            p.at = at;
        }
        if let Some(a) = &mut self.audio {
            a.resume(id, at);
        }
        #[cfg(feature = "video")]
        if let Some(v) = &mut self.video {
            v.resume(id, at);
        }
    }

    fn fade(&mut self, id: CueId, f: &FadeRequest, at: Instant) {
        if let Some(a) = &mut self.audio {
            if let Some(db) = f.volume_db {
                a.fade(id, db, f.duration, f.curve, at);
            }
            if let Some(objs) = &f.objects {
                a.fade_objects(id, objs, f.duration, f.curve, at);
            }
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
        if self.pending_audio.contains_key(&id) {
            return false;
        }
        let audio_done = self.audio.as_ref().is_none_or(|a| a.is_finished(id));
        #[cfg(feature = "video")]
        let video_done = self.video.as_ref().is_none_or(|v| v.is_finished(id));
        #[cfg(not(feature = "video"))]
        let video_done = true;
        audio_done && video_done
    }

    fn duration(&self, cue: &Cue) -> Option<f64> {
        let file = match &cue.kind {
            // Unknown until probed: the engine then waits for the media to report its end.
            CueKind::Audio(a) => self.prober.get(&a.path)?.ok()?.duration,
            CueKind::Video(v) if is_image_path(&v.path) => None,
            CueKind::Text(t) => return (t.duration > 0.0).then_some(t.duration),
            CueKind::Mic(m) => return (m.duration > 0.0).then_some(m.duration),
            CueKind::Camera(c) => return (c.duration > 0.0).then_some(c.duration),
            CueKind::Video(v) => self.prober.get(&v.path)?.ok()?.duration,
            _ => return None,
        };
        effective_duration(cue, file)
    }

    fn light(&mut self, c: &crate::model::LightingCue, design: &crate::light::Lighting, at: Instant) -> Result<f64, String> {
        use crate::model::LightAction;
        match c.action {
            LightAction::Go => {
                let id = c.cue.ok_or("no lighting cue chosen")?;
                let (seq, i) = design.find_cue(id).ok_or("lighting cue not found in the design")?;
                self.light.go_cue(id, at);
                Ok(seq.cues[i].duration())
            }
            LightAction::Release => {
                match c.sequence {
                    Some(s) => self.light.release(s, c.fade, at),
                    None => self.light.release_all(c.fade, at),
                }
                Ok(c.fade.max(0.0))
            }
        }
    }

    fn drain_errors(&mut self) -> Vec<String> {
        let mut out = std::mem::take(&mut self.notes);
        out.extend(self.audio.as_mut().map(|a| a.drain_errors()).unwrap_or_default());
        out.extend(self.light.drain_errors());
        #[cfg(feature = "video")]
        if let Some(v) = &mut self.video {
            out.extend(v.drain_errors());
        }
        out
    }

    fn update(&mut self, now: Instant) {
        self.watch_audio(now);
        // Audio-reactive effects read the current output's meters.
        let tap = self.audio.as_ref().map(|a| a.meter_tap());
        if tap.as_ref().map(|t| t.id()) != self.light_audio {
            self.light_audio = tap.as_ref().map(|t| t.id());
            self.light.set_audio(tap);
        }
        self.watch_mics(now);
        self.start_pending_audio(now);
        if let Some(a) = &mut self.audio {
            a.advance_fades(now);
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

    /// Network paths are recognised in every spelling Windows accepts; local ones pass.
    #[test]
    fn windows_network_paths() {
        for net in [r"\\server\share\a.wav", "//server/share/a.wav", r"\\?\UNC\server\share\a.wav", r"\\?\GLOBALROOT\Device\Mup\s\x",
            r"\\.\UNC\server\x", r"\??\UNC\server\x", r"\\attacker@SSL\DavWWWRoot\x.png", r"/\server\x"] {
            assert!(windows_network_path(net), "{net}");
        }
        for local in [r"C:\Show\a.wav", "D:/media/a.wav", r"\\?\C:\Show\a.wav", r"\\.\D:\a.wav", r"\Show\a.wav", "a.wav", "/home/u/a.wav", ""] {
            assert!(!windows_network_path(local), "{local}");
        }
        assert_eq!(is_network_path(Path::new(r"\\server\share\a.wav")), cfg!(windows));
    }
    use rosc::OscType;

    /// `Prober::stat` answers from its cache without touching the disk (the UI calls it per
    /// visible cue per frame under the show lock), and notices files appearing and changing.
    #[test]
    fn stat_is_cached_and_refreshed_in_the_background() {
        let p = Prober::new();
        let dir = std::env::temp_dir().join(format!("openq-stat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.wav");
        let _ = std::fs::remove_file(&file);
        let wait = |want: Option<Option<u64>>| {
            let t0 = std::time::Instant::now();
            loop {
                if p.stat(&file) == want {
                    return;
                }
                assert!(t0.elapsed() < STAT_TTL * 3, "stat never became {want:?}");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        assert_eq!(p.stat(&file), None, "first ask is unknown, not a blocking check");
        assert!(p.exists(&file), "unknown counts as present");
        wait(Some(None));
        assert!(!p.exists(&file));
        std::fs::write(&file, [0u8; 10]).unwrap();
        wait(Some(Some(10)));
        assert!(p.exists(&file));
        assert_eq!(p.stat(Path::new("")), Some(None));
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// Preloading runs on the engine thread with the show locked, so it must never probe a
    /// file itself: a file that can't be read yet (here a FIFO, which blocks like a stalled
    /// network share) is left to the background prober and offered again later.
    #[cfg(unix)]
    #[test]
    fn preloading_never_blocks_on_a_file() {
        use crate::engine::Backend;
        let dir = std::env::temp_dir().join(format!("openq-preload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("hung-drive-stalled.wav");
        let _ = std::fs::remove_file(&fifo);
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let mut media = Media::new(Prober::new());
        let cue = Cue::new(CueKind::Audio(crate::model::AudioCue { path: fifo.clone(), ..Default::default() }));
        let t0 = Instant::now();
        media.set_preloaded(&[&cue]);
        assert!(t0.elapsed() < std::time::Duration::from_millis(100), "set_preloaded blocked for {:?}", t0.elapsed());
        assert!(!media.preload_retry(t0), "retries are throttled");
        assert!(media.preload_retry(t0 + std::time::Duration::from_millis(60)), "a retry is asked for");
        let _ = std::fs::remove_file(&fifo);
    }

    /// GO on an audio cue whose file hasn't been probed yet must not probe on the engine thread:
    /// the start waits for the background probe (a stalled file never blocks the tick).
    #[test]
    fn starting_an_unprobed_file_never_blocks() {
        use crate::engine::Backend;
        let dir = std::env::temp_dir().join(format!("openq-cold-go-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("hung-drive-stalled.wav");
        let _ = std::fs::remove_file(&fifo);
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let wav = dir.join("cold.wav");
        decoder::tests::write_wav(&wav, 48000, 0.5);

        let mut media = Media::new(Prober::new());
        media.audio = Some(audio::AudioOutput::open_virtual(48000, 2, None).unwrap());
        let stalled = Cue::new(CueKind::Audio(crate::model::AudioCue { path: fifo.clone(), ..Default::default() }));
        let t0 = Instant::now();
        assert_eq!(media.start(&stalled, t0), Ok(()));
        assert!(t0.elapsed() < std::time::Duration::from_millis(100), "start blocked for {:?}", t0.elapsed());
        assert!(!media.is_finished(stalled.id), "a waiting start counts as playing");
        assert_eq!(media.duration(&stalled), None, "unknown until probed");
        media.update(Instant::now());
        media.stop(stalled.id);
        assert!(media.is_finished(stalled.id));
        // Its probe is still stuck on the FIFO; other files are probed anyway.
        let cold = Cue::new(CueKind::Audio(crate::model::AudioCue { path: wav.clone(), ..Default::default() }));
        assert_eq!(media.start(&cold, Instant::now()), Ok(()));
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !media.audio.as_ref().unwrap().has(cold.id) {
            assert!(Instant::now() < deadline, "the cue never started after its probe");
            media.update(Instant::now());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let d = media.duration(&cold).expect("probed");
        assert!((d - 0.5).abs() < 0.01, "{d}");
        media.stop(cold.id);

        // Release the probe stuck opening the FIFO.
        let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
        let _ = std::fs::remove_file(&fifo);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The waveform cache keeps to its memory budget, dropping the views not looked at lately.
    #[test]
    fn waveform_cache_is_bounded() {
        let w = Waveforms::new();
        let big = |n: usize| Ok(Arc::new(decoder::Waveform::new(vec![[0.0, 0.0]; n], 0.01, 0.0)));
        let per = Waveforms::MAX_BYTES / 16 / 3; // three fit
        for i in 0..3 {
            w.insert(PathBuf::from(format!("/f{i}")), big(per));
        }
        assert_eq!(w.cached(), 3);
        assert!(w.get(Path::new("/f0")).is_some(), "use the oldest again");
        w.insert(PathBuf::from("/f3"), big(per));
        assert_eq!(w.cached(), 3);
        assert!(w.cache.lock().map.contains_key(Path::new("/f0")), "recently used is kept");
        assert!(!w.cache.lock().map.contains_key(Path::new("/f1")), "least recently used goes");
    }

    /// A decoder that stops delivering without failing (a hung network share) is reported once.
    #[test]
    fn a_stalled_decoder_is_reported() {
        let dir = std::env::temp_dir().join(format!("openq-stall-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A FIFO nobody writes to blocks whoever opens it. A show can't use one (it isn't a
        // file: refused at once), but here it stands in for a hung network drive.
        let fifo = dir.join("hung.wav");
        let _ = std::fs::remove_file(&fifo);
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        assert!(check_local(&fifo).is_err());
        let fifo = dir.join("hung-drive.wav");
        let _ = std::fs::remove_file(&fifo);
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let mut a = audio::AudioOutput::open_virtual(48000, 2, None).unwrap();
        let cue = CueId::new_v4();
        let params = crate::model::AudioCue { path: fifo.clone(), ..Default::default() };
        a.start(cue, &params, 2, Instant::now()).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let mut reports = Vec::new();
        while reports.is_empty() {
            assert!(Instant::now() < deadline, "stall never reported");
            std::thread::sleep(std::time::Duration::from_millis(50));
            a.update();
            reports = a.drain_errors();
        }
        assert!(reports[0].contains("stopped loading"), "{reports:?}");
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(a.drain_errors().is_empty(), "reported once");
        a.stop(cue);
        // Unblock the decoder thread stuck opening the FIFO.
        let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
