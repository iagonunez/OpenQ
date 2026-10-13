// Release builds on Windows are GUI apps: no console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use openq::{displays, errlog, i18n, media, recovery, show, ui};
use parking_lot::Mutex;
use std::sync::Arc;

fn main() -> eframe::Result {
    if std::env::args().nth(1).as_deref() == Some(displays::LIST_FLAG) {
        if let Err(e) = displays::print_list() {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    // stderr as before, plus warnings/errors/crashes in the error log file (Help → Error log).
    errlog::init("openq=info");

    // UI language; on the first run (none saved) the app asks for it.
    let language_chosen = i18n::init();
    media::text::preload();
    let prober = media::Prober::new();
    let show = Arc::new(Mutex::new(show::Show::new(prober.clone())));
    let open_arg = std::env::args().nth(1).map(std::path::PathBuf::from);

    // Crash safety: autosave unsaved changes, write crash reports, and look for shows an
    // earlier OpenQ left behind when it went down.
    let autosave_now = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let autosave = match recovery::Autosave::new() {
        Ok(a) => Some(Arc::new(Mutex::new(a))),
        Err(e) => {
            log::warn!("autosave unavailable: {e:#}");
            None
        }
    };
    let orphans = recovery::find_orphans(autosave.as_ref().map(|a| a.lock()).as_deref());
    if let Some(a) = &autosave {
        recovery::install_panic_hook(&show, a.clone());
        recovery::spawn_autosave(show.clone(), a.clone(), autosave_now.clone());
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("OpenQ")
        .with_icon(ui::app_icon())
        // Wayland app id: matches the installed openq.desktop (and its Icon=openq).
        .with_app_id("openq")
        .with_inner_size([1280.0, 820.0])
        .with_min_inner_size([900.0, 560.0])
        .with_drag_and_drop(true)
        // On Windows OpenQ draws its own title bar with the window buttons.
        .with_decorations(!ui::custom_window_frame());
    if cfg!(target_os = "macos") {
        // The system title bar becomes transparent and OpenQ's own bar takes its place; macOS
        // keeps drawing its window buttons on top of it.
        viewport = viewport.with_fullsize_content_view(true).with_titlebar_shown(false).with_title_shown(false);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };

    eframe::run_native(
        "OpenQ",
        options,
        Box::new(move |cc| {
            ui::install_theme(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            show::spawn_engine_thread(show.clone(), move || ctx.request_repaint());
            let mut app = ui::App::new(show, prober);
            app.set_recovery(autosave, autosave_now, orphans);
            if !language_chosen && std::env::var_os("OPENQ_SCREENSHOT").is_none() {
                app.ask_language();
            }
            if let Some(p) = open_arg {
                app.open_path(p);
            }
            Ok(Box::new(app))
        }),
    )
}
