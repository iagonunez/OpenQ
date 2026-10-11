use openq::{displays, media, show, ui};
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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("openq=info")).init();

    let prober = media::Prober::new();
    let show = Arc::new(Mutex::new(show::Show::new(prober.clone())));
    let open_arg = std::env::args().nth(1).map(std::path::PathBuf::from);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("OpenQ")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 560.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };

    eframe::run_native(
        "OpenQ",
        options,
        Box::new(move |cc| {
            ui::install_theme(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            show::spawn_engine_thread(show.clone(), move || ctx.request_repaint());
            let mut app = ui::App::new(show, prober);
            if let Some(p) = open_arg {
                app.open_path(p);
            }
            Ok(Box::new(app))
        }),
    )
}
