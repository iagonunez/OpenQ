//! Bundle a show: copy the show file and every media file it plays into one folder, so the show
//! can move to another computer (or be archived) in one piece.
//!
//! The bundle is `<folder>/<show name>/<show name>.openq` with the media in `audio/` and `video/`
//! next to it; the copy's media paths point there (stored relative, like any show next to its
//! media). Files with the same name from different folders get " 2", " 3"... Missing files are left
//! out and listed. The copy runs on a thread of its own ([`Job`]); the open show is unchanged.

use crate::model::{CueKind, Workspace};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// What a bundle copies and the show it writes.
pub struct Plan {
    /// Folder made for the bundle.
    pub dir: PathBuf,
    /// The show file written into it.
    pub show: PathBuf,
    /// Media to copy: (from, to).
    pub copies: Vec<(PathBuf, PathBuf)>,
    /// Media files that don't exist (left out; their cues keep the old path).
    pub missing: Vec<PathBuf>,
    /// The show to write, its media paths pointing into the bundle.
    pub ws: Workspace,
}

/// A file name usable on every system (no path separators or reserved characters).
fn clean_name(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c }).collect();
    let s = s.trim().trim_matches('.').to_string();
    if s.is_empty() { "show".into() } else { s }
}

/// Work out a bundle of `ws` named `name` in `folder`. `exists` says whether a media file is there
/// (the caller's cached check, [`crate::media::Prober::exists`], or the file system).
pub fn plan(ws: &Workspace, name: &str, folder: &Path, exists: &dyn Fn(&Path) -> bool) -> Plan {
    let name = clean_name(name);
    let dir = folder.join(&name);
    let mut ws = ws.clone();
    let mut copies: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut missing = Vec::new();
    let mut by_source: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut taken: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for list in ws.cue_list.all_roots_mut() {
        for_each_media(list, &mut |path, sub| {
            if path.as_os_str().is_empty() {
                return;
            }
            if let Some(to) = by_source.get(path.as_path()) {
                *path = to.clone();
                return;
            }
            if !exists(path) {
                if !missing.contains(path) {
                    missing.push(path.clone());
                }
                return;
            }
            let file = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| "media".into());
            let (stem, ext) = match file.rsplit_once('.') {
                Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
                _ => (file.clone(), String::new()),
            };
            let mut to = dir.join(sub).join(clean_name(&file));
            let mut n = 2;
            while !taken.insert(to.clone()) {
                to = dir.join(sub).join(clean_name(&format!("{stem} {n}{ext}")));
                n += 1;
            }
            by_source.insert(path.clone(), to.clone());
            copies.push((path.clone(), to.clone()));
            *path = to;
        });
    }
    let show = dir.join(format!("{name}.{}", Workspace::EXTENSION));
    Plan { dir, show, copies, missing, ws }
}

fn for_each_media(list: &mut [crate::model::Cue], f: &mut impl FnMut(&mut PathBuf, &str)) {
    for c in list {
        match &mut c.kind {
            CueKind::Audio(a) => f(&mut a.path, "audio"),
            CueKind::Video(v) => f(&mut v.path, "video"),
            CueKind::Group { children, .. } => for_each_media(children, f),
            _ => {}
        }
    }
}

/// How a running bundle is getting on.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    /// Files copied so far.
    pub files: usize,
    /// Files to copy.
    pub total_files: usize,
    /// Bytes copied so far.
    pub bytes: u64,
    /// Bytes to copy.
    pub total_bytes: u64,
    /// Finished: the show file written, or why not.
    pub done: Option<Result<PathBuf, String>>,
}

/// A bundle being written on its own thread.
pub struct Job {
    /// Its progress (read by the UI).
    pub progress: Arc<Mutex<Progress>>,
    cancel: Arc<AtomicBool>,
    /// Media left out because they don't exist.
    pub missing: Vec<PathBuf>,
}

impl Job {
    /// Start copying.
    pub fn start(plan: Plan) -> Self {
        let progress = Arc::new(Mutex::new(Progress { total_files: plan.copies.len(), ..Default::default() }));
        let cancel = Arc::new(AtomicBool::new(false));
        let missing = plan.missing.clone();
        let (p, c) = (progress.clone(), cancel.clone());
        let spawned = std::thread::Builder::new().name("openq-bundle".into()).spawn(move || {
            let r = crate::recovery::catch(|| write(&plan, &p, &c)).map_err(|e| format!("{e:#}"));
            p.lock().done = Some(r.map(|()| plan.show.clone()));
        });
        if let Err(e) = spawned {
            progress.lock().done = Some(Err(e.to_string()));
        }
        Self { progress, cancel, missing }
    }

