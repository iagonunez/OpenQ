//! Cue playback engine: the runtime state machine behind GO.
//!
//! The engine is pure logic driven by `tick(now)`; all media work goes through the [`Backend`]
//! trait so it can be tested without audio/video hardware.
//!
//! Timing model: every scheduled moment (pre-wait end, post-wait end, auto-follow) is computed
//! from *logical* timestamps rather than from when a tick happened to run, so jitter never
//! accumulates along a chain of cues. Starts are dispatched up to [`LOOKAHEAD`] early with their
//! exact target time, letting the audio backend start them sample-accurately.
//!
//! A tick makes passes over the running cues (sorted once per pass) until nothing changes, so
//! thousands of running cues stay well under a millisecond. Cues that running cues will continue
//! into are preloaded along with the next GO's. A cue that keeps restarting itself with no time
//! in between (a Start cue aimed at itself) is held off and reported instead of spinning.
//!
//! Arm, Disarm and Target cues change other cues. The engine only reads the workspace, so it
//! queues those changes as [`CueEdit`]s (the show applies them after the tick, [`Engine::take_edits`])
//! and acts on them at once in the meantime. It remembers what each cue was before, so a Reset cue
//! can put it back.

use crate::model::{Cue, CueId, CueKind, FadeCurve, GroupMode, KindTag, LightingCue, MIN_DB, NetworkCue, Workspace};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// How far ahead of their due time starts are handed to the backend, at least (the backend adds
/// its own buffering, [`Backend::lookahead`]).
pub const LOOKAHEAD: Duration = Duration::from_millis(25);
const MAX_PRELOAD: usize = 16;
/// A cue fired more often than this within one tick is in a loop with no time in it (a Start
/// cue aimed at itself, two aimed at each other...): it is held off until the next tick.
const MAX_FIRES_PER_TICK: u32 = 32;
const MAX_LOG: usize = 200;

/// A change a cue made to another cue, for the show to write into the workspace.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CueEdit {
    /// Arm (true) or disarm (false) a cue.
    Armed(CueId, bool),
    /// Point a cue at another target.
    Target(CueId, Option<CueId>),
}

/// What a cue was before Arm, Disarm or Target cues changed it (for Reset cues).
#[derive(Clone, Copy, Debug, Default)]
struct Original {
    armed: Option<bool>,
    target: Option<Option<CueId>>,
}

/// What a Fade cue asks the backend to do to one running media cue.
#[derive(Clone, Debug, PartialEq)]
pub struct FadeRequest {
    /// Target level in dB (None = leave the level alone).
    pub volume_db: Option<f32>,
    /// Target opacity 0..1 for video/text layers.
    pub opacity: Option<f32>,
    /// Target position/scale/rotation/crop for video/text layers.
    pub geometry: Option<crate::model::Geometry>,
    /// Target object-audio positions, one per source channel.
    pub objects: Option<Vec<crate::spatial::AudioObject>>,
    /// Target video effect settings for video/text/camera layers.
    pub video_fx: Option<Vec<crate::effects::VideoEffect>>,
    /// Target output fader levels (dB per output) for sound.
    pub out_levels: Option<Vec<f32>>,
    /// Target audio effect settings for sound.
    pub audio_fx: Option<Vec<crate::effects::AudioEffect>>,
    /// Fade length in seconds.
    pub duration: f64,
    /// Shape of the fade.
    pub curve: FadeCurve,
}

/// Media playback as seen by the engine.
pub trait Backend {
    /// Preload these cues so they start instantly; anything else preloaded may be released.
    fn set_preloaded(&mut self, cues: &[&Cue]);
    /// Start a media cue so that it is heard/seen at `at` (which may be slightly in the future).
    fn start(&mut self, cue: &Cue, at: Instant) -> Result<(), String>;
    /// Start a media cue `offset` seconds into its playback (a Load cue loaded it there).
    fn start_from(&mut self, cue: &Cue, at: Instant, offset: f64) -> Result<(), String> {
        if offset > 0.0 { Err("this cue can't start part way through".into()) } else { self.start(cue, at) }
    }
    /// Stop a media cue at once.
    fn stop(&mut self, id: CueId);
    /// Freeze a media cue where it is.
    fn pause(&mut self, id: CueId);
    /// Continue a paused media cue so it is heard/seen again at `at`.
    fn resume(&mut self, id: CueId, at: Instant);
    /// Ramp a running media cue's level/opacity/geometry/object positions/video effects.
    fn fade(&mut self, id: CueId, fade: &FadeRequest, at: Instant);
    /// True once a started media cue reached its end on its own (or failed).
    fn is_finished(&self, id: CueId) -> bool;
    /// Total action duration including loops, if finite and known.
    fn duration(&self, cue: &Cue) -> Option<f64>;
    /// Called every tick before the engine runs.
    fn update(&mut self, now: Instant);
    /// How far ahead starts must be handed over to land on time: [`LOOKAHEAD`] (engine ticks
    /// delayed by the show lock) plus how far ahead the backend renders.
    fn lookahead(&self) -> Duration {
        LOOKAHEAD
    }
    /// True when the last [`Backend::set_preloaded`] skipped cues that aren't ready yet (files
    /// still being probed in the background) and the preload set should be offered again.
    /// Backends throttle this themselves.
    fn preload_retry(&mut self, _now: Instant) -> bool {
        false
    }
    /// Asynchronous playback errors since the last call.
    fn drain_errors(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// Make a running audio cue leave its looping slice. Returns false if it can't.
    fn devamp(&mut self, _id: CueId) -> bool {
        false
    }
    /// Freeze a fade in progress on this cue at its current values.
    fn hold_fade(&mut self, _id: CueId) {}
    /// Send a Network cue's message.
    fn send_network(&mut self, _net: &NetworkCue) -> Result<(), String> {
        Err("network output unavailable".into())
    }
    /// Send a MIDI cue's message.
    fn send_midi(&mut self, _midi: &crate::model::MidiCue) -> Result<(), String> {
        Err("MIDI output unavailable".into())
    }
    /// Run a Light cue (GO a lighting cue, release a sequence) at `at`. Returns how long its
    /// fades take, in seconds.
    /// `design` is the workspace's lighting design (for the cue's timing).
    fn light(&mut self, _cue: &LightingCue, _design: &crate::light::Lighting, _at: Instant) -> Result<f64, String> {
        Err("lighting unavailable".into())
    }
}

/// A running (fired) cue.
#[derive(Clone, Debug)]
pub struct Instance {
    /// Type of the cue, for display.
    pub kind: KindTag,
    /// When GO fired it.
    pub fired_at: Instant,
    /// When the action starts (fired_at + pre-wait), shifted forward by pauses.
    pub action_at: Instant,
    /// Pre-wait length in seconds.
    pub pre_wait: f64,
    /// Set while paused.
    pub paused_at: Option<Instant>,
    /// The action (media start, wait, fade...) has begun.
    pub started: bool,
    /// The action has finished.
    pub action_done: bool,
    /// Known action length in seconds (None = until media/children report completion).
    pub action_duration: Option<f64>,
    /// The auto-continue/follow has been handled.
    pub continue_done: bool,
    /// Group instance that launched this cue.
    pub parent: Option<CueId>,
    /// For running Fade cues: the media being faded and the request (so pausing can freeze it
    /// and resuming can continue it).
    pub fade: Option<(Vec<CueId>, FadeRequest)>,
}

impl Instance {
    fn effective_now(&self, now: Instant) -> Instant {
        self.paused_at.unwrap_or(now)
    }

    /// Seconds elapsed in the pre-wait (0..pre_wait).
    pub fn pre_wait_elapsed(&self, now: Instant) -> f64 {
        let now = self.effective_now(now);
        (self.pre_wait - secs_between(now, self.action_at)).clamp(0.0, self.pre_wait.max(0.0))
    }

    /// Seconds since the action started (0 during pre-wait).
    pub fn action_elapsed(&self, now: Instant) -> f64 {
        let now = self.effective_now(now);
        if now <= self.action_at { 0.0 } else { (now - self.action_at).as_secs_f64() }
    }

    /// Still waiting out the pre-wait.
    pub fn in_pre_wait(&self, now: Instant) -> bool {
        !self.started || self.effective_now(now) < self.action_at
    }

    /// Paused by the user.
    pub fn is_paused(&self) -> bool {
        self.paused_at.is_some()
    }
}

/// A running cue as seen from outside the engine (what remote replicas are sent).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct InstanceSnapshot {
    /// Cue id.
    pub id: CueId,
    /// Type of the cue.
    pub kind: KindTag,
    /// Pre-wait length in seconds.
    pub pre_wait: f64,
    /// Seconds of pre-wait already elapsed.
    pub pre_wait_elapsed: f64,
    /// Seconds since the action started.
    pub action_elapsed: f64,
    /// Paused.
    pub paused: bool,
    /// Action started.
    pub started: bool,
    /// Action finished.
    pub action_done: bool,
    /// Known action length in seconds.
    pub action_duration: Option<f64>,
    /// Auto-continue handled.
    pub continue_done: bool,
    /// Group that launched it.
    pub parent: Option<CueId>,
}

