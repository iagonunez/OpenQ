//! The Coupling mode window: computers that back each other up. Kept apart from the Remote window
//! (pairing tablets and other operators): different words, a different code flow, its own menu entry.

use super::*;
use crate::remote::coupling::Grant;

/// Joining a group by code, on a thread of its own.
#[derive(Default)]
pub(super) struct JoinUi {
    address: String,
    code: String,
    busy: bool,
    result: Arc<Mutex<Option<Result<Grant, String>>>>,
    error: Option<String>,
}

/// Whether this computer has the media of the show it mirrors, looked at on a thread.
#[derive(Default)]
pub(super) struct Ready {
    checked: Option<Instant>,
    running: bool,
    result: Arc<Mutex<Option<(usize, usize)>>>,
    last: Option<(usize, usize)>,
}

/// How often the media are looked for while the window is open.
const READY_EVERY: Duration = Duration::from_secs(4);
/// Most files looked at.
const READY_MAX: usize = 20_000;

impl App {
    /// Remember the package this computer plays its media from (for a computer in a group), so a
    /// show it takes over finds its files there.
    pub(in crate::ui) fn remote_use_package(&mut self, u: &crate::package::Unpacked) {
        let mut c = self.remote.config.lock();
        if c.coupling.group.is_none() {
            return;
        }
        c.coupling.media_root = Some(u.dir.clone());
        drop(c);
        self.remote.save_config();
        self.notify(t("Coupling: this computer plays its media from this package"));
    }

