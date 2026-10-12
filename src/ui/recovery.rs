//! Crash recovery in the UI: the "Recover unsaved work" window shown at start when an earlier
//! OpenQ didn't quit normally, and the autosave session of this one.

use super::{App, icons, theme};
use crate::recovery::{Autosave, Orphan};
use egui::RichText;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// This instance's autosave session and what an earlier crash left behind.
#[derive(Default)]
pub struct Recovery {
    /// Autosave session (None if the config folder isn't writable).
    pub autosave: Option<Arc<Mutex<Autosave>>>,
    /// Ask the autosave thread to write now.
    pub now: Arc<AtomicBool>,
    /// Shows left behind by instances that crashed, newest first.
    pub orphans: Vec<Orphan>,
}

impl Recovery {
    /// Make the autosave thread write a fresh copy at once.
    pub fn save_soon(&self) {
        self.now.store(true, Ordering::Release);
    }
}

/// "3 min ago" style age of a unix time.
fn age(saved: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let s = now.saturating_sub(saved);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}

impl App {
    /// The window offering shows autosaved before a crash.
    pub(super) fn recovery_window(&mut self, ctx: &egui::Context) {
        if self.recovery.orphans.is_empty() {
            return;
        }
        let mut action: Option<(usize, bool)> = None;
        let mut later = false;
        egui::Window::new(theme::with_icon(icons::WARNING, "Recover unsaved work"))
            .id(egui::Id::new("recovery"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .fixed_size([540.0, 0.0])
            .show(ctx, |ui| {
                ui.label("OpenQ didn't quit normally last time. These shows had changes that weren't saved:");
                ui.add_space(6.0);
                for (i, o) in self.recovery.orphans.iter().enumerate() {
                    theme::card_frame().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.set_max_width(ui.available_width() - 170.0);
                                ui.label(RichText::new(o.name()).font(theme::semibold(14.0)));
                                let place = o.info.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "never saved".into());
                                ui.add(
                                    egui::Label::new(RichText::new(format!("autosaved {} · {place}", age(o.info.saved))).small().color(theme::TEXT_DIM))
                                        .truncate(),
                                )
                                .on_hover_text(place.as_str());
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("Discard").on_hover_text("Delete this copy for good").clicked() {
                                    action = Some((i, false));
                                }
                                if ui.button(theme::with_icon(icons::REPLAY, "Recover")).clicked() {
                                    action = Some((i, true));
                                }
                            });
                        });
                    });
                    ui.add_space(4.0);
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Recovered shows open with unsaved changes: save to keep them.").small().color(theme::TEXT_DIM));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Later").on_hover_text("Ask again next time OpenQ starts").clicked() {
                            later = true;
                        }
                    });
                });
            });
        if later {
            self.recovery.orphans.clear();
            return;
        }
        let Some((i, recover)) = action else { return };
        let o = self.recovery.orphans.remove(i);
        if !recover {
            o.discard();
            return;
        }
        if !self.confirm_discard() {
            self.recovery.orphans.insert(i, o);
            return;
        }
        match o.load() {
            Ok(ws) => {
                {
                    let mut s = self.show.lock();
                    s.replace_workspace(ws, o.info.path.clone());
                    s.edited();
                }
                self.reset_undo();
                self.selection.clear();
                self.collapsed.clear();
                self.last_playhead = None;
                o.discard();
                // Our own session holds it from now on.
                self.recovery.save_soon();
                self.notify(format!("Recovered {} — save to keep it", o.name()));
            }
            Err(e) => self.notify(format!("Cannot recover {}: {e:#}", o.name())),
        }
    }
}
