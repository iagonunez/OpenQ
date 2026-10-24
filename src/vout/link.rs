//! OpnQ's side of the video output process: [`VideoLink`] stands where the video player used to
//! be. It starts `opnq --video-output` the first time video is needed, sends it cue commands
//! with start times on a shared clock, and plays movie soundtracks itself (into OpnQ's mixer,
//! like audio cues). If the process crashes, hangs or can't start, the show goes on: the link
//! restarts it (with growing pauses) and brings back the layers that were showing.

use super::proto::{self, FromVideo, Layer, Ns, ToVideo, VideoFade, Windows};
use crate::engine::FadeRequest;
use crate::media::audio::VoiceShared;
use crate::media::video::{AudioTap, VideoOutput};
use crate::model::{CameraCue, CueId, Stage, StageId, TextCue, VideoCue};
use parking_lot::{Condvar, Mutex};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A stage window event for the UI.
#[derive(Clone, Debug, PartialEq)]
pub enum StageEvent {
    /// Corner pins moved in a mapping window.
    Corners(StageId, [[f32; 2]; 4]),
    /// A window opened by itself.
    Opened(StageId),
    /// The user closed a window.
    Closed(StageId),
    /// A window that closes when nothing plays closed.
    IdleClosed(StageId),
    /// The user turned fullscreen on or off.
    Fullscreen(StageId, bool),
    /// A requested snapshot was saved (or failed).
    Snapshot(Result<PathBuf, String>),
}

/// How the video output process is doing.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Not started yet (nothing has needed video).
    Idle,
    /// Starting.
    Starting,
    /// Running and in sync.
    Ready,
    /// It stopped; starting it again.
    Restarting(String),
    /// It can't run here (no display or GPU); trying again now and then.
    Unavailable(String),
}

/// How long the process may go without a heartbeat before it counts as hung.
const HANG: Duration = Duration::from_secs(4);
/// Pause before the first restart; it doubles up to [`MAX_BACKOFF`].
const BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// A process that ran this long resets the backoff.
const STABLE: Duration = Duration::from_secs(30);

struct ProcState {
    wanted: bool,
    quit: bool,
    /// To the running process's writer thread (only while it is in sync).
    tx: Option<mpsc::Sender<ToVideo>>,
    /// Bumped each time a process becomes ready.
    generation: u64,
    events: Vec<FromVideo>,
    status: Status,
    notes: Vec<String>,
}

/// The supervisor thread's shared state.
struct Proc {
    epoch: Instant,
    state: Mutex<ProcState>,
    wake: Condvar,
}

impl Proc {
    fn ns(&self, t: Instant) -> Ns {
        match t.checked_duration_since(self.epoch) {
            Some(d) => d.as_nanos() as Ns,
            None => -(self.epoch.duration_since(t).as_nanos() as Ns),
        }
    }

    fn send(&self, msg: ToVideo) {
        if let Some(tx) = &self.state.lock().tx {
            let _ = tx.send(msg);
        }
    }
}

/// A fade or hold sent to a layer, kept to replay after a restart.
#[derive(Clone)]
enum FadeStep {
    Fade(VideoFade),
    Hold,
}

/// Fades and holds kept per layer; older ones are forgotten (their end values are lost on a
/// restart).
const KEEP_FADES: usize = 32;

/// A layer as OpnQ last asked for it (to bring it back after a restart).
struct Live {
    layer: Layer,
    at: Instant,
    paused_at: Option<Instant>,
    /// Time spent paused so far.
    paused: Duration,
    /// The video process reported it ended.
    done: bool,
    /// Latest position the video process reported.
    position: Option<f64>,
    /// Pictures shown so far, as the video process last reported.
    frames: u64,
    /// Told the engine log that video is unavailable for this cue.
    unavailable_reported: bool,
    /// Fades and holds sent to it, in order, with their times.
    fades: Vec<(FadeStep, Instant)>,
}

impl Live {
    /// Seconds shown so far (pauses excluded).
    fn played(&self, now: Instant) -> f64 {
        let end = self.paused_at.unwrap_or(now);
        end.saturating_duration_since(self.at).saturating_sub(self.paused).as_secs_f64()
    }
}

/// Movies and texts to open ahead of GO.
type Preload = (Vec<(CueId, VideoCue)>, Vec<(CueId, TextCue)>);

