//! Show package (`.opnqs`): a show and every media file it plays in one file, to move a show
//! to another computer, archive it, or give each computer of a coupling group
//! ([`crate::remote::coupling`]) the same files.
//!
//! Layout: `OPNQPKG`, a version byte, the length of the manifest (u32, little endian), the
//! manifest (JSON: package id, name, and for every file its path, size and SHA-256), then the
//! files' bytes one after the other in the manifest's order. The first file is the show itself
//! (`<name>.opnq`, its media paths relative to the package). Everything is checked before a byte
//! is written to disk: path names ([`valid_path`]: no `..`, absolute or drive paths, backslashes,
//! hidden or reserved names, nothing that two names could share on Windows or macOS), sizes
//! against the file's length, and every file's hash while it is extracted. A package opens by
//! unpacking into `<config>/packages/<name>-<id>/` ([`extract`]), once: an unpacked package that
//! is still complete is reused. The open show is not changed by exporting.

use crate::bundle::{Plan, Progress};
use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;

/// File extension of packages.
pub const EXTENSION: &str = "opnqs";
const MAGIC: &[u8; 8] = b"OPNQPKG\0";
const VERSION: u8 = 1;
/// Largest manifest accepted.
pub const MAX_MANIFEST: usize = 32 << 20;
/// Most files in a package.
pub const MAX_ENTRIES: usize = 100_000;
/// Longest path inside a package, and longest name in it.
const MAX_PATH: usize = 1024;
const MAX_NAME: usize = 255;
/// Name of the manifest copy kept in an unpacked package.
const MARKER: &str = ".opnq-package.json";

/// One file of a package.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    /// Path inside the package, with `/`.
    pub path: String,
    /// Length in bytes.
    pub size: u64,
    /// SHA-256 of the bytes (lowercase hex).
    pub sha256: String,
}

/// What a package holds.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Manifest {
    /// Names this package (an unpacked copy lives in a folder made from it).
    pub id: Uuid,
    /// The show's name.
    pub name: String,
    /// OpnQ version that wrote it.
    pub app: String,
    /// Files; the first is the show.
    pub entries: Vec<Entry>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A path inside a package that is safe to create below a folder on any system.
pub fn valid_path(p: &str) -> Result<(), String> {
    if p.is_empty() || p.len() > MAX_PATH {
        return Err("empty or too long".into());
    }
    for part in p.split('/') {
        if part.is_empty() || part.len() > MAX_NAME {
            return Err(format!("bad part in {p:?}"));
        }
        // `.` and `..`, hidden files (the unpacked marker is one), and names Windows trims.
        if part.starts_with('.') || part.ends_with('.') || part.ends_with(' ') {
            return Err(format!("{p:?}: names can't start with a dot or end with a dot or space"));
        }
        if part.chars().any(|c| c.is_control() || matches!(c, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')) {
            return Err(format!("{p:?} has characters that aren't allowed"));
        }
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        let reserved = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].contains(&stem.as_str())
            || ((stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4 && stem.as_bytes()[3].is_ascii_digit());
        if reserved {
            return Err(format!("{p:?} is a reserved name"));
        }
    }
    Ok(())
}

impl Manifest {
    /// Refuse anything a package must not hold (see [`valid_path`]). Total size is checked
    /// without overflow.
    pub fn check(&self) -> Result<()> {
        if self.entries.is_empty() || self.entries.len() > MAX_ENTRIES {
            bail!("a package holds 1 to {MAX_ENTRIES} files");
        }
        if !self.entries[0].path.ends_with(".opnq") || self.entries[0].path.contains('/') {
            bail!("the first file must be the show");
        }
        let mut names = HashSet::new();
        let mut dirs = HashSet::new();
        let mut total: u64 = 0;
        for e in &self.entries {
            valid_path(&e.path).map_err(anyhow::Error::msg)?;
            if e.sha256.len() != 64 || !e.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
                bail!("{:?} has no valid hash", e.path);
            }
            total = total.checked_add(e.size).context("sizes add up to too much")?;
            // Case-insensitive, as macOS and Windows file systems are.
            let lower = e.path.to_lowercase();
            if !names.insert(lower.clone()) {
                bail!("{:?} appears twice", e.path);
            }
            let mut at = 0;
            while let Some(i) = lower[at..].find('/') {
                dirs.insert(lower[..at + i].to_string());
                at += i + 1;
            }
        }
        // A name can't be a file and a folder.
        if let Some(clash) = names.intersection(&dirs).next() {
            bail!("{clash:?} is both a file and a folder");
        }
        Ok(())
    }

    /// Bytes of all the files.
    pub fn total(&self) -> u64 {
        self.entries.iter().map(|e| e.size).fold(0, u64::saturating_add)
    }

    /// Folder name an unpacked copy gets.
    fn folder(&self) -> String {
        let name: String = self.name.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' { c } else { '_' }).collect();
        let name = name.trim().chars().take(60).collect::<String>();
        format!("{}-{}", if name.is_empty() { "show" } else { &name }, &self.id.simple().to_string()[..8])
    }
}

