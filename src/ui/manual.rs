//! Help → User manual: the manual (`docs/manual`) is compiled into the program. Opening it writes
//! the pages to a per-version folder in the system's temporary directory and hands the page in the
//! UI language (English, Spanish or Galician; [`page_for`]) to the default browser, so it
//! works offline and always matches the running version. Each page links to the other languages.

use std::path::{Path, PathBuf};

/// Every file of the manual, by its path relative to `index.html`.
const FILES: &[(&str, &[u8])] = &[
    ("index.html", include_bytes!("../../docs/manual/index.html")),
    ("index.es.html", include_bytes!("../../docs/manual/index.es.html")),
    ("index.gl.html", include_bytes!("../../docs/manual/index.gl.html")),
    ("img/animation.webp", include_bytes!("../../docs/manual/img/animation.webp")),
    ("img/animation-stage.webp", include_bytes!("../../docs/manual/img/animation-stage.webp")),
    ("img/audio-fx.webp", include_bytes!("../../docs/manual/img/audio-fx.webp")),
    ("img/object-audio.webp", include_bytes!("../../docs/manual/img/object-audio.webp")),
    ("img/room.webp", include_bytes!("../../docs/manual/img/room.webp")),
    ("img/video-fx.webp", include_bytes!("../../docs/manual/img/video-fx.webp")),
    ("img/video-fx-stage.webp", include_bytes!("../../docs/manual/img/video-fx-stage.webp")),
    ("img/edit-mode.webp", include_bytes!("../../docs/manual/img/edit-mode.webp")),
    ("img/eq.webp", include_bytes!("../../docs/manual/img/eq.webp")),
    ("img/geometry-preview.webp", include_bytes!("../../docs/manual/img/geometry-preview.webp")),
    ("img/io.webp", include_bytes!("../../docs/manual/img/io.webp")),
    ("img/levels.webp", include_bytes!("../../docs/manual/img/levels.webp")),
    ("img/mapping.webp", include_bytes!("../../docs/manual/img/mapping.webp")),
    ("img/light-cue.webp", include_bytes!("../../docs/manual/img/light-cue.webp")),
    ("img/lighting-patch.webp", include_bytes!("../../docs/manual/img/lighting-patch.webp")),
    ("img/lighting-plan.webp", include_bytes!("../../docs/manual/img/lighting-plan.webp")),
    ("img/fixture-types.webp", include_bytes!("../../docs/manual/img/fixture-types.webp")),
    ("img/lighting-programmer.webp", include_bytes!("../../docs/manual/img/lighting-programmer.webp")),
    ("img/lighting-effects.webp", include_bytes!("../../docs/manual/img/lighting-effects.webp")),
    ("img/lighting-sequences.webp", include_bytes!("../../docs/manual/img/lighting-sequences.webp")),
    ("img/lighting-executors.webp", include_bytes!("../../docs/manual/img/lighting-executors.webp")),
    ("img/lighting-outputs.webp", include_bytes!("../../docs/manual/img/lighting-outputs.webp")),
    ("img/lighting-monitor.webp", include_bytes!("../../docs/manual/img/lighting-monitor.webp")),
    ("img/mic.webp", include_bytes!("../../docs/manual/img/mic.webp")),
    ("img/recovery.webp", include_bytes!("../../docs/manual/img/recovery.webp")),
    ("img/remote-share.webp", include_bytes!("../../docs/manual/img/remote-share.webp")),
    ("img/show-mode.webp", include_bytes!("../../docs/manual/img/show-mode.webp")),
    ("img/text-cue.webp", include_bytes!("../../docs/manual/img/text-cue.webp")),
    ("img/triggers.webp", include_bytes!("../../docs/manual/img/triggers.webp")),
    ("img/waveform.webp", include_bytes!("../../docs/manual/img/waveform.webp")),
    // Icons in the manual use the app's own Material Symbols subset.
    ("fonts/MaterialSymbolsRounded.ttf", include_bytes!("../../assets/fonts/MaterialSymbolsRounded.ttf")),
    ("img/es/animation-stage.webp", include_bytes!("../../docs/manual/img/es/animation-stage.webp")),
    ("img/es/animation.webp", include_bytes!("../../docs/manual/img/es/animation.webp")),
    ("img/es/audio-fx.webp", include_bytes!("../../docs/manual/img/es/audio-fx.webp")),
    ("img/es/edit-mode.webp", include_bytes!("../../docs/manual/img/es/edit-mode.webp")),
    ("img/es/eq.webp", include_bytes!("../../docs/manual/img/es/eq.webp")),
    ("img/es/geometry-preview.webp", include_bytes!("../../docs/manual/img/es/geometry-preview.webp")),
    ("img/es/io.webp", include_bytes!("../../docs/manual/img/es/io.webp")),
    ("img/es/levels.webp", include_bytes!("../../docs/manual/img/es/levels.webp")),
    ("img/es/lighting-patch.webp", include_bytes!("../../docs/manual/img/es/lighting-patch.webp")),
    ("img/es/lighting-plan.webp", include_bytes!("../../docs/manual/img/es/lighting-plan.webp")),
    ("img/es/lighting-programmer.webp", include_bytes!("../../docs/manual/img/es/lighting-programmer.webp")),
    ("img/es/mapping.webp", include_bytes!("../../docs/manual/img/es/mapping.webp")),
    ("img/es/mic.webp", include_bytes!("../../docs/manual/img/es/mic.webp")),
    ("img/es/object-audio.webp", include_bytes!("../../docs/manual/img/es/object-audio.webp")),
    ("img/es/recovery.webp", include_bytes!("../../docs/manual/img/es/recovery.webp")),
    ("img/es/remote-share.webp", include_bytes!("../../docs/manual/img/es/remote-share.webp")),
    ("img/es/room.webp", include_bytes!("../../docs/manual/img/es/room.webp")),
    ("img/es/show-mode.webp", include_bytes!("../../docs/manual/img/es/show-mode.webp")),
    ("img/es/text-cue.webp", include_bytes!("../../docs/manual/img/es/text-cue.webp")),
    ("img/es/triggers.webp", include_bytes!("../../docs/manual/img/es/triggers.webp")),
    ("img/es/video-fx-stage.webp", include_bytes!("../../docs/manual/img/es/video-fx-stage.webp")),
    ("img/es/video-fx.webp", include_bytes!("../../docs/manual/img/es/video-fx.webp")),
    ("img/es/waveform.webp", include_bytes!("../../docs/manual/img/es/waveform.webp")),
    ("img/gl/animation-stage.webp", include_bytes!("../../docs/manual/img/gl/animation-stage.webp")),
    ("img/gl/animation.webp", include_bytes!("../../docs/manual/img/gl/animation.webp")),
    ("img/gl/audio-fx.webp", include_bytes!("../../docs/manual/img/gl/audio-fx.webp")),
    ("img/gl/edit-mode.webp", include_bytes!("../../docs/manual/img/gl/edit-mode.webp")),
    ("img/gl/eq.webp", include_bytes!("../../docs/manual/img/gl/eq.webp")),
    ("img/gl/geometry-preview.webp", include_bytes!("../../docs/manual/img/gl/geometry-preview.webp")),
    ("img/gl/io.webp", include_bytes!("../../docs/manual/img/gl/io.webp")),
    ("img/gl/levels.webp", include_bytes!("../../docs/manual/img/gl/levels.webp")),
    ("img/gl/lighting-patch.webp", include_bytes!("../../docs/manual/img/gl/lighting-patch.webp")),
    ("img/gl/lighting-plan.webp", include_bytes!("../../docs/manual/img/gl/lighting-plan.webp")),
    ("img/gl/lighting-programmer.webp", include_bytes!("../../docs/manual/img/gl/lighting-programmer.webp")),
    ("img/gl/mapping.webp", include_bytes!("../../docs/manual/img/gl/mapping.webp")),
    ("img/gl/mic.webp", include_bytes!("../../docs/manual/img/gl/mic.webp")),
    ("img/gl/object-audio.webp", include_bytes!("../../docs/manual/img/gl/object-audio.webp")),
    ("img/gl/recovery.webp", include_bytes!("../../docs/manual/img/gl/recovery.webp")),
    ("img/gl/remote-share.webp", include_bytes!("../../docs/manual/img/gl/remote-share.webp")),
    ("img/gl/room.webp", include_bytes!("../../docs/manual/img/gl/room.webp")),
    ("img/gl/show-mode.webp", include_bytes!("../../docs/manual/img/gl/show-mode.webp")),
    ("img/gl/text-cue.webp", include_bytes!("../../docs/manual/img/gl/text-cue.webp")),
    ("img/gl/triggers.webp", include_bytes!("../../docs/manual/img/gl/triggers.webp")),
    ("img/gl/video-fx-stage.webp", include_bytes!("../../docs/manual/img/gl/video-fx-stage.webp")),
    ("img/gl/video-fx.webp", include_bytes!("../../docs/manual/img/gl/video-fx.webp")),
    ("img/gl/waveform.webp", include_bytes!("../../docs/manual/img/gl/waveform.webp")),
];

