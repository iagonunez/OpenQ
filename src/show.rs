//! The live show: workspace + engine + media, shared between the UI and the engine thread.

use crate::engine::Engine;
use crate::media::{Media, Prober};
use crate::model::Workspace;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(1);

pub struct Show {
    pub ws: Workspace,
    pub engine: Engine,
    pub media: Media,
    pub path: Option<PathBuf>,
    pub dirty: bool,
    /// Set once media has been torn down; the engine stops ticking.
    pub shut_down: bool,
    /// Bumped on every edit (drives undo snapshots).
    pub edit_gen: u64,
}

pub type SharedShow = Arc<Mutex<Show>>;

impl Show {
    pub fn new(prober: Arc<Prober>) -> Self {
        Self { ws: Workspace::default(), engine: Engine::new(), media: Media::new(prober), path: None, dirty: false, shut_down: false, edit_gen: 0 }
    }

    pub fn tick(&mut self, now: Instant) {
        if self.shut_down {
            return;
        }
        let Show { ws, engine, media, .. } = self;
        engine.tick(ws, media, now);
    }

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
