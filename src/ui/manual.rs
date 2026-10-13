//! Help → User manual: the manual (`docs/manual`) is compiled into the program. Opening it writes
//! the pages to a per-version folder in the system's temporary directory and hands the page in the
//! system language (English, Spanish or Galician; [`page_for`]) to the default browser, so it
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
    ("img/mic.webp", include_bytes!("../../docs/manual/img/mic.webp")),
    ("img/recovery.webp", include_bytes!("../../docs/manual/img/recovery.webp")),
    ("img/remote-share.webp", include_bytes!("../../docs/manual/img/remote-share.webp")),
    ("img/show-mode.webp", include_bytes!("../../docs/manual/img/show-mode.webp")),
    ("img/text-cue.webp", include_bytes!("../../docs/manual/img/text-cue.webp")),
    ("img/triggers.webp", include_bytes!("../../docs/manual/img/triggers.webp")),
    ("img/waveform.webp", include_bytes!("../../docs/manual/img/waveform.webp")),
    // Icons in the manual use the app's own Material Symbols subset.
    ("fonts/MaterialSymbolsRounded.ttf", include_bytes!("../../assets/fonts/MaterialSymbolsRounded.ttf")),
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

/// Ask the operating system to open a file with its default application.
pub(super) fn open_with_default_app(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    let mut cmd = std::process::Command::new("explorer");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(path).spawn().map(|_| ())
}

/// The manual page for a POSIX locale name such as `es_ES.UTF-8` or `gl` (English otherwise).
fn page_for(locale: &str) -> &'static str {
    match locale.get(..2).map(str::to_ascii_lowercase).as_deref() {
        Some("es") => "index.es.html",
        Some("gl") => "index.gl.html",
        _ => "index.html",
    }
}

/// The system language from the usual environment variables (set on Linux and in most macOS
/// terminals; English when none is set, e.g. on Windows).
fn system_locale() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"].iter().filter_map(|v| std::env::var(v).ok()).find(|v| !v.is_empty()).unwrap_or_default()
}

/// Extract the manual for this version and open it in the browser. Returns where it was written.
pub(super) fn open() -> anyhow::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("openq-manual-{}", env!("CARGO_PKG_VERSION")));
    extract(&dir).map_err(|e| anyhow::anyhow!("cannot write the manual to {}: {e}", dir.display()))?;
    let page = dir.join(page_for(&system_locale()));
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
        for entry in std::fs::read_dir(root.join("img")).unwrap() {
            let name = format!("img/{}", entry.unwrap().file_name().to_string_lossy());
            assert!(bundled.contains(&name.as_str()), "{name} is not in manual::FILES");
        }
        let html = std::str::from_utf8(FILES[0].1).unwrap();
        for (rel, _) in &FILES[1..] {
            assert!(html.contains(rel), "{rel} is bundled but not used by the manual");
        }
        // The translations show the same pictures.
        for (rel, bytes) in &FILES[1..3] {
            let page = std::str::from_utf8(bytes).unwrap();
            for (img, _) in FILES.iter().filter(|f| f.0.starts_with("img/")) {
                assert!(page.contains(img), "{rel} doesn't show {img}");
            }
        }
        assert_eq!((page_for("es_ES.UTF-8"), page_for("gl_ES"), page_for("en_GB"), page_for("")), ("index.es.html", "index.gl.html", "index.html", "index.html"));
        let dir = std::env::temp_dir().join(format!("openq-manual-test-{}", std::process::id()));
        let index = extract(&dir).unwrap();
        assert!(index.is_file() && dir.join("fonts/MaterialSymbolsRounded.ttf").is_file());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
