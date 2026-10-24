//! Show → Overrides: the [`crate::overrides`] switches, and the status bar warning while any is off.

use super::{App, icons, theme};
use crate::control::Command;
use crate::i18n::t;
use crate::overrides::{self, Override};
use crate::show::Show;
use egui::RichText;

impl App {
    pub(super) fn overrides_window(&mut self, ctx: &egui::Context, show: &mut Show) {
        if !self.show_overrides {
            return;
        }
        let off = self.remote.overrides_off();
        let mut open = true;
        let mut changes = Vec::new();
        egui::Window::new(theme::with_icon(icons::BLOCK, t("Overrides")))
            .id(egui::Id::new("overrides-window"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(440.0)
            .show(ctx, |ui| {
                ui.label(RichText::new(t("Switch a kind of input or output off for the whole show. OpnQ always starts with everything on.")).small().color(theme::TEXT_DIM));
                ui.add_space(6.0);
                for o in Override::ALL {
                    let mut on = !overrides::is_off(off, o);
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut on, RichText::new(o.label()).strong()).changed() {
                            changes.push((o, on));
                        }
                    });
                    ui.label(RichText::new(o.help()).small().color(theme::TEXT_DIM));
                    ui.add_space(4.0);
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.add_enabled(off != 0, egui::Button::new(t("All on"))).clicked() {
                        changes.extend(Override::ALL.map(|o| (o, true)));
                    }
                });
            });
        for (o, on) in changes {
            self.control(show, Command::Override(o, on));
        }
        self.show_overrides = open;
    }

    /// Status bar item while overrides are off (click opens the window).
    pub(super) fn overrides_status(&mut self, ui: &mut egui::Ui) {
        let off = self.remote.overrides_off();
        if off == 0 {
            return;
        }
        let names: Vec<&str> = Override::ALL.into_iter().filter(|o| overrides::is_off(off, *o)).map(|o| o.label()).collect();
        let text = crate::tr!("off: {list}", list = names.join(", "));
        let r = ui
            .add(egui::Button::new(RichText::new(theme::with_icon(icons::BLOCK, &text)).small().color(theme::AMBER)).frame(false))
            .on_hover_text(t("Overrides are cutting inputs or outputs. Click to change them."));
        if r.clicked() {
            self.show_overrides = true;
        }
    }
}
