//! The video output process (`openq --video-output`): stage windows with their own wgpu
//! renderer, and the movie/still/text/camera players. OpenQ talks to it through stdin/stdout
//! ([`super::proto`]). It never waits on OpenQ's UI, and if it crashes or hangs OpenQ carries on
//! and starts a new one.
//!
//! Threads: the main thread runs the winit event loop and draws the windows (each paced at its
//! display's refresh rate); `openq-video-in` applies OpenQ's commands; `openq-video-tick` advances
//! fades and reports finished layers, positions, errors and heartbeats; `openq-video-out` writes
//! the reports. Decoding runs on one thread per movie, as in [`crate::media::video`].

use super::proto::{self, FromVideo, Layer, Ns, ToVideo, Windows};
use super::render::{Gpu, Overlay, Scene};
use crate::engine::FadeRequest;
use crate::media::video::{StageLayer, VideoOutput, camera_layer_params};
use crate::model::{CueId, Stage, StageId};
use eframe::wgpu;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::monitor::MonitorHandle;
use winit::window::{Fullscreen, Window, WindowId};

/// How often the tick thread runs.
const TICK: Duration = Duration::from_millis(10);
/// How often showing layers report their progress.
const POSITION_EVERY: Duration = Duration::from_millis(100);
/// How often the watched layer's picture is sent ([`ToVideo::Watch`]).
const THUMB_EVERY: Duration = Duration::from_millis(200);
/// How often OpenQ hears that the process is alive.
const HEARTBEAT: Duration = Duration::from_millis(250);
/// The drawing loop counts as stuck after this long without a turn (no heartbeats then).
const STUCK: Duration = Duration::from_secs(3);
/// How often the display list is checked while a stage is on a display.
const DISPLAY_RECHECK: Duration = Duration::from_secs(5);
/// Longest wait of an idle event loop (new layers and finished ones are picked up this fast).
const IDLE_POLL: Duration = Duration::from_millis(15);
/// Corner drags are sent to OpenQ at most this often.
const CORNER_SEND: Duration = Duration::from_millis(40);
/// A stage that closes when nothing plays ([`Stage::close_when_idle`]) waits this long after its
/// last layer, so a cue following straight on doesn't make the window close and open again.
const IDLE_CLOSE: Duration = Duration::from_secs(1);

/// Everything the threads share.
struct Core {
    video: VideoOutput,
    epoch: Instant,
    /// This clock minus OpenQ's.
    offset: i64,
    stages: Vec<Stage>,
    windows: Windows,
    /// Cues with a layer that hasn't been reported finished.
    active: HashSet<CueId>,
    /// Cues showing a movie (their progress includes the media position).
    movies: HashSet<CueId>,
    snapshots: Vec<(StageId, PathBuf)>,
    /// The layer whose picture OpenQ's geometry preview shows.
    watch: Option<CueId>,
    quit: bool,
    /// Stages or windows changed: the event loop syncs its windows.
    changed: bool,
}

impl Core {
    /// An OpenQ time on this process's clock.
    fn instant(&self, ns: Ns) -> Instant {
        let mine = ns.saturating_add(self.offset);
        if mine >= 0 {
            self.epoch + Duration::from_nanos(mine as u64)
        } else {
            self.epoch.checked_sub(Duration::from_nanos(mine.unsigned_abs())).unwrap_or(self.epoch)
        }
    }