/// Longest time the engine schedules, seconds (about 30 years).
const MAX_SECS: f64 = 1e9;

/// `secs` as a duration that can't panic or overflow clock arithmetic: negative and NaN give 0,
/// anything huge (from a damaged show file or a replica) is capped at about 30 years.
pub fn duration(secs: f64) -> Duration {
    Duration::from_secs_f64(if secs.is_nan() { 0.0 } else { secs.clamp(0.0, MAX_SECS) })
}

fn before(t: Instant, secs: f64) -> Instant {
    t.checked_sub(duration(secs)).unwrap_or(t)
}

/// Seconds from `now` until `t` (negative if `t` is in the past).
fn secs_between(now: Instant, t: Instant) -> f64 {
    if t >= now { (t - now).as_secs_f64() } else { -(now - t).as_secs_f64() }
}

fn after(t: Instant, secs: f64) -> Instant {
    t.checked_add(duration(secs)).unwrap_or(t)
}

/// The cue playback state machine. Pure logic: all media work goes through a [`Backend`].
#[derive(Default)]
pub struct Engine {
    /// Cue that GO fires next.
    pub playhead: Option<CueId>,
    /// Running cues by id.
    pub instances: HashMap<CueId, Instance>,
    /// Recent warnings and errors, oldest first.
    pub log: Vec<String>,
    last_go: Option<Instant>,
    preload_dirty: bool,
    panic: Option<(Instant, Vec<CueId>)>,
    /// Groups with running children, kept up to date during a tick pass.
    busy_parents: HashSet<CueId>,
    /// How often each cue was fired during the current tick (loop detection).
    fired_in_tick: HashMap<CueId, u32>,
    /// Cues already reported as looping (so the log isn't flooded).
    loops_reported: HashSet<CueId>,
    /// Changes Arm/Disarm/Target cues made, not yet written into the workspace.
    edits: Vec<CueEdit>,
    /// Arm states and targets set by cues, in force until the workspace has them.
    armed_now: HashMap<CueId, bool>,
    targets_now: HashMap<CueId, Option<CueId>>,
    /// What cues were before cues changed them.
    originals: HashMap<CueId, Original>,
    /// Where Load cues put cues: seconds into the cue (pre-wait included) its next start begins.
    loaded: HashMap<CueId, f64>,
    /// Seconds into the action a loaded cue starts at, between its fire and its action.
    offsets: HashMap<CueId, f64>,
}

impl Engine {
    /// An idle engine.
    pub fn new() -> Self {
        Self { preload_dirty: true, ..Default::default() }
    }

    /// True while any cue is running.
    pub fn is_active(&self) -> bool {
        !self.instances.is_empty()
    }

    /// True while a Stop-all fade is in progress (a second Stop stops at once).
    pub fn is_panicking(&self) -> bool {
        self.panic.is_some()
    }

    /// Running cues for remote replicas; `media_elapsed` gives the exact media position where
    /// known.
    pub fn snapshot(&self, now: Instant, media_elapsed: impl Fn(CueId) -> Option<f64>) -> Vec<InstanceSnapshot> {
        let mut v: Vec<InstanceSnapshot> = self
            .instances
            .iter()
            .map(|(id, i)| InstanceSnapshot {
                id: *id,
                kind: i.kind,
                pre_wait: i.pre_wait,
                pre_wait_elapsed: i.pre_wait_elapsed(now),
                action_elapsed: if i.in_pre_wait(now) { 0.0 } else { media_elapsed(*id).unwrap_or_else(|| i.action_elapsed(now)) },
                paused: i.is_paused(),
                started: i.started,
                action_done: i.action_done,
                action_duration: i.action_duration,
                continue_done: i.continue_done,
                parent: i.parent,
            })
            .collect();
        v.sort_by_key(|s| s.id);
        v
    }

    /// Make this engine a display-only mirror of a remote one. Times are rebuilt against
    /// `now`, so progress keeps moving smoothly between updates. Never tick a mirrored engine.
    pub fn mirror(&mut self, playhead: Option<CueId>, panicking: bool, instances: &[InstanceSnapshot], log: Vec<String>, now: Instant) {
        self.playhead = playhead;
        self.panic = panicking.then(|| (now, Vec::new()));
        self.log = log;
        self.instances = instances
            .iter()
            .map(|s| {
                // The main device's times, checked: a negative or NaN pre-wait made
                // `pre_wait_elapsed` panic.
                let pre_wait = if s.pre_wait.is_finite() { s.pre_wait.clamp(0.0, crate::limits::MAX_TIME) } else { 0.0 };
                let action_at = if s.started && s.pre_wait_elapsed >= pre_wait {
                    before(now, s.action_elapsed)
                } else {
                    after(now, pre_wait - s.pre_wait_elapsed)
                };
                let inst = Instance {
                    kind: s.kind,
                    fired_at: before(action_at, pre_wait),
                    action_at,
                    pre_wait,
                    paused_at: s.paused.then_some(now),
                    started: s.started,
                    action_done: s.action_done,
                    action_duration: s.action_duration,
                    continue_done: s.continue_done,
                    parent: s.parent,
                    fade: None,
                };
                (s.id, inst)
            })
            .collect();
    }

    /// Move the playhead (re-evaluating what to preload).
    pub fn set_playhead(&mut self, id: Option<CueId>) {
        if self.playhead != id {
            self.playhead = id;
            self.preload_dirty = true;
        }
    }

    /// Changes cues made to other cues since the last call (Arm, Disarm, Target cues). The caller
    /// writes them into the workspace; the engine acts on them until then.
    pub fn take_edits(&mut self) -> Vec<CueEdit> {
        self.armed_now.clear();
        self.targets_now.clear();
        std::mem::take(&mut self.edits)
    }

    /// Forget what cues changed and loaded (another show was opened).
    pub fn forget_cue_changes(&mut self) {
        self.edits.clear();
        self.armed_now.clear();
        self.targets_now.clear();
        self.originals.clear();
        self.loaded.clear();
        self.offsets.clear();
    }

    /// Seconds into the cue a Load cue put it at (its next start begins there).
    pub fn loaded_at(&self, id: CueId) -> Option<f64> {
        self.loaded.get(&id).copied()
    }

    /// Whether the cue is armed, counting Arm/Disarm cues that just ran.
    pub fn is_armed(&self, cue: &Cue) -> bool {
        self.armed_now.get(&cue.id).copied().unwrap_or(cue.armed)
    }

    /// Call after the workspace was edited so preloaded media gets refreshed.
    pub fn workspace_changed(&mut self) {
        self.preload_dirty = true;
        self.loops_reported.clear();
    }

    /// Add a warning to the engine log (shown in View → Engine log).
    pub fn report(&mut self, msg: impl Into<String>) {
        self.log(msg);
    }

    fn log(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        log::warn!("{msg}");
        self.log.push(msg);
        if self.log.len() > MAX_LOG {
            self.log.remove(0);
        }
    }

    // ---------------------------------------------------------------- transport

    /// Fire the cue at the playhead and advance the playhead.
    pub fn go(&mut self, ws: &Workspace, be: &mut dyn Backend, now: Instant) {
        let protect = ws.settings.double_go_protection;
        if let Some(last) = self.last_go
            && protect > 0.0
            && (now - last).as_secs_f64() < protect
        {
            return;
        }
        let Some(id) = self.playhead else { return };
        let Some(cue) = ws.cue_list.find(id) else {
            self.set_playhead(None);
            return;
        };
        self.last_go = Some(now);

        let parent = ws.cue_list.parent_of(id).filter(|p| self.instances.contains_key(p));
        self.fire(ws, be, id, now, parent);

        let next = match &cue.kind {
            CueKind::Group { mode: GroupMode::StartFirstAndEnter, children } if !children.is_empty() => {
                after_chain(ws, children[0].id)
            }
            _ => after_chain(ws, id),
        };
        self.set_playhead(next);
        self.tick(ws, be, now);
    }

    /// Fire a specific cue right now without moving the playhead (e.g. from a Start cue or UI).
    pub fn start_cue(&mut self, ws: &Workspace, be: &mut dyn Backend, id: CueId, now: Instant) {
        self.fire(ws, be, id, now, None);
        self.tick(ws, be, now);
    }

