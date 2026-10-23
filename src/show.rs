//! The live show: workspace + engine + media, shared between the UI and the engine thread.

use crate::engine::Engine;
use crate::media::{Media, Prober};
use crate::model::{CueId, Workspace};
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(1);

/// The whole running show: workspace, engine and media backends, shared by UI, engine thread,
/// OSC and remote connections behind a mutex.
pub struct Show {
    /// The cue list, stages and settings being played.
    pub ws: Workspace,
    /// Cue state machine.
    pub engine: Engine,
    /// Audio/video/network backends.
    pub media: Media,
    /// File the workspace was loaded from / saved to.
    pub path: Option<PathBuf>,
    /// Unsaved changes.
    pub dirty: bool,
    /// Set once media has been torn down; the engine stops ticking.
    pub shut_down: bool,
    /// Bumped on every edit and whenever the workspace is swapped (drives undo snapshots, the
    /// copies kept by replicas and the autosave, [`Show::lookups`]). Bump it only through
    /// [`Show::edited`], [`Show::edited_cues`] or [`Show::changed`], which also log it.
    pub edit_gen: u64,
    /// What each recent edit generation changed.
    pub changes: crate::changes::ChangeLog,
    /// Cue numbers and media paths, rebuilt when [`Show::edit_gen`] moves.
    lookups: Option<(u64, Arc<Lookups>)>,
    /// This instance mirrors a remote main device: the engine is display-only and never ticks.
    pub replica: bool,
    /// Incoming timecode for the cue lists that follow it.
    pub timecode: crate::media::tcio::Follower,
    /// Last timecode position seen by each following list (by name).
    tc_last: std::collections::HashMap<String, f64>,
    /// Watches the clock for wall-clock triggers.
    wallclock: crate::wallclock::Watch,
    /// Engine thread timing and recoveries.
    pub stats: TickStats,
    /// Set when the engine recovered from an internal error (the UI tells the user, the
    /// autosave writes a fresh copy); cleared by whoever handles it.
    pub recovered: Option<String>,
    /// Lighting design (and replica flag) last handed to the light thread.
    light_loaded: Option<(crate::light::Lighting, bool)>,
    /// The programmer's fixture selection, shared with remote devices (a visualiser shows it and
    /// may set it). The UI keeps its own copy in step.
    pub light_selection: Vec<crate::light::model::FixtureId>,
    /// Bumped when a remote device sets [`Show::light_selection`] (the UI then takes it).
    pub light_selection_gen: u64,
    /// Network settings changes from replicas waiting for the operator to allow or deny them
    /// ([`crate::remote::netguard`]), oldest first.
    pub network_requests: Vec<crate::remote::netguard::NetworkRequest>,
}

/// How well the engine thread is keeping time.
#[derive(Clone, Copy, Debug, Default)]
pub struct TickStats {
    /// Ticks run.
    pub ticks: u64,
    /// Ticks that started more than [`LATE_TICK`] after the previous one.
    pub late: u64,
    /// Longest gap between two ticks.
    pub max_gap: Duration,
    /// Longest single tick.
    pub max_tick: Duration,
    /// Internal errors the engine recovered from.
    pub panics: u64,
}

/// A tick gap longer than this counts as late (actions that aren't scheduled ahead, such as
/// Wait cue ends, may be this much late).
pub const LATE_TICK: Duration = Duration::from_millis(5);

/// Lookups over the whole workspace that remote devices and OSC ask for many times between edits.
#[derive(Default)]
pub struct Lookups {
    /// Cue number → the first cue with it (in [`crate::model::CueList::iter_all`] order).
    pub numbers: std::collections::HashMap<String, CueId>,
    /// Media files the show uses ([`crate::remote::vet::media_paths`]).
    pub media: std::collections::HashSet<PathBuf>,
}

/// The show as shared between threads.
pub type SharedShow = Arc<Mutex<Show>>;