    fn apply(&mut self, msg: ToVideo, out: &mpsc::Sender<FromVideo>) {
        match msg {
            ToVideo::Ping(_) => {}
            ToVideo::Clock(o) => self.offset = o,
            ToVideo::Stages(s) => {
                self.stages = s;
                self.changed = true;
            }
            ToVideo::Windows(w) => {
                self.windows = w;
                self.changed = true;
            }
            ToVideo::Preload { videos, texts } => {
                let v: Vec<_> = videos.iter().map(|(id, v)| (*id, v, None)).collect();
                self.video.set_preloaded(&v);
                let t: Vec<_> = texts.iter().map(|(id, t)| (*id, t)).collect();
                self.video.set_preloaded_text(&t);
            }
            ToVideo::Start { cue, layer, at, anim_offset, skip } => {
                let at = self.instant(at);
                let now = Instant::now();
                log::debug!("start {cue} {:+.1} ms from now", if at >= now { (at - now).as_secs_f64() } else { -(now - at).as_secs_f64() } * 1000.0);
                self.movies.remove(&cue);
                match &layer {
                    Layer::Movie(v) => {
                        if let Err(e) = self.video.start_skipped(cue, v, at, None, skip) {
                            let _ = out.send(FromVideo::Error(format!("{}: {e}", v.path.display())));
                            let _ = out.send(FromVideo::Finished(cue));
                            return;
                        }
                        if !crate::media::is_image_path(&v.path) {
                            self.movies.insert(cue);
                        }
                    }
                    Layer::Text(t) => self.video.start_text(cue, t, at),
                    Layer::Camera(c) => self.video.start_camera(cue, c, at),
                }
                if anim_offset > 0.0 {
                    self.video.set_clock_offset(cue, anim_offset);
                }
                self.active.insert(cue);
            }
            ToVideo::Update { cue, layer } => match &layer {
                Layer::Movie(v) => self.video.update_live(cue, v),
                Layer::Text(t) => self.video.update_text(cue, t),
                Layer::Camera(c) => self.video.update_live(cue, &camera_layer_params(c)),
            },
            ToVideo::Stop(cue) => {
                self.video.stop(cue);
                self.active.remove(&cue);
                self.movies.remove(&cue);
            }
            ToVideo::StopAll => {
                self.video.stop_all();
                self.active.clear();
                self.movies.clear();
            }
            ToVideo::Pause { cue, at } => {
                let at = self.instant(at);
                self.video.pause_at(cue, at);
            }
            ToVideo::Resume { cue, at } => {
                let at = self.instant(at);
                self.video.resume(cue, at);
            }
            ToVideo::Hold(cue) => self.video.hold(cue),
            ToVideo::HoldAt { cue, at } => {
                let at = self.instant(at);
                self.video.hold_at(cue, at);
            }
            ToVideo::Fade { cue, fade, at } => {
                let f = FadeRequest {
                    volume_db: None,
                    opacity: fade.opacity,
                    geometry: fade.geometry,
                    objects: None,
                    video_fx: fade.video_fx,
                    out_levels: None,
                    audio_fx: None,
                    duration: fade.duration,
                    curve: fade.curve,
                };
                let at = self.instant(at);
                self.video.fade(cue, &f, at);
            }
            ToVideo::Watch(cue) => self.watch = cue,
            ToVideo::HardwareDecoding(on) => crate::media::video::set_hardware_decoding(on),
            ToVideo::Snapshot { stage, path } => {
                self.snapshots.push((stage, path));
                self.changed = true;
            }
            ToVideo::Quit => self.quit = true,
            ToVideo::Crash => panic!("crash requested"),
        }
    }
}

/// Wakes the event loop.
#[derive(Debug, Clone, Copy)]
struct Wake;

/// Run the video output process. Returns the exit code.
pub fn main() -> i32 {
    // Only stderr: stdout carries the protocol.
    let _ = crate::errlog::stderr_logger("openq=info").target(env_logger::Target::Stderr).try_init();
    crate::i18n::init();
    let (out_tx, out_rx) = mpsc::channel::<FromVideo>();
    let writer = std::thread::Builder::new()
        .name("openq-video-out".into())
        .spawn(move || {
            let mut stdout = std::io::stdout().lock();
            for m in out_rx {
                if proto::write(&mut stdout, &m).is_err() {
                    break;
                }
            }
        })
        .expect("cannot start a thread");
    let code = run(out_tx);
    let _ = writer.join();
    code
}

fn run(out: mpsc::Sender<FromVideo>) -> i32 {
    let fatal = |out: &mpsc::Sender<FromVideo>, why: String| {
        log::error!("{why}");
        let _ = out.send(FromVideo::Fatal(why));
        2
    };
    let video = match VideoOutput::new() {
        Ok(v) => v,
        Err(e) => return fatal(&out, format!("{e:#}")),
    };
    let mut builder = EventLoop::<Wake>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        // No Dock icon or menu bar of its own: the windows belong to OpenQ.
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder.with_activation_policy(ActivationPolicy::Accessory);
    }
    let event_loop = match builder.build() {
        Ok(e) => e,
        Err(e) => return fatal(&out, format!("no display: {e}")),
    };
    let core = Arc::new(Mutex::new(Core {
        video,
        epoch: Instant::now(),
        offset: 0,
        stages: Vec::new(),
        windows: Windows::default(),
        active: HashSet::new(),
        movies: HashSet::new(),
        snapshots: Vec::new(),
        watch: None,
        quit: false,
        changed: true,
    }));
    let alive = Arc::new(AtomicU64::new(0));
    let probe = Arc::new(GpuProbe { queue: Mutex::new(None), pending: AtomicU64::new(NO_PROBE) });
    spawn_input(core.clone(), out.clone(), event_loop.create_proxy());
    spawn_tick(core.clone(), out.clone(), alive.clone(), probe.clone());
    let mut app = App {
        core,
        out,
        alive,
        probe,
        instance: wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env()),
        gpu: None,
        wins: HashMap::new(),
        by_window: HashMap::new(),
        auto_opened: HashSet::new(),
        idle_since: HashMap::new(),
        monitors: Vec::new(),
        monitors_at: None,
        exit_code: 0,
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        return fatal(&app.out, format!("event loop: {e}"));
    }
    app.exit_code
}

/// Milliseconds since `epoch` (for the shared "alive" stamp).
fn stamp(epoch: Instant) -> u64 {
    epoch.elapsed().as_millis() as u64
}

