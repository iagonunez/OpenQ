//! Crash safety: keeping a show running when something inside it fails, and getting the work
//! back when the whole process dies.
//!
//! * [`catch`] turns a panic in a worker (decoder, prober, capture thread...) into an ordinary
//!   error, so one bad file can't silently kill the thread that serves every other file.
//! * [`Autosave`] keeps a copy of the unsaved show in the config folder (`recovery/`). Each
//!   running instance holds an OS file lock on its session; a copy whose lock nobody holds was
//!   left behind by a crash, and [`find_orphans`] offers it back on the next start.
//! * [`install_panic_hook`] writes a crash report (`crash/`) with a backtrace, adds a line to the
//!   [error log](crate::errlog), and, when the panicking thread is the UI thread (other threads'
//!   panics are recovered), makes a last-moment autosave.
//! * [`Fault`] points let the stress tests (feature `fault-injection`) make real code panic on
//!   purpose; without the feature they compile to nothing.

use crate::model::Workspace;
use crate::show::SharedShow;
use parking_lot::Mutex;
use std::any::Any;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often the autosave thread looks for unsaved changes.
pub const AUTOSAVE_INTERVAL: Duration = Duration::from_secs(5);
/// Crash reports kept in the crash folder (oldest are deleted).
const MAX_CRASH_LOGS: usize = 20;
/// Name of the engine thread, whose panics are recovered in place.
pub const ENGINE_THREAD: &str = "openq-engine";

/// The text of a panic payload.
pub fn panic_message(p: &(dyn Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".into()
    }
}

/// Run `f`, turning a panic into an error ("internal error: …").
pub fn catch<T>(f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => Err(anyhow::anyhow!("internal error: {}", panic_message(&*p))),
    }
}

// ------------------------------------------------------------------ fault injection

/// Places where the stress tests can make OpenQ panic on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The next engine tick.
    EngineTick,
    /// The next audio mixer callback.
    AudioCallback,
    /// The next audio file decoder thread.
    Decoder,
    /// The next media probe.
    Prober,
    /// The next lighting frame.
    LightTick,
}

#[cfg(feature = "fault-injection")]
static ARMED: [AtomicBool; 5] = [const { AtomicBool::new(false) }; 5];

/// Make the next pass through `fault`'s point panic. Only does anything with the
/// `fault-injection` feature.
pub fn arm(fault: Fault) {
    #[cfg(feature = "fault-injection")]
    ARMED[fault as usize].store(true, Ordering::Release);
    #[cfg(not(feature = "fault-injection"))]
    let _ = fault;
}

/// A fault point: panics here once after [`arm`]. Free without the `fault-injection` feature.
#[inline(always)]
pub(crate) fn fault_point(fault: Fault) {
    #[cfg(feature = "fault-injection")]
    if ARMED[fault as usize].swap(false, Ordering::AcqRel) {
        panic!("injected fault: {fault:?}");
    }
    #[cfg(not(feature = "fault-injection"))]
    let _ = fault;
}

// ------------------------------------------------------------------ autosave

/// Folder holding autosaved sessions.
pub fn recovery_dir() -> Option<PathBuf> {
    crate::remote::config_dir().map(|d| d.join("recovery"))
}

/// Folder holding crash reports.
pub fn crash_dir() -> Option<PathBuf> {
    crate::remote::config_dir().map(|d| d.join("crash"))
}

fn unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// What an autosave is a copy of.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct AutosaveInfo {
    /// The show file it belongs to (None: never saved).
    pub path: Option<PathBuf>,
    /// When it was written (seconds since 1970).
    pub saved: u64,
}

/// Write `bytes` to `path` so a crash or power cut leaves either the old or the new file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut f = File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// This instance's autosave session. Dropping it releases the lock but keeps the copy: only
/// [`Autosave::clear`] (a clean save or quit) removes it.
pub struct Autosave {
    dir: PathBuf,
    id: String,
    _lock: File,
    last_gen: Option<u64>,
    has_copy: bool,
}