/// OpnQ's handle on the video output process (see the module docs).
pub struct VideoLink {
    proc: Arc<Proc>,
    /// Movie soundtracks, decoded here into the mixer.
    tracks: VideoOutput,
    live: HashMap<CueId, Live>,
    /// The movies and texts to preload, as last sent.
    preload: Preload,
    stages: Vec<Stage>,
    windows: Windows,
    generation: u64,
    errors: Vec<String>,
    stage_events: Vec<StageEvent>,
    /// Snapshots asked for before the process was ready.
    snapshots: Vec<ToVideo>,
    /// The layer whose picture is wanted ([`VideoLink::watch`]).
    watch: Option<CueId>,
    /// Its latest picture, numbered.
    thumb: Option<(u64, CueId, Arc<crate::media::video::Thumbnail>)>,
    /// Movies decode on the graphics card where it can.
    hardware_decoding: bool,
}

impl VideoLink {
    /// Initialise FFmpeg (for soundtracks). The process starts when video is first needed.
    pub fn new() -> anyhow::Result<Self> {
        let proc = Arc::new(Proc {
            epoch: Instant::now(),
            state: Mutex::new(ProcState {
                wanted: false,
                quit: false,
                tx: None,
                generation: 0,
                events: Vec::new(),
                status: Status::Idle,
                notes: Vec::new(),
            }),
            wake: Condvar::new(),
        });
        let p = proc.clone();
        std::thread::Builder::new().name("opnq-video-link".into()).spawn(move || supervise(&p))?;
        Ok(Self {
            proc,
            tracks: VideoOutput::soundtracks()?,
            live: HashMap::new(),
            preload: Default::default(),
            stages: Vec::new(),
            windows: Windows::default(),
            generation: 0,
            errors: Vec::new(),
            stage_events: Vec::new(),
            snapshots: Vec::new(),
            watch: None,
            thumb: None,
            hardware_decoding: true,
        })
    }

    /// Start the video output process if it isn't running yet.
    fn want(&self) {
        let mut st = self.proc.state.lock();
        if !st.wanted {
            st.wanted = true;
            st.status = Status::Starting;
            self.proc.wake.notify_all();
        }
    }

    /// How the video output process is doing.
    pub fn status(&self) -> Status {
        self.proc.state.lock().status.clone()
    }

    /// The workspace's stages (sent when they change).
    pub fn set_stages(&mut self, stages: &[Stage]) {
        if self.stages != stages {
            self.stages = stages.to_vec();
            self.proc.send(ToVideo::Stages(self.stages.clone()));
        }
    }

    /// Stage windows to show (sent when they change; opening one starts the process).
    pub fn set_windows(&mut self, w: Windows) {
        if self.windows != w {
            if !w.open.is_empty() {
                self.want();
            }
            self.windows = w;
            self.proc.send(ToVideo::Windows(self.windows.clone()));
        }
    }

    /// Save a picture of a stage window's next frame.
    pub fn snapshot(&mut self, stage: StageId, path: PathBuf) {
        self.want();
        let msg = ToVideo::Snapshot { stage, path };
        if self.proc.state.lock().tx.is_some() {
            self.proc.send(msg);
        } else {
            // Sent once the process is ready.
            self.snapshots.push(msg);
        }
    }

    /// Decode movies on the graphics card's video engine where it takes them (sent when it
    /// changes; movies started from then on).
    pub fn set_hardware_decoding(&mut self, on: bool) {
        if self.hardware_decoding != on {
            self.hardware_decoding = on;
            self.proc.send(ToVideo::HardwareDecoding(on));
        }
    }

    /// Get small copies of this layer's picture a few times a second (None: stop). The geometry
    /// preview shows them while the cue plays.
    pub fn watch(&mut self, cue: Option<CueId>) {
        if self.watch != cue {
            self.watch = cue;
            if self.thumb.as_ref().is_some_and(|t| Some(t.1) != cue) {
                self.thumb = None;
            }
            self.proc.send(ToVideo::Watch(cue));
        }
    }

    /// The latest picture of the watched layer, numbered (the number changes with the picture).
    pub fn thumbnail(&self, cue: CueId) -> Option<(u64, Arc<crate::media::video::Thumbnail>)> {
        self.thumb.as_ref().filter(|t| t.1 == cue && self.live.contains_key(&cue)).map(|t| (t.0, t.2.clone()))
    }

