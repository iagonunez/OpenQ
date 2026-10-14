//! Crash recovery in the UI: the "Recover unsaved work" window shown at start when an earlier
//! OpenQ didn't quit normally, and the autosave session of this one.

use super::{App, icons, theme};
use crate::recovery::{Autosave, Orphan};
use egui::RichText;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::i18n::t;

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
        0..60 => t("just now").into(),
        60..3600 => tr!("{n} min ago", n = s / 60),
        3600..86400 => tr!("{n} h ago", n = s / 3600),
        _ => tr!("{n} days ago", n = s / 86400),
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
        egui::Window::new(theme::with_icon(icons::WARNING, t("Recover unsaved work")))
            .id(egui::Id::new("recovery"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .fixed_size([540.0, 0.0])
            .show(ctx, |ui| {
                ui.label(t("OpenQ didn't quit normally last time. These shows had changes that weren't saved:"));
                ui.add_space(6.0);
                for (i, o) in self.recovery.orphans.iter().enumerate() {
                    theme::card_frame().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.set_max_width(ui.available_width() - 170.0);
                                ui.label(RichText::new(o.name()).font(theme::semibold(14.0)));
                                let place = o.info.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| t("never saved").into());
                                ui.add(
                                    egui::Label::new(RichText::new(tr!("autosaved {age} · {place}", age = age(o.info.saved), place)).small().color(theme::TEXT_DIM))
                                        .truncate(),
                                )
                                .on_hover_text(place.as_str());
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button(t("Discard")).on_hover_text(t("Delete this copy for good")).clicked() {
                                    action = Some((i, false));
                                }
                                if ui.button(theme::with_icon(icons::REPLAY, t("Recover"))).clicked() {
                                    action = Some((i, true));
                                }
                            });
                        });
                    });
                    ui.add_space(4.0);
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(t("Recovered shows open with unsaved changes: save to keep them.")).small().color(theme::TEXT_DIM));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(t("Later")).on_hover_text(t("Ask again next time OpenQ starts")).clicked() {
                            later = true;
                        }
                    });
                });
            });
        if later {
            self.recovery.orphans.clear();
            return;
        }
        if let Some((i, recover)) = action {
            self.recovery_choice(i, recover);
        }
    }

    /// Recover or discard orphan `i`. Called while the frame holds the show lock, so it must not lock it.
    fn recovery_choice(&mut self, i: usize, recover: bool) {
        let o = self.recovery.orphans.remove(i);
        if !recover {
            o.discard();
            return;
        }
        // Recovering locks the show and may open a dialog: not while this frame holds the lock.
        self.deferred.push(super::Deferred::Recover(o));
    }

    /// Replace the current show with an autosaved one. Runs without the show lock held.
    pub(super) fn recover(&mut self, o: Orphan) {
        if !self.may_discard(super::Discarding::Recover(o.clone())) {
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
                self.notify(tr!("Recovered {name} — save to keep it", name = o.name()));
            }
            Err(e) => self.notify_error(tr!("Cannot recover {name}: {e}", name = o.name(), e = format!("{e:#}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::Prober;
    use crate::model::{Cue, CueKind, Workspace};
    use crate::show::{SharedShow, Show};
    use egui::accesskit::Role;

    /// One headless frame of the whole window with `events` as input; returns the AccessKit tree.
    fn frame(ctx: &egui::Context, app: &mut App, events: Vec<egui::Event>) -> egui::accesskit::TreeUpdate {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 1000.0))),
            events,
            ..Default::default()
        };
        let out = ctx.run_ui(input, |ui| app.draw(ui, None));
        out.platform_output.accesskit_update.expect("accesskit enabled")
    }

    /// Centre of the button whose label contains `text`.
    fn button(tree: &egui::accesskit::TreeUpdate, text: &str) -> Option<egui::Pos2> {
        tree.nodes.iter().find_map(|(_, n)| {
            let b = n.bounds()?;
            (n.role() == Role::Button && n.label().is_some_and(|l| l.contains(text)))
                .then(|| egui::pos2(((b.x0 + b.x1) / 2.0) as f32, ((b.y0 + b.y1) / 2.0) as f32))
        })
    }

    /// Clicking Recover in the real window loads the autosaved show. It used to lock the show a
    /// second time from inside the frame and freeze the app.
    #[test]
    fn recover_button_click() {
        // A deadlock would hang the test run: fail it instead.
        let done = Arc::new(AtomicBool::new(false));
        let watchdog = done.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(20));
            if !watchdog.load(Ordering::Acquire) {
                eprintln!("recover_button_click: the UI froze after clicking Recover");
                std::process::exit(1);
            }
        });

        let dir = std::env::temp_dir().join(format!("openq-recover-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut ws = Workspace::default();
        ws.cue_list.cues.push(Cue::new(CueKind::Memo));
        // Dropping the session releases its lock like a crashed instance, leaving the copy behind.
        Autosave::new_in(&dir).unwrap().write(&ws, None).unwrap();
        let orphans = crate::recovery::find_orphans_in(&dir, None);
        assert_eq!(orphans.len(), 1);

        let prober = Prober::new();
        let shared: SharedShow = Arc::new(Mutex::new(Show::new(prober.clone())));
        let mut app = App::new(shared.clone(), prober);
        app.recovery.orphans = orphans;

        crate::media::set_max_texture_side(2048);
        let ctx = egui::Context::default();
        super::theme::install(&ctx);
        ctx.enable_accesskit();
        // Windows take a frame or two to size and place themselves.
        let mut tree = frame(&ctx, &mut app, vec![]);
        for _ in 0..3 {
            tree = frame(&ctx, &mut app, vec![]);
        }
        let pos = button(&tree, "Recover").expect("Recover button on screen");
        let press = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        frame(&ctx, &mut app, vec![egui::Event::PointerMoved(pos)]);
        frame(&ctx, &mut app, vec![press(true)]);
        frame(&ctx, &mut app, vec![press(false)]);
        let tree = frame(&ctx, &mut app, vec![]);
        done.store(true, Ordering::Release);

        let s = shared.lock();
        assert_eq!(s.ws.cue_list.cues.len(), 1, "show recovered");
        assert!(s.dirty, "recovered show is unsaved");
        drop(s);
        assert!(app.recovery.orphans.is_empty());
        assert!(button(&tree, "Discard").is_none(), "recovery window closed");
        assert!(crate::recovery::find_orphans_in(&dir, None).is_empty(), "autosaved copy removed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