/// End the process if a helper thread panics: OpenQ starts a fresh one.
fn guarded(name: &'static str, f: impl FnOnce() + Send + 'static) {
    let spawned = std::thread::Builder::new().name(name.into()).spawn(move || {
        if let Err(e) = crate::recovery::catch(|| {
            f();
            Ok(())
        }) {
            log::error!("{name}: {e:#}");
            std::process::exit(101);
        }
    });
    if spawned.is_err() {
        std::process::exit(101);
    }
}

fn spawn_input(core: Arc<Mutex<Core>>, out: mpsc::Sender<FromVideo>, proxy: EventLoopProxy<Wake>) {
    guarded("openq-video-in", move || {
        let epoch = core.lock().epoch;
        let mut input = std::io::BufReader::new(std::io::stdin().lock());
        let mut buf = Vec::new();
        loop {
            match proto::read::<ToVideo>(&mut input, &mut buf) {
                Ok(Some(ToVideo::Ping(id))) => {
                    // Answered at once, without the core lock: the clock check measures the pipe.
                    let _ = out.send(FromVideo::Pong(id, epoch.elapsed().as_nanos() as i64));
                }
                Ok(Some(m)) => {
                    let mut c = core.lock();
                    c.apply(m, &out);
                    let quit = c.quit;
                    drop(c);
                    let _ = proxy.send_event(Wake);
                    if quit {
                        return;
                    }
                }
                // OpenQ went away: close everything.
                Ok(None) | Err(_) => {
                    core.lock().quit = true;
                    let _ = proxy.send_event(Wake);
                    return;
                }
            }
        }
    });
}

fn spawn_tick(core: Arc<Mutex<Core>>, out: mpsc::Sender<FromVideo>, alive: Arc<AtomicU64>, probe: Arc<GpuProbe>) {
    guarded("openq-video-tick", move || {
        let epoch = core.lock().epoch;
        let (mut last_pos, mut last_beat) = (Instant::now(), Instant::now() - HEARTBEAT);
        let mut last_thumb = (Instant::now(), None::<(CueId, u64)>);
        loop {
            std::thread::sleep(TICK);
            let now = Instant::now();
            let mut c = core.lock();
            if c.quit {
                return;
            }
            c.video.update(now);
            let finished: Vec<CueId> = c.active.iter().copied().filter(|id| c.video.is_finished(*id)).collect();
            for id in finished {
                c.active.remove(&id);
                c.movies.remove(&id);
                let _ = out.send(FromVideo::Finished(id));
            }
            for e in c.video.drain_errors() {
                let _ = out.send(FromVideo::Error(e));
            }
            if now - last_pos >= POSITION_EVERY {
                last_pos = now;
                let frames = c.video.frame_counts();
                for id in &c.active {
                    let pos = if c.movies.contains(id) { c.video.elapsed(*id) } else { None };
                    let n = frames.iter().find(|f| f.0 == *id).map_or(0, |f| f.1);
                    let _ = out.send(FromVideo::Progress(*id, pos, n));
                }
            }
            let watched = c.watch.and_then(|id| Some((id, c.video.frame_of(id)?)));
            drop(c);
            // The preview's live picture: a small copy when it changed (made outside the core lock).
            if let Some((id, frame)) = watched
                && now - last_thumb.0 >= THUMB_EVERY
            {
                let f = frame.lock();
                if last_thumb.1 != Some((id, f.generation)) {
                    last_thumb = (now, Some((id, f.generation)));
                    if let Some(t) = crate::media::video::thumbnail(&f, proto::THUMB_SIDE) {
                        drop(f);
                        use base64::Engine;
                        let rgba = base64::engine::general_purpose::STANDARD.encode(&t.rgba);
                        let _ = out.send(FromVideo::Thumb { cue: id, size: t.size, stage_size: t.stage_size, rgba });
                    }
                }
            }
            // Heartbeats only while the window loop turns and the GPU finishes work: a stuck
            // loop or a hung driver stops them, and OpenQ replaces the process.
            let turned = alive.load(Ordering::Relaxed);
            let ms = stamp(epoch);
            if now - last_beat >= HEARTBEAT && ms.saturating_sub(turned) < STUCK.as_millis() as u64 && probe.ok(ms) {
                last_beat = now;
                if out.send(FromVideo::Heartbeat).is_err() {
                    return;
                }
            }
        }
    });
}

/// What the event loop tells a window's render thread.
#[derive(Default)]
struct View {
    /// Window size in pixels.
    size: [u32; 2],
    /// Corners moved in this window, drawn until OpenQ's stage list has them.
    corners: Option<[[f32; 2]; 4]>,
    /// Mapping pins and instructions.
    overlay: Option<Arc<Overlay>>,
    /// Draw again even if nothing else changed.
    dirty: bool,
    quit: bool,
}

/// A window's render thread's inbox.
#[derive(Default)]
struct Painter {
    view: Mutex<View>,
    wake: parking_lot::Condvar,
}

impl Painter {
    fn update(&self, f: impl FnOnce(&mut View)) {
        let mut v = self.view.lock();
        f(&mut v);
        v.dirty = true;
        self.wake.notify_all();
    }
}