    fn fire(&mut self, ws: &Workspace, be: &mut dyn Backend, id: CueId, at: Instant, parent: Option<CueId>) {
        let Some(cue) = ws.cue_list.find(id) else { return };
        let n = self.fired_in_tick.entry(id).or_insert(0);
        *n += 1;
        if *n > MAX_FIRES_PER_TICK {
            if self.loops_reported.insert(id) {
                self.log(format!(
                    "Cue {} '{}' restarts itself with no time in between (a Start or Goto loop?); held off",
                    cue.number,
                    cue.display_name()
                ));
            }
            return;
        }
        if self.instances.contains_key(&id) {
            // Re-firing a running cue restarts it.
            self.stop_cue(ws, be, id);
        }
        let mut pre = cue.pre_wait.max(0.0);
        self.offsets.remove(&id);
        if let Some(off) = self.loaded.remove(&id) {
            // A Load cue put it part way through: skip that much of the pre-wait, then the action.
            let skip = off.min(pre);
            pre -= skip;
            if off - skip > 0.0 {
                self.offsets.insert(id, off - skip);
            }
        }
        // What this cue continues into may need preloading now.
        self.preload_dirty = true;
        if let Some(p) = parent {
            self.busy_parents.insert(p);
        }
        self.instances.insert(
            id,
            Instance {
                kind: cue.kind.tag(),
                fired_at: at,
                action_at: after(at, pre),
                pre_wait: pre,
                paused_at: None,
                started: false,
                action_done: false,
                action_duration: None,
                continue_done: false,
                parent,
                fade: None,
            },
        );
    }

    /// Ids of `id` plus every running instance launched (transitively) by it, plus any
    /// running tree descendants.
    fn instance_subtree(&self, ws: &Workspace, id: CueId) -> Vec<CueId> {
        // Linear in the running instances (stopping a group of thousands must stay quick).
        let tree: HashSet<CueId> = ws.cue_list.descendants(id).into_iter().collect();
        let mut launched: HashMap<CueId, Vec<CueId>> = HashMap::new();
        for (k, inst) in &self.instances {
            if let Some(p) = inst.parent {
                launched.entry(p).or_default().push(*k);
            }
        }
        let mut seen = HashSet::from([id]);
        let mut out = vec![id];
        for k in self.instances.keys() {
            if tree.contains(k) && seen.insert(*k) {
                out.push(*k);
            }
        }
        let mut i = 0;
        while i < out.len() {
            if let Some(children) = launched.get(&out[i]) {
                for k in children {
                    if seen.insert(*k) {
                        out.push(*k);
                    }
                }
            }
            i += 1;
        }
        out
    }

    /// Ids of `id` and all groups enclosing it (in the tree or via launching parent).
    fn ancestors(&self, ws: &Workspace, id: CueId) -> HashSet<CueId> {
        let mut out = HashSet::new();
        let mut stack = vec![id];
        while let Some(c) = stack.pop() {
            if !out.insert(c) {
                continue;
            }
            if let Some(p) = ws.cue_list.parent_of(c) {
                stack.push(p);
            }
            if let Some(p) = self.instances.get(&c).and_then(|i| i.parent) {
                stack.push(p);
            }
        }
        out
    }

    /// Stop one cue, its children and anything it is fading.
    pub fn stop_cue(&mut self, ws: &Workspace, be: &mut dyn Backend, id: CueId) {
        for k in self.instance_subtree(ws, id) {
            if let Some(inst) = self.instances.remove(&k)
                && inst.kind.is_media()
            {
                be.stop(k);
            }
        }
    }

    fn stop_all_except(&mut self, be: &mut dyn Backend, keep: &HashSet<CueId>) {
        let ids: Vec<CueId> = self.instances.keys().copied().filter(|k| !keep.contains(k)).collect();
        for k in ids {
            if let Some(inst) = self.instances.remove(&k)
                && inst.kind.is_media()
            {
                be.stop(k);
            }
        }
    }

    /// Panic: fade everything out over `fade` seconds then stop. A second call while fading
    /// (or `fade == 0`) stops immediately.
    pub fn stop_all(&mut self, be: &mut dyn Backend, now: Instant, fade: f64) {
        if fade <= 0.0 || self.panic.is_some() {
            self.panic = None;
            self.stop_all_except(be, &HashSet::new());
            return;
        }
        let mut media = Vec::new();
        let ids: Vec<CueId> = self.instances.keys().copied().collect();
        for k in ids {
            let inst = self.instances.get_mut(&k).unwrap();
            inst.continue_done = true;
            let is_media = inst.kind.is_media();
            if is_media && inst.started && !inst.action_done {
                media.push(k);
            } else {
                let inst = self.instances.remove(&k).unwrap();
                if is_media {
                    be.stop(k);
                }
                let _ = inst;
            }
        }
        let req = FadeRequest {
            volume_db: Some(MIN_DB),
            opacity: Some(0.0),
            geometry: None,
            objects: None,
            video_fx: None,
            out_levels: None,
            audio_fx: None,
            duration: fade,
            curve: FadeCurve::Linear,
        };
        for k in &media {
            if let Some(inst) = self.instances.get_mut(k) {
                if inst.paused_at.is_some() {
                    // Nothing to hear; stop paused media right away.
                    self.instances.remove(k);
                    be.stop(*k);
                    continue;
                }
                inst.parent = None;
                be.fade(*k, &req, now);
            }
        }
        self.panic = Some((after(now, fade), media));
    }

    /// Pause one running cue.
    pub fn pause_cue(&mut self, ws: &Workspace, be: &mut dyn Backend, id: CueId, now: Instant) {
        for k in self.instance_subtree(ws, id) {
            self.pause_instance(be, k, now);
        }
    }

    /// Resume one paused cue.
    pub fn resume_cue(&mut self, ws: &Workspace, be: &mut dyn Backend, id: CueId, now: Instant) {
        for k in self.instance_subtree(ws, id) {
            self.resume_instance(be, k, now);
        }
    }

    /// Pause every running cue.
    pub fn pause_all(&mut self, be: &mut dyn Backend, now: Instant) {
        let ids: Vec<CueId> = self.instances.keys().copied().collect();
        for k in ids {
            self.pause_instance(be, k, now);
        }
    }

    /// Resume every paused cue.
    pub fn resume_all(&mut self, be: &mut dyn Backend, now: Instant) {
        let ids: Vec<CueId> = self.instances.keys().copied().collect();
        for k in ids {
            self.resume_instance(be, k, now);
        }
    }

    fn pause_instance(&mut self, be: &mut dyn Backend, id: CueId, now: Instant) {
        let Some(inst) = self.instances.get_mut(&id) else { return };
        if inst.paused_at.is_some() {
            return;
        }
        inst.paused_at = Some(now);
        if inst.started && inst.kind.is_media() {
            be.pause(id);
        }
        if inst.started
            && !inst.action_done
            && now >= inst.action_at
            && let Some((targets, _)) = &inst.fade
        {
            for t in targets {
                be.hold_fade(*t);
            }
        }
    }

    fn resume_instance(&mut self, be: &mut dyn Backend, id: CueId, now: Instant) {
        let Some(inst) = self.instances.get_mut(&id) else { return };
        let Some(p) = inst.paused_at.take() else { return };
        let shift = now.saturating_duration_since(p);
        let elapsed = inst.action_elapsed(p);
        let fading = inst.started && !inst.action_done && p >= inst.action_at;
        inst.fired_at += shift;
        inst.action_at += shift;
        if inst.started && inst.kind.is_media() {
            be.resume(id, now);
        }
        // Continue a paused fade from where its targets were frozen, over the remaining time.
        if fading && let Some((targets, req)) = &inst.fade {
            let mut rest = req.clone();
            rest.duration = (req.duration - elapsed).max(0.0);
            for t in targets {
                be.fade(*t, &rest, now);
            }
        }
    }

    // ---------------------------------------------------------------- tick