    /// Stop copying (what was copied stays; no show file is written).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
}

/// Copy the media and write the show (the show last: a bundle with a show file is complete).
pub fn write(plan: &Plan, progress: &Mutex<Progress>, cancel: &AtomicBool) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::io::{Read, Write};
    let total: u64 = plan.copies.iter().filter_map(|(from, _)| std::fs::metadata(from).ok()).map(|m| m.len()).sum();
    progress.lock().total_bytes = total;
    std::fs::create_dir_all(&plan.dir).with_context(|| plan.dir.display().to_string())?;
    let mut buf = vec![0u8; 1 << 20];
    for (from, to) in &plan.copies {
        crate::media::check_local(from)?;
        if let Some(d) = to.parent() {
            std::fs::create_dir_all(d).with_context(|| d.display().to_string())?;
        }
        let tmp = to.with_extension("openq-part");
        let mut copy = || -> anyhow::Result<()> {
            let mut src = std::fs::File::open(from).with_context(|| from.display().to_string())?;
            let mut dst = std::fs::File::create(&tmp).with_context(|| tmp.display().to_string())?;
            loop {
                if cancel.load(Ordering::Acquire) {
                    anyhow::bail!("{}", crate::i18n::t("cancelled"));
                }
                let n = src.read(&mut buf).with_context(|| from.display().to_string())?;
                if n == 0 {
                    break;
                }
                dst.write_all(&buf[..n]).with_context(|| tmp.display().to_string())?;
                progress.lock().bytes += n as u64;
            }
            dst.sync_all()?;
            std::fs::rename(&tmp, to).with_context(|| to.display().to_string())?;
            Ok(())
        };
        if let Err(e) = copy() {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        progress.lock().files += 1;
    }
    plan.ws.save(&plan.show)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AudioCue, Cue, VideoCue};

    /// Media from two folders with one file name get two names; a cue using a file twice copies it
    /// once; the bundled show loads with its media inside the bundle.
    #[test]
    fn bundles_media_next_to_the_show() {
        let root = std::env::temp_dir().join(format!("openq-bundle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("x.wav"), b"one").unwrap();
        std::fs::write(b.join("x.wav"), b"two").unwrap();
        std::fs::write(a.join("m.mp4"), b"movie").unwrap();
        let audio = |p: &Path| Cue::new(CueKind::Audio(AudioCue { path: p.to_path_buf(), ..Default::default() }));
        let mut ws = Workspace::default();
        ws.cue_list.cues = vec![
            audio(&a.join("x.wav")),
            audio(&b.join("x.wav")),
            audio(&a.join("x.wav")),
            audio(&root.join("gone.wav")),
            Cue::new(CueKind::Video(VideoCue { path: a.join("m.mp4"), ..Default::default() })),
        ];
        let p = plan(&ws, "My: show", &root.join("out"), &|p| p.exists());
        assert_eq!(p.copies.len(), 3);
        assert_eq!(p.missing, vec![root.join("gone.wav")]);
        write(&p, &Mutex::new(Progress::default()), &AtomicBool::new(false)).unwrap();
        let back = Workspace::load(&p.show).unwrap();
        let paths: Vec<PathBuf> = back.cue_list.cues.iter().filter_map(|c| c.kind.media_path().map(Path::to_path_buf)).collect();
        let dir = root.join("out").join("My_ show");
        assert_eq!(p.show, dir.join("My_ show.openq"));
        assert_eq!(paths[0], dir.join("audio/x.wav"));
        assert_eq!(paths[1], dir.join("audio/x 2.wav"));
        assert_eq!(paths[2], paths[0]);
        assert_eq!(paths[3], root.join("gone.wav"));
        assert_eq!(paths[4], dir.join("video/m.mp4"));
        assert_eq!(std::fs::read(&paths[1]).unwrap(), b"two");
        let saved = std::fs::read_to_string(&p.show).unwrap();
        assert!(saved.contains("\"audio/x 2.wav\""), "stored relative");
        let _ = std::fs::remove_dir_all(&root);
    }
}
