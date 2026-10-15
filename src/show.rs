//! The live show: workspace + engine + media, shared between the UI and the engine thread.

use crate::engine::Engine;
use crate::media::{Media, Prober};
use crate::model::Workspace;
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
    /// Bumped on every edit (drives undo snapshots).
    pub edit_gen: u64,
    /// This instance mirrors a remote main device: the engine is display-only and never ticks.
    pub replica: bool,
    /// Incoming timecode for the cue lists that follow it.
    pub timecode: crate::media::tcio::Follower,
    /// Last timecode position seen by each following list (by name).
    tc_last: std::collections::HashMap<String, f64>,
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
            replica: false,
            timecode: Default::default(),
            tc_last: Default::default(),
            stats: TickStats::default(),
            recovered: None,
            light_loaded: None,
            light_selection: Vec::new(),
            light_selection_gen: 0,
        }
    }

    /// Advance the engine to `now` (called at ~1 kHz by the engine thread).
    pub fn tick(&mut self, now: Instant) {
        if self.shut_down || self.replica {
            return;
        }
        crate::recovery::fault_point(crate::recovery::Fault::EngineTick);
        self.follow_timecode(now);
        let Show { ws, engine, media, .. } = self;
        engine.tick(ws, media, now);
    }

    /// Fire cues whose timecode trigger the incoming timecode has just passed. Only a steady
    /// forward run fires cues; a jump (relocate, rewind) or dropout just moves the reference.
    fn follow_timecode(&mut self, now: Instant) {
        use crate::model::{CueList, TimecodeSource};
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
        let mut seen = Vec::new();
        for (info, cues) in lists {
            if info.timecode.source == TimecodeSource::Off {
                continue;
            }
            let Some(pos) = self.timecode.position(&info.timecode, now) else {
                continue;
            };
            seen.push(info.name.clone());
            if let Some(&last) = self.tc_last.get(&info.name)
                && pos.secs > last
                && pos.secs - last < 1.0
            {
                due.extend(CueList::timecode_due(cues, last, pos.secs));
            }
            self.tc_last.insert(info.name.clone(), pos.secs);
        }
        self.tc_last.retain(|k, _| seen.contains(k));
        let Show { ws, engine, media, .. } = self;
        for id in due {
            engine.start_cue(ws, media, id, now);
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
        self.dirty = true;
        self.edit_gen += 1;
        self.engine.workspace_changed();
        self.media.set_room(&self.ws.settings.speakers);
        self.sync_lighting();
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
    /// OSC) stay as they are here.
    pub fn apply_remote_edit(&mut self, mut ws: Workspace) {
        let mine = &self.ws.settings;
        ws.settings.audio_device = mine.audio_device.clone();
        ws.settings.osc_enabled = mine.osc_enabled;
        ws.settings.osc_port = mine.osc_port;
        if ws.settings.master_volume_db != mine.master_volume_db
            && let Some(a) = &mut self.media.audio
        {
            a.set_master(ws.settings.master_volume_db);
        }
        self.ws = ws;
        if self.engine.playhead.is_some_and(|p| self.ws.cue_list.find(p).is_none()) {
            self.engine.set_playhead(None);
        }
        self.edited();
    }

    /// Take an edit made on a remote device as changes to this workspace
    /// ([`crate::remote::workspace_patch`]). False (nothing changed) if they don't fit.
    pub fn apply_remote_patch(&mut self, cues: &[crate::model::Cue], lighting: Option<&crate::light::patch::LightingPatch>) -> bool {
        if !crate::remote::apply_workspace_patch(&mut self.ws, cues, lighting) {
            return false;
        }
        if self.engine.playhead.is_some_and(|p| self.ws.cue_list.find(p).is_none()) {
            self.engine.set_playhead(None);
        }
        self.edited();
        true
    }

    /// Swap in another workspace: stops everything, reopens audio with its settings.
    pub fn replace_workspace(&mut self, ws: Workspace, path: Option<PathBuf>) {
        let now = Instant::now();
        self.engine.stop_all(&mut self.media, now, 0.0);
        self.ws = ws;
        self.path = path;
        self.dirty = false;
        self.engine.set_playhead(self.ws.cue_list.first());
        self.engine.workspace_changed();
        let master = self.ws.settings.master_volume_db;
        self.media.set_room(&self.ws.settings.speakers);
        self.media.light.release_all(0.0, now);
        self.sync_lighting();
        self.media.reopen_audio(self.ws.settings.audio_device.as_deref(), master);
        if let Some(a) = &mut self.media.audio {
            a.set_master(master);
        }
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
