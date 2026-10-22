//! Show → Warnings: every cue that won't do what it looks like it does ([`crate::warnings`]).
//! Click a row to select the cue (its list is brought forward).

use super::{App, icons, theme};
use crate::control::Command;
use crate::i18n::t;
use crate::model::CueId;
use crate::show::Show;
use crate::warnings::{FileState, Severity, Warning};
use egui::RichText;
use std::time::{Duration, Instant};

/// How often the list is worked out again while nothing was edited (files come and go).
const RECHECK: Duration = Duration::from_secs(1);

/// The last check: for which edit, when, and what it found.
#[derive(Default)]
pub(super) struct WarningsUi {
    pub(super) open: bool,
    found: Option<(u64, Instant, Vec<Warning>)>,
    errors_only: bool,
}

impl App {
    pub(super) fn warnings_window(&mut self, ctx: &egui::Context, show: &mut Show) {
        if !self.warnings.open {
            return;
        }
        let stale = self.warnings.found.as_ref().is_none_or(|(g, at, _)| *g != show.edit_gen || at.elapsed() >= RECHECK);
        if stale {
            let prober = self.prober.clone();
            let file = |p: &std::path::Path| {
                if !prober.exists(p) {
                    FileState::Missing
                } else if let Some(Err(e)) = prober.get(p) {
                    FileState::Unreadable(e)
                } else {
                    FileState::Fine
                }
            };
            self.warnings.found = Some((show.edit_gen, Instant::now(), crate::warnings::check(&show.ws, &file)));
            ctx.request_repaint_after(RECHECK);
        }
        let mut open = true;
        let mut pick: Option<(usize, CueId)> = None;
        let found = &self.warnings.found.as_ref().expect("just set").2;
        let errors = found.iter().filter(|w| w.severity == Severity::Error).count();
        let errors_only = &mut self.warnings.errors_only;
        egui::Window::new(theme::with_icon(icons::WARNING, t("Warnings")))
            .id(egui::Id::new("warnings-window"))
            .open(&mut open)
            .collapsible(false)
            .default_size([560.0, 380.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let text = if found.is_empty() {
                        t("Nothing to report: every cue looks ready.").to_string()
                    } else {
                        crate::tr!("{errors} errors, {warnings} warnings", errors = errors, warnings = found.len() - errors)
                    };
                    ui.label(text);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.checkbox(errors_only, t("Errors only"));
                    });
                });
                ui.separator();
                let names = show.ws.cue_list.list_names().0;
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for w in found.iter().filter(|w| !*errors_only || w.severity == Severity::Error) {
                        let Some(cue) = show.ws.cue_list.find(w.cue) else { continue };
                        let (glyph, colour) = match w.severity {
                            Severity::Error => (icons::ERROR, theme::RED),
                            Severity::Warning => (icons::WARNING, theme::AMBER),
                        };
                        let list = if names.len() > 1 { names.get(w.tab).map(|n| format!("{n} · ")).unwrap_or_default() } else { String::new() };
                        let r = ui
                            .horizontal(|ui| {
                                ui.label(theme::icon(glyph, 15.0).color(colour));
                                ui.label(RichText::new(format!("{list}{}", cue.number)).font(theme::mono(12.5)).color(theme::TEXT_DIM));
                                ui.label(RichText::new(cue.display_name()).strong());
                                ui.label(RichText::new(&w.text).color(theme::TEXT_DIM));
                            })
                            .response
                            .interact(egui::Sense::click())
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text(t("Select this cue"));
                        if r.clicked() {
                            pick = Some((w.tab, w.cue));
                        }
                    }
                });
            });
        self.warnings.open = open;
        if let Some((tab, id)) = pick {
            if tab != show.ws.cue_list.pos.min(show.ws.cue_list.others.len()) {
                self.control(show, Command::SwitchList(tab));
            }
            self.select(show, Some(id));
        }
    }
}
