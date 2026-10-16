//! Output selection: which system display each stage window goes fullscreen on.

use super::App;
use super::widgets::{AMBER, RED};
use crate::displays::{self, Display};
use crate::model::Stage;
use crate::i18n::t;

/// System displays, filled in by a background query.
#[derive(Default)]
pub(super) struct DisplayList {
    pub list: Vec<Display>,
    pub loaded: bool,
    pub loading: bool,
    pub error: Option<String>,
    /// When the list was last asked for (it is refreshed while stages are on displays).
    pub asked: Option<std::time::Instant>,
}

impl App {
    pub(super) fn refresh_displays(&self, ctx: &egui::Context) {
        {
            let mut d = self.displays.lock();
            if d.loading {
                return;
            }
            d.loading = true;
            d.asked = Some(std::time::Instant::now());
        }
        let (list, ctx) = (self.displays.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = displays::list();
            let mut d = list.lock();
            d.loading = false;
            d.loaded = true;
            match r {
                Ok(l) => {
                    d.list = l;
                    d.error = None;
                }
                Err(e) => d.error = Some(format!("{e:#}")),
            }
            ctx.request_repaint();
        });
    }

    /// Choice of output for one stage: a window or fullscreen on a system display.
    /// Returns true when the choice changed.
    pub(super) fn output_choices(&mut self, ui: &mut egui::Ui, st: &mut Stage) -> bool {
        let mut changed = ui
            .selectable_value(&mut st.output, None, super::theme::with_icon(super::icons::DESKTOP_WINDOWS, t("Window")))
            .changed();
        let d = self.displays.lock();
        for disp in &d.list {
            changed |= ui
                .selectable_value(&mut st.output, Some(disp.name.clone()), super::theme::with_icon(super::icons::TV, &disp.label()))
                .changed();
        }
        if let Some(name) = &st.output
            && d.loaded
            && !d.list.iter().any(|x| &x.name == name)
        {
            ui.label(egui::RichText::new(tr!("{name} (not connected)", name)).color(AMBER));
        }
        if d.loading {
            ui.label(egui::RichText::new(t("Looking for displays…")).weak());
        }
        if let Some(e) = &d.error {
            ui.label(egui::RichText::new(e).color(RED));
        }
        drop(d);
        if ui.button(super::theme::with_icon(super::icons::REFRESH, t("Refresh displays"))).clicked() {
            self.refresh_displays(ui.ctx());
        }
        if changed && st.output.is_some() {
            self.stage_open.insert(st.id);
        }
        changed
    }
}