/// One stage window, as the event loop sees it. Its picture is drawn by its own thread
/// ([`render_loop`]): a window the compositor isn't showing can block its frames for good
/// (Wayland), and that must not hold up the other outputs.
struct Win {
    window: Arc<Window>,
    painter: Arc<Painter>,
    /// Fullscreen on this display (None: on the current one), or windowed.
    fullscreen: Option<Option<String>>,
    cursor: Option<PhysicalPosition<f64>>,
    shift: bool,
    last_click: Option<Instant>,
    /// Corner being dragged.
    drag: Option<usize>,
    /// Corners moved here, until OpenQ's stage list has them.
    corners: Option<([[f32; 2]; 4], Instant)>,
    corners_sent: Instant,
    /// What the mapping overlay was drawn for.
    overlay_key: String,
}

impl Win {
    fn size(&self) -> [u32; 2] {
        let s = self.window.inner_size();
        [s.width.max(1), s.height.max(1)]
    }

    /// The corners to draw with: ones moved here (until OpenQ has them), else the stage's.
    fn corners(&self, stage: &Stage) -> [[f32; 2]; 4] {
        match self.corners {
            Some((c, at)) if self.drag.is_some() || (c != stage.corners && at.elapsed() < Duration::from_secs(1)) => c,
            _ => stage.corners,
        }
    }
}

impl Drop for Win {
    fn drop(&mut self) {
        // Gone from the screen at once; its thread ends on its next turn.
        self.window.set_visible(false);
        self.painter.update(|v| v.quit = true);
    }
}

/// Lets the tick thread check that the GPU still finishes work (a hung driver stops the
/// heartbeats, and OpenQ replaces the process).
struct GpuProbe {
    queue: Mutex<Option<(wgpu::Device, wgpu::Queue)>>,
    /// A check is on its way, sent at this stamp.
    pending: AtomicU64,
}

const NO_PROBE: u64 = u64::MAX;

impl GpuProbe {
    /// True unless a check has been waiting for longer than [`STUCK`].
    fn ok(self: &Arc<Self>, now: u64) -> bool {
        let Some((device, queue)) = self.queue.lock().clone() else { return true };
        let pending = self.pending.load(Ordering::Acquire);
        if pending == NO_PROBE {
            self.pending.store(now, Ordering::Release);
            let me = self.clone();
            queue.on_submitted_work_done(move || me.pending.store(NO_PROBE, Ordering::Release));
            queue.submit([]);
        }
        let _ = device.poll(wgpu::PollType::Poll);
        let pending = self.pending.load(Ordering::Acquire);
        pending == NO_PROBE || now.saturating_sub(pending) < STUCK.as_millis() as u64
    }
}

struct App {
    core: Arc<Mutex<Core>>,
    out: mpsc::Sender<FromVideo>,
    alive: Arc<AtomicU64>,
    probe: Arc<GpuProbe>,
    instance: wgpu::Instance,
    gpu: Option<Arc<Mutex<Gpu>>>,
    wins: HashMap<StageId, Win>,
    by_window: HashMap<WindowId, StageId>,
    /// Stages whose window opened by itself (reported to OpenQ once).
    auto_opened: HashSet<StageId>,
    /// Stages closing when idle that have had nothing to show since then.
    idle_since: HashMap<StageId, Instant>,
    monitors: Vec<MonitorHandle>,
    monitors_at: Option<Instant>,
    exit_code: i32,
}

/// The stage whose layers `stage` shows: the one it spans, or itself.
fn source_of<'a>(stage: &'a Stage, stages: &'a [Stage]) -> &'a Stage {
    stage.span_of.filter(|id| *id != stage.id).and_then(|id| stages.iter().find(|s| s.id == id)).unwrap_or(stage)
}

/// A monitor's name as OpenQ's display list shows it.
fn monitor_name(m: &MonitorHandle, i: usize) -> String {
    m.name().unwrap_or_else(|| format!("Display {}", i + 1)).replace('\t', " ")
}

fn window_icon() -> Option<winit::window::Icon> {
    let icon = crate::ui::app_icon();
    winit::window::Icon::from_rgba(icon.rgba.clone(), icon.width, icon.height).ok()
}

/// The layers a window shows, and whether it is being mapped.
fn scene_input(stage: StageId, stages: &[Stage], layers: &[StageLayer], windows: &Windows) -> Option<(Stage, Stage, Vec<StageLayer>, bool)> {
    let stage = stages.iter().find(|s| s.id == stage)?;
    let source = source_of(stage, stages);
    let first = stages.first().map(|s| s.id);
    let resolve = |s: Option<StageId>| s.filter(|id| stages.iter().any(|o| o.id == *id)).or(first);
    let mine: Vec<StageLayer> = layers.iter().filter(|l| resolve(l.stage) == Some(source.id)).cloned().collect();
    Some((stage.clone(), source.clone(), mine, windows.mapping.contains(&stage.id)))
}