/// Where unpacked packages live.
pub fn packages_dir() -> Option<PathBuf> {
    crate::remote::config_dir().map(|d| d.join("packages"))
}

fn relative(plan: &Plan, to: &Path) -> Result<String> {
    let rel = to.strip_prefix(&plan.dir).with_context(|| to.display().to_string())?;
    let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    Ok(parts.join("/"))
}

/// Write the package of `plan` (made by [`crate::bundle::plan`]) to `out`. Two passes over the
/// media (hash, then copy, checking the hash again: a file that changes meanwhile is an error);
/// written beside `out` and renamed into place, so `out` is whole or absent.
pub fn export(plan: &Plan, out: &Path, progress: &Mutex<Progress>, cancel: &AtomicBool) -> Result<()> {
    let show_name = plan.show.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "show.opnq".into());
    let show_json = plan.ws.to_json(Some(&plan.dir))?;
    let mut sources: Vec<(String, Option<&Path>)> = vec![(show_name, None)];
    for (from, to) in &plan.copies {
        sources.push((relative(plan, to)?, Some(from)));
    }
    let total: u64 = plan.copies.iter().filter_map(|(f, _)| std::fs::metadata(f).ok()).map(|m| m.len()).sum::<u64>() + show_json.len() as u64;
    {
        let mut p = progress.lock();
        p.total_bytes = total * 2;
        p.total_files = sources.len();
    }
    let cancelled = || -> Result<()> {
        if cancel.load(Ordering::Acquire) {
            bail!("{}", crate::i18n::t("cancelled"));
        }
        Ok(())
    };
    let mut buf = vec![0u8; 1 << 20];
    // Pass 1: sizes and hashes.
    let mut entries = Vec::new();
    for (path, from) in &sources {
        let (size, sha) = match from {
            None => (show_json.len() as u64, hex(&Sha256::digest(show_json.as_bytes()))),
            Some(from) => {
                crate::media::check_local(from)?;
                let mut f = std::fs::File::open(from).with_context(|| from.display().to_string())?;
                let (mut h, mut size) = (Sha256::new(), 0u64);
                loop {
                    cancelled()?;
                    let n = f.read(&mut buf).with_context(|| from.display().to_string())?;
                    if n == 0 {
                        break;
                    }
                    h.update(&buf[..n]);
                    size += n as u64;
                    progress.lock().bytes += n as u64;
                }
                (size, hex(&h.finalize()))
            }
        };
        entries.push(Entry { path: path.clone(), size, sha256: sha });
    }
    let manifest = Manifest {
        id: Uuid::new_v4(),
        name: plan.show.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        app: env!("CARGO_PKG_VERSION").into(),
        entries,
    };
    manifest.check()?;
    let json = serde_json::to_vec(&manifest)?;
    if json.len() > MAX_MANIFEST {
        bail!("too many files");
    }
    // Pass 2: the file.
    let mut tmp = out.as_os_str().to_owned();
    tmp.push(".part");
    let tmp = PathBuf::from(tmp);
    let mut write = || -> Result<()> {
        let mut w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&tmp).with_context(|| tmp.display().to_string())?);
        w.write_all(MAGIC)?;
        w.write_all(&[VERSION])?;
        w.write_all(&(json.len() as u32).to_le_bytes())?;
        w.write_all(&json)?;
        for ((_, from), e) in sources.iter().zip(&manifest.entries) {
            match from {
                None => {
                    w.write_all(show_json.as_bytes())?;
                    progress.lock().bytes += e.size;
                }
                Some(from) => {
                    let mut f = std::fs::File::open(from).with_context(|| from.display().to_string())?;
                    let (mut h, mut left) = (Sha256::new(), e.size);
                    while left > 0 {
                        cancelled()?;
                        let want = buf.len().min(left as usize);
                        let n = f.read(&mut buf[..want]).with_context(|| from.display().to_string())?;
                        if n == 0 {
                            bail!("{} changed while it was being packed", from.display());
                        }
                        h.update(&buf[..n]);
                        w.write_all(&buf[..n])?;
                        left -= n as u64;
                        progress.lock().bytes += n as u64;
                    }
                    if f.read(&mut buf[..1])? != 0 || hex(&h.finalize()) != e.sha256 {
                        bail!("{} changed while it was being packed", from.display());
                    }
                }
            }
            progress.lock().files += 1;
        }
        let f = w.into_inner().map_err(|e| e.into_error())?;
        f.sync_all()?;
        std::fs::rename(&tmp, out).with_context(|| out.display().to_string())?;
        Ok(())
    };
    let r = write();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Read and check a package's manifest. The file's length must be exactly the header plus the
