//! Python effect scripts, run in a WebAssembly sandbox.
//!
//! A script only generates custom effects ([`Shape::Custom`](super::effects::Shape) tables); it
//! never sees the show, the patch or the computer. The interpreter is CPython built for WASI
//! (`python.wasm` + a small standard library, installed by oqman next to the app; see
//! [`find_runtime`]), executed by wasmtime with:
//! * no files except the standard library, mounted read-only;
//! * no network, environment variables or other programs;
//! * a memory cap ([`MEMORY_LIMIT`]) and a run-time limit (epoch interruption, default
//!   [`DEFAULT_TIMEOUT`]); [`ScriptRun::cancel`] stops a run the same way;
//! * stdin (the request) and stdout/stderr in memory, capped.
//!
//! The reply is JSON produced inside the sandbox, so it is untrusted: every effect goes through
//! [`fxfile::sanitize`](super::fxfile::sanitize) before OpenQ uses it.
//!
//! [`Python`] runs scripts one at a time on its own thread (`openq-python`). The interpreter
//! module is compiled once (several seconds) and cached as native code in
//! `<config>/python-cache`, keyed by the module's hash and the wasmtime build.

use super::effects::Effect;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

/// Time a script may run.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
/// Memory a script's interpreter may use.
pub const MEMORY_LIMIT: usize = 256 << 20;
/// Largest reply read from the sandbox.
#[cfg_attr(not(feature = "scripting"), allow(dead_code))]
const MAX_REPLY: usize = 64 << 20;

/// The `openq` module scripts import.
pub const API: &str = include_str!("../../assets/python/openq.py");
/// Type stub for the `openq` module (IDE completion).
pub const STUB: &str = include_str!("../../assets/python/openq.pyi");
/// Runs the script inside the sandbox and writes the reply.
#[cfg_attr(not(feature = "scripting"), allow(dead_code))]
const RUNNER: &str = include_str!("../../assets/python/openq_runner.py");

/// An example script for new effects.
pub const EXAMPLE: &str = r#"import openq

# A rainbow that runs across the fixtures: 8 pattern rows, each a step further round the wheel.
fx = openq.effect("Rainbow", attributes=["red", "green", "blue"], steps=64, width=8)
fx.fill(lambda t, row, width: openq.hsv(t + row / width))
fx.speed = 0.25
"#;

/// The installed interpreter: `python.wasm` and its `lib` directory.
#[derive(Clone, Debug, PartialEq)]
pub struct Runtime {
    /// Directory holding `python.wasm` and `lib/`.
    pub dir: PathBuf,
}

impl Runtime {
    /// The interpreter module.
    pub fn wasm(&self) -> PathBuf {
        self.dir.join("python.wasm")
    }

    fn valid(dir: &Path) -> bool {
        dir.join("python.wasm").is_file() && dir.join("lib").is_dir()
    }
}

/// Where the interpreter is: `OPENQ_PYTHON_WASI`, then `python/` next to the executable (and
/// `Resources/python` in a macOS bundle), then `target/python-wasi` in development builds
/// (`tools/fetch_python_wasi.py`). None: not installed (oqman installs it).
pub fn find_runtime() -> Option<Runtime> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("OPENQ_PYTHON_WASI") {
        dirs.push(PathBuf::from(d));
    }
    if let Some(exe) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        dirs.push(exe.join("python"));
        dirs.push(exe.join("../Resources/python"));
    }
    if cfg!(debug_assertions) {
        dirs.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("target/python-wasi"));
    }
    dirs.into_iter().find(|d| Runtime::valid(d)).map(|dir| Runtime { dir })
}

/// A script to run.
#[derive(Clone, Debug)]
pub struct Request {
    /// Name shown in tracebacks.
    pub name: String,
    /// Python source.
    pub code: String,
    /// Stop the script after this long.
    pub timeout: Duration,
}

impl Request {
    /// A request with the default timeout.
    pub fn new(name: &str, code: &str) -> Self {
        Self { name: name.into(), code: code.into(), timeout: DEFAULT_TIMEOUT }
    }
}