    pub(in crate::ui) fn coupling_window(&mut self, ctx: &egui::Context, show: &mut Show) {
        self.poll_join();
        if !self.remote.coupling_window {
            return;
        }
        let mut open = true;
        egui::Window::new(theme::with_icon(icons::REFRESH, t("Coupling mode")))
            .id(egui::Id::new("coupling-window"))
            .open(&mut open)
            .default_size([700.0, 700.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.coupling_ui(ui, show));
            });
        self.remote.coupling_window = open;
    }

    /// A join that finished: keep the group's key and list.
    fn poll_join(&mut self) {
        let Some(result) = self.remote.join.result.lock().take() else { return };
        self.remote.join.busy = false;
        match result {
            Ok(grant) => {
                {
                    let mut c = self.remote.config.lock();
                    c.coupling.join(grant);
                    c.server_enabled = true;
                }
                self.remote.join = Default::default();
                self.remote.save_config();
                self.notify(t("Joined the coupling group"));
            }
            Err(e) => self.remote.join.error = Some(e),
        }
    }

    fn coupling_ui(&mut self, ui: &mut Ui, show: &mut Show) {
        egui::Frame::new().fill(theme::BG_INPUT).corner_radius(8).inner_margin(10).show(ui, |ui| {
            ui.label(
                RichText::new(t(
                    "Coupling mode is for computers that back each other up: they hold the same show, one plays it and the others \
                     follow along. If the one playing stops, the next in the list takes over.",
                ))
                .color(theme::TEXT_DIM),
            );
            ui.label(
                RichText::new(t("To control a show from a tablet or another operator's computer, use Remote → Share this OpnQ (pairing) instead."))
                    .small()
                    .color(theme::TEXT_FAINT),
            );
        });
        ui.add_space(8.0);
        let (co, me, name) = {
            let c = self.remote.config.lock();
            (c.coupling.clone(), c.instance_id, c.instance_name.clone())
        };
        if co.group.is_none() {
            self.not_in_a_group(ui, me, name);
        } else {
            self.in_a_group(ui, show, co, me);
        }
    }

    fn not_in_a_group(&mut self, ui: &mut Ui, me: uuid::Uuid, name: String) {
        ui.label(theme::caption(t("1 · Start a group on the first computer")));
        ui.label(t("Makes a group with this computer as the first in its list. Then add the others from here."));
        if ui.button(theme::with_icon(icons::ADD, t("Create a group with this computer"))).clicked() {
            {
                let mut c = self.remote.config.lock();
                c.coupling.create(Member { id: me, name, address: String::new() });
                c.server_enabled = true;
            }
            self.remote.save_config();
        }
        ui.add_space(12.0);
        ui.label(theme::caption(t("2 · Join it from each other computer")));
        ui.label(t("On a computer already in the group, press \"Add a computer\" to get a code. Enter that computer's address and the code here."));
        let busy = self.remote.join.busy;
        egui::Grid::new("coupling-join").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(t("Address"));
            ui.add_enabled(!busy, egui::TextEdit::singleline(&mut self.remote.join.address).hint_text(t("192.168.1.20 or host:port")).desired_width(240.0));
            ui.end_row();
            ui.label(t("Code"));
            ui.add_enabled(
                !busy,
                egui::TextEdit::singleline(&mut self.remote.join.code).hint_text("XXXX-XXXX").font(theme::mono(14.0)).desired_width(160.0),
            );
            ui.end_row();
        });
        ui.horizontal(|ui| {
            let ok = !busy && !self.remote.join.address.trim().is_empty() && !self.remote.join.code.trim().is_empty();
            if ui.add_enabled(ok, egui::Button::new(theme::with_icon(icons::CABLE, t("Join the group")))).clicked() {
                self.remote.join.error = None;
                self.remote.join.busy = true;
                let (cfg, slot) = (self.remote.config.clone(), self.remote.join.result.clone());
                let (addr, code) = (self.remote.join.address.trim().to_string(), self.remote.join.code.clone());
                let started = std::thread::Builder::new().name("opnq-coupling-join".into()).spawn(move || {
                    let r = crate::recovery::catch(|| crate::remote::client::join(&cfg, &addr, &code)).map_err(|e| format!("{e:#}"));
                    *slot.lock() = Some(r);
                });
                if let Err(e) = started {
                    self.remote.join.busy = false;
                    self.remote.join.error = Some(e.to_string());
                }
            }
            if busy {
                ui.spinner();
            }
        });
        if let Some(e) = &self.remote.join.error {
            ui.label(RichText::new(e).color(RED));
        }
        if busy {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }

    fn in_a_group(&mut self, ui: &mut Ui, show: &mut Show, co: Coupling, me: uuid::Uuid) {
        let mut next = co.clone();
        let peers = self.remote.supervisor.as_ref().map(Supervisor::peers).unwrap_or_default();
        let playing = self.remote.acting.is_some();
        let following = self.remote.link.is_some() && self.remote.replica.is_some();

        // Where this computer stands.
        ui.horizontal(|ui| {
            let (text, color) = if !next.enabled {
                (t("Coupling is off on this computer").to_string(), theme::TEXT_DIM)
            } else if !next.active_for(me) {
                (t("Add another computer to start").to_string(), AMBER)
            } else if playing {
                (t("This computer plays the show").to_string(), theme::GREEN)
            } else if let (true, Some(r)) = (following, &self.remote.replica) {
                (tr!("Standby: following {server}", server = r.server.clone()), theme::GREEN)
            } else {
                (t("Standby: waiting for a main device").to_string(), AMBER)
            };
            ui.label(theme::icon(icons::FIBER_MANUAL_RECORD, 14.0).color(color));
            ui.label(RichText::new(text).strong());
        });
        ui.checkbox(&mut next.enabled, t("Coupling on"));
        if !playing && following && next.active_for(me) {
            if ui
                .button(theme::with_icon(icons::REFRESH, t("Take the show over now")))
                .on_hover_text(t("The computer playing now steps down and follows this one"))
                .clicked()
            {
                self.take_over_now();
            }
            ui.label(RichText::new(t("This computer follows the show and cannot edit it; edit on the one playing.")).small().color(theme::TEXT_FAINT));
        }

        // The list.
        ui.add_space(8.0);
        ui.label(theme::caption(t("Order of play (the first plays the show)")));
        let (mut up, mut down, mut remove) = (None, None, None);
        let count = next.members.len();
        for (i, m) in next.members.iter_mut().enumerate() {
            let this = m.id == me;
            ui.horizontal(|ui| {
                ui.add_sized([18.0, 20.0], egui::Label::new(RichText::new(format!("{}", i + 1)).font(theme::mono(13.0)).color(theme::TEXT_DIM)));
                ui.vertical(|ui| {
                    ui.set_width(230.0);
                    let name = if this { tr!("{name} (this computer)", name = m.name.clone()) } else { m.name.clone() };
                    ui.add(egui::Label::new(if this { RichText::new(name).strong() } else { RichText::new(name) }).truncate());
                    if !this {
                        ui.add(egui::TextEdit::singleline(&mut m.address).font(theme::mono(11.5)).hint_text(t("host:port")).desired_width(220.0));
                    }
                });
                let (text, color) = if this {
                    if playing { (t("playing the show").to_string(), theme::GREEN) } else { (t("standby").to_string(), theme::TEXT_DIM) }
                } else {
                    match peers.get(&m.id) {
                        None | Some(Peer::Unknown) => ("…".to_string(), theme::TEXT_FAINT),
                        Some(Peer::Main { .. }) => (t("playing the show").to_string(), theme::GREEN),
                        Some(Peer::Standby { .. }) => (t("standby").to_string(), theme::TEXT_DIM),
                        Some(Peer::NotPaired(why)) => (tr!("cannot log in: {why}", why = why.clone()), AMBER),
                        Some(Peer::Unreachable(why)) => (tr!("not reachable: {why}", why = why.clone()), RED),
                    }
                };
                ui.vertical(|ui| {
                    ui.set_width(170.0);
                    ui.add(egui::Label::new(RichText::new(text).small().color(color)).wrap());
                });
                if ui.add_enabled(i > 0, egui::Button::new(theme::icon(icons::ARROW_UPWARD, 16.0))).clicked() {
                    up = Some(i);
                }
                if ui.add_enabled(i + 1 < count, egui::Button::new(theme::icon(icons::ARROW_DOWNWARD, 16.0))).clicked() {
                    down = Some(i);
                }
                if !this && theme::icon_button(ui, icons::DELETE, t("Take it out of the list")).clicked() {
                    remove = Some(i);
                }
            });
            ui.add_space(2.0);
        }
        if let Some(i) = up {
            next.members.swap(i, i - 1);
        }
        if let Some(i) = down {
            next.members.swap(i, i + 1);
        }
        if let Some(i) = remove {
            next.members.remove(i);
        }

        // Adding a computer: a code, shown here, typed there.
        ui.add_space(8.0);
        if next.members.len() < coupling::MAX_MEMBERS {
            if let Some(s) = &self.remote.server {
                match s.pairing() {
                    Some((code, left, tries)) => {
                        egui::Frame::new().fill(theme::BG_INPUT).corner_radius(8).inner_margin(12).show(ui, |ui| {
                            ui.vertical_centered(|ui| {
                                ui.label(theme::caption(t("Code for the new computer")));
                                ui.label(RichText::new(&code).font(theme::mono(34.0)).color(theme::TEXT));
                                let addrs: Vec<String> = self.remote.local_ips.iter().map(|ip| format!("{ip}:{}", s.port())).collect();
                                ui.label(
                                    RichText::new(tr!(
                                        "On the new computer: Remote → Coupling mode → Join the group. Address {address}",
                                        address = if addrs.is_empty() { tr!("port {port}", port = s.port()) } else { addrs.join("  ·  ") }
                                    ))
                                    .small()
                                    .color(theme::TEXT_DIM),
                                );
                                ui.label(
                                    RichText::new(tr!(
                                        "Valid for {time} · {tries} tries left",
                                        time = format!("{}:{:02}", left.as_secs() / 60, left.as_secs() % 60),
                                        tries
                                    ))
                                    .small()
                                    .color(theme::TEXT_DIM),
                                );
                                if ui.button(t("Cancel")).clicked() {
                                    s.close_pairing();
                                }
                            });
                        });
                        ui.ctx().request_repaint_after(Duration::from_millis(500));
                    }
                    None => {
                        if ui.button(theme::with_icon(icons::ADD, t("Add a computer…"))).clicked() {
                            s.open_pairing();
                        }
                    }
                }
            } else {
                ui.label(RichText::new(t("Turn coupling on to add computers.")).weak());
            }
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(t("Count the main device as lost after"));
            ui.add(egui::DragValue::new(&mut next.silence).range(1.0..=30.0).speed(0.1).suffix(" s"));
        });

        // Media.
        ui.add_space(10.0);
        ui.label(theme::caption(t("Media on this computer")));
        self.media_row(ui, show, &mut next);

        // Leaving.
        ui.add_space(12.0);
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button(t("Leave the group")).on_hover_text(t("Forget the group's key and list on this computer")).clicked() {
                next.leave();
            }
            let armed = self.remote.confirm_new_group;
            let label = if armed { t("Really start a new group?") } else { t("Start a new group…") };
            if ui
                .button(RichText::new(label).color(if armed { RED } else { theme::TEXT }))
                .on_hover_text(t(
                    "A new key: computers that were in the old group can no longer connect, and must join again. Use it when a computer you took out of the list must be locked out.",
                ))
                .clicked()
            {
                if armed {
                    let mine = next.members.iter().find(|m| m.id == me).cloned();
                    next.leave();
                    if let Some(m) = mine {
                        next.create(Member { address: String::new(), ..m });
                    }
                    self.remote.confirm_new_group = false;
                } else {
                    self.remote.confirm_new_group = true;
                }
            }
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new(t(
                "Computers in a group trust each other fully: any of them can change the list and play the show. Use a network that is only for the show. \
                 If two lose sight of each other, the one that took over last keeps playing.",
            ))
            .small()
            .color(theme::TEXT_FAINT),
        );

        if next != co {
            if next.members != co.members {
                next.edited();
            }
            next.clean();
            if next.group != co.group {
                // A new or forgotten group: nothing of the old one carries over.
                self.remote.acting = None;
                self.remote.orphan = None;
                self.remote.following = None;
            }
            let mut c = self.remote.config.lock();
            c.coupling = next;
            c.server_enabled |= c.coupling.enabled;
            drop(c);
            self.remote.save_config();
        }
    }

    /// Where this computer's media come from, and whether they are all there.
    fn media_row(&mut self, ui: &mut Ui, show: &mut Show, next: &mut Coupling) {
        match &next.media_root {
            Some(root) => {
                ui.label(RichText::new(tr!("From the show package at {path}", path = root.display().to_string())).font(theme::mono(11.5)).color(theme::TEXT_DIM));
            }
            None => {
                ui.label(RichText::new(t("From the same paths as on the main device")).color(theme::TEXT_DIM));
            }
        }
        ui.horizontal(|ui| {
            if ui.add_enabled(self.package.is_none(), egui::Button::new(theme::with_icon(icons::FOLDER_OPEN, t("Use a show package…")))).clicked()
                && let Some(p) = rfd::FileDialog::new().add_filter(t("OpnQ show package"), &[crate::package::EXTENSION]).pick_file()
            {
                // Unpacked like any package; the window for it also opens the show it holds, which a
                // standby computer doesn't want: only the media folder is kept.
                self.remote.pending_media_package = Some(crate::package::Job::open(p));
            }
            if next.media_root.is_some() && ui.button(t("Use the same paths")).clicked() {
                next.media_root = None;
            }
        });
        if let Some(job) = &self.remote.pending_media_package {
            let p = job.progress.lock().clone();
            match p.done {
                None => {
                    ui.horizontal(|ui| {
                        ui.add(egui::ProgressBar::new(if p.total_bytes > 0 { p.bytes as f32 / p.total_bytes as f32 } else { 0.0 }).desired_width(240.0));
                        if ui.button(t("Cancel")).clicked() {
                            job.cancel();
                        }
                    });
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                }
                Some(r) => {
                    let unpacked = job.unpacked.lock().clone();
                    self.remote.pending_media_package = None;
                    match (r, unpacked) {
                        (Ok(_), Some(u)) => next.media_root = Some(u.dir),
                        (Err(e), _) => self.notify_error(tr!("Opening the package failed: {error}", error = e)),
                        _ => {}
                    }
                }
            }
        }
        // Are the files there?
        self.refresh_ready(show, next.media_root.clone());
        match self.remote.ready.last {
            Some((found, total)) if found == total => {
                ui.label(RichText::new(tr!("All {total} media files are here", total = total.to_string())).color(theme::GREEN));
            }
            Some((found, total)) => {
                ui.label(RichText::new(tr!("{missing} of {total} media files are missing here", missing = (total - found).to_string(), total = total.to_string())).color(AMBER));
            }
            None => {
                ui.label(RichText::new(t("Media are checked once this computer follows a show")).small().color(theme::TEXT_FAINT));
            }
        }
    }

    /// Look (on a thread) for the media files of the mirrored show, as they would be found if this
    /// computer took over.
    fn refresh_ready(&mut self, show: &mut Show, media_root: Option<PathBuf>) {
        let ready = &mut self.remote.ready;
        if let Some(r) = ready.result.lock().take() {
            ready.last = Some(r);
            ready.running = false;
        }
        let relevant = show.replica && self.remote.replica.is_some();
        if !relevant {
            ready.last = None;
            return;
        }
        if ready.running || ready.checked.is_some_and(|t| t.elapsed() < READY_EVERY) {
            return;
        }
        ready.running = true;
        ready.checked = Some(Instant::now());
        let mut files = Vec::new();
        {
            let mut ws = show.ws.clone();
            if let Some(unpacked) = media_root.as_deref().and_then(crate::package::unpacked) {
                crate::package::remap(&mut ws, &unpacked.dir, &unpacked.manifest);
            }
            ws.for_each_file_mut(&mut |p| {
                if !p.as_os_str().is_empty() && files.len() < READY_MAX {
                    files.push(p.clone());
                }
            });
        }
        let slot = ready.result.clone();
        let started = std::thread::Builder::new().name("opnq-coupling-ready".into()).spawn(move || {
            files.sort();
            files.dedup();
            let found = files.iter().filter(|p| !crate::media::is_network_path(p) && std::fs::metadata(p).is_ok_and(|m| m.is_file())).count();
            *slot.lock() = Some((found, files.len()));
        });
        if started.is_err() {
            ready.running = false;
        }
    }
}

