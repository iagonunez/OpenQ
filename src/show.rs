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
            replica: false,
            timecode: Default::default(),
            tc_last: Default::default(),
        }
    }

    /// Advance the engine to `now` (called at ~1 kHz by the engine thread).
    pub fn tick(&mut self, now: Instant) {
        if self.shut_down || self.replica {
            return;
        }
        self.follow_timecode(now);
        let Show { ws, engine, media, .. } = self;
        engine.tick(ws, media, now);
    }

    /// Fire cues whose timecode trigger the incoming timecode has just passed. Only a steady
    /// forward run fires cues; a jump (relocate, rewind) or dropout just moves the reference.
    fn follow_timecode(&mut self, now: Instant) {
        use crate::model::{CueList, TimecodeSource};
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
        self.media.reopen_audio(self.ws.settings.audio_device.as_deref(), master);
        if let Some(a) = &mut self.media.audio {
            a.set_master(master);
        }
    }
}

/// Run the engine at ~1 kHz on its own thread so timing doesn't depend on UI frame rate.
pub fn spawn_engine_thread(show: SharedShow, repaint: impl Fn() + Send + 'static) {
    std::thread::Builder::new()
        .name("openq-engine".into())
        .spawn(move || {
            let mut was_active = false;
            loop {
                let active = {
                    let mut s = show.lock();
                    s.tick(Instant::now());
                    s.engine.is_active()
                };
                if active != was_active {
                    // Make sure the UI notices start/end of activity promptly.
                    repaint();
                    was_active = active;
                }
                std::thread::sleep(TICK);
            }
        })
        .expect("spawn engine thread");
}