/// Why a run produced no effects.
#[derive(Clone, Debug, PartialEq)]
pub enum ScriptError {
    /// The sandboxed interpreter isn't installed.
    NotInstalled,
    /// The interpreter couldn't be loaded or started.
    Runtime(String),
    /// The script raised an exception (or had a syntax error).
    Script {
        /// `Type: message`.
        message: String,
        /// Line in the script, when the error points into it.
        line: Option<u32>,
        /// Full Python traceback.
        traceback: String,
    },
    /// The script ran past its time limit.
    Timeout(Duration),
    /// Stopped by [`ScriptRun::cancel`].
    Cancelled,
    /// The reply or an effect in it was invalid.
    BadResult(String),
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use crate::i18n::t;
        match self {
            ScriptError::NotInstalled => f.write_str(t("Python for effect scripts isn't installed (oqman installs it with OpenQ).")),
            ScriptError::Runtime(e) => write!(f, "{}: {e}", t("Python couldn't start")),
            ScriptError::Script { message, line: Some(l), .. } => write!(f, "{} {l}: {message}", t("line")),
            ScriptError::Script { message, .. } => f.write_str(message),
            ScriptError::Timeout(d) => f.write_str(&crate::tr!("The script ran longer than {s} s and was stopped.", s = format!("{}", d.as_secs_f32()))),
            ScriptError::Cancelled => f.write_str(t("The script was stopped.")),
            ScriptError::BadResult(e) => write!(f, "{}: {e}", t("The script returned an invalid effect")),
        }
    }
}

/// What a finished run produced.
#[derive(Clone, Debug)]
pub struct Finished {
    /// What the script printed.
    pub output: String,
    /// The effects it generated (checked, numbers 0, script attached), or why there are none.
    pub result: Result<Vec<Effect>, ScriptError>,
}

impl Finished {
    fn failed(e: ScriptError) -> Self {
        Self { output: String::new(), result: Err(e) }
    }
}

/// A script in flight. Dropping it stops the script (or skips it if it hasn't started), so
/// runs nobody waits for don't hold up the queue; [`ScriptRun::cancel`] stops it too.
pub struct ScriptRun {
    rx: Receiver<Finished>,
    cancel: Arc<AtomicBool>,
}

impl ScriptRun {
    /// The result, once there is one (never blocks).
    pub fn poll(&self) -> Option<Finished> {
        match self.rx.try_recv() {
            Ok(f) => Some(f),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Finished::failed(ScriptError::Runtime("stopped".into()))),
        }
    }

    /// Stop the script.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Block until the run is over (tests and tools).
    pub fn wait(&self) -> Finished {
        self.rx.recv().unwrap_or_else(|_| Finished::failed(ScriptError::Runtime("stopped".into())))
    }
}

impl Drop for ScriptRun {
    fn drop(&mut self) {
        self.cancel();
    }
}

enum Job {
    Run { req: Request, reply: Sender<Finished>, cancel: Arc<AtomicBool> },
    Prepare,
}

/// Whether the interpreter is usable, as far as the host knows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// Not looked for or not loaded yet.
    Unknown,
    /// Loaded; scripts can run.
    Ready,
    /// Not installed.
    Missing,
    /// Installed but failed to load.
    Broken,
}

/// The script host: one thread, one script at a time (runs queue up).
pub struct Python {
    jobs: Option<Sender<Job>>,
    status: Arc<std::sync::atomic::AtomicU8>,
}

impl Default for Python {
    /// A host that looks for the interpreter ([`find_runtime`]) on its own thread, so creating
    /// it touches no disk (safe in a UI frame).
    fn default() -> Self {
        Self::spawn(None, true)
    }
}

impl Python {
    /// Start the host thread for an interpreter (None: every run fails with
    /// [`ScriptError::NotInstalled`]).
    pub fn new(runtime: Option<Runtime>) -> Self {
        Self::spawn(runtime, false)
    }

