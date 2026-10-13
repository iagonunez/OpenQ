//! Help → About: version, license and every attribution, compiled into the program so that each
//! copy carries its notices.

use super::{App, icons, theme};
use egui::{RichText, Ui};
use crate::i18n::t;

const LICENSE: &str = include_str!("../../LICENSE.md");
const NOTICE: &str = include_str!("../../NOTICE.md");
const THIRD_PARTY: &str = include_str!("../../THIRD_PARTY_LICENSES.md");
const FONT_LICENSES: [(&str, &str); 4] = [
    ("Inter — SIL Open Font License 1.1", include_str!("../../assets/fonts/OFL-Inter.txt")),
    ("JetBrains Mono — SIL Open Font License 1.1", include_str!("../../assets/fonts/OFL-JetBrainsMono.txt")),
    ("Barlow Condensed — SIL Open Font License 1.1", include_str!("../../assets/fonts/OFL-BarlowCondensed.txt")),
    ("Material Symbols — Apache License 2.0", include_str!("../../assets/fonts/LICENSE-MaterialSymbols.txt")),
];

/// OpenQ's copyright line.
fn copyright() -> &'static str {
    LICENSE.lines().find(|l| l.starts_with("Copyright")).unwrap_or("")
}

/// Long text in a scroll area that only lays out the visible lines.
fn long_text(ui: &mut Ui, id: &str, text: &str) {
    let lines: Vec<&str> = text.lines().collect();
    let row_h = ui.fonts_mut(|f| f.row_height(&theme::mono(11.5))) + ui.spacing().item_spacing.y;
    egui::ScrollArea::both().id_salt(id).max_height(280.0).auto_shrink([false, true]).show_rows(ui, row_h, lines.len(), |ui, range| {
        for line in &lines[range] {
            ui.add(egui::Label::new(RichText::new(*line).font(theme::mono(11.5)).color(theme::TEXT_DIM)).extend());
        }
    });
}

impl App {
    pub(super) fn about_window(&mut self, ctx: &egui::Context) {
        if !self.show_about {
            return;
        }
        let mut open = true;
        egui::Window::new(theme::with_icon(icons::INFO, t("About OpenQ")))
            .id(egui::Id::new("about"))
            .open(&mut open)
            .default_size([620.0, 560.0])
            .collapsible(false)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().id_salt("about-scroll").show(ui, |ui| {
                    ui.horizontal(|ui| {
                        use egui::text::{LayoutJob, TextFormat};
                        let mut job = LayoutJob::default();
                        job.append("Open", 0.0, TextFormat::simple(theme::title(34.0), theme::TEXT));
                        job.append("Q", 0.0, TextFormat::simple(theme::title(34.0), theme::GREEN));
                        ui.label(job);
                        ui.label(RichText::new(tr!("version {v}", v = env!("CARGO_PKG_VERSION"))).color(theme::TEXT_DIM));
                    });
                    ui.label(t("Show control and cue playback."));
                    ui.hyperlink(env!("CARGO_PKG_REPOSITORY"));
                    ui.add_space(6.0);
                    ui.label(RichText::new(copyright()).strong());
                    ui.label(
                        t("Licensed under the Apache License 2.0. Use it for anything, \
                         paid shows and productions included, and copy, change, share or sell it with this notice and \
                         the license."),
                    );
                    ui.add_space(8.0);
                    egui::CollapsingHeader::new(t("License")).show(ui, |ui| long_text(ui, "lic", LICENSE));
                    egui::CollapsingHeader::new(t("Notices and credits")).default_open(true).show(ui, |ui| long_text(ui, "notice", NOTICE));
                    egui::CollapsingHeader::new(t("Font licenses")).show(ui, |ui| {
                        for (name, text) in FONT_LICENSES {
                            egui::CollapsingHeader::new(name).show(ui, |ui| long_text(ui, name, text));
                        }
                    });
                    let crates = THIRD_PARTY.lines().filter(|l| l.starts_with("| ") && !l.starts_with("| Crate")).count();
                    egui::CollapsingHeader::new(tr!("Third-party software licenses ({crates} crates)", crates)).id_salt("third-party")
                        .show(ui, |ui| long_text(ui, "third", THIRD_PARTY));
                });
            });
        self.show_about = open;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn notices_are_bundled() {
        assert!(super::copyright().starts_with("Copyright 2026"));
        assert!(super::LICENSE.contains("Apache License"));
        assert!(super::THIRD_PARTY.contains("| symphonia |"));
        assert!(super::NOTICE.contains(super::copyright()));
    }
}