impl Show {
    /// An empty show with the default audio device opened.
    pub fn new(prober: Arc<Prober>) -> Self {
        Self {
            ws: Workspace::default(),
            engine: Engine::new(),
            media: Media::new(prober),
            path: None,
            dirty: false,
            shut_down: false,
            edit_gen: 0,
            changes: Default::default(),
            lookups: None,
            replica: false,
            timecode: Default::default(),
            tc_last: Default::default(),
            wallclock: Default::default(),
            stats: TickStats::default(),
            recovered: None,
            light_loaded: None,
            light_selection: Vec::new(),
            light_selection_gen: 0,
            network_requests: Vec::new(),
        }
    }

    /// Advance the engine to `now` (called at ~1 kHz by the engine thread).
    pub fn tick(&mut self, now: Instant) {
        if self.shut_down || self.replica {
            return;
        }
        crate::recovery::fault_point(crate::recovery::Fault::EngineTick);
        self.follow_timecode(now);
        self.follow_wallclock(now);
        let Show { ws, engine, media, .. } = self;
        engine.tick(ws, media, now);
        self.apply_cue_edits();
    }

    /// Write what Arm, Disarm and Target cues changed into the workspace.
    fn apply_cue_edits(&mut self) {
        use crate::engine::CueEdit;
        let edits = self.engine.take_edits();
        if edits.is_empty() {
            return;
        }
        let mut changed = Vec::new();
        for e in edits {
            match e {
                CueEdit::Armed(id, armed) => {
                    if let Some(c) = self.ws.cue_list.find_mut(id)
                        && c.armed != armed
                    {
                        c.armed = armed;
                        changed.push(id);
                    }
                }
                CueEdit::Target(id, target) => {
                    if let Some(t) = self.ws.cue_list.find_mut(id).and_then(|c| c.kind.target_mut())
                        && *t != target
                    {
                        *t = target;
                        changed.push(id);
                    }
                }
            }
        }
        if !changed.is_empty() {
            changed.sort_unstable();
            changed.dedup();
            self.edited_cues(changed);
        }
    }

    /// Fire cues whose timecode trigger the incoming timecode has just passed. Only a steady
    /// forward run fires cues; a jump (relocate, rewind) or dropout just moves the reference.
    fn follow_timecode(&mut self, now: Instant) {
        use crate::model::{CueList, TimecodeJump, TimecodeSource};
        // Runs every millisecond: bail out before allocating when no list follows timecode.
        let cl = &self.ws.cue_list;
        let follows = |i: &crate::model::ListInfo| i.timecode.source != TimecodeSource::Off;
        if self.tc_last.is_empty() && !follows(&cl.info) && !cl.others.iter().any(|o| follows(&o.info)) {
            return;
        }
        let lists = self.ws.cue_list.lists();
        let syncs: Vec<_> = lists.iter().map(|l| l.0.timecode.clone()).filter(|t| t.source != TimecodeSource::Off).collect();
        if syncs.is_empty() && self.tc_last.is_empty() {
            return;
        }
        self.timecode.set_wanted(syncs);
        let mut due = Vec::new();
        let mut chase = Vec::new();
        let mut seen = Vec::new();
        for (info, cues) in lists {
            let sync = &info.timecode;
            if sync.source == TimecodeSource::Off {
                continue;
            }
            let Some(pos) = self.timecode.position(sync, now) else {
                continue;
            };
            let secs = pos.secs + sync.offset;
            seen.push(info.name.clone());
            let allowed = crate::overrides::allowed(crate::overrides::Override::TimecodeIn);
            let last = self.tc_last.get(&info.name).copied();
            let steady = last.is_some_and(|l| secs > l && secs - l < 1.0);
            if allowed && steady {
                due.extend(CueList::timecode_due(cues, last.unwrap_or(secs), secs));
            } else if allowed && last != Some(secs) {
                match sync.on_jump {
                    TimecodeJump::Ignore => {}
                    TimecodeJump::FireSkipped => {
                        if let Some(l) = last.filter(|l| secs > *l) {
                            due.extend(CueList::timecode_due(cues, l, secs));
                        }
                    }
                    TimecodeJump::Chase => {
                        let mut all: Vec<_> = CueList::timecode_due(cues, f64::NEG_INFINITY, f64::INFINITY)
                            .into_iter()
                            .filter_map(|id| Some((id, self.ws.cue_list.find(id)?.timecode_trigger?)))
                            .collect();
                        all.sort_by(|a, b| a.1.total_cmp(&b.1));
                        chase.push((secs, all));
                    }
                }
            }
            self.tc_last.insert(info.name.clone(), secs);
        }
        self.tc_last.retain(|k, _| seen.contains(k));
        let Show { ws, engine, media, .. } = self;
        for (secs, all) in chase {
            for &(id, _) in &all {
                if engine.is_running(id) {
                    engine.stop_cue(ws, media, id);
                }
            }
            for (id, at) in all.into_iter().filter(|(_, t)| *t <= secs) {
                engine.chase_cue(ws, media, id, secs - at, now);
            }
        }
        for id in due {
            engine.trigger_cue(ws, media, id, now);
        }
    }

