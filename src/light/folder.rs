//! A folder of files the user collects across shows (the fixture library, the effect library),
//! read and written on a worker thread so the UI never touches the disk in a frame.
//!
//! Jobs run one at a time and in order on one thread (started on the first job), so a remove
//! never overtakes its save; after each job the folder is read again and published as a new
//! [`Snapshot`].

use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What a library folder holds.
pub trait Stored: Clone + Send + Sync + 'static {
    /// Name of the worker thread.
    const THREAD: &'static str;
    /// Every item in the folder, in display order; files that don't read are reported as
    /// `(file name, error)`.
    fn load_dir(dir: &Path) -> (Vec<Self>, Vec<(String, String)>);
}

/// What the library holds right now.
#[derive(Clone)]
pub struct Snapshot<T> {
    /// Items, in display order.
    pub items: Vec<T>,
    /// Files that didn't read: (file name, error).
    pub errors: Vec<(String, String)>,
    /// Jobs are queued or running.
    pub loading: bool,
    /// Bumped after every load (the UI refreshes its menus).
    pub generation: u64,
}

impl<T> Default for Snapshot<T> {
    fn default() -> Self {
        Self { items: Vec::new(), errors: Vec::new(), loading: false, generation: 0 }
    }
}

struct State<T> {
    /// Shared with the UI: a snapshot is replaced, never edited in place, so handing it out is cheap.
    snap: Arc<Snapshot<T>>,
    /// Jobs queued or running.
    pending: usize,
}

impl<T> Default for State<T> {
    fn default() -> Self {
        Self { snap: Arc::new(Snapshot::default()), pending: 0 }
    }
}

impl<T: Clone> State<T> {
    /// Publish `loading` (a new generation, so the UI's spinner follows it).
    fn set_loading(&mut self) {
        let loading = self.pending > 0;
        if self.snap.loading != loading {
            let snap = Arc::make_mut(&mut self.snap);
            snap.loading = loading;
            snap.generation += 1;
        }
    }
}

type Work = Box<dyn FnOnce(&Path) -> anyhow::Result<()> + Send>;
type Done = Box<dyn FnOnce(anyhow::Result<()>) + Send>;
type Queue = Arc<Mutex<Option<std::sync::mpsc::Sender<(Work, Done)>>>>;

/// A library folder. Cheap to clone (one shared state and worker).
pub struct Library<T> {
    dir: Option<PathBuf>,
    state: Arc<Mutex<State<T>>>,
    queue: Queue,
}

impl<T> Clone for Library<T> {
    fn clone(&self) -> Self {
        Self { dir: self.dir.clone(), state: self.state.clone(), queue: self.queue.clone() }
    }
}

impl<T: Stored> Library<T> {
    /// A library on `dir` (None: no config dir; stays empty). Starts loading.
    pub fn new(dir: Option<PathBuf>) -> Self {
        let lib = Self { dir, state: Arc::default(), queue: Arc::default() };
        lib.reload();
        lib
    }

    /// The folder.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// The current contents (shared, not copied).
    pub fn snapshot(&self) -> Arc<Snapshot<T>> {
        self.state.lock().snap.clone()
    }

    /// The contents if they changed since `generation`.
    pub fn snapshot_if_newer(&self, generation: u64) -> Option<Arc<Snapshot<T>>> {
        let s = self.state.lock();
        (s.snap.generation != generation).then(|| s.snap.clone())
    }

    /// Read the folder again in the background.
    pub fn reload(&self) {
        self.job(|_| Ok(()), |_| {});
    }

    /// Run `work` on the folder in the background, then read it again; `done` gets the result
    /// (on the worker thread).
    pub fn job(&self, work: impl FnOnce(&Path) -> anyhow::Result<()> + Send + 'static, done: impl FnOnce(anyhow::Result<()>) + Send + 'static) {
        let Some(dir) = self.dir.clone() else {
            done(Err(anyhow::anyhow!("no settings folder")));
            return;
        };
        let mut q = self.queue.lock();
        if q.is_none() {
            let (tx, rx) = std::sync::mpsc::channel::<(Work, Done)>();
            let state = self.state.clone();
            let spawned = std::thread::Builder::new().name(T::THREAD.into()).spawn(move || {
                for (work, done) in rx {
                    let r = crate::recovery::catch(|| work(&dir));
                    let (items, errors) = T::load_dir(&dir);
                    {
                        let mut s = state.lock();
                        s.pending -= 1;
                        let generation = s.snap.generation + 1;
                        s.snap = Arc::new(Snapshot { items, errors, loading: s.pending > 0, generation });
                    }
                    // A panicking callback must not stop the worker.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| done(r)));
                }
            });
            match spawned {
                Ok(_) => *q = Some(tx),
                Err(e) => {
                    done(Err(anyhow::anyhow!("can't start the library thread: {e}")));
                    return;
                }
            }
        }
        {
            let mut s = self.state.lock();
            s.pending += 1;
            s.set_loading();
        }
        if q.as_ref().expect("started").send((Box::new(work), Box::new(done))).is_err() {
            let mut s = self.state.lock();
            s.pending -= 1;
            s.set_loading();
        }
    }

    /// Wait until every queued job has run (tests).
    pub fn wait(&self) {
        while self.state.lock().pending > 0 {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}
