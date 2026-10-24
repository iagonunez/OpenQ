//! Help → User manual: the manual (`docs/manual`) is compiled into the program. Opening it writes
//! the pages to a per-version folder in the system's temporary directory and hands the page in the
//! UI language (English, Spanish or Galician; [`page_for`]) to the default browser, so it
//! works offline and always matches the running version. Each page links to the other languages.

use std::path::{Path, PathBuf};

/// Every file of the manual, by its path relative to `index.html`: the contents page and one page per chapter in
/// each language (built around their content by `tools/build_manual.py`), the shared style sheet and script, pictures.
const FILES: &[(&str, &[u8])] = &[
    ("index.html", include_bytes!("../../docs/manual/index.html")),
    ("index.es.html", include_bytes!("../../docs/manual/index.es.html")),
    ("index.gl.html", include_bytes!("../../docs/manual/index.gl.html")),
    ("start.html", include_bytes!("../../docs/manual/start.html")),
    ("start.es.html", include_bytes!("../../docs/manual/start.es.html")),
    ("start.gl.html", include_bytes!("../../docs/manual/start.gl.html")),
    ("cues.html", include_bytes!("../../docs/manual/cues.html")),
    ("cues.es.html", include_bytes!("../../docs/manual/cues.es.html")),
    ("cues.gl.html", include_bytes!("../../docs/manual/cues.gl.html")),
    ("audio.html", include_bytes!("../../docs/manual/audio.html")),
    ("audio.es.html", include_bytes!("../../docs/manual/audio.es.html")),
    ("audio.gl.html", include_bytes!("../../docs/manual/audio.gl.html")),
    ("video.html", include_bytes!("../../docs/manual/video.html")),
    ("video.es.html", include_bytes!("../../docs/manual/video.es.html")),
    ("video.gl.html", include_bytes!("../../docs/manual/video.gl.html")),
    ("lighting.html", include_bytes!("../../docs/manual/lighting.html")),
    ("lighting.es.html", include_bytes!("../../docs/manual/lighting.es.html")),
    ("lighting.gl.html", include_bytes!("../../docs/manual/lighting.gl.html")),
    ("remote.html", include_bytes!("../../docs/manual/remote.html")),
    ("remote.es.html", include_bytes!("../../docs/manual/remote.es.html")),
    ("remote.gl.html", include_bytes!("../../docs/manual/remote.gl.html")),
    ("help.html", include_bytes!("../../docs/manual/help.html")),
    ("help.es.html", include_bytes!("../../docs/manual/help.es.html")),
    ("help.gl.html", include_bytes!("../../docs/manual/help.gl.html")),
    ("manual.css", include_bytes!("../../docs/manual/manual.css")),
    ("manual.js", include_bytes!("../../docs/manual/manual.js")),
    ("img/animation.webp", include_bytes!("../../docs/manual/img/animation.webp")),
    ("img/animation-stage.webp", include_bytes!("../../docs/manual/img/animation-stage.webp")),
    ("img/cart.webp", include_bytes!("../../docs/manual/img/cart.webp")),
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
    ("img/lighting-playback.webp", include_bytes!("../../docs/manual/img/lighting-playback.webp")),
    ("img/fixture-types.webp", include_bytes!("../../docs/manual/img/fixture-types.webp")),
    ("img/lighting-programmer.webp", include_bytes!("../../docs/manual/img/lighting-programmer.webp")),
    ("img/lighting-effects.webp", include_bytes!("../../docs/manual/img/lighting-effects.webp")),
    ("img/lighting-sequences.webp", include_bytes!("../../docs/manual/img/lighting-sequences.webp")),
    ("img/lighting-points.webp", include_bytes!("../../docs/manual/img/lighting-points.webp")),
    ("img/lighting-plot.webp", include_bytes!("../../docs/manual/img/lighting-plot.webp")),
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
    ("img/es/cart.webp", include_bytes!("../../docs/manual/img/es/cart.webp")),
    ("img/es/animation.webp", include_bytes!("../../docs/manual/img/es/animation.webp")),
    ("img/es/audio-fx.webp", include_bytes!("../../docs/manual/img/es/audio-fx.webp")),
    ("img/es/edit-mode.webp", include_bytes!("../../docs/manual/img/es/edit-mode.webp")),
    ("img/es/eq.webp", include_bytes!("../../docs/manual/img/es/eq.webp")),
    ("img/es/geometry-preview.webp", include_bytes!("../../docs/manual/img/es/geometry-preview.webp")),
    ("img/es/io.webp", include_bytes!("../../docs/manual/img/es/io.webp")),
    ("img/es/levels.webp", include_bytes!("../../docs/manual/img/es/levels.webp")),
    ("img/es/lighting-patch.webp", include_bytes!("../../docs/manual/img/es/lighting-patch.webp")),
    ("img/es/lighting-plan.webp", include_bytes!("../../docs/manual/img/es/lighting-plan.webp")),
    ("img/es/lighting-playback.webp", include_bytes!("../../docs/manual/img/es/lighting-playback.webp")),
    ("img/es/lighting-points.webp", include_bytes!("../../docs/manual/img/es/lighting-points.webp")),
    ("img/es/fixture-types.webp", include_bytes!("../../docs/manual/img/es/fixture-types.webp")),
    ("img/es/light-cue.webp", include_bytes!("../../docs/manual/img/es/light-cue.webp")),
    ("img/es/lighting-effects.webp", include_bytes!("../../docs/manual/img/es/lighting-effects.webp")),
    ("img/es/lighting-monitor.webp", include_bytes!("../../docs/manual/img/es/lighting-monitor.webp")),
    ("img/es/lighting-outputs.webp", include_bytes!("../../docs/manual/img/es/lighting-outputs.webp")),
    ("img/es/lighting-sequences.webp", include_bytes!("../../docs/manual/img/es/lighting-sequences.webp")),
    ("img/es/lighting-plot.webp", include_bytes!("../../docs/manual/img/es/lighting-plot.webp")),
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
    ("img/gl/cart.webp", include_bytes!("../../docs/manual/img/gl/cart.webp")),
    ("img/gl/animation.webp", include_bytes!("../../docs/manual/img/gl/animation.webp")),
    ("img/gl/audio-fx.webp", include_bytes!("../../docs/manual/img/gl/audio-fx.webp")),
    ("img/gl/edit-mode.webp", include_bytes!("../../docs/manual/img/gl/edit-mode.webp")),
    ("img/gl/eq.webp", include_bytes!("../../docs/manual/img/gl/eq.webp")),
    ("img/gl/geometry-preview.webp", include_bytes!("../../docs/manual/img/gl/geometry-preview.webp")),
    ("img/gl/io.webp", include_bytes!("../../docs/manual/img/gl/io.webp")),
    ("img/gl/levels.webp", include_bytes!("../../docs/manual/img/gl/levels.webp")),
    ("img/gl/lighting-patch.webp", include_bytes!("../../docs/manual/img/gl/lighting-patch.webp")),
    ("img/gl/lighting-plan.webp", include_bytes!("../../docs/manual/img/gl/lighting-plan.webp")),
    ("img/gl/lighting-playback.webp", include_bytes!("../../docs/manual/img/gl/lighting-playback.webp")),
    ("img/gl/lighting-points.webp", include_bytes!("../../docs/manual/img/gl/lighting-points.webp")),
    ("img/gl/fixture-types.webp", include_bytes!("../../docs/manual/img/gl/fixture-types.webp")),
    ("img/gl/light-cue.webp", include_bytes!("../../docs/manual/img/gl/light-cue.webp")),
    ("img/gl/lighting-effects.webp", include_bytes!("../../docs/manual/img/gl/lighting-effects.webp")),
    ("img/gl/lighting-monitor.webp", include_bytes!("../../docs/manual/img/gl/lighting-monitor.webp")),
    ("img/gl/lighting-outputs.webp", include_bytes!("../../docs/manual/img/gl/lighting-outputs.webp")),
    ("img/gl/lighting-sequences.webp", include_bytes!("../../docs/manual/img/gl/lighting-sequences.webp")),
    ("img/gl/lighting-plot.webp", include_bytes!("../../docs/manual/img/gl/lighting-plot.webp")),
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
    let dir = std::env::temp_dir().join(format!("opnq-manual-{version}-{}", uuid::Uuid::new_v4().simple()));
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

    /// The text of every page in one language (`""` English, `"es"`, `"gl"`), with the style sheet.
    fn pages(lang: &str) -> String {
        let is_lang = |name: &str| match lang {
            "" => !name.ends_with(".es.html") && !name.ends_with(".gl.html"),
            l => name.ends_with(&format!(".{l}.html")),
        };
        FILES.iter()
            .filter(|f| (f.0.ends_with(".html") && is_lang(f.0)) || f.0 == "manual.css")
            .map(|f| std::str::from_utf8(f.1).unwrap())
            .collect()
    }

    #[test]
    fn every_manual_file_is_bundled_and_referenced_files_exist() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/manual");
        let bundled: Vec<&str> = FILES.iter().map(|f| f.0).collect();
        for dir in [".", "img", "img/es", "img/gl"] {
            for entry in std::fs::read_dir(root.join(dir)).unwrap() {
                let entry = entry.unwrap();
                if entry.path().is_file() {
                    let file = entry.file_name().to_string_lossy().into_owned();
                    let name = if dir == "." { file } else { format!("{dir}/{file}") };
                    assert!(bundled.contains(&name.as_str()), "{name} is not in manual::FILES");
                }
            }
        }
        let html = pages("");
        for (rel, _) in FILES.iter().filter(|f| !f.0.ends_with(".html") && !f.0.starts_with("img/es/") && !f.0.starts_with("img/gl/")) {
            assert!(html.contains(rel), "{rel} is bundled but not used by the manual");
        }
        // Every page in a language links to the others there, and nothing links to a page that isn't bundled.
        for lang in ["", "es", "gl"] {
            let text = pages(lang);
            for link in text.split("href=\"").skip(1).filter_map(|l| l.split(['"', '#']).next()) {
                if link.ends_with(".html") {
                    assert!(bundled.contains(&link), "the manual links to {link}, which is not bundled");
                }
            }
        }
        for (rel, lang) in [("img/es/", "es"), ("img/gl/", "gl")] {
            let text = pages(lang);
            for (img, _) in FILES.iter().filter(|f| f.0.starts_with(rel)) {
                assert!(text.contains(img), "the {lang} manual doesn't show {img}");
            }
        }
        // Each translation shows its own language's pictures: img/X.webp (English) in the English pages,
        // img/es/X.webp in the Spanish ones and img/gl/X.webp in the Galician ones. A picture without a
        // translation stays English and is referenced as img/X.webp in every language.
        for lang in ["es", "gl"] {
            let text = pages(lang);
            for (img, _) in FILES.iter().filter(|f| f.0.starts_with("img/") && !f.0.starts_with("img/es/") && !f.0.starts_with("img/gl/")) {
                let translated = img.replacen("img/", &format!("img/{lang}/"), 1);
                let shown = if FILES.iter().any(|f| f.0 == translated) { translated } else { img.to_string() };
                assert!(text.contains(&shown), "the {lang} manual doesn't show {shown}");
            }
        }
        assert_eq!((page_for("es_ES.UTF-8"), page_for("gl_ES"), page_for("en_GB"), page_for("")), ("index.es.html", "index.gl.html", "index.html", "index.html"));
        let dir = std::env::temp_dir().join(format!("opnq-manual-test-{}", std::process::id()));
        let index = extract(&dir).unwrap();
        assert!(index.is_file() && dir.join("fonts/MaterialSymbolsRounded.ttf").is_file());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