    /// Fire armed cues whose wall-clock trigger time has just come.
    fn follow_wallclock(&mut self, now: Instant) {
        let mut due = self.wallclock.poll(&self.ws.cue_list, self.edit_gen, now);
        if !crate::overrides::allowed(crate::overrides::Override::WallClock) {
            due.clear();
        }
        let Show { ws, engine, media, .. } = self;
        for id in due {
            if ws.cue_list.find(id).is_some_and(|c| engine.is_armed(c)) {
                engine.trigger_cue(ws, media, id, now);
            }
        }
    }

    /// [`Show::tick`], recovering from an internal error instead of letting it take the engine
    /// thread down: everything stops at once (a half-updated cue state can't be trusted), the
    /// playhead stays, and the show carries on with the next GO.
    pub fn guarded_tick(&mut self, now: Instant) {
        let Err(p) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.tick(now))) else { return };
        let msg = crate::recovery::panic_message(&*p);
        log::error!("engine panic: {msg}");
        self.stats.panics += 1;
        let Show { engine, media, .. } = self;
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.stop_all(media, now, 0.0))).is_ok();
        if !stopped {
            // Even stopping failed: start from a fresh engine and silence every backend.
            let playhead = engine.playhead;
            *engine = Engine::new();
            engine.set_playhead(playhead);
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| media.stop_everything()));
        }
        let note = format!("Internal error, all cues stopped (the show can go on): {msg}");
        self.engine.report(note.clone());
        self.recovered = Some(note);
    }

    /// GO: fire the cue at the playhead.
    pub fn go(&mut self) {
        let Show { ws, engine, media, .. } = self;
        engine.go(ws, media, Instant::now());
    }

    /// Stop everything and release audio/video before the process exits, so no GStreamer or
    /// audio thread is still running (e.g. initialising a hardware decoder) during teardown.
    pub fn shutdown(&mut self) {
        let Show { engine, media, .. } = self;
        engine.stop_all(media, Instant::now(), 0.0);
        media.shutdown();
        self.shut_down = true;
    }

    /// Call after any edit to the workspace.
    pub fn edited(&mut self) {
        self.edited_with(None, false);
    }

    /// Call after an edit that changed only these cues' own fields (no cue added, removed or
    /// moved, a group's children untouched) and perhaps parts of the workspace other than the
    /// cue lists. Copies kept by replicas and the autosave then take only these cues
    /// ([`crate::changes`]).
    pub fn edited_cues(&mut self, cues: impl IntoIterator<Item = CueId>) {
        self.edited_with(Some(cues.into_iter().collect()), false);
    }

    /// Call after an edit that added, removed or moved cues (or lists): `cues` are every cue it
    /// added or changed in its own fields (a duplicated or pasted group's children too). Copies
    /// then take the lists' shape and those cues instead of the whole show ([`crate::changes`]);
    /// a cue missing from `cues` makes them take the whole show once.
    pub fn edited_shape(&mut self, cues: impl IntoIterator<Item = CueId>) {
        self.edited_with(Some(cues.into_iter().collect()), true);
    }

    /// Call after an edit outside the cue lists (settings, stages, the lighting design).
    pub fn edited_outside_cues(&mut self) {
        self.edited_with(Some(Vec::new()), false);
    }

    /// The workspace changed without being edited here (swapped, undone, or changed by the main
    /// device on a replica): bump [`Show::edit_gen`] so copies and lookups look again.
    pub fn changed(&mut self) {
        self.edit_gen += 1;
        self.changes.push(self.edit_gen, None, false);
    }

    fn edited_with(&mut self, cues: Option<Vec<CueId>>, shape: bool) {
        self.dirty = true;
        self.edit_gen += 1;
        self.changes.push(self.edit_gen, cues, shape);
        self.engine.workspace_changed();
        self.media.set_room(&self.ws.settings.speakers);
        if !self.replica {
            // (A replica plays nothing: its patches' devices are the main device's.)
            self.media.set_patches(&self.ws.settings.patches);
            self.media.set_audition_device(self.ws.settings.audition_device.as_deref());
        }
        self.sync_lighting();
    }

    /// Cue numbers and media paths of the whole show, built once per [`Show::edit_gen`].
    pub fn lookups(&mut self) -> Arc<Lookups> {
        if let Some((g, l)) = &self.lookups
            && *g == self.edit_gen
        {
            return l.clone();
        }
        let mut l = Lookups::default();
        for c in self.ws.cue_list.iter_all() {
            l.numbers.entry(c.number.clone()).or_insert(c.id);
        }
        l.media = crate::remote::vet::media_paths(&self.ws).into_iter().collect();
        let l = Arc::new(l);
        self.lookups = Some((self.edit_gen, l.clone()));
        l
    }

    /// Recompile the lighting design for the light thread if it (or the replica flag) changed.
    /// A replica plays nothing (it mirrors the main device's states and output), so it loads
    /// an empty design once: no compile, no DMX outputs or inputs. The kept copy shares the
    /// fixture and cue lists ([`crate::light::model::Shared`]), so when the edit was elsewhere the
    /// compare is a few pointer checks rather than a walk over the whole design.
    pub fn sync_lighting(&mut self) {
        if self.light_loaded.as_ref().is_some_and(|(l, r)| *r == self.replica && *l == self.ws.lighting) {
            return;
        }
        let was = self.light_loaded.as_ref().map(|(_, r)| *r);
        // Edits playback doesn't read (fixture positions, plot shapes, names...) don't recompile.
        let compile = !self.light_loaded.as_ref().is_some_and(|(l, r)| *r == self.replica && crate::light::playback::LightShow::same_input(l, &self.ws.lighting));
        let light = &mut self.media.light;
        if self.replica {
            if was != Some(true) {
                light.load(&crate::light::Lighting::default());
            }
        } else {
            light.clear_mirror();
            if compile { light.load(&self.ws.lighting) } else { light.load_io(&self.ws.lighting) }
        }
        self.light_loaded = Some((self.ws.lighting.clone(), self.replica));
    }

    /// Take a workspace edited on a remote device. Machine-specific settings (audio device,
    /// OSC) stay as they are here; values are brought into their plausible ranges
    /// ([`crate::limits`]).
    pub fn apply_remote_edit(&mut self, mut ws: Workspace) {
        crate::limits::workspace(&mut ws);
        let mine = &self.ws.settings;
        ws.settings.audio_device = mine.audio_device.clone();
        // Which device each audio patch is on stays this computer's choice.
        for (i, p) in ws.settings.patches.iter_mut().enumerate() {
            p.device = mine.patches.get(i).and_then(|m| m.device.clone());
        }
        ws.settings.osc_enabled = mine.osc_enabled;
        ws.settings.audition_device = mine.audition_device.clone();
        ws.settings.hardware_video_decoding = mine.hardware_video_decoding;
        ws.settings.osc_port = mine.osc_port;
        // A replica's display choices wait for the operator (`remote::netguard`, which already
        // put these back); kept here too, so nothing else can move the operator's screens.
        for st in &mut ws.stages {
            st.output = self.ws.stages.iter().find(|s| s.id == st.id).and_then(|s| s.output.clone());
        }
        if ws.settings.master_volume_db != mine.master_volume_db {
            self.media.set_master(ws.settings.master_volume_db);
        }
        self.ws = ws;
        if self.engine.playhead.is_some_and(|p| self.ws.cue_list.find(p).is_none()) {
            self.engine.set_playhead(None);
        }
        self.edited();
    }

    /// Take an edit made on a remote device as changes to this workspace
    /// ([`crate::remote::workspace_patch`]). False (nothing changed) if they don't fit. Only the
    /// patched cues (and the design, if patched) are brought into range: the rest already is.
    pub fn apply_remote_patch(&mut self, mut cues: Vec<crate::model::Cue>, lighting: Option<&crate::light::patch::LightingPatch>) -> bool {
        cues.iter_mut().for_each(crate::limits::cue);
        // (A copy of the design costs little: its big parts are shared.)
        let before = lighting.map(|_| self.ws.lighting.clone());
        if !crate::remote::apply_workspace_patch(&mut self.ws, &cues, lighting) {
            return false;
        }
        if let Some(before) = before {
            // Cue patches only change cues; a lighting patch may add cues beyond the count.
            if crate::limits::light_cue_count(&self.ws.lighting) > crate::limits::MAX_LIGHT_CUES {
                self.ws.lighting = before;
                return false;
            }
            // Unchanged parts of the design keep their allocations (see `limits::lighting`).
            crate::limits::lighting(&mut self.ws.lighting);
        }
        self.edited_cues(cues.iter().map(|c| c.id));
        true
    }

    /// Take cues added, removed or moved on a remote device, merged into this workspace as it is
    /// now ([`crate::changes::Reshape::apply`]), with the lighting design's changes. False
    /// (nothing changed) if they don't fit or would make too many cues.
    pub fn apply_remote_ops(&mut self, mut ops: crate::changes::Reshape, lighting: Option<&crate::light::patch::LightingPatch>) -> bool {
        ops.cues_mut().for_each(crate::limits::cue);
        if crate::limits::cue_count(&self.ws) + ops.added.len() > crate::limits::MAX_CUES {
            return false;
        }
        let mut design = None;
        if let Some(p) = lighting {
            let mut l = self.ws.lighting.clone();
            if !crate::light::patch::apply(&mut l, p) || crate::limits::light_cue_count(&l) > crate::limits::MAX_LIGHT_CUES {
                return false;
            }
            crate::limits::lighting(&mut l);
            design = Some(l);
        }
        if !ops.apply(&mut self.ws) {
            return false;
        }
        if let Some(l) = design {
            self.ws.lighting = l;
        }
        if self.engine.playhead.is_some_and(|p| self.ws.cue_list.find(p).is_none()) {
            self.engine.set_playhead(None);
        }
        self.edited_shape(ops.touched());
        true
    }

    /// Queue a replica's held network settings change for the operator (nothing if empty).
    pub fn request_network_change(&mut self, device: &str, change: crate::remote::netguard::NetworkChange) {
        if change.is_empty() {
            return;
        }
        let id = self.network_requests.last().map_or(1, |r| r.id + 1);
        self.network_requests.push(crate::remote::netguard::NetworkRequest { id, device: device.to_string(), change });
        let extra = self.network_requests.len().saturating_sub(crate::remote::netguard::MAX_REQUESTS);
        self.network_requests.drain(..extra);
    }

    /// Answer a network settings request: allowed ones are applied to the show.
    pub fn answer_network_request(&mut self, id: u64, allow: bool) {
        let Some(i) = self.network_requests.iter().position(|r| r.id == id) else { return };
        let r = self.network_requests.remove(i);
        if allow {
            r.change.apply(&mut self.ws);
            self.edited();
        }
    }

    /// Swap in another workspace: stops everything, reopens audio with its settings.
    pub fn replace_workspace(&mut self, ws: Workspace, path: Option<PathBuf>) {
        let now = Instant::now();
        self.engine.stop_all(&mut self.media, now, 0.0);
        self.ws = ws;
        self.path = path;
        self.changed();
        // Requests were about the show that was open.
        self.network_requests.clear();
        self.dirty = false;
        self.engine.set_playhead(self.ws.cue_list.first());
        self.engine.forget_cue_changes();
        self.engine.workspace_changed();
        let master = self.ws.settings.master_volume_db;
        self.media.set_room(&self.ws.settings.speakers);
        self.media.light.release_all(0.0, now);
        self.sync_lighting();
        self.media.reopen_audio(self.ws.settings.audio_device.as_deref(), master);
        self.media.set_master(master);
        self.media.set_patches(&self.ws.settings.patches);
        self.media.set_audition_device(self.ws.settings.audition_device.as_deref());
    }
}