impl Autosave {
    /// Start a session in `dir` (normally [`recovery_dir`]).
    pub fn new_in(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let millis = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let id = format!("{}-{millis}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));
        let lock = File::create(dir.join(format!("session-{id}.lock")))?;
        lock.try_lock().map_err(|e| anyhow::anyhow!("cannot lock autosave session: {e}"))?;
        Ok(Self { dir: dir.to_path_buf(), id, _lock: lock, last_gen: None, has_copy: false })
    }

    /// Start a session in the config folder.
    pub fn new() -> anyhow::Result<Self> {
        Self::new_in(&recovery_dir().ok_or_else(|| anyhow::anyhow!("no config directory"))?)
    }

    fn data_path(&self) -> PathBuf {
        self.dir.join(format!("session-{}.openq", self.id))
    }

    fn info_path(&self) -> PathBuf {
        self.dir.join(format!("session-{}.json", self.id))
    }

    /// Store a copy of `ws` (media paths stay absolute, so it loads from anywhere).
    pub fn write(&mut self, ws: &Workspace, path: Option<&Path>) -> anyhow::Result<()> {
        write_atomic(&self.data_path(), serde_json::to_string(ws)?.as_bytes())?;
        let info = AutosaveInfo { path: path.map(Path::to_path_buf), saved: unix_secs() };
        write_atomic(&self.info_path(), serde_json::to_string(&info)?.as_bytes())?;
        self.has_copy = true;
        Ok(())
    }

    /// Remove the copy (the show was saved, or OpenQ is quitting normally).
    pub fn clear(&mut self) {
        if self.has_copy {
            let _ = std::fs::remove_file(self.data_path());
            let _ = std::fs::remove_file(self.info_path());
            self.has_copy = false;
        }
        self.last_gen = None;
    }

    /// Autosave `show` if it has changes not copied yet, or clear the copy once it's saved.
    /// The workspace is cloned under the lock and written after releasing it.
    pub fn sync(&mut self, show: &SharedShow) -> anyhow::Result<()> {
        let snap = {
            let s = show.lock();
            if s.shut_down {
                // Quitting: the UI clears the copy (or keeps it, if it never got that far).
                return Ok(());
            } else if !s.dirty {
                None
            } else if self.last_gen == Some(s.edit_gen) {
                return Ok(());
            } else {
                Some((s.ws.clone(), s.path.clone(), s.edit_gen))
            }
        };
        match snap {
            None => self.clear(),
            Some((ws, path, generation)) => {
                self.write(&ws, path.as_deref())?;
                self.last_gen = Some(generation);
            }
        }
        Ok(())
    }

    /// Forget what was written, so the next [`Autosave::sync`] writes again.
    pub fn invalidate(&mut self) {
        self.last_gen = None;
    }

    fn is_own(&self, name: &str) -> bool {
        name.contains(&self.id)
    }
}

impl Drop for Autosave {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.dir.join(format!("session-{}.lock", self.id)));
    }
}

/// Autosave in the background every [`AUTOSAVE_INTERVAL`] (or at once when `now` is set).
pub fn spawn_autosave(show: SharedShow, autosave: Arc<Mutex<Autosave>>, now: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("openq-autosave".into())
        .spawn(move || {
            let mut last = Instant::now();
            loop {
                std::thread::sleep(Duration::from_millis(200));
                if !now.swap(false, Ordering::AcqRel) && last.elapsed() < AUTOSAVE_INTERVAL {
                    continue;
                }
                last = Instant::now();
                if show.lock().shut_down {
                    break;
                }
                if let Err(e) = autosave.lock().sync(&show) {
                    log::warn!("autosave: {e:#}");
                }
            }
        })
        .expect("spawn autosave thread");
}

/// A show autosaved by an OpenQ that didn't quit normally.
#[derive(Clone, Debug)]
pub struct Orphan {
    /// The copy of the show.
    pub data: PathBuf,
    /// What it is a copy of.
    pub info: AutosaveInfo,
}

impl Orphan {
    /// Read the copy.
    pub fn load(&self) -> anyhow::Result<Workspace> {
        Ok(serde_json::from_str(&std::fs::read_to_string(&self.data)?)?)
    }

    /// Delete the copy.
    pub fn discard(&self) {
        let _ = std::fs::remove_file(&self.data);
        let _ = std::fs::remove_file(self.data.with_extension("json"));
        let _ = std::fs::remove_file(self.data.with_extension("lock"));
    }

    /// Display name: the show file's name, or "Untitled".
    pub fn name(&self) -> String {
        self.info
            .path
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into())
    }
}

/// Autosaves in `dir` whose session no running OpenQ holds, newest first. `own` (this
/// instance's session) is never listed.
pub fn find_orphans_in(dir: &Path, own: Option<&Autosave>) -> Vec<Orphan> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let data = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let ext = data.extension().and_then(|x| x.to_str()).unwrap_or_default().to_string();
        if !name.starts_with("session-") || own.is_some_and(|a| a.is_own(&name)) {
            continue;
        }
        if ext == "lock" && !data.with_extension("openq").exists() {
            // A session that ended (or crashed) with nothing unsaved: just tidy up its lock.
            if File::options().write(true).open(&data).ok().is_some_and(|f| f.try_lock().is_ok()) {
                let _ = std::fs::remove_file(&data);
            }
            continue;
        }
        if ext != "openq" {
            continue;
        }
        // A live instance holds its lock file; if we can take it, nobody does.
        let lock = data.with_extension("lock");
        let alive = File::options().write(true).open(&lock).ok().is_some_and(|f| f.try_lock().is_err());
        if alive {
            continue;
        }
        let info = std::fs::read_to_string(data.with_extension("json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        out.push(Orphan { data, info });
    }
    out.sort_by_key(|o| std::cmp::Reverse(o.info.saved));
    out
}