    /// Advance time: end pre-waits, start actions, finish fades and waits, handle continues.
    pub fn tick(&mut self, ws: &Workspace, be: &mut dyn Backend, now: Instant) {
        self.fired_in_tick.clear();
        be.update(now);
        for e in be.drain_errors() {
            self.log(e);
        }

        if let Some((until, ids)) = &self.panic
            && now >= *until
        {
            let ids = ids.clone();
            self.panic = None;
            for k in ids {
                if self.instances.remove(&k).is_some() {
                    be.stop(k);
                }
            }
        }

        let horizon = now + be.lookahead();
        // Passes over the running cues until nothing changes (bounded for safety).
        for _ in 0..1_000 {
            if !self.pass(ws, be, now, horizon) {
                break;
            }
        }

        if !self.preload_dirty && be.preload_retry(now) {
            self.preload_dirty = true;
        }
        if self.preload_dirty {
            self.preload_dirty = false;
            // What running cues will continue into fires on its own, possibly within
            // milliseconds, so it comes first; then what the next GO fires.
            let mut running: Vec<(Instant, CueId)> = self
                .instances
                .iter()
                .filter(|(_, i)| !i.continue_done)
                .filter_map(|(id, i)| {
                    let c = ws.cue_list.find(*id)?;
                    (c.continue_mode != crate::model::ContinueMode::DoNotContinue).then_some((i.fired_at, *id))
                })
                .collect();
            running.sort();
            let mut seeds: Vec<CueId> = running.into_iter().filter_map(|(_, id)| ws.cue_list.next_sibling(id)).collect();
            seeds.extend(self.playhead);
            let ids = preload_set(ws, &seeds);
            let cues: Vec<&Cue> = ids
                .iter()
                .filter(|id| !self.instances.contains_key(id))
                .filter_map(|id| ws.cue_list.find(*id))
                .collect();
            be.set_preloaded(&cues);
        }
    }

    /// One pass over the running cues, earliest-fired first, moving each as far as it can go
    /// now. Returns whether anything changed (another pass may then be needed: a cue may have
    /// fired others, or finished the last child of a group seen earlier in the pass).
    /// Sorting once per pass instead of once per transition keeps big shows (thousands of
    /// running cues) to a fraction of a millisecond per tick.
    fn pass(&mut self, ws: &Workspace, be: &mut dyn Backend, now: Instant, horizon: Instant) -> bool {
        let mut ids: Vec<(Instant, CueId)> = self.instances.iter().map(|(k, i)| (i.fired_at, *k)).collect();
        ids.sort_unstable();
        // Groups with running children. Children that end during the pass leave their group
        // listed until the next pass; children fired during the pass are added by `fire`.
        self.busy_parents = self.instances.values().filter_map(|i| i.parent).collect();
        let mut changed = false;
        for (_, id) in ids {
            for _ in 0..100 {
                if !self.advance(ws, be, now, horizon, id) {
                    break;
                }
                changed = true;
            }
        }
        changed
    }

    /// Make one state transition of cue `id`, if one is due. Returns whether it did.
    fn advance(
        &mut self,
        ws: &Workspace,
        be: &mut dyn Backend,
        now: Instant,
        horizon: Instant,
        id: CueId,
    ) -> bool {
        {
            let Some(inst) = self.instances.get(&id) else { return false };
            let Some(cue) = ws.cue_list.find(id) else {
                // Cue deleted while running (paused or not).
                let inst = self.instances.remove(&id).unwrap();
                if inst.kind.is_media() {
                    be.stop(id);
                }
                return true;
            };
            if inst.paused_at.is_some() {
                return false;
            }

            if !inst.started {
                if inst.action_at <= horizon {
                    let at = inst.action_at;
                    self.instances.get_mut(&id).unwrap().started = true;
                    let dur = self.start_action(ws, be, cue, at, now);
                    if let Some(inst) = self.instances.get_mut(&id) {
                        inst.action_duration = dur;
                    }
                    return true;
                }
                return false;
            }

            if !inst.action_done {
                let done = match inst.kind {
                    _ if !self.is_armed(cue) => true,
                    k if k.is_media() => be.is_finished(id),
                    KindTag::Group => {
                        inst.action_at <= now && !self.busy_parents.contains(&id)
                    }
                    _ => match inst.action_duration {
                        Some(d) => now >= after(inst.action_at, d),
                        None => true,
                    },
                };
                if done {
                    self.instances.get_mut(&id).unwrap().action_done = true;
                    self.on_action_complete(ws, be, cue);
                    return true;
                }
            }

            let inst = &self.instances[&id];
            if !inst.continue_done {
                let due = match cue.continue_mode {
                    crate::model::ContinueMode::DoNotContinue => None,
                    crate::model::ContinueMode::AutoContinue => Some(after(inst.action_at, cue.post_wait)),
                    crate::model::ContinueMode::AutoFollow => match inst.action_duration {
                        Some(d) => Some(after(inst.action_at, d)),
                        None if inst.action_done => Some(now),
                        // Waits for the media to report its end.
                        None => return false,
                    },
                };
                match due {
                    None => {
                        self.instances.get_mut(&id).unwrap().continue_done = true;
                        return true;
                    }
                    Some(t) if t <= horizon => {
                        let parent = inst.parent;
                        self.instances.get_mut(&id).unwrap().continue_done = true;
                        if let Some(next) = ws.cue_list.next_sibling(id) {
                            self.fire(ws, be, next, t, parent);
                        }
                        return true;
                    }
                    Some(_) => {}
                }
            }

            let inst = &self.instances[&id];
            if inst.action_done && inst.continue_done {
                let inst = self.instances.remove(&id).unwrap();
                if inst.kind.is_media() {
                    // Release the finished player/voice.
                    be.stop(id);
                }
                return true;
            }
        }
        false
    }