    fn spawn(runtime: Option<Runtime>, search: bool) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let status = Arc::new(std::sync::atomic::AtomicU8::new(Status::Unknown as u8));
        let shared = status.clone();
        std::thread::Builder::new()
            .name("openq-python".into())
            .spawn(move || {
                let mut host = Host { runtime, search, sandbox: None, status: shared };
                for job in rx {
                    match job {
                        Job::Prepare => {
                            if let Err(e) = host.sandbox() {
                                log::warn!("effect scripts: {e}");
                            }
                        }
                        // Cancelled (or dropped) before it started: skip it.
                        Job::Run { reply, cancel, .. } if cancel.load(Ordering::Relaxed) => {
                            let _ = reply.send(Finished::failed(ScriptError::Cancelled));
                        }
                        Job::Run { req, reply, cancel } => {
                            let r = crate::recovery::catch(|| Ok(host.run(&req, &cancel)));
                            let _ = reply.send(r.unwrap_or_else(|e| Finished::failed(ScriptError::Runtime(e.to_string()))));
                        }
                    }
                }
            })
            .expect("spawn openq-python");
        Self { jobs: Some(tx), status }
    }

    /// Whether scripts can run (known once the host has tried to load the interpreter).
    pub fn status(&self) -> Status {
        match self.status.load(Ordering::Relaxed) {
            1 => Status::Ready,
            2 => Status::Missing,
            3 => Status::Broken,
            _ => Status::Unknown,
        }
    }

    /// Load the interpreter now (compiling it the first time) so the first run is quick.
    pub fn prepare(&self) {
        let _ = self.jobs.as_ref().expect("host running").send(Job::Prepare);
    }

    /// Queue a script.
    pub fn run(&self, req: Request) -> ScriptRun {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let _ = self.jobs.as_ref().expect("host running").send(Job::Run { req, reply: tx, cancel: cancel.clone() });
        ScriptRun { rx, cancel }
    }
}

impl Drop for Python {
    /// Closes the queue without waiting: a script may run for up to its timeout and the first
    /// load compiles for seconds, which must not hold up whoever drops the host (quitting the
    /// app). The thread ends after its current job; runs still queued were cancelled when their
    /// [`ScriptRun`]s were dropped.
    fn drop(&mut self) {
        self.jobs = None;
    }
}

struct Host {
    runtime: Option<Runtime>,
    /// Look for the runtime on first use.
    search: bool,
    status: Arc<std::sync::atomic::AtomicU8>,
    #[cfg(feature = "scripting")]
    sandbox: Option<sandbox::Sandbox>,
    #[cfg(not(feature = "scripting"))]
    sandbox: Option<()>,
}

impl Host {
    fn set_status(&self, s: Status) {
        self.status.store(s as u8, Ordering::Relaxed);
    }

    #[cfg(feature = "scripting")]
    fn sandbox(&mut self) -> Result<&sandbox::Sandbox, ScriptError> {
        if self.search {
            self.search = false;
            self.runtime = find_runtime();
        }
        if self.sandbox.is_none() {
            let Some(rt) = self.runtime.as_ref() else {
                self.set_status(Status::Missing);
                return Err(ScriptError::NotInstalled);
            };
            match sandbox::Sandbox::load(rt) {
                Ok(s) => self.sandbox = Some(s),
                Err(e) => {
                    self.set_status(Status::Broken);
                    return Err(ScriptError::Runtime(format!("{e:#}")));
                }
            }
            self.set_status(Status::Ready);
        }
        Ok(self.sandbox.as_ref().expect("loaded"))
    }

    #[cfg(not(feature = "scripting"))]
    fn sandbox(&mut self) -> Result<(), ScriptError> {
        let _ = (&self.runtime, &self.sandbox, self.search);
        self.set_status(Status::Missing);
        Err(ScriptError::NotInstalled)
    }

    #[cfg(feature = "scripting")]
    fn run(&mut self, req: &Request, cancel: &Arc<AtomicBool>) -> Finished {
        let sb = match self.sandbox() {
            Ok(s) => s,
            Err(e) => return Finished::failed(e),
        };
        let input = serde_json::json!({ "api": API, "name": req.name, "code": req.code }).to_string();
        match sb.run(input.into_bytes(), req.timeout, cancel) {
            Ok(reply) => parse_reply(&reply, req),
            Err(e) => Finished::failed(e),
        }
    }

    #[cfg(not(feature = "scripting"))]
    fn run(&mut self, _req: &Request, _cancel: &Arc<AtomicBool>) -> Finished {
        Finished { output: String::new(), result: self.sandbox().map(|_| Vec::new()) }
    }
}

