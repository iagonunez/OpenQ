//! Cue playback engine: the runtime state machine behind GO.
//!
//! The engine is pure logic driven by `tick(now)`; all media work goes through the [`Backend`]
//! trait so it can be tested without audio/video hardware.
//!
//! Timing model: every scheduled moment (pre-wait end, post-wait end, auto-follow) is computed
//! from *logical* timestamps rather than from when a tick happened to run, so jitter never
//! accumulates along a chain of cues. Starts are dispatched up to [`LOOKAHEAD`] early with their
//! exact target time, letting the audio backend start them sample-accurately.

use crate::model::{Cue, CueId, CueKind, FadeCurve, GroupMode, KindTag, MIN_DB, NetworkCue, Workspace};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// How far ahead of their due time starts are handed to the backend.
pub const LOOKAHEAD: Duration = Duration::from_millis(25);
const MAX_PRELOAD: usize = 16;
const MAX_LOG: usize = 200;

#[derive(Clone, Debug, PartialEq)]
pub struct FadeRequest {
    pub volume_db: Option<f32>,
    pub opacity: Option<f32>,
    pub geometry: Option<crate::model::Geometry>,
    pub duration: f64,
    pub curve: FadeCurve,
}

/// Media playback as seen by the engine.
pub trait Backend {
    /// Preload these cues so they start instantly; anything else preloaded may be released.
    fn set_preloaded(&mut self, cues: &[&Cue]);
    /// Start a media cue so that it is heard/seen at `at` (which may be slightly in the future).
    fn start(&mut self, cue: &Cue, at: Instant) -> Result<(), String>;
    fn stop(&mut self, id: CueId);
    fn pause(&mut self, id: CueId);
    fn resume(&mut self, id: CueId, at: Instant);
    fn fade(&mut self, id: CueId, fade: &FadeRequest, at: Instant);
    /// True once a started media cue reached its end on its own (or failed).
    fn is_finished(&self, id: CueId) -> bool;
    /// Total action duration including loops, if finite and known.
    fn duration(&self, cue: &Cue) -> Option<f64>;
    /// Called every tick before the engine runs.
    fn update(&mut self, now: Instant);
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
}

#[derive(Clone, Debug)]
pub struct Instance {
    pub kind: KindTag,
    pub fired_at: Instant,
    /// When the action starts (fired_at + pre-wait), shifted forward by pauses.
    pub action_at: Instant,
    pub pre_wait: f64,
    pub paused_at: Option<Instant>,
    pub started: bool,
    pub action_done: bool,
    /// Known action length in seconds (None = until media/children report completion).
    pub action_duration: Option<f64>,
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
        (self.pre_wait - secs_between(now, self.action_at)).clamp(0.0, self.pre_wait)
    }

    /// Seconds since the action started (0 during pre-wait).
    pub fn action_elapsed(&self, now: Instant) -> f64 {
        let now = self.effective_now(now);
        if now <= self.action_at { 0.0 } else { (now - self.action_at).as_secs_f64() }
    }

    pub fn in_pre_wait(&self, now: Instant) -> bool {
        !self.started || self.effective_now(now) < self.action_at
    }

    pub fn is_paused(&self) -> bool {
        self.paused_at.is_some()
    }
}

/// A running cue as seen from outside the engine (what remote replicas are sent).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct InstanceSnapshot {
    pub id: CueId,
    pub kind: KindTag,
    pub pre_wait: f64,
    pub pre_wait_elapsed: f64,
    pub action_elapsed: f64,
    pub paused: bool,
    pub started: bool,
    pub action_done: bool,
    pub action_duration: Option<f64>,
    pub continue_done: bool,
    pub parent: Option<CueId>,
}

fn before(t: Instant, secs: f64) -> Instant {
    t.checked_sub(Duration::from_secs_f64(secs.max(0.0))).unwrap_or(t)
}

/// Seconds from `now` until `t` (negative if `t` is in the past).
fn secs_between(now: Instant, t: Instant) -> f64 {
    if t >= now { (t - now).as_secs_f64() } else { -(now - t).as_secs_f64() }
}

fn after(t: Instant, secs: f64) -> Instant {
    t + Duration::from_secs_f64(secs.max(0.0))
}

#[derive(Default)]
pub struct Engine {
    pub playhead: Option<CueId>,
    pub instances: HashMap<CueId, Instance>,
    pub log: Vec<String>,
    last_go: Option<Instant>,
    preload_dirty: bool,
    panic: Option<(Instant, Vec<CueId>)>,
}

impl Engine {
    pub fn new() -> Self {
        Self { preload_dirty: true, ..Default::default() }
    }