    /// Stage window events since the last call.
    pub fn take_stage_events(&mut self) -> Vec<StageEvent> {
        std::mem::take(&mut self.stage_events)
    }

    /// Open these movies (and their soundtracks) ahead of time.
    pub fn set_preloaded(&mut self, cues: &[(CueId, &VideoCue, Option<AudioTap>)]) {
        let tracks: Vec<_> = cues.iter().filter(|(_, v, t)| t.is_some() && !crate::media::is_image_path(&v.path)).cloned().collect();
        self.tracks.set_preloaded(&tracks);
        let videos: Vec<(CueId, VideoCue)> = cues.iter().map(|(id, v, _)| (*id, (*v).clone())).collect();
        if videos != self.preload.0 {
            self.preload.0 = videos;
            self.send_preload();
        }
    }

    /// Render these Text cues ahead of GO.
    pub fn set_preloaded_text(&mut self, cues: &[(CueId, &TextCue)]) {
        let texts: Vec<(CueId, TextCue)> = cues.iter().map(|(id, t)| (*id, (*t).clone())).collect();
        if texts != self.preload.1 {
            self.preload.1 = texts;
            self.send_preload();
        }
    }

    fn send_preload(&self) {
        if !self.preload.0.is_empty() || !self.preload.1.is_empty() {
            self.want();
        }
        self.proc.send(ToVideo::Preload { videos: self.preload.0.clone(), texts: self.preload.1.clone() });
    }

    fn start_layer(&mut self, cue: CueId, layer: Layer, at: Instant) {
        self.want();
        self.proc.send(ToVideo::Start { cue, layer: layer.clone(), at: self.proc.ns(at), anim_offset: 0.0, skip: 0.0 });
        self.live.insert(
            cue,
            Live { layer, at, paused_at: None, paused: Duration::ZERO, done: false, position: None, frames: 0, unavailable_reported: false, fades: Vec::new() },
        );
    }

    /// Start a movie or image at `at`; `tap` routes its soundtrack into the mixer.
    pub fn start(&mut self, cue: CueId, params: &VideoCue, at: Instant, tap: Option<AudioTap>) -> Result<(), String> {
        if params.path.as_os_str().is_empty() {
            return Err("no video file".into());
        }
        self.tracks.stop(cue);
        if tap.is_some() && !crate::media::is_image_path(&params.path) {
            self.tracks.start(cue, params, at, tap)?;
        }
        self.start_layer(cue, Layer::Movie(params.clone()), at);
        Ok(())
    }

    /// Show a live camera.
    pub fn start_camera(&mut self, cue: CueId, c: &CameraCue, at: Instant) {
        self.start_layer(cue, Layer::Camera(c.clone()), at);
    }

    /// Show a Text cue.
    pub fn start_text(&mut self, cue: CueId, t: &TextCue, at: Instant) {
        self.start_layer(cue, Layer::Text(t.clone()), at);
    }

    /// Live edits to a showing Text cue.
    pub fn update_text(&mut self, cue: CueId, t: &TextCue) {
        self.edit(cue, Layer::Text(t.clone()));
    }

    /// Live edits to a showing Camera cue.
    pub fn update_camera(&mut self, cue: CueId, c: &CameraCue) {
        self.edit(cue, Layer::Camera(c.clone()));
    }

    /// Live edits to a playing movie (placement, opacity, effects, stage).
    pub fn update_live(&mut self, cue: CueId, params: &VideoCue) {
        self.tracks.update_live(cue, params);
        self.edit(cue, Layer::Movie(params.clone()));
    }

    fn edit(&mut self, cue: CueId, layer: Layer) {
        let Some(l) = self.live.get_mut(&cue) else { return };
        if l.layer != layer {
            // An edit replaces what fades did to that value (as in the video process).
            let (old, new) = (look(&l.layer), look(&layer));
            for (step, _) in &mut l.fades {
                if let FadeStep::Fade(f) = step {
                    if old.0 != new.0 {
                        f.opacity = None;
                    }
                    if old.1 != new.1 {
                        f.geometry = None;
                    }
                    if old.2 != new.2 {
                        f.video_fx = None;
                    }
                }
            }
            l.fades.retain(|(s, _)| !matches!(s, FadeStep::Fade(f) if f.opacity.is_none() && f.geometry.is_none() && f.video_fx.is_none()));
            l.layer = layer.clone();
            self.proc.send(ToVideo::Update { cue, layer });
        }
    }