/// files (a truncated or padded package is refused before anything is unpacked).
pub fn read_manifest(f: &mut std::fs::File, file_len: u64) -> Result<(Manifest, u64)> {
    let mut head = [0u8; 13];
    f.read_exact(&mut head).context("not an OpnQ show package (too short)")?;
    if &head[..8] != MAGIC {
        bail!("not an OpnQ show package");
    }
    if head[8] != VERSION {
        bail!("this package is from a newer OpnQ (format {})", head[8]);
    }
    let len = u32::from_le_bytes([head[9], head[10], head[11], head[12]]) as usize;
    if len == 0 || len > MAX_MANIFEST || len as u64 > file_len {
        bail!("damaged package (manifest)");
    }
    let mut json = vec![0u8; len];
    f.read_exact(&mut json).context("damaged package (manifest cut short)")?;
    let manifest: Manifest = serde_json::from_slice(&json).context("damaged package (manifest)")?;
    manifest.check()?;
    let data_at = 13 + len as u64;
    if data_at.checked_add(manifest.total()) != Some(file_len) {
        bail!("damaged package (it is {} bytes, its files need {})", file_len, data_at.saturating_add(manifest.total()));
    }
    Ok((manifest, data_at))
}

/// An unpacked package.
#[derive(Clone, Debug)]
pub struct Unpacked {
    /// Folder it was unpacked into.
    pub dir: PathBuf,
    /// The show file inside it.
    pub show: PathBuf,
    /// What the package holds.
    pub manifest: Manifest,
}

/// `dir` as an unpacked package, if it is one and still all there.
pub fn unpacked(dir: &Path) -> Option<Unpacked> {
    let raw = crate::recovery::read_file(&dir.join(MARKER), MAX_MANIFEST as u64).ok()?;
    let manifest: Manifest = serde_json::from_slice(&raw).ok()?;
    manifest.check().ok()?;
    for e in &manifest.entries {
        let m = std::fs::metadata(dir.join(&e.path)).ok()?;
        if !m.is_file() || m.len() != e.size {
            return None;
        }
    }
    let show = dir.join(&manifest.entries[0].path);
    Some(Unpacked { dir: dir.to_path_buf(), show, manifest })
}