/// A window's render thread: draws whenever something shows (paced by the display's vsync),
/// once more when it goes empty (black), and on request.
/// An output's mask as the renderer takes it (loading on first use).
fn stage_mask(stage: &Stage) -> Option<(crate::media::mask::MaskState, bool)> {
    let m = stage.mask.as_ref().filter(|m| !m.path.as_os_str().is_empty())?;
    Some((crate::media::mask::cache().get(&m.path), m.invert))
}

fn render_loop(
    id: StageId,
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    mut config: wgpu::SurfaceConfiguration,
    gpu: Arc<Mutex<Gpu>>,
    core: Arc<Mutex<Core>>,
    painter: Arc<Painter>,
) {
    let (device, mut target) = {
        let g = gpu.lock();
        (g.device.clone(), g.target())
    };
    let mut drawn_empty = false;
    loop {
        let (size, corners, overlay, dirty) = {
            let mut v = painter.view.lock();
            if v.quit {
                return;
            }
            let dirty = std::mem::take(&mut v.dirty);
            (v.size, v.corners, v.overlay.clone(), dirty)
        };
        if size != [config.width, config.height] && size[0] > 0 && size[1] > 0 {
            (config.width, config.height) = (size[0], size[1]);
            surface.configure(&device, &config);
        }
        let (stages, windows, layers) = {
            let c = core.lock();
            (c.stages.clone(), c.windows.clone(), c.video.stage_layers())
        };
        let input = scene_input(id, &stages, &layers, &windows);
        let content = input.as_ref().is_some_and(|(_, _, l, m)| !l.is_empty() || *m);
        let idle = |painter: &Painter| {
            let mut v = painter.view.lock();
            if !v.dirty && !v.quit {
                painter.wake.wait_for(&mut v, IDLE_POLL);
            }
        };
        let Some((stage, source, mine, mapping)) = input else {
            idle(&painter);
            continue;
        };
        if !content && drawn_empty && !dirty {
            idle(&painter);
            continue;
        }
        // May block while the window isn't on screen: only this window waits.
        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                surface.configure(&device, &config);
                continue;
            }
            _ => {
                painter.view.lock().dirty |= dirty;
                std::thread::sleep(IDLE_POLL);
                continue;
            }
        };
        let scene = Scene {
            stage: &stage,
            source: &source,
            layers: &mine,
            mapping,
            corners: corners.unwrap_or(stage.corners),
            overlay: overlay.as_deref().filter(|_| mapping),
            mask: stage_mask(&stage),
        };
        let view = frame.texture.create_view(&Default::default());
        {
            let mut g = gpu.lock();
            g.render(&mut target, &scene, &view, config.format);
            g.collect_garbage();
        }
        window.pre_present_notify();
        frame.present();
        drawn_empty = !content;
    }
}

impl App {
    fn fail(&mut self, el: &ActiveEventLoop, why: String) {
        log::error!("{why}");
        let _ = self.out.send(FromVideo::Fatal(why));
        self.exit_code = 2;
        el.exit();
    }

    fn monitor(&self, name: &str) -> Option<MonitorHandle> {
        self.monitors.iter().enumerate().find(|(i, m)| monitor_name(m, *i) == name).map(|(_, m)| m.clone())
    }