    /// Connect a started movie's soundtrack to its mixer voice.
    pub fn attach_audio(&mut self, cue: CueId, tx: rtrb::Producer<f32>, state: Arc<VoiceShared>) {
        self.tracks.attach_audio(cue, tx, state);
    }

    /// Remove a layer.
    pub fn stop(&mut self, cue: CueId) {
        self.tracks.stop(cue);
        if self.live.remove(&cue).is_some() {
            self.proc.send(ToVideo::Stop(cue));
        }
    }

    /// Remove every layer.
    pub fn stop_all(&mut self) {
        self.tracks.stop_all();
        self.live.clear();
        self.proc.send(ToVideo::StopAll);
    }

    /// Freeze fades in progress.
    pub fn hold(&mut self, cue: CueId) {
        self.tracks.hold(cue);
        self.proc.send(ToVideo::Hold(cue));
        if let Some(l) = self.live.get_mut(&cue) {
            remember(l, FadeStep::Hold, Instant::now());
        }
    }

    /// Freeze a layer and its soundtrack.
    pub fn pause(&mut self, cue: CueId) {
        let now = Instant::now();
        self.tracks.pause(cue);
        if let Some(l) = self.live.get_mut(&cue)
            && l.paused_at.is_none()
        {
            l.paused_at = Some(now);
            self.proc.send(ToVideo::Pause { cue, at: self.proc.ns(now) });
        }
    }

    /// Continue a paused layer at `at`.
    pub fn resume(&mut self, cue: CueId, at: Instant) {
        self.tracks.resume(cue, at);
        if let Some(l) = self.live.get_mut(&cue)
            && let Some(p) = l.paused_at.take()
        {
            l.paused += at.saturating_duration_since(p);
            self.proc.send(ToVideo::Resume { cue, at: self.proc.ns(at) });
        }
    }

    /// Opacity, geometry and effect fades (the soundtrack level fades in the mixer).
    pub fn fade(&mut self, cue: CueId, f: &FadeRequest, at: Instant) {
        if f.opacity.is_none() && f.geometry.is_none() && f.video_fx.is_none() {
            return;
        }
        let fade = VideoFade { opacity: f.opacity, geometry: f.geometry, video_fx: f.video_fx.clone(), duration: f.duration, curve: f.curve };
        if let Some(l) = self.live.get_mut(&cue) {
            remember(l, FadeStep::Fade(fade.clone()), at);
        }
        self.proc.send(ToVideo::Fade { cue, fade, at: self.proc.ns(at) });
    }

    /// Pictures a layer has shown so far (as last reported, a few times a second).
    pub fn frames(&self, cue: CueId) -> Option<u64> {
        self.live.get(&cue).map(|l| l.frames)
    }

    /// True if the cue has a layer.
    pub fn has(&self, cue: CueId) -> bool {
        self.live.contains_key(&cue)
    }

    /// True once a layer ended on its own (or can't be shown) and its soundtrack is done.
    pub fn is_finished(&self, cue: CueId) -> bool {
        let Some(l) = self.live.get(&cue) else { return true };
        let unavailable = matches!(self.proc.state.lock().status, Status::Unavailable(_));
        (l.done || unavailable) && self.tracks.is_finished(cue)
    }

    /// Seconds of media played.
    pub fn elapsed(&self, cue: CueId) -> Option<f64> {
        if self.tracks.has(cue) {
            return self.tracks.elapsed(cue);
        }
        let l = self.live.get(&cue)?;
        l.position.or_else(|| {
            let base = match &l.layer {
                Layer::Movie(v) => v.start_time.max(0.0),
                _ => 0.0,
            };
            Some(base + l.played(Instant::now()))
        })
    }