/// Unpack `package` below `root`, or find it already unpacked. Every file is hashed as it is
/// written; on any error or cancel nothing is left behind.
pub fn extract(package: &Path, root: &Path, progress: &Mutex<Progress>, cancel: &AtomicBool) -> Result<Unpacked> {
    crate::media::check_local(package)?;
    let mut f = std::fs::File::open(package).with_context(|| package.display().to_string())?;
    let meta = f.metadata()?;
    if !meta.is_file() {
        bail!("not a file");
    }
    let (manifest, data_at) = read_manifest(&mut f, meta.len())?;
    {
        let mut p = progress.lock();
        p.total_files = manifest.entries.len();
        p.total_bytes = manifest.total();
    }
    std::fs::create_dir_all(root).with_context(|| root.display().to_string())?;
    let dir = root.join(manifest.folder());
    if let Some(done) = unpacked(&dir)
        && done.manifest == manifest
    {
        return Ok(done);
    }
    let mut tmp = dir.as_os_str().to_owned();
    tmp.push(".part");
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir(&tmp).with_context(|| tmp.display().to_string())?;
    let mut work = || -> Result<()> {
        use std::io::Seek;
        f.seek(std::io::SeekFrom::Start(data_at))?;
        let mut buf = vec![0u8; 1 << 20];
        for e in &manifest.entries {
            let to = tmp.join(&e.path);
            if let Some(d) = to.parent() {
                std::fs::create_dir_all(d)?;
            }
            // `create_new`: nothing already there (a link, say) is written through.
            let mut out = std::io::BufWriter::new(std::fs::OpenOptions::new().write(true).create_new(true).open(&to).with_context(|| e.path.clone())?);
            let (mut h, mut left) = (Sha256::new(), e.size);
            while left > 0 {
                if cancel.load(Ordering::Acquire) {
                    bail!("{}", crate::i18n::t("cancelled"));
                }
                let want = buf.len().min(left as usize);
                f.read_exact(&mut buf[..want]).with_context(|| format!("the package ends inside {}", e.path))?;
                h.update(&buf[..want]);
                out.write_all(&buf[..want])?;
                left -= want as u64;
                progress.lock().bytes += want as u64;
            }
            if hex(&h.finalize()) != e.sha256 {
                bail!("{} is damaged (its hash doesn't match)", e.path);
            }
            out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
            progress.lock().files += 1;
        }
        std::fs::write(tmp.join(MARKER), serde_json::to_vec(&manifest)?)?;
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::rename(&tmp, &dir).with_context(|| dir.display().to_string())?;
        Ok(())
    };
    if let Err(e) = work() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    let show = dir.join(&manifest.entries[0].path);
    Ok(Unpacked { dir, show, manifest })
}

/// Point the media paths of `ws` at an unpacked package: a path that ends with a file of the
/// package (`…/audio/kick.wav` for `audio/kick.wav`, the longest match) becomes that file in
/// `dir`. For a computer that plays a show another one holds, whose paths are the other's.
/// Returns how many paths changed.
pub fn remap(ws: &mut crate::model::Workspace, dir: &Path, manifest: &Manifest) -> usize {
    let names: Vec<(String, &Entry)> = manifest.entries.iter().skip(1).map(|e| (format!("/{}", e.path.to_lowercase()), e)).collect();
    let mut changed = 0;
    ws.for_each_file_mut(&mut |p| {
        if p.as_os_str().is_empty() {
            return;
        }
        let here = format!("/{}", p.to_string_lossy().replace('\\', "/").to_lowercase());
        if let Some((_, e)) = names.iter().filter(|(n, _)| here.ends_with(n.as_str())).max_by_key(|(n, _)| n.len()) {
            let to = dir.join(&e.path);
            if *p != to {
                *p = to;
                changed += 1;
            }
        }
    });
    changed
}

/// What a running [`Job`] does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Writing a package.
    Export,
    /// Unpacking one.
    Open,
}

/// A package being written or unpacked on its own thread.
pub struct Job {
    /// What it does.
    pub kind: Kind,
    /// Its progress (read by the UI); `done` holds the package written or the show unpacked.
    pub progress: Arc<Mutex<Progress>>,
    cancel: Arc<AtomicBool>,
    /// Media left out of an export because they don't exist.
    pub missing: Vec<PathBuf>,
    /// The unpacked package, once an [`Kind::Open`] job is done.
    pub unpacked: Arc<Mutex<Option<Unpacked>>>,
}

impl Job {
    /// Start writing `out` from `plan`.
    pub fn export(plan: Plan, out: PathBuf) -> Self {
        Self::spawn(Kind::Export, plan.missing.clone(), move |p, c, _| {
            export(&plan, &out, p, c)?;
            Ok(out)
        })
    }