    /// Open, close and place the windows OpenQ (and the layers) want.
    fn sync(&mut self, el: &ActiveEventLoop) {
        let (stages, windows, layers, snapshots) = {
            let mut c = self.core.lock();
            if c.quit {
                el.exit();
                return;
            }
            c.changed = false;
            (c.stages.clone(), c.windows.clone(), c.video.stage_layers(), std::mem::take(&mut c.snapshots))
        };
        let first = stages.first().map(|s| s.id);
        let resolve = |s: Option<StageId>| s.filter(|id| stages.iter().any(|o| o.id == *id)).or(first);
        // Stages showing layers open by themselves: the layer's own stage, or the outputs that
        // span it (a surface that only exists to be spanned doesn't open on its own).
        let mut want: HashSet<StageId> = windows.open.iter().copied().filter(|id| stages.iter().any(|s| s.id == *id)).collect();
        let mut showing = HashSet::new();
        for l in &layers {
            if let Some(s) = resolve(l.stage) {
                let spans: Vec<StageId> = stages.iter().filter(|o| o.span_of == Some(s) && o.id != s).map(|o| o.id).collect();
                let opened = if spans.is_empty() { vec![s] } else { spans };
                for id in opened {
                    showing.insert(id);
                    if want.insert(id) && self.auto_opened.insert(id) {
                        let _ = self.out.send(FromVideo::Opened(id));
                    }
                }
            }
        }
        // Outputs that close when nothing plays: closed a moment after their last layer, and
        // OpenQ told (the next layer opens them again).
        let now = Instant::now();
        let closing: Vec<StageId> = stages.iter().filter(|s| s.close_when_idle && want.contains(&s.id)).map(|s| s.id).collect();
        for id in closing {
            if showing.contains(&id) || windows.mapping.contains(&id) {
                self.idle_since.remove(&id);
                continue;
            }
            let since = *self.idle_since.entry(id).or_insert(now);
            if now - since >= IDLE_CLOSE {
                self.idle_since.remove(&id);
                want.remove(&id);
                self.core.lock().windows.open.retain(|s| *s != id);
                let _ = self.out.send(FromVideo::IdleClosed(id));
            }
        }
        self.idle_since.retain(|id, _| want.contains(id) && stages.iter().any(|s| s.id == *id && s.close_when_idle));
        self.auto_opened.retain(|id| want.contains(id));
        // Displays: listed at the start and every few seconds while a stage is on one.
        let on_display = stages.iter().any(|s| s.output.is_some() && want.contains(&s.id));
        if self.monitors_at.is_none_or(|t| on_display && t.elapsed() >= DISPLAY_RECHECK) {
            self.monitors = el.available_monitors().collect();
            self.monitors_at = Some(Instant::now());
        }
        let closing: Vec<StageId> = self.wins.keys().copied().filter(|id| !want.contains(id)).collect();
        for id in closing {
            if let Some(w) = self.wins.remove(&id) {
                self.by_window.remove(&w.window.id());
            }
        }
        for stage in &stages {
            if !want.contains(&stage.id) {
                continue;
            }
            let display = stage.output.as_deref().and_then(|n| self.monitor(n));
            let fullscreen = match (&stage.output, &display) {
                (Some(name), Some(_)) => Some(Some(name.clone())),
                // Its display is gone: windowed until it comes back.
                (Some(_), None) => None,
                (None, _) => windows.fullscreen.contains(&stage.id).then_some(None),
            };
            if !self.wins.contains_key(&stage.id)
                && let Err(e) = self.open(el, stage)
            {
                if self.gpu.is_none() {
                    return self.fail(el, format!("{e:#}"));
                }
                log::error!("cannot open the window for {}: {e:#}", stage.name);
                continue;
            }
            let Some(w) = self.wins.get_mut(&stage.id) else { continue };
            if w.fullscreen != fullscreen {
                let mode = match &fullscreen {
                    Some(Some(_)) => Some(Fullscreen::Borderless(display)),
                    Some(None) => Some(Fullscreen::Borderless(None)),
                    None => None,
                };
                w.window.set_fullscreen(mode);
                w.fullscreen = fullscreen;
            }
            let mapping = windows.mapping.contains(&stage.id);
            // The audience never sees a pointer on an output.
            w.window.set_cursor_visible(mapping || w.fullscreen.is_none());
            let title = format!("{} — OpenQ Stage", stage.name);
            if w.window.title() != title {
                w.window.set_title(&title);
            }
            self.refresh(stage.id, Some((stage, mapping)));
        }
        for (stage, path) in snapshots {
            self.snapshot(&stages, &layers, &windows, stage, path);
        }
    }

    /// Tell a window's render thread its size, corners and mapping overlay (redrawn on change).
    fn refresh(&mut self, id: StageId, known: Option<(&Stage, bool)>) {
        let fetched;
        let (stage, mapping) = match known {
            Some(k) => k,
            None => {
                let c = self.core.lock();
                fetched = c.stages.iter().find(|s| s.id == id).cloned().map(|s| (s, c.windows.mapping.contains(&id)));
                drop(c);
                match &fetched {
                    Some((s, m)) => (s, *m),
                    None => return,
                }
            }
        };
        let Some(w) = self.wins.get_mut(&id) else { return };
        let size = w.size();
        let corners = w.corners(stage);
        let moved = (corners != stage.corners).then_some(corners);
        let mut overlay = None;
        let key = if mapping {
            let hot = w.drag.or_else(|| w.cursor.and_then(|p| nearest_pin(corners, size, p, 26.0)));
            let msg = crate::tr!("Mapping {name} — drag the corners · arrow keys nudge the hovered corner (Shift: 10 px)", name = stage.name);
            let key = format!("{size:?}{corners:?}{hot:?}{msg}");
            if key != w.overlay_key {
                match super::pattern::pins(size, corners, hot, &msg) {
                    Ok(rgba) => overlay = Some(Arc::new(Overlay { generation: fastrand::u64(..), size, rgba })),
                    Err(e) => log::warn!("mapping pins: {e:#}"),
                }
            }
            key
        } else {
            String::new()
        };
        let changed = key != w.overlay_key;
        w.overlay_key = key;
        let v = w.painter.view.lock();
        let same = v.size == size && v.corners == moved && !changed;
        drop(v);
        if !same {
            w.painter.update(|v| {
                v.size = size;
                v.corners = moved;
                if changed {
                    v.overlay = overlay;
                }
            });
        }
    }

    fn ensure_gpu(&mut self, surface: Option<&wgpu::Surface<'_>>) -> anyhow::Result<Arc<Mutex<Gpu>>> {
        if self.gpu.is_none() {
            let gpu = Gpu::new(&self.instance, surface)?;
            // Text and SVGs are rendered to fit this GPU's textures.
            crate::media::set_max_texture_side(gpu.max_side as usize);
            *self.probe.queue.lock() = Some((gpu.device.clone(), gpu.queue.clone()));
            self.gpu = Some(Arc::new(Mutex::new(gpu)));
        }
        Ok(self.gpu.clone().expect("gpu"))
    }