/// Write the manual into `dir` (replacing what is there) and return the path of its index page.
fn extract(dir: &Path) -> std::io::Result<PathBuf> {
    for (rel, bytes) in FILES {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, bytes)?;
    }
    Ok(dir.join("index.html"))
}

/// Where the manual is written: the per-user settings folder. Not a fixed name in the shared
/// temporary folder, where another user of the computer could have put links or their own pages
/// first.
fn manual_dir() -> anyhow::Result<PathBuf> {
    let version = env!("CARGO_PKG_VERSION");
    if let Some(d) = crate::remote::config_dir() {
        return Ok(d.join("manual").join(version));
    }
    // No settings folder: a new folder of our own (creating it fails if the name is taken).
    let dir = std::env::temp_dir().join(format!("openq-manual-{version}-{}", uuid::Uuid::new_v4().simple()));
    #[allow(unused_mut)] // only Unix sets a mode
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(&dir)?;
    Ok(dir)
}

/// Ask the operating system to open a file with its default application.
pub(super) fn open_with_default_app(path: &Path) -> std::io::Result<()> {
    // Absolute: a relative path (from a show file) could start with `-` and pass as an option.
    let path = std::path::absolute(path)?;
    #[cfg(target_os = "windows")]
    let mut cmd = std::process::Command::new("explorer");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(&path).spawn().map(|_| ())
}