    /// Start unpacking `package` into the packages folder.
    pub fn open(package: PathBuf) -> Self {
        Self::spawn(Kind::Open, Vec::new(), move |p, c, slot| {
            let root = packages_dir().context("no config folder to unpack into")?;
            let u = extract(&package, &root, p, c)?;
            let show = u.show.clone();
            *slot.lock() = Some(u);
            Ok(show)
        })
    }

    fn spawn(
        kind: Kind,
        missing: Vec<PathBuf>,
        f: impl FnOnce(&Mutex<Progress>, &AtomicBool, &Mutex<Option<Unpacked>>) -> Result<PathBuf> + Send + 'static,
    ) -> Self {
        let progress = Arc::new(Mutex::new(Progress::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let unpacked = Arc::new(Mutex::new(None));
        let (p, c, u) = (progress.clone(), cancel.clone(), unpacked.clone());
        let spawned = std::thread::Builder::new().name("opnq-package".into()).spawn(move || {
            let r = crate::recovery::catch(|| f(&p, &c, &u)).map_err(|e| format!("{e:#}"));
            p.lock().done = Some(r);
        });
        if let Err(e) = spawned {
            progress.lock().done = Some(Err(e.to_string()));
        }
        Self { kind, progress, cancel, missing, unpacked }
    }

    /// Stop (what was written is removed).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AudioCue, Cue, CueKind, Workspace};

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("opnq-pkg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn progress() -> Mutex<Progress> {
        Mutex::new(Progress::default())
    }

    /// A show with two audio files from two folders; returns the package written.
    fn make(root: &Path) -> PathBuf {
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("x.wav"), b"first").unwrap();
        std::fs::write(b.join("x.wav"), vec![7u8; 3_000_000]).unwrap();
        let audio = |p: PathBuf| Cue::new(CueKind::Audio(AudioCue { path: p, ..Default::default() }));
        let mut ws = Workspace::default();
        ws.cue_list.cues = vec![audio(a.join("x.wav")), audio(b.join("x.wav")), audio(root.join("gone.wav"))];
        let plan = crate::bundle::plan(&ws, "My show", &root.join("out"), &|p| p.exists());
        let out = root.join("my.opnqs");
        export(&plan, &out, &progress(), &AtomicBool::new(false)).unwrap();
        out
    }

    #[test]
    fn a_package_round_trips_into_a_show_with_its_media() {
        let root = dir("round");
        let pkg = make(&root);
        assert!(!root.join("my.opnqs.part").exists());
        let u = extract(&pkg, &root.join("unpacked"), &progress(), &AtomicBool::new(false)).unwrap();
        let ws = Workspace::load(&u.show).unwrap();
        let paths: Vec<_> = ws.cue_list.cues.iter().filter_map(|c| c.kind.media_path().map(Path::to_path_buf)).collect();
        assert_eq!(paths[0], u.dir.join("audio/x.wav"));
        assert_eq!(paths[1], u.dir.join("audio/x 2.wav"));
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"first");
        assert_eq!(std::fs::read(&paths[1]).unwrap().len(), 3_000_000);
        // Unpacking again finds it done; a changed file is unpacked again.
        let mtime = std::fs::metadata(&u.show).unwrap().modified().unwrap();
        let again = extract(&pkg, &root.join("unpacked"), &progress(), &AtomicBool::new(false)).unwrap();
        assert_eq!(std::fs::metadata(&again.show).unwrap().modified().unwrap(), mtime);
        std::fs::write(&paths[0], b"oops!").unwrap();
        std::fs::write(&paths[1], b"shorter").unwrap();
        extract(&pkg, &root.join("unpacked"), &progress(), &AtomicBool::new(false)).unwrap();
        assert_eq!(std::fs::read(&paths[1]).unwrap().len(), 3_000_000);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn paths_that_could_escape_or_clash_are_refused() {
        for bad in [
            "", "/abs.wav", "../x.wav", "a/../x.wav", "a/./x.wav", "a//x.wav", "a/", "a\\\\b.wav", "C:x.wav", "C:/x.wav", ".hidden", "a/.git",
            "dot.", "space ", "NUL", "nul.txt", "a/COM1.wav", "lpt9", "a:b", "q?.wav", "x\0y", "tab\t", &"n".repeat(300), &format!("{}/x", "d/".repeat(600)),
        ] {
            assert!(valid_path(bad).is_err(), "{bad:?} should be refused");
        }
        for good in ["audio/kick.wav", "My show.opnq", "video/Ünï cödé.mp4", "a/b/c/d.mid", "COM10.wav", "console.wav"] {
            assert!(valid_path(good).is_ok(), "{good:?} should be fine");
        }
    }

    fn entry(path: &str) -> Entry {
        Entry { path: path.into(), size: 1, sha256: "0".repeat(64) }
    }

    fn manifest(paths: &[&str]) -> Manifest {
        Manifest { id: Uuid::new_v4(), name: "s".into(), app: "t".into(), entries: paths.iter().map(|p| entry(p)).collect() }
    }

    #[test]
    fn a_manifest_with_clashing_or_odd_files_is_refused() {
        assert!(manifest(&["s.opnq", "audio/a.wav"]).check().is_ok());
        assert!(manifest(&[]).check().is_err());
        assert!(manifest(&["audio/a.wav"]).check().is_err(), "first must be the show");
        assert!(manifest(&["dir/s.opnq"]).check().is_err());
        assert!(manifest(&["s.opnq", "audio/a.wav", "audio/A.WAV"]).check().is_err(), "case-insensitive duplicates");
        assert!(manifest(&["s.opnq", "audio", "audio/a.wav"]).check().is_err(), "file and folder");
        assert!(manifest(&["s.opnq", "Audio/a.wav", "audio"]).check().is_err());
        let mut m = manifest(&["s.opnq", "a.wav"]);
        m.entries[1].sha256 = "ABC".into();
        assert!(m.check().is_err());
        m.entries[1].sha256 = "G".repeat(64);
        assert!(m.check().is_err());
        let mut m = manifest(&["s.opnq", "a.wav", "b.wav"]);
        m.entries[1].size = u64::MAX;
        m.entries[2].size = u64::MAX;
        assert!(m.check().is_err(), "sizes that overflow");
    }

    fn unpack_err(pkg: &Path, root: &Path) -> String {
        match extract(pkg, root, &progress(), &AtomicBool::new(false)) {
            Ok(_) => panic!("should have been refused"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn damaged_packages_are_refused_and_leave_nothing_behind() {
        let root = dir("damaged");
        let pkg = make(&root);
        let good = std::fs::read(&pkg).unwrap();
        let unpacked = root.join("unpacked");
        let leftovers = || std::fs::read_dir(&unpacked).map(|d| d.count()).unwrap_or(0);

        let case = |name: &str, bytes: Vec<u8>| -> String {
            let p = root.join(format!("{name}.opnqs"));
            std::fs::write(&p, bytes).unwrap();
            unpack_err(&p, &unpacked)
        };
        assert!(case("empty", vec![]).contains("not an OpnQ"));
        assert!(case("text", b"hello there, this is not a package".to_vec()).contains("not an OpnQ"));
        let mut v = good.clone();
        v[8] = 99;
        assert!(case("version", v).contains("newer"));
        // Truncated anywhere, and padded.
        for cut in [20, 100, good.len() / 2, good.len() - 1] {
            let e = case("cut", good[..cut].to_vec());
            assert!(e.contains("damaged") || e.contains("not an OpnQ"), "{e}");
        }
        let mut v = good.clone();
        v.push(0);
        assert!(case("padded", v).contains("damaged"));
        // A manifest length that points past the file, or is huge.
        for len in [0u32, u32::MAX, 1 << 30] {
            let mut v = good.clone();
            v[9..13].copy_from_slice(&len.to_le_bytes());
            assert!(case("len", v).contains("damaged"));
        }
        // A flipped byte in a file's data: caught by its hash, nothing left on disk.
        let mut v = good.clone();
        let n = v.len();
        v[n - 10] ^= 0xFF;
        assert!(case("flip", v).contains("hash"));
        assert_eq!(leftovers(), 0, "no partial folder");
        // A changed hash in the manifest: the file no longer matches it.
        let mut v = good.clone();
        let at = v.windows(10).position(|w| w == b"\"sha256\":\"").unwrap() + 10;
        v[at] = if v[at] == b'a' { b'b' } else { b'a' };
        assert!(case("manifest", v).contains("hash"));
        assert_eq!(leftovers(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A hand-made package whose names try to leave the folder: refused before anything is written.
    #[test]
    fn a_package_with_an_escaping_path_writes_nothing() {
        let root = dir("escape");
        for path in ["../evil.wav", "/tmp/evil.wav", "a/../../evil.wav", "..\\\\evil.wav"] {
            let data = b"x";
            let m = Manifest {
                id: Uuid::new_v4(),
                name: "s".into(),
                app: "t".into(),
                entries: vec![
                    Entry { path: "s.opnq".into(), size: 1, sha256: hex(&Sha256::digest(data)) },
                    Entry { path: path.into(), size: 1, sha256: hex(&Sha256::digest(data)) },
                ],
            };
            let json = serde_json::to_vec(&m).unwrap();
            let mut v = MAGIC.to_vec();
            v.push(VERSION);
            v.extend((json.len() as u32).to_le_bytes());
            v.extend(&json);
            v.extend(b"xx");
            let p = root.join("evil.opnqs");
            std::fs::write(&p, v).unwrap();
            let e = unpack_err(&p, &root.join("unpacked"));
            assert!(e.contains("not allowed") || e.contains("bad part") || e.contains("dot") || e.contains("empty"), "{path}: {e}");
        }
        assert!(!root.join("evil.wav").exists() && !root.parent().unwrap().join("evil.wav").exists());
        assert!(std::fs::read_dir(root.join("unpacked")).map(|d| d.count()).unwrap_or(0) == 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cancelling_leaves_nothing() {
        let root = dir("cancel");
        let pkg = make(&root);
        let cancel = AtomicBool::new(true);
        assert!(extract(&pkg, &root.join("unpacked"), &progress(), &cancel).is_err());
        assert_eq!(std::fs::read_dir(root.join("unpacked")).unwrap().count(), 0);
        let (a, b) = (root.join("x.wav"), root.join("y.opnqs"));
        std::fs::write(&a, b"a").unwrap();
        let mut ws = Workspace::default();
        ws.cue_list.cues = vec![Cue::new(CueKind::Audio(AudioCue { path: a, ..Default::default() }))];
        let plan = crate::bundle::plan(&ws, "s", &root.join("o"), &|p| p.exists());
        assert!(export(&plan, &b, &progress(), &cancel).is_err());
        assert!(!b.exists() && !root.join("y.opnqs.part").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_regular_files_open() {
        let root = dir("kinds");
        assert!(extract(&root, &root.join("u"), &progress(), &AtomicBool::new(false)).is_err(), "a folder");
        assert!(extract(&root.join("missing.opnqs"), &root.join("u"), &progress(), &AtomicBool::new(false)).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn media_paths_are_found_in_a_package_by_their_tail() {
        let root = dir("remap");
        let pkg = make(&root);
        let u = extract(&pkg, &root.join("unpacked"), &progress(), &AtomicBool::new(false)).unwrap();
        // The main device's copy of the show lives elsewhere, with its own paths.
        let audio = |p: &str| Cue::new(CueKind::Audio(AudioCue { path: p.into(), ..Default::default() }));
        let mut ws = Workspace::default();
        ws.cue_list.cues = vec![
            audio("/home/other/packages/My_show-1234/audio/x.wav"),
            audio("D:\\Shows\\Audio\\X 2.WAV"),
            audio("/elsewhere/nothing.wav"),
            audio("/elsewhere/notaudio/x.wav"),
        ];
        let n = remap(&mut ws, &u.dir, &u.manifest);
        let paths: Vec<_> = ws.cue_list.cues.iter().filter_map(|c| c.kind.media_path().map(Path::to_path_buf)).collect();
        assert_eq!(n, 2);
        assert_eq!(paths[0], u.dir.join("audio/x.wav"));
        assert_eq!(paths[1], u.dir.join("audio/x 2.wav"));
        assert_eq!(paths[2], PathBuf::from("/elsewhere/nothing.wav"));
        assert_eq!(paths[3], PathBuf::from("/elsewhere/notaudio/x.wav"), "needs the folder too");
        let _ = std::fs::remove_dir_all(&root);
    }
}
