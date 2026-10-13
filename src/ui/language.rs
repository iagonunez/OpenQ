//! UI language: the first-run picker and the Settings → General row ([`crate::i18n`]).

use super::{App, icons, theme};
use crate::i18n::{self, Lang, t};
use egui::RichText;

impl App {
    /// Asks for the UI language at the next frame (first run: none saved yet).
    pub fn ask_language(&mut self) {
        self.ask_language = true;
    }

    /// The first-run language picker. A click switches the UI at once; Continue saves it.
    pub(super) fn language_window(&mut self, ctx: &egui::Context) {
        if !self.ask_language {
            return;
        }
        egui::Window::new(theme::with_icon(icons::LANGUAGE, "Language · Idioma · Lingua"))
            .id(egui::Id::new("language"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .fixed_size([360.0, 0.0])
            .show(ctx, |ui| {
                ui.label(t("Choose the language of OpenQ:"));
                ui.add_space(8.0);
                for l in Lang::ALL {
                    let on = i18n::lang() == l;
                    let b = egui::Button::selectable(on, RichText::new(l.name()).size(16.0)).min_size(egui::vec2(ui.available_width(), 34.0));
                    if ui.add(b).clicked() {
                        i18n::set_lang(l);
                    }
                }
                ui.add_space(6.0);
                ui.label(RichText::new(t("You can change it later in Settings → General.")).weak().small());
                ui.add_space(6.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(RichText::new(t("Continue")).strong()).clicked() {
                        self.ask_language = false;
                        self.save_language();
                    }
                });
            });
    }

    /// Settings → General: the UI language, saved as soon as it changes.
    pub(super) fn language_setting(&mut self, ui: &mut egui::Ui) {
        let mut lang = i18n::lang();
        egui::ComboBox::from_id_salt("language").selected_text(lang.name()).show_ui(ui, |ui| {
            for l in Lang::ALL {
                ui.selectable_value(&mut lang, l, l.name());
            }
        });
        if lang != i18n::lang() {
            i18n::set_lang(lang);
            self.save_language();
        }
    }

    fn save_language(&mut self) {
        if let Err(e) = i18n::save() {
            self.notify_error(tr!("Cannot save the language: {e}", e));
        }
    }
}