    /// Advance soundtracks, take the video process's news, and bring layers back after a restart.
    pub fn update(&mut self, now: Instant) {
        self.tracks.update(now);
        let (events, generation, status, notes) = {
            let mut st = self.proc.state.lock();
            (std::mem::take(&mut st.events), st.generation, st.status.clone(), std::mem::take(&mut st.notes))
        };
        self.errors.extend(notes);
        if generation != self.generation {
            self.generation = generation;
            self.restore(now);
        }
        for e in events {
            match e {
                FromVideo::Finished(cue) => {
                    if let Some(l) = self.live.get_mut(&cue) {
                        l.done = true;
                    }
                }
                FromVideo::Progress(cue, p, frames) => {
                    if let Some(l) = self.live.get_mut(&cue) {
                        l.position = p.or(l.position);
                        l.frames = frames;
                    }
                }
                FromVideo::Error(e) => self.errors.push(e),
                FromVideo::Corners(s, c) => {
                    if c.iter().flatten().all(|v| v.is_finite() && v.abs() < 100.0) {
                        self.stage_events.push(StageEvent::Corners(s, c));
                    }
                }
                FromVideo::Opened(s) => self.stage_events.push(StageEvent::Opened(s)),
                FromVideo::Closed(s) => self.stage_events.push(StageEvent::Closed(s)),
                FromVideo::IdleClosed(s) => self.stage_events.push(StageEvent::IdleClosed(s)),
                FromVideo::Fullscreen(s, on) => self.stage_events.push(StageEvent::Fullscreen(s, on)),
                FromVideo::Snapshot(r) => self.stage_events.push(StageEvent::Snapshot(r)),
                FromVideo::Thumb { cue, size, stage_size, rgba } => {
                    use base64::Engine;
                    let ok_size = size[0] > 0 && size[1] > 0 && size[0].max(size[1]) <= proto::THUMB_SIDE;
                    if Some(cue) == self.watch
                        && ok_size
                        && stage_size.iter().all(|v| v.is_finite() && *v > 0.0)
                        && let Ok(rgba) = base64::engine::general_purpose::STANDARD.decode(rgba)
                        && rgba.len() == (size[0] * size[1] * 4) as usize
                    {
                        let n = self.thumb.as_ref().map_or(1, |t| t.0 + 1);
                        self.thumb = Some((n, cue, Arc::new(crate::media::video::Thumbnail { size, stage_size, rgba })));
                    }
                }
                FromVideo::Pong(..) | FromVideo::Heartbeat | FromVideo::Fatal(_) => {}
            }
        }
        if let Status::Unavailable(why) = status {
            for l in self.live.values_mut().filter(|l| !l.unavailable_reported) {
                l.unavailable_reported = true;
                self.errors.push(format!("video output unavailable: {why}"));
            }
        }
    }

    /// A new video process is ready: send it everything and bring back the layers that were
    /// showing, at the point they had reached.
    fn restore(&mut self, now: Instant) {
        let p = &self.proc;
        p.send(ToVideo::Stages(self.stages.clone()));
        p.send(ToVideo::Windows(self.windows.clone()));
        p.send(ToVideo::Preload { videos: self.preload.0.clone(), texts: self.preload.1.clone() });
        p.send(ToVideo::Watch(self.watch));
        p.send(ToVideo::HardwareDecoding(self.hardware_decoding));
        // Layers resume a moment from now, where they will be by then.
        let at = now + Duration::from_millis(100);
        for (cue, l) in &mut self.live {
            l.done = false;
            l.position = None;
            l.frames = 0;
            if l.at >= now && l.paused_at.is_none() {
                // Not shown yet: start as planned.
                p.send(ToVideo::Start { cue: *cue, layer: l.layer.clone(), at: p.ns(l.at), anim_offset: 0.0, skip: 0.0 });
                replay_fades(p, *cue, &l.fades);
                continue;
            }
            let lead = if l.paused_at.is_some() { 0.0 } else { 0.1 };
            let played = l.played(now) + lead;
            let mut layer = l.layer.clone();
            let mut skip = 0.0;
            if let Layer::Movie(v) = &mut layer
                && !crate::media::is_image_path(&v.path)
            {
                if v.infinite_loop || v.play_count > 1 {
                    // The video process works out the pass and the point in it.
                    skip = played;
                } else {
                    let pos = self.tracks.elapsed(*cue).map(|p| p + lead).unwrap_or(v.start_time.max(0.0) + played);
                    v.start_time = pos;
                }
            }
            p.send(ToVideo::Start { cue: *cue, layer, at: p.ns(at), anim_offset: played, skip });
            // Fades and holds again, at their own times: values end where they were.
            replay_fades(p, *cue, &l.fades);
            if l.paused_at.is_some() {
                p.send(ToVideo::Pause { cue: *cue, at: p.ns(at) });
            }
        }
        for s in self.snapshots.drain(..) {
            p.send(s);
        }
    }

    /// Errors since the last call.
    pub fn drain_errors(&mut self) -> Vec<String> {
        let mut out = std::mem::take(&mut self.errors);
        out.extend(self.tracks.drain_errors());
        out
    }

