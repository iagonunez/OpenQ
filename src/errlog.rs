//! Error log: every warning and error OpenQ logs (engine log entries, device failures, failed
//! saves, worker panics, crashes) is also appended to a plain-text file, so problems can be
//! looked at after the show. The file is `<config dir>/logs/openq.log` ([`log_path`]); Help →
//! Error log opens it.
//!
//! [`init`] installs the global logger: messages still go to stderr through `env_logger` (as
//! filtered by `RUST_LOG`), and `Warn`/`Error` records from any crate go to the file whatever
//! the filter says. When the file grows past [`MAX_BYTES`] at start-up it is moved to
//! `openq.old.log` (one old file is kept). Times are UTC.

use parking_lot::Mutex;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Size above which the log is rotated when OpenQ starts.
pub const MAX_BYTES: u64 = 2 << 20;

/// Folder holding the error log.
pub fn log_dir() -> Option<PathBuf> {
    crate::remote::config_dir().map(|d| d.join("logs"))
}

/// The error log file.
pub fn log_path() -> Option<PathBuf> {
    log_dir().map(|d| d.join("openq.log"))
}

/// An append-only log file.
pub struct ErrorLog {
    file: Mutex<File>,
    path: PathBuf,
}

impl ErrorLog {
    /// Open (or create) `openq.log` in `dir`, rotating it first when it is over [`MAX_BYTES`],
    /// and write a session header.
    pub fn open_in(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("openq.log");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
            std::fs::rename(&path, dir.join("openq.old.log"))?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let log = ErrorLog { file: Mutex::new(file), path };
        log.line(&format!(
            "---- OpenQ {} started (pid {}, {} {})",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
        Ok(log)
    }

    /// Where the file is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one entry: time, level, thread and source, then the message.
    pub fn write(&self, level: log::Level, target: &str, msg: &str) {
        let thread = std::thread::current();
        self.line(&format!("{level:<5} [{}] {target}: {msg}", thread.name().unwrap_or("?")));
    }

    fn line(&self, text: &str) {
        // Unbuffered: a crash right after must not lose the line.
        let mut f = self.file.lock();
        let _ = writeln!(f, "{} {text}", timestamp(SystemTime::now()));
        let _ = f.flush();
    }
}

/// `YYYY-MM-DD hh:mm:ss.mmmZ` (UTC).
pub(crate) fn timestamp(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        d.subsec_millis()
    )
}

/// Sources whose warnings are known to be harmless (kept out of the file, still on stderr).
const QUIET: &[&str] = &["epaint::text::fonts"];

struct Logger {
    stderr: env_logger::Logger,
    file: Option<ErrorLog>,
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        self.stderr.enabled(m) || (self.file.is_some() && m.level() <= log::Level::Warn)
    }

    fn log(&self, r: &log::Record) {
        if self.stderr.matches(r) {
            self.stderr.log(r);
        }
        if let Some(f) = &self.file
            && r.level() <= log::Level::Warn
            && !QUIET.contains(&r.target())
        {
            f.write(r.level(), r.target(), &r.args().to_string());
        }
    }

    fn flush(&self) {
        self.stderr.flush();
    }
}

/// Install the global logger: stderr with `default_filter` (overridden by `RUST_LOG`), plus the
/// error log file. Returns the file's path, or `None` when it could not be opened (logging then
/// goes to stderr only).
pub fn init(default_filter: &str) -> Option<PathBuf> {
    let stderr = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter)).build();
    let file = log_dir().map(|d| ErrorLog::open_in(&d)).transpose().unwrap_or_else(|e| {
        eprintln!("error log unavailable: {e}");
        None
    });
    let path = file.as_ref().map(|f| f.path().to_path_buf());
    let max = if file.is_some() { stderr.filter().max(log::LevelFilter::Warn) } else { stderr.filter() };
    if log::set_boxed_logger(Box::new(Logger { stderr, file })).is_ok() {
        log::set_max_level(max);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_utc_civil_dates() {
        let t = UNIX_EPOCH + std::time::Duration::from_millis(1_791_900_245_123);
        assert_eq!(timestamp(t), "2026-10-13 14:04:05.123Z");
        assert_eq!(timestamp(UNIX_EPOCH + std::time::Duration::from_secs(951_782_400)), "2000-02-29 00:00:00.000Z");
    }

    #[test]
    fn appends_entries_and_rotates_big_files() {
        let dir = std::env::temp_dir().join(format!("openq-errlog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = ErrorLog::open_in(&dir).unwrap();
        log.write(log::Level::Error, "openq::engine", "Cue 3 'Song': file missing");
        drop(log);
        let text = std::fs::read_to_string(dir.join("openq.log")).unwrap();
        assert!(text.contains("started (pid") && text.contains("ERROR [") && text.contains("openq::engine: Cue 3 'Song': file missing"), "{text}");

        std::fs::write(dir.join("openq.log"), vec![b'x'; MAX_BYTES as usize + 1]).unwrap();
        ErrorLog::open_in(&dir).unwrap();
        assert_eq!(std::fs::metadata(dir.join("openq.old.log")).unwrap().len(), MAX_BYTES + 1);
        assert!(std::fs::metadata(dir.join("openq.log")).unwrap().len() < 200);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