/// The manual page for a language code or POSIX locale name such as `es_ES.UTF-8` or `gl` (English otherwise).
fn page_for(locale: &str) -> &'static str {
    match locale.get(..2).map(str::to_ascii_lowercase).as_deref() {
        Some("es") => "index.es.html",
        Some("gl") => "index.gl.html",
        _ => "index.html",
    }
}

/// Extract the manual for this version and open it in the browser. Returns where it was written.
pub(super) fn open() -> anyhow::Result<PathBuf> {
    let dir = manual_dir()?;
    extract(&dir).map_err(|e| anyhow::anyhow!("cannot write the manual to {}: {e}", dir.display()))?;
    let page = dir.join(page_for(crate::i18n::lang().code()));
    open_with_default_app(&page).map_err(|e| anyhow::anyhow!("cannot open a browser ({e}); the manual is at {}", page.display()))?;
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_manual_file_is_bundled_and_referenced_files_exist() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/manual");
        let bundled: Vec<&str> = FILES.iter().map(|f| f.0).collect();
        for dir in ["img", "img/es", "img/gl"] {
            for entry in std::fs::read_dir(root.join(dir)).unwrap() {
                let entry = entry.unwrap();
                if entry.path().is_file() {
                    let name = format!("{dir}/{}", entry.file_name().to_string_lossy());
                    assert!(bundled.contains(&name.as_str()), "{name} is not in manual::FILES");
                }
            }
        }
        let html = std::str::from_utf8(FILES[0].1).unwrap();
        for (rel, _) in FILES[1..].iter().filter(|f| !f.0.starts_with("img/es/") && !f.0.starts_with("img/gl/")) {
            assert!(html.contains(rel), "{rel} is bundled but not used by the manual");
        }
        for (rel, lang) in [("img/es/", "index.es.html"), ("img/gl/", "index.gl.html")] {
            let page = std::str::from_utf8(FILES.iter().find(|f| f.0 == lang).unwrap().1).unwrap();
            for (img, _) in FILES.iter().filter(|f| f.0.starts_with(rel)) {
                assert!(page.contains(img), "{lang} doesn't show {img}");
            }
        }
        // Each translation shows its own language's pictures: img/X.webp (English) in index.html,
        // img/es/X.webp in index.es.html and img/gl/X.webp in index.gl.html. A picture without a
        // translation stays English and is referenced as img/X.webp in every page.
        for (rel, lang) in [("index.es.html", "es"), ("index.gl.html", "gl")] {
            let page = std::str::from_utf8(FILES.iter().find(|f| f.0 == rel).unwrap().1).unwrap();
            for (img, _) in FILES.iter().filter(|f| f.0.starts_with("img/") && !f.0.starts_with("img/es/") && !f.0.starts_with("img/gl/")) {
                let translated = img.replacen("img/", &format!("img/{lang}/"), 1);
                let shown = if FILES.iter().any(|f| f.0 == translated) { translated } else { img.to_string() };
                assert!(page.contains(&shown), "{rel} doesn't show {shown}");
            }
        }
        assert_eq!((page_for("es_ES.UTF-8"), page_for("gl_ES"), page_for("en_GB"), page_for("")), ("index.es.html", "index.gl.html", "index.html", "index.html"));
        let dir = std::env::temp_dir().join(format!("openq-manual-test-{}", std::process::id()));
        let index = extract(&dir).unwrap();
        assert!(index.is_file() && dir.join("fonts/MaterialSymbolsRounded.ttf").is_file());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