    fn open(&mut self, el: &ActiveEventLoop, stage: &Stage) -> anyhow::Result<()> {
        let aspect = stage.height.max(1) as f64 / stage.width.max(1) as f64;
        let attrs = Window::default_attributes()
            .with_title(format!("{} — OpenQ Stage", stage.name))
            .with_inner_size(LogicalSize::new(960.0, 960.0 * aspect))
            .with_window_icon(window_icon())
            // Opening an output never takes the keyboard from the operator's window.
            .with_active(false);
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        let attrs = {
            use winit::platform::wayland::WindowAttributesExtWayland;
            attrs.with_name("openq", "openq-stage")
        };
        let window = Arc::new(el.create_window(attrs)?);
        let surface = self.instance.create_surface(window.clone())?;
        let gpu = self.ensure_gpu(Some(&surface))?;
        let config = {
            let g = gpu.lock();
            let caps = surface.get_capabilities(&g.adapter);
            anyhow::ensure!(!caps.formats.is_empty(), "the GPU can't draw to this window");
            let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
            let alpha_mode =
                if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) { wgpu::CompositeAlphaMode::Opaque } else { caps.alpha_modes[0] };
            let size = window.inner_size();
            let config = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format,
                width: size.width.max(1),
                height: size.height.max(1),
                // Each window's thread is paced by its own display.
                present_mode: wgpu::PresentMode::AutoVsync,
                desired_maximum_frame_latency: 2,
                alpha_mode,
                view_formats: Vec::new(),
            };
            surface.configure(&g.device, &config);
            config
        };
        let painter = Arc::new(Painter::default());
        painter.update(|v| v.size = [config.width, config.height]);
        let (w, p, core) = (window.clone(), painter.clone(), self.core.clone());
        let id = stage.id;
        std::thread::Builder::new().name(crate::recovery::thread_name("openq-video-out-", &stage.name)).spawn(move || {
            if let Err(e) = crate::recovery::catch(|| {
                render_loop(id, w, surface, config, gpu, core, p);
                Ok(())
            }) {
                // A panic while drawing: start over in a fresh process.
                log::error!("stage window: {e:#}");
                std::process::exit(101);
            }
        })?;
        self.by_window.insert(window.id(), stage.id);
        self.wins.insert(
            stage.id,
            Win {
                window,
                painter,
                fullscreen: None,
                cursor: None,
                shift: false,
                last_click: None,
                drag: None,
                corners: None,
                corners_sent: Instant::now(),
                overlay_key: String::new(),
            },
        );
        Ok(())
    }

    fn snapshot(&mut self, stages: &[Stage], layers: &[StageLayer], windows: &Windows, stage: StageId, path: PathBuf) {
        let res = (|| -> anyhow::Result<PathBuf> {
            let (st, source, mine, mapping) = scene_input(stage, stages, layers, windows).ok_or_else(|| anyhow::anyhow!("no such stage"))?;
            let size = match self.wins.get(&stage) {
                Some(w) => w.size(),
                None => super::render::canvas_size(&st, &source, 4096),
            };
            let corners = self.wins.get(&stage).map_or(st.corners, |w| w.corners(&st));
            let gpu = self.ensure_gpu(None)?;
            let mut g = gpu.lock();
            let mut t = g.target();
            let scene = Scene { stage: &st, source: &source, layers: &mine, mapping, corners, overlay: None, mask: stage_mask(&st) };
            let img = g.render_to_image(&mut t, &scene, size)?;
            drop(g);
            img.save(&path)?;
            Ok(path)
        })();
        let _ = self.out.send(FromVideo::Snapshot(res.map_err(|e| format!("{e:#}"))));
    }

    /// Fullscreen on or off for a window without a display of its own (one with a display stays
    /// on it), and tell OpenQ.
    fn toggle_fullscreen(&mut self, el: &ActiveEventLoop, id: StageId, on: bool) {
        {
            let mut c = self.core.lock();
            c.windows.fullscreen.retain(|s| *s != id);
            if on {
                c.windows.fullscreen.push(id);
            }
        }
        let _ = self.out.send(FromVideo::Fullscreen(id, on));
        self.sync(el);
    }

    fn send_corners(&mut self, id: StageId, force: bool) {
        let Some(w) = self.wins.get_mut(&id) else { return };
        let Some((c, _)) = w.corners else { return };
        if force || w.corners_sent.elapsed() >= CORNER_SEND {
            w.corners_sent = Instant::now();
            let _ = self.out.send(FromVideo::Corners(id, c));
        }
    }
}

/// The corner pin within `radius` pixels of `p`, if any (the nearest one).
fn nearest_pin(corners: [[f32; 2]; 4], size: [u32; 2], p: PhysicalPosition<f64>, radius: f32) -> Option<usize> {
    let d = |k: usize| {
        let (x, y) = (corners[k][0] * size[0] as f32, corners[k][1] * size[1] as f32);
        ((x - p.x as f32).powi(2) + (y - p.y as f32).powi(2)).sqrt()
    };
    (0..4).filter(|k| d(*k) <= radius).min_by(|a, b| d(*a).total_cmp(&d(*b)))
}