/// Turn the sandbox's reply into checked effects.
#[cfg_attr(not(feature = "scripting"), allow(dead_code))]
fn parse_reply(reply: &[u8], req: &Request) -> Finished {
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct Reply {
        ok: bool,
        output: String,
        effects: Vec<serde_json::Value>,
        message: String,
        line: Option<u32>,
        traceback: String,
    }
    let r: Reply = match serde_json::from_slice(reply) {
        Ok(r) => r,
        Err(e) => {
            let text = String::from_utf8_lossy(&reply[..reply.len().min(2000)]).into_owned();
            return Finished { output: text, result: Err(ScriptError::BadResult(format!("unreadable reply ({e})"))) };
        }
    };
    let output = r.output.chars().take(1 << 18).collect();
    if !r.ok {
        return Finished { output, result: Err(ScriptError::Script { message: r.message, line: r.line, traceback: r.traceback }) };
    }
    let mut effects = Vec::new();
    for (i, v) in r.effects.into_iter().enumerate().take(64) {
        let e = serde_json::from_value::<Effect>(v).map_err(|e| e.to_string()).and_then(|e| super::fxfile::sanitize(e).map_err(|e| format!("{e:#}")));
        match e {
            Ok(mut e) => {
                e.script = req.code.clone();
                effects.push(e);
            }
            Err(m) => return Finished { output, result: Err(ScriptError::BadResult(format!("effect {}: {m}", i + 1))) },
        }
    }
    Finished { output, result: Ok(effects) }
}