/// Run the engine at ~1 kHz on its own thread so timing doesn't depend on UI frame rate.
/// Ticks are paced against absolute deadlines (a slow tick doesn't push the next ones back),
/// and a tick that panics is recovered by [`Show::guarded_tick`] instead of ending the thread.
pub fn spawn_engine_thread(show: SharedShow, repaint: impl Fn() + Send + 'static) {
    std::thread::Builder::new()
        .name(crate::recovery::ENGINE_THREAD.into())
        .spawn(move || {
            let mut was_active = false;
            let mut next = Instant::now();
            let mut last: Option<Instant> = None;
            loop {
                let (active, recovered) = {
                    let mut s = show.lock();
                    let now = Instant::now();
                    s.guarded_tick(now);
                    let took = now.elapsed();
                    let st = &mut s.stats;
                    st.ticks += 1;
                    st.max_tick = st.max_tick.max(took);
                    if let Some(l) = last {
                        let gap = now - l;
                        st.max_gap = st.max_gap.max(gap);
                        if gap > LATE_TICK {
                            st.late += 1;
                        }
                    }
                    last = Some(now);
                    (s.engine.is_active(), s.recovered.is_some())
                };
                if active != was_active || recovered {
                    // Make sure the UI notices start/end of activity (or a recovery) promptly.
                    repaint();
                    was_active = active;
                }
                next += TICK;
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else if now - next > Duration::from_millis(100) {
                    // Far behind (machine suspended, debugger): don't try to catch up.
                    next = now;
                }
            }
        })
        .expect("spawn engine thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Command, apply};
    use crate::light::{library, model::*};
    use crate::model::{Cue, CueKind, LightAction, LightingCue};

    /// A Light cue in the cue list GOes its designed lighting cue on the light thread, and
    /// waits for the lighting fade before continuing.
    #[test]
    fn light_cue_plays_the_design() {
        let mut show = Show::new(crate::media::Prober::new());
        let mut ws = Workspace::default();
        let t = library::generic("generic/dimmer").unwrap();
        ws.lighting.add_type(&t);
        ws.lighting.fixtures.push(Fixture { id: 1, fixture_type: t.id.clone(), mode: t.modes[0].name.clone(), address: 1, ..Default::default() });
        let mut lc = LightCue { fade: 0.0, delay: 0.5, ..Default::default() };
        lc.values.entry(1).or_default().insert(Attribute::Dimmer, 1.0.into());
        let target = lc.id;
        ws.lighting.sequences.push(Sequence { id: 1, cues: vec![lc].into(), ..Default::default() });
        ws.cue_list.cues.push(Cue::new(CueKind::Light(LightingCue { action: LightAction::Go, cue: Some(target), ..Default::default() })));
        show.replace_workspace(ws, None);
        let t0 = Instant::now();
        apply(&mut show, Command::Go, t0).unwrap();
        show.tick(t0);
        let id = show.ws.cue_list.cues[0].id;
        assert_eq!(show.engine.instances.get(&id).and_then(|i| i.action_duration), Some(0.5));
        let lit = || show.media.light.frame().get(1).is_some_and(|u| u.data[0] == 255);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !lit() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(lit(), "fixture 1 at full after the delay");
        // Executor and masters through commands (what replicas send).
        use crate::control::LightControl;
        apply(&mut show, Command::Light(LightControl::GrandMaster(0.5)), Instant::now()).unwrap();
        let half = || show.media.light.frame().get(1).is_some_and(|u| u.data[0] == 128);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !half() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(half(), "grand master at 50 %");
        apply(&mut show, Command::Light(LightControl::Release(None, 0.0)), Instant::now()).unwrap();
        let dark = || show.media.light.frame().get(1).is_some_and(|u| u.data[0] == 0);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !dark() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(dark(), "released");
    }
}