    /// Run a cue's action. Returns the action duration if known.
    fn start_action(
        &mut self,
        ws: &Workspace,
        be: &mut dyn Backend,
        cue: &Cue,
        at: Instant,
        now: Instant,
    ) -> Option<f64> {
        let offset = self.offsets.remove(&cue.id).unwrap_or(0.0);
        if !self.is_armed(cue) {
            return Some(0.0);
        }
        // A Target cue that just ran may have pointed this cue elsewhere.
        let retargeted;
        let cue = match self.targets_now.get(&cue.id) {
            Some(t) => {
                let mut c = cue.shallow_clone();
                if let Some(x) = c.kind.target_mut() {
                    *x = *t;
                }
                retargeted = c;
                &retargeted
            }
            None => cue,
        };
        match &cue.kind {
            CueKind::Audio(_)
            | CueKind::Video(_)
            | CueKind::Text(_)
            | CueKind::Mic(_)
            | CueKind::Camera(_)
            | CueKind::Timecode(_) => {
                let started = if offset > 0.0 { be.start_from(cue, at, offset) } else { be.start(cue, at) };
                match started {
                    Ok(()) => be.duration(cue).map(|d| (d - offset).max(0.0)),
                    Err(e) => {
                        self.log(format!("Cue {} '{}': {e}", cue.number, cue.display_name()));
                        Some(0.0)
                    }
                }
            }
            CueKind::Wait { duration } => Some((duration - offset).max(0.0)),
            CueKind::Memo => Some(0.0),
            CueKind::Fade(f) => {
                let Some(target) = f.target else {
                    self.log(format!("Fade cue {} has no target", cue.number));
                    return Some(0.0);
                };
                let req = FadeRequest {
                    volume_db: f.volume_db,
                    opacity: f.opacity,
                    geometry: f.geometry,
                    objects: f.objects.clone(),
                    video_fx: f.video_fx.clone(),
                    out_levels: f.out_levels.clone(),
                    audio_fx: f.audio_fx.clone(),
                    duration: (f.duration - offset).max(0.0),
                    curve: f.curve,
                };
                let targets: Vec<CueId> = self
                    .instance_subtree(ws, target)
                    .into_iter()
                    .filter(|k| {
                        self.instances
                            .get(k)
                            .is_some_and(|i| i.kind.is_media())
                    })
                    .collect();
                if targets.is_empty() {
                    self.log(format!("Fade cue {}: target is not running", cue.number));
                }
                for t in &targets {
                    be.fade(*t, &req, at);
                }
                let d = req.duration;
                if let Some(inst) = self.instances.get_mut(&cue.id) {
                    inst.fade = Some((targets, req));
                }
                Some(d)
            }
            CueKind::Start { target } => {
                if let Some(t) = target {
                    self.fire(ws, be, *t, at, None);
                }
                Some(0.0)
            }
            CueKind::Stop { target } => {
                match target {
                    Some(t) => {
                        self.release_light_cues(ws, be, *t, at);
                        self.stop_cue(ws, be, *t)
                    }
                    None => {
                        let keep = self.ancestors(ws, cue.id);
                        self.stop_all_except(be, &keep);
                        if ws.lighting.stop_all_releases {
                            let all = LightingCue { action: crate::model::LightAction::Release, fade: ws.lighting.release_fade, ..Default::default() };
                            let _ = be.light(&all, &ws.lighting, at);
                        }
                    }
                }
                Some(0.0)
            }
            CueKind::Pause { target } => {
                match target {
                    Some(t) => self.pause_cue(ws, be, *t, now),
                    None => {
                        let keep = self.ancestors(ws, cue.id);
                        let ids: Vec<CueId> =
                            self.instances.keys().copied().filter(|k| !keep.contains(k)).collect();
                        for k in ids {
                            self.pause_instance(be, k, now);
                        }
                    }
                }
                Some(0.0)
            }
            CueKind::Goto { target } => {
                self.set_playhead(*target);
                Some(0.0)
            }
            CueKind::Devamp { target } => {
                let Some(target) = target else {
                    self.log(format!("Devamp cue {} has no target", cue.number));
                    return Some(0.0);
                };
                let hit = self
                    .instance_subtree(ws, *target)
                    .into_iter()
                    .filter(|k| self.instances.get(k).is_some_and(|i| i.kind == KindTag::Audio))
                    // Every vamping voice is released (no `any`: it would stop at the first).
                    .filter(|&k| be.devamp(k))
                    .count()
                    > 0;
                if !hit {
                    self.log(format!("Devamp cue {}: target is not playing", cue.number));
                }
                Some(0.0)
            }
            CueKind::Network(net) => {
                if let Err(e) = be.send_network(net) {
                    self.log(format!("Network cue {}: {e}", cue.number));
                }
                Some(0.0)
            }
            CueKind::Arm { target } | CueKind::Disarm { target } => {
                let arm = matches!(cue.kind, CueKind::Arm { .. });
                match target.and_then(|t| ws.cue_list.find(t)) {
                    Some(tc) => {
                        let was = self.is_armed(tc);
                        self.originals.entry(tc.id).or_default().armed.get_or_insert(was);
                        self.armed_now.insert(tc.id, arm);
                        self.edits.push(CueEdit::Armed(tc.id, arm));
                        self.preload_dirty = true;
                    }
                    None => self.log(format!("{} cue {} has no target", cue.kind.tag().label(), cue.number)),
                }
                Some(0.0)
            }
            CueKind::Target { target, new_target } => {
                match target.and_then(|t| ws.cue_list.find(t)) {
                    Some(tc) if tc.kind.takes_target() => {
                        let was = self.targets_now.get(&tc.id).copied().unwrap_or(tc.kind.target());
                        self.originals.entry(tc.id).or_default().target.get_or_insert(was);
                        self.targets_now.insert(tc.id, *new_target);
                        self.edits.push(CueEdit::Target(tc.id, *new_target));
                    }
                    Some(_) => self.log(format!("Target cue {}: its target doesn't take a target", cue.number)),
                    None => self.log(format!("Target cue {} has no target", cue.number)),
                }
                Some(0.0)
            }
            CueKind::Load { target, time } => {
                match target.filter(|t| ws.cue_list.find(*t).is_some()) {
                    Some(t) => {
                        self.loaded.insert(t, time.max(0.0));
                        // A running cue jumps there.
                        if self.instances.contains_key(&t) {
                            self.fire(ws, be, t, at, None);
                        }
                    }
                    None => self.log(format!("Load cue {} has no target", cue.number)),
                }
                Some(0.0)
            }
            CueKind::Reset { target } => {
                let Some(t) = target.filter(|t| ws.cue_list.find(*t).is_some()) else {
                    self.log(format!("Reset cue {} has no target", cue.number));
                    return Some(0.0);
                };
                self.release_light_cues(ws, be, t, at);
                self.stop_cue(ws, be, t);
                let mut ids = ws.cue_list.descendants(t);
                ids.push(t);
                for id in ids {
                    self.loaded.remove(&id);
                    self.offsets.remove(&id);
                    let Some(o) = self.originals.remove(&id) else { continue };
                    if let Some(a) = o.armed {
                        self.armed_now.insert(id, a);
                        self.edits.push(CueEdit::Armed(id, a));
                    }
                    if let Some(tg) = o.target {
                        self.targets_now.insert(id, tg);
                        self.edits.push(CueEdit::Target(id, tg));
                    }
                }
                self.preload_dirty = true;
                Some(0.0)
            }
            CueKind::Midi(midi) => {
                if let Err(e) = be.send_midi(midi) {
                    self.log(format!("MIDI cue {}: {e}", cue.number));
                }
                Some(0.0)
            }
            CueKind::Light(l) => match be.light(l, &ws.lighting, at) {
                Ok(d) => Some(d),
                Err(e) => {
                    self.log(format!("Light cue {}: {e}", cue.number));
                    Some(0.0)
                }
            },
            CueKind::Group { mode, children } => {
                if children.is_empty() {
                    return Some(0.0);
                }
                match mode {
                    GroupMode::StartAll => {
                        for c in children {
                            self.fire(ws, be, c.id, at, Some(cue.id));
                        }
                    }
                    GroupMode::StartFirst | GroupMode::StartFirstAndEnter => {
                        self.fire(ws, be, children[0].id, at, Some(cue.id));
                    }
                    GroupMode::StartRandom => {
                        let i = fastrand::usize(..children.len());
                        self.fire(ws, be, children[i].id, at, Some(cue.id));
                    }
                }
                None
            }
        }
    }

    /// A Stop cue aimed at a Light cue (or a group holding some) releases the sequences those
    /// cues play, over the design's release fade.
    fn release_light_cues(&mut self, ws: &Workspace, be: &mut dyn Backend, target: CueId, at: Instant) {
        fn walk(c: &Cue, out: &mut Vec<crate::light::model::SequenceId>, ws: &Workspace) {
            match &c.kind {
                CueKind::Light(l) if l.action == crate::model::LightAction::Go => {
                    if let Some((s, _)) = l.cue.and_then(|id| ws.lighting.find_cue(id)) {
                        out.push(s.id);
                    }
                }
                CueKind::Group { children, .. } => children.iter().for_each(|ch| walk(ch, out, ws)),
                _ => {}
            }
        }
        let Some(c) = ws.cue_list.find(target) else { return };
        let mut seqs = Vec::new();
        walk(c, &mut seqs, ws);
        seqs.sort();
        seqs.dedup();
        for s in seqs {
            let rel = LightingCue { action: crate::model::LightAction::Release, sequence: Some(s), fade: ws.lighting.release_fade, ..Default::default() };
            if let Err(e) = be.light(&rel, &ws.lighting, at) {
                self.log(format!("Stop cue: {e}"));
            }
        }
    }

    fn on_action_complete(&mut self, ws: &Workspace, be: &mut dyn Backend, cue: &Cue) {
        if self.is_armed(cue)
            && let CueKind::Fade(f) = &cue.kind
            && f.stop_target_when_done
            && let Some(t) = self.targets_now.get(&cue.id).copied().unwrap_or(f.target)
        {
            self.stop_cue(ws, be, t);
        }
    }
}

/// The cue the playhead moves to after firing `id`: skip everything `id` triggers through its
/// continue chain, stepping out of groups at the end of their lists.
/// A loop, with a step budget: in a damaged tree (two cues with one id) the walk could otherwise
/// go round forever or overflow the stack.
pub fn after_chain(ws: &Workspace, id: CueId) -> Option<CueId> {
    let list = &ws.cue_list;
    let mut steps = 0usize;
    let mut cur = id;
    loop {
        loop {
            steps += 1;
            if steps > MAX_CHAIN_STEPS {
                return None;
            }
            let c = list.find(cur)?;
            if c.continue_mode != crate::model::ContinueMode::DoNotContinue
                && let Some(n) = list.next_sibling(cur)
            {
                cur = n;
                continue;
            }
            break;
        }
        match list.next_sibling(cur) {
            Some(n) => return Some(n),
            None => cur = list.parent_of(cur)?,
        }
    }
}

/// Most cues [`after_chain`] walks through.
const MAX_CHAIN_STEPS: usize = 1_000_000;