    pub fn is_active(&self) -> bool {
        !self.instances.is_empty()
    }

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
                let action_at = if s.started && s.pre_wait_elapsed >= s.pre_wait {
                    before(now, s.action_elapsed)
                } else {
                    after(now, s.pre_wait - s.pre_wait_elapsed)
                };
                let inst = Instance {
                    kind: s.kind,
                    fired_at: before(action_at, s.pre_wait),
                    action_at,
                    pre_wait: s.pre_wait,
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

    pub fn set_playhead(&mut self, id: Option<CueId>) {
        if self.playhead != id {
            self.playhead = id;
            self.preload_dirty = true;
        }
    }

    /// Call after the workspace was edited so preloaded media gets refreshed.
    pub fn workspace_changed(&mut self) {
        self.preload_dirty = true;
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
        if self.instances.contains_key(&id) {
            // Re-firing a running cue restarts it.
            self.stop_cue(ws, be, id);
        }
        let pre = cue.pre_wait.max(0.0);
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
        let mut out = vec![id];
        let tree: HashSet<CueId> = ws.cue_list.descendants(id).into_iter().collect();
        let mut i = 0;
        while i < out.len() {
            let cur = out[i];
            for (k, inst) in &self.instances {
                if (inst.parent == Some(cur) || tree.contains(k)) && !out.contains(k) {
                    out.push(*k);
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

    pub fn pause_cue(&mut self, ws: &Workspace, be: &mut dyn Backend, id: CueId, now: Instant) {
        for k in self.instance_subtree(ws, id) {
            self.pause_instance(be, k, now);
        }
    }

    pub fn resume_cue(&mut self, ws: &Workspace, be: &mut dyn Backend, id: CueId, now: Instant) {
        for k in self.instance_subtree(ws, id) {
            self.resume_instance(be, k, now);
        }
    }

    pub fn pause_all(&mut self, be: &mut dyn Backend, now: Instant) {
        let ids: Vec<CueId> = self.instances.keys().copied().collect();
        for k in ids {
            self.pause_instance(be, k, now);
        }
    }

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

    pub fn tick(&mut self, ws: &Workspace, be: &mut dyn Backend, now: Instant) {
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

        let horizon = now + LOOKAHEAD;
        // Each step makes one state transition; loop until stable (bounded for safety).
        for _ in 0..10_000 {
            if !self.step(ws, be, now, horizon) {
                break;
            }
        }

        if self.preload_dirty {
            self.preload_dirty = false;
            let ids = preload_set(ws, self.playhead);
            let cues: Vec<&Cue> = ids
                .iter()
                .filter(|id| !self.instances.contains_key(id))
                .filter_map(|id| ws.cue_list.find(*id))
                .collect();
            be.set_preloaded(&cues);
        }
    }

    fn step(&mut self, ws: &Workspace, be: &mut dyn Backend, now: Instant, horizon: Instant) -> bool {
        // Deterministic order: earliest-fired first.
        let mut ids: Vec<(Instant, CueId)> =
            self.instances.iter().map(|(k, i)| (i.fired_at, *k)).collect();
        ids.sort();

        for (_, id) in ids {
            let Some(inst) = self.instances.get(&id) else { continue };
            if inst.paused_at.is_some() {
                continue;
            }
            let Some(cue) = ws.cue_list.find(id) else {
                // Cue deleted while running.
                let inst = self.instances.remove(&id).unwrap();
                if inst.kind.is_media() {
                    be.stop(id);
                }
                return true;
            };

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
                continue;
            }

            if !inst.action_done {
                let done = match inst.kind {
                    _ if !cue.armed => true,
                    k if k.is_media() => be.is_finished(id),
                    KindTag::Group => {
                        inst.action_at <= now && !self.instances.values().any(|i| i.parent == Some(id))
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
                        None => {
                            continue;
                        }
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
        if !cue.armed {
            return Some(0.0);
        }
        match &cue.kind {
            CueKind::Audio(_) | CueKind::Video(_) | CueKind::Text(_) => match be.start(cue, at) {
                Ok(()) => be.duration(cue),
                Err(e) => {
                    self.log(format!("Cue {} '{}': {e}", cue.number, cue.display_name()));
                    Some(0.0)
                }
            },
            CueKind::Wait { duration } => Some(duration.max(0.0)),
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
                    duration: f.duration.max(0.0),
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
                    Some(t) => self.stop_cue(ws, be, *t),
                    None => {
                        let keep = self.ancestors(ws, cue.id);
                        self.stop_all_except(be, &keep);
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
                    .fold(false, |acc, k| be.devamp(k) || acc);
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

    fn on_action_complete(&mut self, ws: &Workspace, be: &mut dyn Backend, cue: &Cue) {
        if cue.armed
            && let CueKind::Fade(f) = &cue.kind
            && f.stop_target_when_done
            && let Some(t) = f.target
        {
            self.stop_cue(ws, be, t);
        }
    }
}

/// The cue the playhead moves to after firing `id`: skip everything `id` triggers through its
/// continue chain, stepping out of groups at the end of their lists.
pub fn after_chain(ws: &Workspace, id: CueId) -> Option<CueId> {
    let list = &ws.cue_list;
    let mut cur = id;
    loop {
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
        Some(n) => Some(n),
        None => list.parent_of(cur).and_then(|p| after_chain(ws, p)),
    }
}

/// Cues that could fire on the next GO: the playhead cue, its continue chain and group children.
fn preload_set(ws: &Workspace, playhead: Option<CueId>) -> Vec<CueId> {
    let list = &ws.cue_list;
    let mut out = Vec::new();
    let mut stack: Vec<CueId> = playhead.into_iter().collect();
    while let Some(id) = stack.pop() {
        if out.len() >= MAX_PRELOAD || out.contains(&id) {
            continue;
        }
        let Some(c) = list.find(id) else { continue };
        out.push(id);
        for ch in c.children().iter().rev() {
            stack.push(ch.id);
        }
        if c.continue_mode != crate::model::ContinueMode::DoNotContinue
            && let Some(n) = list.next_sibling(id)
        {
            stack.insert(0, n);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ContinueMode, CueKind, FadeCue, KindTag};

    #[derive(Default)]
    struct Mock {
        events: Vec<String>,
        started: HashMap<CueId, Instant>,
        finished: HashSet<CueId>,
        durations: HashMap<CueId, f64>,
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
        fn set_preloaded(&mut self, _cues: &[&Cue]) {}
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