/// Autosaves left behind by crashed instances, newest first.
pub fn find_orphans(own: Option<&Autosave>) -> Vec<Orphan> {
    recovery_dir().map(|d| find_orphans_in(&d, own)).unwrap_or_default()
}

// ------------------------------------------------------------------ crash reports

/// Write a crash report for a panic: message, thread, location and backtrace.
fn write_crash_log(info: &std::panic::PanicHookInfo) -> Option<PathBuf> {
    let dir = crash_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let thread = std::thread::current();
    let report = format!(
        "OpenQ {} crash report\ntime: {} (unix)\nthread: {}\npanic: {}\nat: {}\n\nbacktrace:\n{}\n",
        env!("CARGO_PKG_VERSION"),
        unix_secs(),
        thread.name().unwrap_or("?"),
        panic_message(info.payload()),
        info.location().map(|l| l.to_string()).unwrap_or_default(),
        std::backtrace::Backtrace::force_capture(),
    );
    let path = dir.join(format!("crash-{}-{}.log", unix_secs(), std::process::id()));
    std::fs::write(&path, report).ok()?;
    // Keep the folder small.
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut logs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "log")).collect();
        if logs.len() > MAX_CRASH_LOGS {
            logs.sort();
            for p in &logs[..logs.len() - MAX_CRASH_LOGS] {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    Some(path)
}

/// Write crash reports on every panic and, when the main (UI) thread panics, which ends the
/// process, make a last autosave of `show`. Other threads' panics are recovered where they
/// happen.
pub fn install_panic_hook(show: &SharedShow, autosave: Arc<Mutex<Autosave>>) {
    let show = Arc::downgrade(show);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        // One line in the error log for every panic, caught or not; the full report (with the
        // backtrace) goes to its own file.
        let report = write_crash_log(info).map(|p| format!(" (report: {})", p.display())).unwrap_or_default();
        log::error!(
            "panic in thread '{}' at {}: {}{report}",
            std::thread::current().name().unwrap_or("?"),
            info.location().map(|l| l.to_string()).unwrap_or_default(),
            panic_message(info.payload())
        );
        if std::thread::current().name() != Some("main") {
            // Worker, engine and audio-callback panics are caught and recovered; the show
            // keeps running.
            return;
        }
        // The UI thread is going down: save what we can. The lock may be held by this very
        // thread, so only wait briefly.
        if let Some(show) = show.upgrade()
            && let Some(s) = show.try_lock_for(Duration::from_millis(300))
            && s.dirty
            && let Some(mut a) = autosave.try_lock_for(Duration::from_millis(300))
        {
            let _ = a.write(&s.ws, s.path.as_deref());
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("openq-recovery-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn catch_turns_panics_into_errors() {
        let r: anyhow::Result<()> = catch(|| panic!("boom"));
        assert!(format!("{:#}", r.unwrap_err()).contains("boom"));
        assert_eq!(catch(|| Ok(3)).unwrap(), 3);
    }

    #[test]
    fn live_sessions_are_not_orphans_dead_ones_are() {
        let dir = tmpdir("orphans");
        let mut ws = Workspace::default();
        ws.settings.osc_port = 4242;
        let mut a = Autosave::new_in(&dir).unwrap();
        a.write(&ws, Some(Path::new("/shows/Hamlet.openq"))).unwrap();
        let mut b = Autosave::new_in(&dir).unwrap();
        b.write(&ws, None).unwrap();
        // Both alive: nothing to recover (and a session never sees itself).
        assert!(find_orphans_in(&dir, Some(&b)).is_empty());
        // `a` dies without clearing (its lock goes with it).
        drop(a);
        let found = find_orphans_in(&dir, Some(&b));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name(), "Hamlet");
        assert_eq!(found[0].load().unwrap().settings.osc_port, 4242);
        found[0].discard();
        assert!(find_orphans_in(&dir, Some(&b)).is_empty());
        // A clean quit leaves nothing behind.
        b.clear();
        drop(b);
        assert!(find_orphans_in(&dir, None).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