#[cfg(feature = "scripting")]
mod sandbox {
    use super::*;
    use anyhow::Context;
    use wasmtime::{Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
    use wasmtime_wasi::p1::{self, WasiP1Ctx};
    use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
    use wasmtime_wasi::{DirPerms, FilePerms, I32Exit, WasiCtxBuilder};

    struct State {
        wasi: WasiP1Ctx,
        limits: StoreLimits,
    }

    pub(super) struct Sandbox {
        engine: Engine,
        module: Module,
        linker: Linker<State>,
        lib: PathBuf,
    }

    impl Sandbox {
        pub(super) fn load(rt: &Runtime) -> anyhow::Result<Self> {
            let mut config = Config::new();
            config.epoch_interruption(true);
            let engine = Engine::new(&config)?;
            let module = load_module(&engine, &rt.wasm())?;
            let mut linker = Linker::new(&engine);
            p1::add_to_linker_sync(&mut linker, |s: &mut State| &mut s.wasi)?;
            Ok(Self { engine, module, linker, lib: rt.dir.join("lib") })
        }

        /// Run the interpreter once with `input` on stdin; returns stdout.
        pub(super) fn run(&self, input: Vec<u8>, timeout: Duration, cancel: &Arc<AtomicBool>) -> Result<Vec<u8>, ScriptError> {
            let rt = |e: wasmtime::Error| ScriptError::Runtime(format!("{e:#}"));
            let stdout = MemoryOutputPipe::new(MAX_REPLY);
            let stderr = MemoryOutputPipe::new(64 << 10);
            let mut b = WasiCtxBuilder::new();
            b.args(&["python", "-I", "-S", "-B", "-X", "utf8", "-c", RUNNER])
                .stdin(MemoryInputPipe::new(input))
                .stdout(stdout.clone())
                .stderr(stderr.clone());
            b.preopened_dir(&self.lib, "/lib", DirPerms::READ, FilePerms::READ).map_err(rt)?;
            let limits = StoreLimitsBuilder::new().memory_size(MEMORY_LIMIT).instances(4).tables(16).memories(4).build();
            let mut store = Store::new(&self.engine, State { wasi: b.build_p1(), limits });
            store.limiter(|s| &mut s.limits);
            store.set_epoch_deadline(1);
            // The watchdog ends the run by moving the engine's epoch on: at the deadline or on
            // cancel. It quits when the run ends first (dropping `done`).
            let (done, finished) = mpsc::channel::<()>();
            let engine = self.engine.clone();
            let flag = cancel.clone();
            let stopped = Arc::new(AtomicBool::new(false));
            let why = stopped.clone();
            let watchdog = std::thread::Builder::new()
                .name("openq-python-watch".into())
                .spawn(move || {
                    let end = std::time::Instant::now() + timeout;
                    loop {
                        let left = end.saturating_duration_since(std::time::Instant::now());
                        match finished.recv_timeout(left.min(Duration::from_millis(20))) {
                            Err(mpsc::RecvTimeoutError::Timeout) if left.is_zero() || flag.load(Ordering::Relaxed) => {
                                why.store(true, Ordering::Relaxed);
                                engine.increment_epoch();
                                return;
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                            _ => return,
                        }
                    }
                })
                .expect("spawn watchdog");
            let result = (|| -> wasmtime::Result<()> {
                let instance = self.linker.instantiate(&mut store, &self.module)?;
                let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
                start.call(&mut store, ())
            })();
            drop(done);
            let _ = watchdog.join();
            match result {
                Ok(()) => {}
                Err(e) if e.downcast_ref::<I32Exit>().is_some_and(|x| x.0 == 0) => {}
                Err(_) if stopped.load(Ordering::Relaxed) => {
                    return Err(if cancel.load(Ordering::Relaxed) { ScriptError::Cancelled } else { ScriptError::Timeout(timeout) });
                }
                Err(e) => {
                    let err = String::from_utf8_lossy(&stderr.contents()).trim().to_string();
                    let detail = if err.is_empty() { format!("{e:#}") } else { err };
                    return Err(ScriptError::Runtime(detail));
                }
            }
            Ok(stdout.contents().to_vec())
        }
    }

    /// The compiled interpreter: from the cache when it matches, else compiled and cached.
    fn load_module(engine: &Engine, wasm: &Path) -> anyhow::Result<Module> {
        use sha2::Digest;
        use std::hash::{Hash, Hasher};
        let bytes = std::fs::read(wasm).with_context(|| format!("cannot read {}", wasm.display()))?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        engine.precompile_compatibility_hash().hash(&mut h);
        let digest = sha2::Sha256::digest(&bytes);
        let key = format!("python-{}-{:016x}.cwasm", digest.iter().take(12).map(|b| format!("{b:02x}")).collect::<String>(), h.finish());
        let cache = crate::remote::config_dir().map(|d| d.join("python-cache"));
        if let Some(path) = cache.as_ref().map(|d| d.join(&key))
            && path.is_file()
        {
            // SAFETY: the file is native code wasmtime wrote into the user's own config
            // directory for this exact module and engine build (both in the name); wasmtime
            // checks the engine compatibility again. Anyone who can change that directory can
            // already run code as the user.
            match unsafe { Module::deserialize_file(engine, &path) } {
                Ok(m) => return Ok(m),
                Err(e) => log::warn!("python cache unusable, compiling again: {e:#}"),
            }
        }
        let t = std::time::Instant::now();
        let module = Module::new(engine, &bytes).map_err(anyhow::Error::from).context("cannot compile python.wasm")?;
        log::info!("compiled python.wasm in {:.1} s", t.elapsed().as_secs_f32());
        if let Some(dir) = cache {
            let write = || -> anyhow::Result<()> {
                std::fs::create_dir_all(&dir)?;
                for old in std::fs::read_dir(&dir)?.flatten() {
                    if old.file_name().to_string_lossy().ends_with(".cwasm") {
                        let _ = std::fs::remove_file(old.path());
                    }
                }
                crate::recovery::write_atomic(&dir.join(&key), &module.serialize()?)
            };
            if let Err(e) = write() {
                log::warn!("cannot cache the compiled python: {e:#}");
            }
        }
        Ok(module)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light::effects::Shape;
    use crate::light::Attribute;

    /// The interpreter is optional on build machines (`tools/fetch_python_wasi.py`): tests that
    /// need it pass with a note when it's missing. One host for every test: the module is big.
    fn host() -> Option<&'static Python> {
        static HOST: std::sync::OnceLock<Option<Python>> = std::sync::OnceLock::new();
        let h = HOST.get_or_init(|| find_runtime().filter(|_| cfg!(feature = "scripting")).map(|r| Python::new(Some(r))));
        if h.is_none() {
            eprintln!("no sandboxed Python (tools/fetch_python_wasi.py): skipped");
        }
        h.as_ref()
    }

    fn run(code: &str) -> Finished {
        host().expect("checked").run(Request::new("test.py", code)).wait()
    }

    #[test]
    fn example_generates_a_custom_effect() {
        if host().is_none() {
            return;
        }
        let f = run(EXAMPLE);
        let fx = f.result.unwrap();
        assert_eq!(fx.len(), 1);
        let e = &fx[0];
        assert_eq!((e.name.as_str(), e.shape, e.speed, e.attribute), ("Rainbow", Shape::Custom, 0.25, Attribute::Red));
        let t = e.table.as_ref().unwrap();
        assert_eq!((t.steps, t.width, t.attributes.len(), t.values.len()), (64, 8, 3, 64 * 8 * 3));
        assert_eq!(t.sample(0, 0, 0.0), 1.0, "hue 0 = red");
        assert_eq!(e.script, EXAMPLE, "the script travels with the effect");
    }

    #[test]
    fn output_errors_and_helpers() {
        if host().is_none() {
            return;
        }
        let code = "import openq, math\nprint('hi', openq.square(0.2), round(openq.sine(0.5), 3))\n\
                    fx = openq.effect('Chase', 'dimmer', steps=4, width=4, smooth=False)\n\
                    fx.fill(lambda t, row, w: 1 if int(t * 4) == row else 0)\n\
                    fx.set(0, 1, 'dimmer', 0.5)\n";
        let f = run(code);
        assert_eq!(f.output, "hi 1.0 1.0\n");
        let t = f.result.unwrap().remove(0).table.unwrap();
        assert_eq!(&t.values[..8], &[1.0, 0.5, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        let f = run("print('before')\n\nimport openq\nopenq.effect('x', 'colour')\n");
        assert_eq!(f.output, "before\n");
        let Err(ScriptError::Script { message, line, .. }) = f.result else { panic!("{:?}", f.result) };
        assert_eq!(line, Some(4));
        assert!(message.contains("unknown attribute"), "{message}");
        assert!(matches!(run("x = (\n").result, Err(ScriptError::Script { line: Some(1), .. })));
        assert!(run("raise SystemExit").result.unwrap().is_empty());
        // A forged reply still goes through the checks.
        let f = run("import openq\nopenq._made.append(type('E', (), {'_json': lambda s: {'shape': 'Custom'}})())");
        assert!(matches!(f.result, Err(ScriptError::BadResult(_))), "{:?}", f.result);
    }

    /// The standard library oqman installs covers what scripts are told they can use.
    #[test]
    fn standard_modules_import() {
        if host().is_none() {
            return;
        }
        let mods = "math cmath random colorsys statistics itertools functools operator collections dataclasses \
                    typing fractions decimal datetime json re string textwrap bisect heapq copy enum struct";
        let code = format!("for m in '{mods}'.split():\n    __import__(m)\nprint('ok')");
        let f = run(&code);
        assert!(f.result.is_ok(), "{:?}", f.result);
        assert_eq!(f.output, "ok\n");
    }

    #[test]
    fn the_sandbox_holds() {
        if host().is_none() {
            return;
        }
        // No files outside the read-only library, no writing, no processes, no network, no env.
        let code = r#"
import os
found = []
for probe in ("/etc/passwd", "/home", "/tmp", ".", os.path.expanduser("~"), "/lib/../../etc/passwd"):
    try:
        os.listdir(probe) if probe in (".", "/home", "/tmp") else open(probe).read()
        found.append(probe)
    except Exception:
        pass
for target in ("/lib/x.py", "/x", "x"):
    try:
        open(target, "w").write("x")
        found.append("wrote " + target)
    except Exception:
        pass
for mod in ("subprocess", "socket", "ctypes", "urllib.request", "http.client", "shutil"):
    try:
        __import__(mod)
        found.append(mod)
    except Exception:
        pass
try:
    import _socket
    s = _socket.socket(_socket.AF_INET, _socket.SOCK_STREAM)
    s.connect(("127.0.0.1", 53000))
    found.append("connected")
except Exception:
    pass
for call in ("system", "fork", "execv", "spawnv", "posix_spawn"):
    if hasattr(os, call):
        found.append("os." + call)
try:
    os.popen("echo hi").read()
    found.append("popen")
except Exception:
    pass
found += list(os.environ)
print(found)
"#;
        let f = run(code);
        assert!(f.result.is_ok(), "{:?}", f.result);
        assert_eq!(f.output.trim(), "[]", "{}", f.output);
    }

    #[test]
    fn limits_stop_runaway_scripts() {
        let Some(py) = host() else { return };
        let mut req = Request::new("loop.py", "while True: pass");
        req.timeout = Duration::from_millis(500);
        let t = std::time::Instant::now();
        assert_eq!(py.run(req).wait().result.unwrap_err(), ScriptError::Timeout(Duration::from_millis(500)));
        assert!(t.elapsed() < Duration::from_secs(5));
        let run = py.run(Request::new("loop.py", "while True: pass"));
        std::thread::sleep(Duration::from_millis(300));
        run.cancel();
        assert_eq!(run.wait().result.unwrap_err(), ScriptError::Cancelled);
        // Memory: a MemoryError inside Python, not an OpenQ crash.
        let f = py.run(Request::new("mem.py", "x = bytearray(1 << 30)")).wait();
        assert!(matches!(&f.result, Err(ScriptError::Script { message, .. }) if message.contains("MemoryError")), "{:?}", f.result);
        assert!(py.run(Request::new("ok.py", "pass")).wait().result.is_ok(), "runs again after");
        // Runs nobody waits for any more stop: three abandoned endless scripts don't hold up the next.
        for _ in 0..3 {
            drop(py.run(Request::new("loop.py", "while True: pass")));
        }
        let t = std::time::Instant::now();
        assert!(py.run(Request::new("ok.py", "pass")).wait().result.is_ok());
        assert!(t.elapsed() < Duration::from_secs(10), "waited {:?} behind abandoned runs", t.elapsed());
    }

    #[test]
    fn missing_runtime_is_reported() {
        let py = Python::new(None);
        assert_eq!(py.status(), Status::Unknown);
        assert_eq!(py.run(Request::new("x", "pass")).wait().result.unwrap_err(), ScriptError::NotInstalled);
        assert_eq!(py.status(), Status::Missing);
    }

    /// Names the Python module hardcodes must match the Rust enum (serde names) and limits.
    #[test]
    fn api_names_match_the_model() {
        let start = API.find("\nATTRIBUTES = [").expect("ATTRIBUTES in openq.py");
        let body = &API[start..];
        let body = &body[body.find('[').unwrap() + 1..body.find(']').unwrap()];
        let names: Vec<&str> = body.split(',').map(|s| s.trim().trim_matches('"')).filter(|s| !s.is_empty()).collect();
        let attrs: Vec<String> = Attribute::ALL.iter().map(|a| serde_json::to_value(a).unwrap().as_str().unwrap().to_string()).collect();
        assert_eq!(names, attrs);
        use crate::light::effects::EffectTable as T;
        for (py, rust) in [("MAX_ATTRIBUTES", T::MAX_ATTRIBUTES), ("MAX_STEPS", T::MAX_STEPS as usize), ("MAX_WIDTH", T::MAX_WIDTH as usize)] {
            assert!(API.contains(&format!("\n{py} = {rust}\n")), "{py} differs from Rust");
        }
        assert!(API.contains("\nMAX_VALUES = 1 << 20\n") && T::MAX_VALUES == 1 << 20);
    }

    /// Every public name of the module is in the stub.
    #[test]
    fn stub_covers_the_api() {
        let start = API.find("__all__ = [").unwrap();
        let body = &API[start + 11..];
        let body = &body[..body.find(']').unwrap()];
        for name in body.split(',').map(|s| s.trim().trim_matches('"')).filter(|s| !s.is_empty()) {
            let declared = [format!("\ndef {name}("), format!("\nclass {name}"), format!("\n{name}:")];
            assert!(declared.iter().any(|d| STUB.contains(d.as_str())), "{name} missing from openq.pyi");
        }
    }
}