    /// Make the video output process crash (tests of the restart).
    #[doc(hidden)]
    pub fn crash(&self) {
        self.proc.send(ToVideo::Crash);
    }

    /// Stop the video output process (closing its windows).
    pub fn shutdown(&mut self) {
        self.live.clear();
        self.tracks.stop_all();
        let mut st = self.proc.state.lock();
        st.quit = true;
        if let Some(tx) = &st.tx {
            let _ = tx.send(ToVideo::Quit);
        }
        self.proc.wake.notify_all();
    }
}

impl Drop for VideoLink {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Keep a fade or hold to replay after a restart (the oldest are forgotten).
fn remember(l: &mut Live, step: FadeStep, at: Instant) {
    if l.fades.len() >= KEEP_FADES {
        l.fades.remove(0);
    }
    l.fades.push((step, at));
}

/// Send a layer's fades and holds again, in order, at their own times.
fn replay_fades(p: &Proc, cue: CueId, fades: &[(FadeStep, Instant)]) {
    for (step, at) in fades {
        let at = p.ns(*at);
        p.send(match step {
            FadeStep::Fade(fade) => ToVideo::Fade { cue, fade: fade.clone(), at },
            FadeStep::Hold => ToVideo::HoldAt { cue, at },
        });
    }
}

/// A layer's opacity, placement and effects as its cue sets them.
fn look(layer: &Layer) -> (f32, crate::model::Geometry, Vec<crate::effects::VideoEffect>) {
    let v = match layer {
        Layer::Movie(v) => std::borrow::Cow::Borrowed(v),
        Layer::Text(t) => std::borrow::Cow::Owned(crate::media::video::text_layer_params(t)),
        Layer::Camera(c) => std::borrow::Cow::Owned(crate::media::video::camera_layer_params(c)),
    };
    (v.opacity, v.geometry, v.video_fx.clone())
}

/// The program to run as the video output: this executable, or `opnq` next to it (tests and
/// examples run from other executables); `OPNQ_VIDEO_EXE` overrides.
fn video_exe() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("OPNQ_VIDEO_EXE") {
        return Ok(p.into());
    }
    // Unit tests never open windows (the `opnq` next to them may be stale, too).
    if cfg!(test) {
        anyhow::bail!("no video output in unit tests");
    }
    let exe = std::env::current_exe()?;
    if exe.file_stem().is_some_and(|s| s == "opnq") {
        return Ok(exe);
    }
    let name = format!("opnq{}", std::env::consts::EXE_SUFFIX);
    let mut dir = exe.parent();
    for _ in 0..2 {
        let Some(d) = dir else { break };
        let candidate = d.join(&name);
        if candidate.is_file() {
            return Ok(candidate);
        }
        dir = d.parent();
    }
    anyhow::bail!("the video output program (opnq) was not found next to {}", exe.display())
}