/// Cues to keep preloaded, starting from `seeds` (the cue the next GO fires, and the cues
/// running cues will continue into): each seed, the children it starts, the cue a Start cue
/// fires, and its own continue chain. At most [`MAX_PRELOAD`] cues.
fn preload_set(ws: &Workspace, seeds: &[CueId]) -> Vec<CueId> {
    let list = &ws.cue_list;
    let mut out = Vec::new();
    for &seed in seeds {
        let mut stack = vec![seed];
        while let Some(id) = stack.pop() {
            if out.len() >= MAX_PRELOAD {
                return out;
            }
            if out.contains(&id) {
                continue;
            }
            let Some(c) = list.find(id) else { continue };
            out.push(id);
            // Pushed in reverse order of firing, so the soonest is preloaded first.
            if c.continue_mode != crate::model::ContinueMode::DoNotContinue
                && let Some(n) = list.next_sibling(id)
            {
                stack.push(n);
            }
            if let CueKind::Start { target: Some(t) } = &c.kind {
                stack.push(*t);
            }
            for ch in c.children().iter().rev() {
                stack.push(ch.id);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wild times (a damaged show file, a replica's edit) neither panic nor overflow the clock.
    /// Times mirrored from a main device can't make the replica's engine panic (a negative
    /// pre-wait did, found by fuzzing).
    #[test]
    fn mirrored_times_are_checked() {
        let snap: InstanceSnapshot = serde_json::from_str(
            r#"{"action_done":false,"action_duration":0.5,"action_elapsed":0.1,"continue_done":true,
            "id":"ca7237c2-e61c-4eea-bf27-ce42930bd313","kind":"Audio","parent":null,"paused":false,
            "pre_wait":-1e-300,"pre_wait_elapsed":0.0,"started":true}"#,
        )
        .unwrap();
        let mut e = Engine::default();
        let now = Instant::now();
        for pre_wait in [-1e-300, -5.0, f64::NAN, f64::INFINITY, 1e300] {
            e.mirror(None, false, &[InstanceSnapshot { pre_wait, ..snap.clone() }], Vec::new(), now);
            let i = &e.instances[&snap.id];
            let _ = (i.pre_wait_elapsed(now), e.snapshot(now, |_| None));
        }
    }

    #[test]
    fn wild_times_are_safe() {
        let now = Instant::now();
        for s in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -5.0, 1e300, f64::MAX] {
            let _ = (after(now, s), before(now, s), duration(s));
        }
        assert_eq!(duration(f64::NAN), Duration::ZERO);
        assert_eq!(duration(-1.0), Duration::ZERO);
        assert_eq!(duration(1e300), Duration::from_secs_f64(MAX_SECS));
        assert!(after(now, 1e300) > now + Duration::from_secs(1 << 20));
    }
    use crate::model::{ContinueMode, CueKind, FadeCue, KindTag};

    #[derive(Default)]
    struct Mock {
        events: Vec<String>,
        started: HashMap<CueId, Instant>,
        finished: HashSet<CueId>,
        durations: HashMap<CueId, f64>,
        preloaded: Vec<CueId>,
        /// Times `set_preloaded` was called.
        preload_calls: usize,
        /// Answers to `preload_retry`, in order (then false).
        retries: Vec<bool>,
    }

    impl Backend for Mock {
        fn devamp(&mut self, id: CueId) -> bool {
            self.events.push(format!("devamp {id}"));
            true
        }
        fn hold_fade(&mut self, id: CueId) {
            self.events.push(format!("hold {id}"));
        }
        fn send_network(&mut self, n: &NetworkCue) -> Result<(), String> {
            self.events.push(format!("net {}", n.address));
            Ok(())
        }
        fn light(&mut self, c: &LightingCue, _design: &crate::light::Lighting, _at: Instant) -> Result<f64, String> {
            self.events.push(format!("light {:?} {:?} {:?}", c.action, c.sequence, c.cue.is_some()));
            Ok(0.0)
        }
        fn set_preloaded(&mut self, cues: &[&Cue]) {
            self.preloaded = cues.iter().map(|c| c.id).collect();
            self.preload_calls += 1;
        }
        fn preload_retry(&mut self, _now: Instant) -> bool {
            if self.retries.is_empty() { false } else { self.retries.remove(0) }
        }
        fn start_from(&mut self, cue: &Cue, at: Instant, offset: f64) -> Result<(), String> {
            self.events.push(format!("start {} from {offset}", cue.number));
            self.started.insert(cue.id, at);
            Ok(())
        }
        fn start(&mut self, cue: &Cue, at: Instant) -> Result<(), String> {
            self.events.push(format!("start {}", cue.number));
            self.started.insert(cue.id, at);
            Ok(())
        }
        fn stop(&mut self, id: CueId) {
            self.events.push(format!("stop {id}"));
            self.started.remove(&id);
        }
        fn pause(&mut self, _id: CueId) {
            self.events.push("pause".into());
        }
        fn resume(&mut self, _id: CueId, _at: Instant) {
            self.events.push("resume".into());
        }
        fn fade(&mut self, _id: CueId, f: &FadeRequest, _at: Instant) {
            self.events.push(format!("fade {:?}", f.volume_db));
        }
        fn is_finished(&self, id: CueId) -> bool {
            self.finished.contains(&id)
        }
        fn duration(&self, cue: &Cue) -> Option<f64> {
            self.durations.get(&cue.id).copied()
        }
        fn update(&mut self, _now: Instant) {}
    }

    fn cue(num: &str, kind: KindTag) -> Cue {
        let mut c = Cue::new(kind.default_kind());
        c.number = num.into();
        c
    }

    fn ws(cues: Vec<Cue>) -> Workspace {
        let mut w = Workspace::default();
        w.settings.double_go_protection = 0.0;
        w.cue_list.cues = cues;
        w
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn running_chains_keep_their_next_cues_preloaded() {
        // 1 → (auto-continue) 2 → 3, then a separate 4. After GO the playhead is on 4, but
        // 2 and 3 are still to come on their own and must stay preloaded.
        let mut a = cue("1", KindTag::Audio);
        a.continue_mode = ContinueMode::AutoContinue;
        a.post_wait = 5.0;
        let mut b = cue("2", KindTag::Audio);
        b.continue_mode = ContinueMode::AutoContinue;
        b.post_wait = 5.0;
        let c = cue("3", KindTag::Audio);
        let d = cue("4", KindTag::Audio);
        let (ai, bi, ci, di) = (a.id, b.id, c.id, d.id);
        let w = ws(vec![a, b, c, d]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        let t0 = Instant::now();
        e.set_playhead(Some(ai));
        e.tick(&w, &mut m, t0);
        assert_eq!(m.preloaded, vec![ai, bi, ci]);
        e.go(&w, &mut m, t0);
        assert_eq!(e.playhead, Some(di));
        assert_eq!(m.preloaded, vec![bi, ci, di]);
        e.tick(&w, &mut m, t0 + ms(5_000));
        assert_eq!(m.preloaded, vec![ci, di]);
    }

    /// The preload set is computed once per change, not every tick, unless the backend asks to
    /// try again (files it couldn't preload yet).
    #[test]
    fn preload_runs_on_changes_and_backend_retries_only() {
        let a = cue("1", KindTag::Audio);
        let ai = a.id;
        let w = ws(vec![a]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        let t0 = Instant::now();
        e.set_playhead(Some(ai));
        for i in 0..100 {
            e.tick(&w, &mut m, t0 + ms(i));
        }
        assert_eq!(m.preload_calls, 1, "preload recomputed on idle ticks");
        m.retries = vec![false, true, false];
        for i in 100..200 {
            e.tick(&w, &mut m, t0 + ms(i));
        }
        assert_eq!(m.preload_calls, 2);
        e.workspace_changed();
        e.tick(&w, &mut m, t0 + ms(200));
        assert_eq!(m.preload_calls, 3);
    }

    #[test]
    fn a_cue_that_starts_itself_is_held_off_and_reported() {
        let mut a = cue("1", KindTag::Start);
        a.kind = CueKind::Start { target: Some(a.id) };
        let ai = a.id;
        let w = ws(vec![a]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        let t0 = Instant::now();
        e.set_playhead(Some(ai));
        let t = Instant::now();
        e.go(&w, &mut m, t0);
        for i in 1..50 {
            e.tick(&w, &mut m, t0 + ms(i));
        }
        assert!(t.elapsed() < Duration::from_millis(50), "{:?}", t.elapsed());
        assert_eq!(e.log.iter().filter(|l| l.contains("restarts itself")).count(), 1, "{:?}", e.log);
    }

    #[test]
    fn go_fires_and_advances_playhead() {
        let a = cue("1", KindTag::Audio);
        let b = cue("2", KindTag::Audio);
        let (ai, bi) = (a.id, b.id);
        let w = ws(vec![a, b]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        let t0 = Instant::now();
        e.set_playhead(Some(ai));
        e.go(&w, &mut m, t0);
        assert_eq!(m.started.get(&ai), Some(&t0));
        assert_eq!(e.playhead, Some(bi));
        e.go(&w, &mut m, t0 + ms(10));
        assert_eq!(e.playhead, None);
        assert_eq!(m.events, vec!["start 1", "start 2"]);
    }

    #[test]
    fn pre_wait_and_auto_continue_use_logical_time() {
        let mut a = cue("1", KindTag::Audio);
        a.pre_wait = 1.0;
        a.post_wait = 2.0;
        a.continue_mode = ContinueMode::AutoContinue;
        let b = cue("2", KindTag::Audio);
        let c = cue("3", KindTag::Memo);
        let (ai, bi, ci) = (a.id, b.id, c.id);
        let w = ws(vec![a, b, c]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        let t0 = Instant::now();
        e.set_playhead(Some(ai));
        e.go(&w, &mut m, t0);
        // Playhead skips the auto-continued cue.
        assert_eq!(e.playhead, Some(ci));
        assert!(m.started.is_empty());

        // Within lookahead of the pre-wait end: start dispatched with the exact time.
        e.tick(&w, &mut m, t0 + ms(990));
        assert_eq!(m.started.get(&ai), Some(&(t0 + ms(1000))));

        // Ticks arrive late/jittery; b still starts at exactly 1s + 2s.
        e.tick(&w, &mut m, t0 + ms(2500));
        assert!(!m.started.contains_key(&bi));
        e.tick(&w, &mut m, t0 + ms(3010));
        assert_eq!(m.started.get(&bi), Some(&(t0 + ms(3000))));
    }

    #[test]
    fn auto_follow_uses_media_duration() {
        let mut a = cue("1", KindTag::Audio);
        a.continue_mode = ContinueMode::AutoFollow;
        let b = cue("2", KindTag::Audio);
        let (ai, bi) = (a.id, b.id);
        let w = ws(vec![a, b]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        m.durations.insert(ai, 4.0);
        let t0 = Instant::now();
        e.set_playhead(Some(ai));
        e.go(&w, &mut m, t0);
        e.tick(&w, &mut m, t0 + ms(3000));
        assert!(!m.started.contains_key(&bi));
        e.tick(&w, &mut m, t0 + ms(3990));
        assert_eq!(m.started.get(&bi), Some(&(t0 + ms(4000))));
    }

    #[test]
    fn auto_follow_unknown_duration_waits_for_finish() {
        let mut a = cue("1", KindTag::Audio);
        a.continue_mode = ContinueMode::AutoFollow;
        let b = cue("2", KindTag::Audio);
        let (ai, bi) = (a.id, b.id);
        let w = ws(vec![a, b]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        let t0 = Instant::now();
        e.set_playhead(Some(ai));
        e.go(&w, &mut m, t0);
        e.tick(&w, &mut m, t0 + ms(5000));
        assert!(!m.started.contains_key(&bi));
        m.finished.insert(ai);
        e.tick(&w, &mut m, t0 + ms(6000));
        assert!(m.started.contains_key(&bi));
        assert!(!e.instances.contains_key(&ai));
    }

    #[test]
    fn group_start_all_and_completion() {
        let x = cue("x", KindTag::Audio);
        let mut y = cue("y", KindTag::Wait);
        y.kind = CueKind::Wait { duration: 2.0 };
        let (xi, yi) = (x.id, y.id);
        let mut g = cue("G", KindTag::Group);
        g.kind = CueKind::Group { mode: GroupMode::StartAll, children: vec![x, y] };
        let gi = g.id;
        let after = cue("after", KindTag::Memo);
        let w = ws(vec![g, after]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        let t0 = Instant::now();
        e.set_playhead(Some(gi));
        e.go(&w, &mut m, t0);
        assert!(m.started.contains_key(&xi));
        assert!(e.instances.contains_key(&yi));
        assert_eq!(e.playhead, Some(w.cue_list.cues[1].id));

        m.finished.insert(xi);
        e.tick(&w, &mut m, t0 + ms(1000));
        assert!(e.instances.contains_key(&gi));
        e.tick(&w, &mut m, t0 + ms(2100));
        assert!(e.instances.is_empty());
    }

    #[test]
    fn group_enter_moves_playhead_inside_then_out() {
        let a = cue("a", KindTag::Memo);
        let b = cue("b", KindTag::Memo);
        let (ai, bi) = (a.id, b.id);
        let mut g = cue("G", KindTag::Group);
        g.kind = CueKind::Group { mode: GroupMode::StartFirstAndEnter, children: vec![a, b] };
        let gi = g.id;
        let z = cue("z", KindTag::Memo);
        let zi = z.id;
        let w = ws(vec![g, z]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        let t0 = Instant::now();
        e.set_playhead(Some(gi));
        e.go(&w, &mut m, t0);
        assert_eq!(e.playhead, Some(bi));
        e.go(&w, &mut m, t0 + ms(10));
        assert_eq!(e.playhead, Some(zi));
        let _ = ai;
    }

    #[test]
    fn playlist_group_chains_children() {
        let mut a = cue("a", KindTag::Audio);
        a.continue_mode = ContinueMode::AutoFollow;
        let b = cue("b", KindTag::Audio);
        let (ai, bi) = (a.id, b.id);
        let mut g = cue("G", KindTag::Group);
        g.kind = CueKind::Group { mode: GroupMode::StartFirst, children: vec![a, b] };
        let gi = g.id;
        let w = ws(vec![g]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        m.durations.insert(ai, 1.0);
        let t0 = Instant::now();
        e.set_playhead(Some(gi));
        e.go(&w, &mut m, t0);
        assert_eq!(e.playhead, None);
        e.tick(&w, &mut m, t0 + ms(1000));
        assert_eq!(m.started.get(&bi), Some(&(t0 + ms(1000))));
        assert_eq!(e.instances[&bi].parent, Some(gi));
    }

    #[test]
    fn pause_shifts_schedule() {
        let mut a = cue("1", KindTag::Wait);
        a.kind = CueKind::Wait { duration: 1.0 };
        a.continue_mode = ContinueMode::AutoFollow;
        let b = cue("2", KindTag::Audio);
        let (ai, bi) = (a.id, b.id);
        let w = ws(vec![a, b]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        let t0 = Instant::now();
        e.set_playhead(Some(ai));
        e.go(&w, &mut m, t0);
        e.pause_all(&mut m, t0 + ms(500));
        e.tick(&w, &mut m, t0 + ms(3000));
        assert!(!m.started.contains_key(&bi));
        e.resume_all(&mut m, t0 + ms(3000));
        e.tick(&w, &mut m, t0 + ms(3400));
        assert!(!m.started.contains_key(&bi));
        e.tick(&w, &mut m, t0 + ms(3500));
        assert_eq!(m.started.get(&bi), Some(&(t0 + ms(3500))));
    }

    #[test]
    fn fade_cue_stops_target_when_done() {
        let a = cue("1", KindTag::Audio);
        let ai = a.id;
        let mut f = cue("2", KindTag::Fade);
        f.kind = CueKind::Fade(FadeCue { target: Some(ai), duration: 2.0, ..Default::default() });
        let fi = f.id;
        let w = ws(vec![a, f]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        let t0 = Instant::now();
        e.set_playhead(Some(ai));
        e.go(&w, &mut m, t0);
        e.go(&w, &mut m, t0 + ms(1000));
        assert!(m.events.iter().any(|s| s.starts_with("fade")));
        e.tick(&w, &mut m, t0 + ms(2500));
        assert!(e.instances.contains_key(&ai));
        e.tick(&w, &mut m, t0 + ms(3000));
        assert!(!e.instances.contains_key(&ai));
        assert!(!e.instances.contains_key(&fi));
        assert!(m.events.contains(&format!("stop {ai}")));
    }

    #[test]
    fn pausing_a_fade_freezes_and_resumes_with_remaining_time() {
        let a = cue("1", KindTag::Audio);
        let ai = a.id;
        let mut f = cue("2", KindTag::Fade);
        f.kind = CueKind::Fade(FadeCue { target: Some(ai), duration: 4.0, ..Default::default() });
        let fi = f.id;
        let w = ws(vec![a, f]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        let t0 = Instant::now();
        e.set_playhead(Some(ai));
        e.go(&w, &mut m, t0);
        e.go(&w, &mut m, t0 + ms(10));
        e.pause_cue(&w, &mut m, fi, t0 + ms(1010));
        assert!(m.events.contains(&format!("hold {ai}")));
        m.events.clear();
        e.resume_cue(&w, &mut m, fi, t0 + ms(5000));
        // Re-issued with the 3 s that were left.
        assert_eq!(m.events, vec![format!("fade {:?}", Some(MIN_DB))]);
        e.tick(&w, &mut m, t0 + ms(7900));
        assert!(e.instances.contains_key(&fi));
        e.tick(&w, &mut m, t0 + ms(8001));
        assert!(!e.instances.contains_key(&fi));
    }

    #[test]
    fn devamp_and_network_cues() {
        let a = cue("1", KindTag::Audio);
        let ai = a.id;
        let mut d = cue("2", KindTag::Devamp);
        d.kind = CueKind::Devamp { target: Some(ai) };
        let mut n = cue("3", KindTag::Network);
        n.kind = CueKind::Network(NetworkCue { address: "/lights/go".into(), ..Default::default() });
        let w = ws(vec![a, d, n]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        let t0 = Instant::now();
        e.set_playhead(Some(ai));
        e.go(&w, &mut m, t0);
        e.go(&w, &mut m, t0 + ms(10));
        e.go(&w, &mut m, t0 + ms(20));
        assert!(m.events.contains(&format!("devamp {ai}")));
        assert!(m.events.contains(&"net /lights/go".to_string()));
        assert!(e.log.is_empty(), "{:?}", e.log);
    }

    /// Stop cues release the lighting their target Light cues play; a Stop without a target
    /// releases everything only when the design asks for it.
    #[test]
    fn stop_cues_release_lighting() {
        use crate::light::model::{LightCue, Sequence};
        use crate::model::{LightAction, LightingCue};
        let lc = LightCue::default();
        let mut l = cue("1", KindTag::Light);
        l.kind = CueKind::Light(LightingCue { action: LightAction::Go, cue: Some(lc.id), ..Default::default() });
        let li = l.id;
        let mut stop = cue("2", KindTag::Stop);
        stop.kind = CueKind::Stop { target: Some(li) };
        let stop_all = cue("3", KindTag::Stop);
        let mut w = ws(vec![l, stop, stop_all]);
        w.lighting.sequences.push(Sequence { id: 7, cues: vec![lc].into(), ..Default::default() });
        let mut e = Engine::new();
        let mut m = Mock::default();
        let t0 = Instant::now();
        e.set_playhead(Some(li));
        e.go(&w, &mut m, t0);
        e.go(&w, &mut m, t0 + ms(10));
        assert!(m.events.contains(&"light Go None true".to_string()), "{:?}", m.events);
        assert!(m.events.contains(&"light Release Some(7) false".to_string()), "{:?}", m.events);
        e.go(&w, &mut m, t0 + ms(20));
        assert_eq!(m.events.iter().filter(|x| x.starts_with("light Release None")).count(), 0, "off by default");
        w.lighting.stop_all_releases = true;
        let first = w.cue_list.cues[2].id;
        e.set_playhead(Some(first));
        e.go(&w, &mut m, t0 + ms(30));
        assert!(m.events.contains(&"light Release None false".to_string()));
    }

    /// Disarm and Target cues act at once (a cue they continue into sees the change), queue the
    /// change for the workspace, and a Reset cue puts back what they changed.
    #[test]
    fn arm_disarm_target_and_reset() {
        let a = cue("A", KindTag::Audio);
        let b = cue("B", KindTag::Audio);
        let (ai, bi) = (a.id, b.id);
        let mut dis = cue("1", KindTag::Disarm);
        dis.kind = CueKind::Disarm { target: Some(ai) };
        dis.continue_mode = ContinueMode::AutoContinue;
        let mut start_a = cue("2", KindTag::Start);
        start_a.kind = CueKind::Start { target: Some(ai) };
        let si = start_a.id;
        let mut retarget = cue("3", KindTag::Target);
        retarget.kind = CueKind::Target { target: Some(si), new_target: Some(bi) };
        retarget.continue_mode = ContinueMode::AutoContinue;
        let mut start_again = cue("4", KindTag::Start);
        start_again.kind = CueKind::Start { target: Some(si) };
        let mut reset = cue("5", KindTag::Reset);
        reset.kind = CueKind::Reset { target: Some(ai) };
        let mut reset_s = cue("6", KindTag::Reset);
        reset_s.kind = CueKind::Reset { target: Some(si) };
        let (di, ri, rsi) = (dis.id, reset.id, reset_s.id);
        let mut w = ws(vec![dis, start_a, retarget, start_again, reset, reset_s, a, b]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        let t0 = Instant::now();
        e.set_playhead(Some(di));
        e.go(&w, &mut m, t0);
        assert!(!m.events.iter().any(|x| x == "start A"), "A was disarmed before the Start cue ran: {:?}", m.events);
        let edits = e.take_edits();
        assert_eq!(edits, vec![CueEdit::Armed(ai, false)]);
        w.cue_list.find_mut(ai).unwrap().armed = false;
        // Target cue → the Start cue it continues into starts B.
        e.go(&w, &mut m, t0 + ms(10));
        assert!(m.events.iter().any(|x| x == "start B"), "{:?}", m.events);
        assert_eq!(e.take_edits(), vec![CueEdit::Target(si, Some(bi))]);
        w.cue_list.find_mut(si).unwrap().kind = CueKind::Start { target: Some(bi) };
        // Reset A: armed again. Reset the Start cue: aimed at A again.
        e.set_playhead(Some(ri));
        e.go(&w, &mut m, t0 + ms(20));
        assert_eq!(e.take_edits(), vec![CueEdit::Armed(ai, true)]);
        e.set_playhead(Some(rsi));
        e.go(&w, &mut m, t0 + ms(30));
        assert_eq!(e.take_edits(), vec![CueEdit::Target(si, Some(ai))]);
        // Nothing left to undo.
        e.set_playhead(Some(ri));
        e.go(&w, &mut m, t0 + ms(40));
        assert!(e.take_edits().is_empty());
    }

    /// A Load cue makes the target's next start skip its pre-wait and start the action part way in.
    #[test]
    fn load_starts_the_target_part_way_through() {
        let mut a = cue("A", KindTag::Audio);
        a.pre_wait = 0.5;
        let ai = a.id;
        let mut load = cue("1", KindTag::Load);
        load.kind = CueKind::Load { target: Some(ai), time: 2.0 };
        let li = load.id;
        let w = ws(vec![load, a]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        m.durations.insert(ai, 10.0);
        let t0 = Instant::now();
        e.set_playhead(Some(li));
        e.go(&w, &mut m, t0);
        assert_eq!(e.loaded_at(ai), Some(2.0));
        e.go(&w, &mut m, t0 + ms(10));
        assert!(m.events.iter().any(|x| x == "start A from 1.5"), "pre-wait skipped: {:?}", m.events);
        assert_eq!(e.instances[&ai].action_duration, Some(8.5));
        assert_eq!(e.loaded_at(ai), None, "used once");
        // Loading a running cue makes it jump there.
        e.set_playhead(Some(li));
        e.go(&w, &mut m, t0 + ms(20));
        assert_eq!(m.events.iter().filter(|x| *x == "start A from 1.5").count(), 2, "{:?}", m.events);
    }

    #[test]
    fn panic_fades_then_stops_and_second_press_is_immediate() {
        let a = cue("1", KindTag::Audio);
        let ai = a.id;
        let w = ws(vec![a]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        let t0 = Instant::now();
        e.set_playhead(Some(ai));
        e.go(&w, &mut m, t0);
        e.stop_all(&mut m, t0, 1.0);
        assert!(e.is_panicking());
        e.tick(&w, &mut m, t0 + ms(500));
        assert!(e.instances.contains_key(&ai));
        e.tick(&w, &mut m, t0 + ms(1001));
        assert!(e.instances.is_empty());

        e.set_playhead(Some(ai));
        e.go(&w, &mut m, t0 + ms(2000));
        e.stop_all(&mut m, t0 + ms(2000), 1.0);
        e.stop_all(&mut m, t0 + ms(2001), 1.0);
        assert!(e.instances.is_empty());
    }

    #[test]
    fn disarmed_cue_passes_continue() {
        let mut a = cue("1", KindTag::Audio);
        a.armed = false;
        a.continue_mode = ContinueMode::AutoFollow;
        let b = cue("2", KindTag::Audio);
        let bi = b.id;
        let ai = a.id;
        let w = ws(vec![a, b]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        e.set_playhead(Some(ai));
        e.go(&w, &mut m, Instant::now());
        assert!(!m.started.contains_key(&ai));
        assert!(m.started.contains_key(&bi));
    }

    #[test]
    fn double_go_protection() {
        let a = cue("1", KindTag::Memo);
        let b = cue("2", KindTag::Memo);
        let (ai, bi) = (a.id, b.id);
        let mut w = ws(vec![a, b]);
        w.settings.double_go_protection = 0.5;
        let mut e = Engine::new();
        let mut m = Mock::default();
        let t0 = Instant::now();
        e.set_playhead(Some(ai));
        e.go(&w, &mut m, t0);
        e.go(&w, &mut m, t0 + ms(100));
        assert_eq!(e.playhead, Some(bi));
    }

    #[test]
    fn stop_and_goto_cues() {
        let a = cue("1", KindTag::Audio);
        let ai = a.id;
        let mut s = cue("2", KindTag::Stop);
        s.kind = CueKind::Stop { target: None };
        let mut g = cue("3", KindTag::Goto);
        g.kind = CueKind::Goto { target: Some(ai) };
        let si = s.id;
        let w = ws(vec![a, s, g]);
        let mut e = Engine::new();
        let mut m = Mock::default();
        let t0 = Instant::now();
        e.set_playhead(Some(ai));
        e.go(&w, &mut m, t0);
        assert_eq!(e.playhead, Some(si));
        e.go(&w, &mut m, t0 + ms(10));
        assert!(!e.instances.contains_key(&ai));
        e.go(&w, &mut m, t0 + ms(20));
        assert_eq!(e.playhead, Some(ai));
    }
}