impl ApplicationHandler<Wake> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        self.sync(el);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, _: Wake) {
        self.sync(el);
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let epoch = self.core.lock().epoch;
        self.alive.store(stamp(epoch), Ordering::Relaxed);
        self.sync(el);
        if self.exit_code == 0 {
            el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + IDLE_POLL));
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, wid: WindowId, event: WindowEvent) {
        let Some(&id) = self.by_window.get(&wid) else { return };
        let mapping = self.core.lock().windows.mapping.contains(&id);
        match event {
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => self.refresh(id, None),
            WindowEvent::RedrawRequested => {
                if let Some(w) = self.wins.get(&id) {
                    w.painter.update(|_| {});
                }
            }
            WindowEvent::CloseRequested => {
                if let Some(w) = self.wins.remove(&id) {
                    self.by_window.remove(&w.window.id());
                }
                self.auto_opened.remove(&id);
                {
                    let mut c = self.core.lock();
                    c.windows.open.retain(|s| *s != id);
                    c.windows.mapping.retain(|s| *s != id);
                }
                let _ = self.out.send(FromVideo::Closed(id));
            }
            WindowEvent::ModifiersChanged(m) => {
                if let Some(w) = self.wins.get_mut(&id) {
                    w.shift = m.state().shift_key();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let stage = self.core.lock().stages.iter().find(|s| s.id == id).cloned();
                let Some(w) = self.wins.get_mut(&id) else { return };
                let prev = w.cursor.replace(position);
                let size = w.size();
                if let (Some(k), Some(prev), Some(stage)) = (w.drag, prev, stage) {
                    let mut c = w.corners(&stage);
                    c[k][0] += ((position.x - prev.x) / size[0] as f64) as f32;
                    c[k][1] += ((position.y - prev.y) / size[1] as f64) as f32;
                    w.corners = Some((c, Instant::now()));
                    self.send_corners(id, false);
                }
                if mapping {
                    self.refresh(id, None);
                }
            }
            WindowEvent::CursorLeft { .. } => {
                if let Some(w) = self.wins.get_mut(&id) {
                    w.cursor = None;
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                let stage = self.core.lock().stages.iter().find(|s| s.id == id).cloned();
                let Some(w) = self.wins.get_mut(&id) else { return };
                match state {
                    ElementState::Pressed => {
                        let size = w.size();
                        let pin = match (&stage, w.cursor) {
                            (Some(st), Some(p)) if mapping => nearest_pin(w.corners(st), size, p, 26.0),
                            _ => None,
                        };
                        if let (Some(k), Some(st)) = (pin, &stage) {
                            w.drag = Some(k);
                            w.corners = Some((w.corners(st), Instant::now()));
                            return;
                        }
                        let double = w.last_click.is_some_and(|t| t.elapsed() < Duration::from_millis(400));
                        w.last_click = Some(Instant::now());
                        if double {
                            w.last_click = None;
                            let on = w.fullscreen.is_none();
                            self.toggle_fullscreen(el, id, on);
                        }
                    }
                    ElementState::Released => {
                        if w.drag.take().is_some() {
                            if let Some((c, _)) = w.corners {
                                w.corners = Some((c, Instant::now()));
                            }
                            self.send_corners(id, true);
                            self.refresh(id, None);
                        }
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => match &event.logical_key {
                Key::Named(NamedKey::F11) if !event.repeat => {
                    let on = self.wins.get(&id).is_some_and(|w| w.fullscreen.is_none());
                    self.toggle_fullscreen(el, id, on);
                }
                Key::Named(key @ (NamedKey::ArrowLeft | NamedKey::ArrowRight | NamedKey::ArrowUp | NamedKey::ArrowDown)) if mapping => {
                    // Nudge the corner nearest the pointer by a pixel (Shift: 10).
                    let stage = self.core.lock().stages.iter().find(|s| s.id == id).cloned();
                    let (Some(stage), Some(w)) = (stage, self.wins.get_mut(&id)) else { return };
                    let size = w.size();
                    let mut c = w.corners(&stage);
                    let Some(pin) = w.cursor.and_then(|p| nearest_pin(c, size, p, f32::MAX)) else { return };
                    let px = if w.shift { 10.0 } else { 1.0 };
                    let (dx, dy) = match key {
                        NamedKey::ArrowLeft => (-px, 0.0),
                        NamedKey::ArrowRight => (px, 0.0),
                        NamedKey::ArrowUp => (0.0, -px),
                        _ => (0.0, px),
                    };
                    c[pin][0] += dx / size[0] as f32;
                    c[pin][1] += dy / size[1] as f32;
                    w.corners = Some((c, Instant::now()));
                    self.send_corners(id, true);
                    self.refresh(id, None);
                }
                _ => {}
            },
            _ => {}
        }
    }
}