/// The supervisor thread: start the process once wanted, keep it running, restart it when it
/// ends or hangs.
fn supervise(proc: &Proc) {
    {
        let mut st = proc.state.lock();
        while !st.wanted && !st.quit {
            proc.wake.wait(&mut st);
        }
    }
    let mut backoff = BACKOFF;
    loop {
        if proc.state.lock().quit {
            return;
        }
        let started = Instant::now();
        let ended = run_child(proc);
        let mut st = proc.state.lock();
        st.tx = None;
        if st.quit {
            return;
        }
        if started.elapsed() >= STABLE {
            backoff = BACKOFF;
        }
        match ended {
            Ended::Fatal(why) => {
                log::error!("video output unavailable: {why}");
                st.status = Status::Unavailable(why);
                backoff = MAX_BACKOFF;
            }
            Ended::Crashed(why) => {
                log::error!("video output stopped: {why}; restarting");
                st.notes.push(format!("Video output stopped ({why}); restarting"));
                st.status = Status::Restarting(why);
            }
        }
        let deadline = Instant::now() + backoff;
        while !st.quit && Instant::now() < deadline {
            proc.wake.wait_until(&mut st, deadline);
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
        if !matches!(st.status, Status::Unavailable(_)) {
            st.status = Status::Starting;
        }
    }
}

enum Ended {
    /// It can't run here.
    Fatal(String),
    /// It crashed, hung or exited.
    Crashed(String),
}

/// Kills the child if the supervisor leaves early.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        // Give it a moment to close its windows on Quit, then make sure it's gone.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.0.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run_child(proc: &Proc) -> Ended {
    let exe = match video_exe() {
        Ok(e) => e,
        Err(e) => return Ended::Fatal(format!("{e:#}")),
    };
    let child = Command::new(&exe)
        .arg(super::FLAG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => Running(c),
        Err(e) => return Ended::Fatal(format!("cannot start {}: {e}", exe.display())),
    };
    let (stdin, stdout, stderr) = (child.0.stdin.take(), child.0.stdout.take(), child.0.stderr.take());
    let (Some(mut stdin), Some(stdout), Some(stderr)) = (stdin, stdout, stderr) else {
        return Ended::Crashed("no pipes".into());
    };
    // Its stderr goes on to ours; the last lines explain a crash.
    let tail: Arc<Mutex<VecDeque<String>>> = Default::default();
    let t = tail.clone();
    let _ = std::thread::Builder::new().name("opnq-video-stderr".into()).spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[video] {line}");
            let mut t = t.lock();
            if t.len() >= 20 {
                t.pop_front();
            }
            t.push_back(line);
        }
    });
    let (ev_tx, ev_rx) = mpsc::channel::<FromVideo>();
    let _ = std::thread::Builder::new().name("opnq-video-read".into()).spawn(move || {
        let mut r = BufReader::new(stdout);
        let mut buf = Vec::new();
        while let Ok(Some(m)) = proto::read::<FromVideo>(&mut r, &mut buf) {
            if ev_tx.send(m).is_err() {
                break;
            }
        }
    });
    let (tx, rx) = mpsc::channel::<ToVideo>();
    let _ = std::thread::Builder::new().name("opnq-video-write".into()).spawn(move || {
        for m in rx {
            if proto::write(&mut stdin, &m).is_err() {
                break;
            }
        }
    });
    let why = |tail: &Mutex<VecDeque<String>>, child: &mut Running, what: &str| -> String {
        let code = child.0.try_wait().ok().flatten().map(|s| format!(" ({s})")).unwrap_or_default();
        let last = tail.lock().iter().rev().find(|l| !l.trim().is_empty()).cloned();
        match last {
            Some(l) => format!("{what}{code}: {l}"),
            None => format!("{what}{code}"),
        }
    };

    // Clock: the round trip with the smallest delay gives the offset.
    let mut best: Option<(Duration, i64)> = None;
    for id in 0..8u32 {
        let sent = Instant::now();
        if tx.send(ToVideo::Ping(id)).is_err() {
            break;
        }
        let deadline = sent + Duration::from_secs(10);
        loop {
            match ev_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(FromVideo::Pong(i, theirs)) if i == id => {
                    let back = Instant::now();
                    let rtt = back - sent;
                    let mid = proc.ns(sent) + (rtt.as_nanos() / 2) as i64;
                    if best.is_none_or(|(r, _)| rtt < r) {
                        best = Some((rtt, theirs - mid));
                    }
                    break;
                }
                Ok(FromVideo::Fatal(e)) => return Ended::Fatal(e),
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => return Ended::Crashed("did not answer".into()),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ended::Crashed(why(&tail, &mut child, "exited at start")),
            }
        }
        if proc.state.lock().quit {
            return Ended::Crashed("quit".into());
        }
    }
    let Some((rtt, offset)) = best else { return Ended::Crashed("no clock".into()) };
    log::info!("video output {} running (clock offset {offset} ns, round trip {rtt:?})", child.0.id());
    let _ = tx.send(ToVideo::Clock(offset));
    {
        let mut st = proc.state.lock();
        st.tx = Some(tx.clone());
        st.generation += 1;
        st.status = Status::Ready;
    }
    let mut last = Instant::now();
    loop {
        if proc.state.lock().quit {
            let _ = tx.send(ToVideo::Quit);
            return Ended::Crashed("quit".into());
        }
        match ev_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(FromVideo::Heartbeat) => last = Instant::now(),
            Ok(FromVideo::Fatal(e)) => return Ended::Fatal(e),
            Ok(m) => {
                last = Instant::now();
                proc.state.lock().events.push(m);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ended::Crashed(why(&tail, &mut child, "exited")),
        }
        if last.elapsed() > HANG {
            let _ = child.0.kill();
            return Ended::Crashed(format!("not responding for {} s", HANG.as_secs()));
        }
    }
}
