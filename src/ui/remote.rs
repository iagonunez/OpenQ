//! Remote UI: sharing this OpenQ with paired devices, and running as a replica of another one.

use super::widgets::{AMBER, RED};
use super::{App, Mode, icons, theme};
use crate::control::Command;
use crate::model::{CueId, Workspace};
use crate::remote::client::{Auth, Link, LinkState};
use crate::remote::discovery::{self, Found};
use crate::remote::server::Server;
use crate::remote::{ClientMsg, RemoteConfig, Role, ServerMsg};
use crate::show::Show;
use egui::{RichText, Ui};
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use crate::i18n::t;

/// Edits are sent at most this often while typing.
const EDIT_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum RemoteTab {
    #[default]
    Share,
    Connect,
}

#[derive(Default)]
struct Search {
    running: bool,
    result: Option<Result<Vec<Found>, String>>,
}

/// While connected to a main device: what we mirror and what to restore afterwards.
struct Replica {
    server: String,
    role: Role,
    title: String,
    dirty: bool,
    meters: Vec<f32>,
    /// Last workspace both sides agree on; local differences are edits to send.
    synced: Option<Workspace>,
    last_edit: Instant,
    /// Playhead as last mirrored, to notice local changes.
    mirrored_playhead: Option<CueId>,
    /// A playhead move we asked for, kept until the main device confirms it.
    pending_playhead: Option<(Option<CueId>, Instant)>,
    /// The local show, put back on disconnect.
    stash: Option<(Workspace, Option<PathBuf>, bool)>,
    /// The DMX output was asked for (the DMX monitor is showing).
    dmx: bool,
}

pub(super) struct RemoteUi {
    config: Arc<Mutex<RemoteConfig>>,
    server: Option<Server>,
    server_error: Option<String>,
    started: bool,
    link: Option<Link>,
    replica: Option<Replica>,
    pub(super) window: bool,
    pub(super) tab: RemoteTab,
    address: String,
    code: String,
    notice: Option<String>,
    search: Arc<Mutex<Search>>,
    local_ips: Vec<std::net::IpAddr>,
}

impl Default for RemoteUi {
    fn default() -> Self {
        Self {
            config: Arc::new(Mutex::new(RemoteConfig::load())),
            server: None,
            server_error: None,
            started: false,
            link: None,
            replica: None,
            window: false,
            tab: RemoteTab::Share,
            address: String::new(),
            code: String::new(),
            notice: None,
            search: Default::default(),
            local_ips: Vec::new(),
        }
    }
}

impl RemoteUi {
    /// Connected to (or connecting to) a main device.
    pub(super) fn is_replica(&self) -> bool {
        self.replica.is_some()
    }

    pub(super) fn role(&self) -> Option<Role> {
        self.replica.as_ref().map(|r| r.role)
    }

    /// Workspace title and unsaved flag of the main device.
    pub(super) fn title(&self) -> Option<(String, bool)> {
        self.replica.as_ref().map(|r| (format!("{} — {}", r.server, r.title), r.dirty))
    }

    pub(super) fn meters(&self) -> Option<&[f32]> {
        self.replica.as_ref().map(|r| r.meters.as_slice())
    }

    /// Short status for the status bar: (text, is_replica).
    pub(super) fn status(&self) -> Option<(String, bool)> {
        if let Some(r) = &self.replica {
            return Some((tr!("Remote: {server} · {role}", server = r.server, role = r.role.label()), true));
        }
        if matches!(self.link.as_ref().map(|l| l.state()), Some(LinkState::Connecting)) {
            return Some((t("Connecting…").into(), true));
        }
        self.server.as_ref().map(|s| {
            let n = s.connected().len();
            let text = match n {
                0 => tr!("Sharing on :{port}", port = s.port()),
                1 => t("1 remote device").into(),
                n => tr!("{n} remote devices", n),
            };
            (text, false)
        })
    }

    fn save_config(&self) {
        if let Err(e) = self.config.lock().save() {
            log::warn!("cannot save remote config: {e:#}");
        }
    }

    fn start_server(&mut self, ctx: &egui::Context, show: crate::show::SharedShow) {
        let c = ctx.clone();
        match Server::start(show, self.config.clone(), move || c.request_repaint()) {
            Ok(s) => {
                self.server = Some(s);
                self.server_error = None;
            }
            Err(e) => self.server_error = Some(format!("{e:#}")),
        }
        self.local_ips = discovery::local_addresses();
    }
}

impl App {
    /// Route a show-control command: to the main device when this is a replica.
    pub(super) fn control(&mut self, show: &mut Show, cmd: Command) {
        if let Some(r) = &self.remote.replica {
            if !r.role.can_control() {
                self.notify(t("This device is view-only"));
            } else if let Some(l) = &self.remote.link {
                l.send(ClientMsg::Command(cmd));
            }
            return;
        }
        if let Err(e) = crate::control::apply(show, cmd, Instant::now()) {
            self.notify(e);
        }
    }

    /// Edits are allowed here (locally, or on a replica with the Edit role).
    pub(super) fn can_edit_show(&self) -> bool {
        self.remote.role().is_none_or(Role::can_edit)
    }

    /// Per-frame remote housekeeping; call before the UI draws.
    pub(super) fn remote_tick(&mut self, ctx: &egui::Context, show: &mut Show) {
        if !self.remote.started {
            self.remote.started = true;
            if self.remote.config.lock().server_enabled {
                self.remote.start_server(ctx, self.show.clone());
            }
        }
        let Some(link) = &self.remote.link else { return };
        match link.state() {
            LinkState::Connecting => {}
            LinkState::Connected { server, role } => {
                if self.remote.replica.is_none() {
                    self.enter_replica(show, server, role);
                }
                self.replica_sync(show);
            }
            LinkState::Closed { reason, needs_pairing } => {
                self.remote.link = None;
                self.leave_replica(show);
                if needs_pairing {
                    self.remote.window = true;
                    self.remote.tab = RemoteTab::Connect;
                }
                self.notify(reason.clone());
                self.remote.notice = Some(reason);
            }
        }
        if self.remote.link.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn enter_replica(&mut self, show: &mut Show, server: String, role: Role) {
        let now = Instant::now();
        let Show { engine, media, .. } = show;
        engine.stop_all(media, now, 0.0);
        let stash = (std::mem::take(&mut show.ws), show.path.take(), show.dirty);
        show.replica = true;
        show.sync_lighting();
        show.dirty = false;
        self.prober.set_remote(true);
        if let Some(l) = &self.remote.link {
            let tx = l.sender();
            self.waveforms.set_remote(Some(Box::new(move |p| {
                let _ = tx.send(ClientMsg::Waveform(p));
            })));
        }
        self.selection.clear();
        self.inline = None;
        self.reset_undo();
        if !role.can_edit() {
            self.mode = Mode::Show;
        }
        self.notify(tr!("Connected to {server} ({role})", server, role = role.label()));
        self.remote.notice = None;
        self.remote.replica = Some(Replica {
            server,
            role,
            title: String::new(),
            dirty: false,
            meters: Vec::new(),
            synced: None,
            last_edit: Instant::now(),
            mirrored_playhead: None,
            pending_playhead: None,
            stash: Some(stash),
            dmx: false,
        });
    }

    fn leave_replica(&mut self, show: &mut Show) {
        let Some(mut r) = self.remote.replica.take() else { return };
        show.replica = false;
        show.sync_lighting();
        show.engine.mirror(None, false, &[], Vec::new(), Instant::now());
        self.prober.set_remote(false);
        self.waveforms.set_remote(None);
        if let Some((ws, path, dirty)) = r.stash.take() {
            show.replace_workspace(ws, path);
            show.dirty = dirty;
        }
        self.selection.clear();
        self.inline = None;
        self.reset_undo();
    }

    pub(super) fn disconnect(&mut self, show: &mut Show) {
        self.remote.link = None;
        self.leave_replica(show);
        self.notify(t("Disconnected"));
    }

    pub(super) fn connect(&mut self, ctx: &egui::Context, show: &mut Show, address: String, auth: Auth) {
        if self.remote.link.is_some() {
            self.disconnect(show);
        }
        self.remote.notice = None;
        let c = ctx.clone();
        self.remote.link = Some(Link::connect(&address, auth, self.remote.config.clone(), move || c.request_repaint()));
    }

    /// Apply what the main device sent and send back local playhead moves and edits.
    fn replica_sync(&mut self, show: &mut Show) {
        // The DMX monitor (its page, or the locked lighting view) shows the main device's output.
        let monitor = self.view == super::lighting::View::Lighting && (self.light.tab == super::lighting::LightTab::Monitor || !self.can_design());
        let Some(link) = &self.remote.link else { return };
        let Some(r) = &mut self.remote.replica else { return };
        let now = Instant::now();
        if monitor != r.dmx {
            r.dmx = monitor;
            link.send(ClientMsg::Subscribe { dmx: monitor });
            if !monitor {
                show.media.light.mirror_frame(&[]);
            }
        }
        let mut notices = Vec::new();

        // Local playhead change since the last mirror (clicking a row, arrow keys...).
        if show.engine.playhead != r.mirrored_playhead {
            if r.role.can_control() {
                link.send(ClientMsg::Command(Command::SetPlayhead(show.engine.playhead)));
                r.pending_playhead = Some((show.engine.playhead, now));
            }
            r.mirrored_playhead = show.engine.playhead;
        }

        let mut new_ws = false;
        for msg in link.drain() {
            match msg {
                ServerMsg::Welcome { .. } => {}
                ServerMsg::Workspace { mut ws, title, keep_lighting } => {
                    r.title = title;
                    if keep_lighting {
                        match &r.synced {
                            Some(synced) => ws.lighting = synced.lighting.clone(),
                            None => {
                                link.send(ClientMsg::Resync);
                                continue;
                            }
                        }
                    }
                    if r.synced.as_ref() != Some(&*ws) {
                        show.ws = (*ws).clone();
                        r.synced = Some(*ws);
                        new_ws = true;
                    }
                }
                ServerMsg::Cues(patch) => {
                    let applied = match &mut r.synced {
                        Some(synced) => crate::remote::apply_cue_patch(synced, &patch),
                        None => false,
                    };
                    if applied {
                        // Local edits not yet echoed are overwritten, as with a whole workspace.
                        crate::remote::apply_cue_patch(&mut show.ws, &patch);
                        show.engine.workspace_changed();
                    } else {
                        link.send(ClientMsg::Resync);
                    }
                }
                ServerMsg::Lighting(patch) => {
                    let applied = r.synced.as_mut().is_some_and(|synced| crate::light::patch::apply(&mut synced.lighting, &patch));
                    if applied {
                        // Local edits not yet echoed are overwritten, as with a whole workspace.
                        if !crate::light::patch::apply(&mut show.ws.lighting, &patch)
                            && let Some(synced) = &r.synced
                        {
                            show.ws.lighting = synced.lighting.clone();
                        }
                        show.sync_lighting();
                    } else {
                        link.send(ClientMsg::Resync);
                    }
                }
                ServerMsg::Dmx(universes) => show.media.light.mirror_frame(&universes),
                ServerMsg::State(s) => {
                    show.media.light.mirror(s.lighting.masters.clone(), s.lighting.sequences.clone());
                    let mut playhead = s.playhead;
                    match r.pending_playhead {
                        Some((want, at)) if want != s.playhead && at.elapsed() < Duration::from_secs(1) => playhead = want,
                        _ => r.pending_playhead = None,
                    }
                    show.engine.mirror(playhead, s.panicking, &s.instances, s.log, now);
                    r.mirrored_playhead = playhead;
                    r.meters = s.meters;
                    r.dirty = s.dirty;
                }
                ServerMsg::MediaInfo(list) => {
                    for (p, info) in list {
                        self.prober.insert(p, info);
                    }
                }
                ServerMsg::Waveform(p, w) => self.waveforms.insert(p, w.map(Arc::new)),
                ServerMsg::Role(role) => {
                    r.role = role;
                    notices.push(tr!("Your role is now: {role}", role = role.label()));
                }
                ServerMsg::Refused(why) => notices.push(why),
            }
        }

        // Local edits go to the main device (or are undone if not allowed).
        if let Some(synced) = &r.synced
            && show.ws != *synced
        {
            if r.role.can_edit() {
                if r.last_edit.elapsed() >= EDIT_INTERVAL {
                    match crate::remote::workspace_patch(synced, &show.ws) {
                        Some((cues, lighting)) => link.send(ClientMsg::Patch { cues, lighting: lighting.map(Box::new) }),
                        None => {
                            let mut ws = show.ws.clone();
                            ws.lighting = Default::default();
                            let lighting = crate::light::patch::diff(&synced.lighting, &show.ws.lighting).map(Box::new);
                            link.send(ClientMsg::Reshape { ws: Box::new(ws), lighting });
                        }
                    }
                    r.synced = Some(show.ws.clone());
                    r.last_edit = now;
                }
            } else {
                show.ws = synced.clone();
            }
        }
        let can_edit = r.role.can_edit();
        if new_ws {
            let ws = &show.ws;
            self.selection.retain(|id| ws.cue_list.find(*id).is_some());
            self.reset_undo();
        }
        if !can_edit && self.mode == Mode::Edit {
            self.mode = Mode::Show;
        }
        for n in notices {
            self.notify(n);
        }
    }

    /// Screenshot hook: open pairing as soon as the server is up.
    pub(super) fn remote_debug_pair(&mut self) {
        if let Some(s) = &self.remote.server {
            s.open_pairing();
        }
    }

    // ------------------------------------------------------------------ window

    /// Ask the operator about network settings changes a replica made (Network cue destinations,
    /// DMX outputs and inputs): they only reach the show once allowed here.
    pub(super) fn network_requests_window(&mut self, ctx: &egui::Context, show: &mut Show) {
        let Some(r) = show.network_requests.first() else { return };
        let (id, more) = (r.id, show.network_requests.len() - 1);
        let mut answer = None;
        egui::Window::new(theme::with_icon(icons::LAN, t("Network settings change")))
            .id(egui::Id::new("network-request"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .fixed_size([520.0, 0.0])
            .show(ctx, |ui| {
                ui.label(tr!("{device} asks to change where this OpenQ sends or listens on the network:", device = r.device.as_str()));
                ui.add_space(6.0);
                theme::card_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for line in r.change.describe() {
                        ui.label(RichText::new(line).font(theme::mono(12.0)));
                    }
                });
                ui.add_space(4.0);
                ui.label(RichText::new(t("Allow only changes you expect: they decide which computers this one sends to.")).small().color(theme::TEXT_DIM));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(theme::with_icon(icons::CHECK_CIRCLE, t("Allow"))).clicked() {
                        answer = Some(true);
                    }
                    if ui.button(t("Deny")).clicked() {
                        answer = Some(false);
                    }
                    if more > 0 {
                        ui.label(RichText::new(tr!("{n} more waiting", n = more.to_string())).small().color(theme::TEXT_DIM));
                    }
                });
            });
        if let Some(allow) = answer {
            show.answer_network_request(id, allow);
        }
    }

    pub(super) fn remote_window(&mut self, ctx: &egui::Context, show: &mut Show) {
        if !self.remote.window {
            return;
        }
        let mut open = true;
        egui::Window::new(theme::with_icon(icons::ROUTER, t("Remote")))
            .id(egui::Id::new("remote-window"))
            .open(&mut open)
            .default_size([560.0, 640.0])
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    theme::tab(ui, &mut self.remote.tab, RemoteTab::Share, icons::LAN, t("Share this OpenQ"));
                    theme::tab(ui, &mut self.remote.tab, RemoteTab::Connect, icons::CABLE, t("Connect to another"));
                });
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.remote.tab {
                    RemoteTab::Share => self.share_tab(ui),
                    RemoteTab::Connect => self.connect_tab(ui, show),
                });
            });
        self.remote.window = open;
    }

    fn share_tab(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        egui::Grid::new("remote-share").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(t("This device"));
            let mut name = self.remote.config.lock().instance_name.clone();
            let r = ui.add(egui::TextEdit::singleline(&mut name).desired_width(220.0));
            if r.changed() {
                self.remote.config.lock().instance_name = name.chars().take(64).collect();
            }
            if r.lost_focus() {
                self.remote.save_config();
            }
            ui.end_row();

            ui.label(t("Remote access"));
            ui.horizontal(|ui| {
                let mut on = self.remote.server.is_some();
                if ui.checkbox(&mut on, t("Let paired devices connect")).changed() {
                    if on {
                        self.remote.start_server(&ctx, self.show.clone());
                    } else {
                        self.remote.server = None;
                    }
                    self.remote.config.lock().server_enabled = self.remote.server.is_some();
                    self.remote.save_config();
                }
                if self.remote.server.is_none() {
                    let mut port = self.remote.config.lock().port;
                    ui.label(t("port"));
                    if ui.add(egui::DragValue::new(&mut port).range(1024..=65535)).changed() {
                        self.remote.config.lock().port = port;
                        self.remote.save_config();
                    }
                }
            });
            ui.end_row();
            if let Some(e) = &self.remote.server_error {
                ui.label("");
                ui.label(RichText::new(e).color(RED));
                ui.end_row();
            }
            if let Some(s) = &self.remote.server {
                ui.label(t("Address"));
                let addrs: Vec<String> = self.remote.local_ips.iter().map(|ip| format!("{ip}:{}", s.port())).collect();
                ui.label(RichText::new(if addrs.is_empty() { tr!("port {port}", port = s.port()) } else { addrs.join("  ·  ") }).font(theme::mono(12.0)));
                ui.end_row();
            }

            ui.label(t("New devices get"));
            let mut role = self.remote.config.lock().default_role;
            if role_combo(ui, "default-role", &mut role) {
                self.remote.config.lock().default_role = role;
                self.remote.save_config();
            }
            ui.end_row();
        });

        ui.add_space(8.0);
        if let Some(s) = &self.remote.server {
            match s.pairing() {
                Some((code, left, tries)) => {
                    egui::Frame::new().fill(theme::BG_INPUT).corner_radius(8).inner_margin(12).show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(theme::caption(t("Pairing code")));
                            ui.label(RichText::new(&code).font(theme::mono(34.0)).color(theme::TEXT));
                            ui.label(
                                RichText::new(tr!(
                                    "Enter it on the other device (Remote → Connect to another). Valid for {time} · {tries} tries left",
                                    time = format!("{}:{:02}", left.as_secs() / 60, left.as_secs() % 60),
                                    tries
                                ))
                                .small()
                                .color(theme::TEXT_DIM),
                            );
                            if ui.button(t("Cancel pairing")).clicked() {
                                s.close_pairing();
                            }
                        });
                    });
                    ctx.request_repaint_after(Duration::from_millis(500));
                }
                None => {
                    if ui.button(theme::with_icon(icons::ADD, t("Pair a new device…"))).clicked() {
                        s.open_pairing();
                    }
                }
            }
        } else {
            ui.label(RichText::new(t("Turn remote access on to pair devices.")).weak());
        }

        ui.add_space(10.0);
        ui.label(theme::caption(t("Paired devices")));
        let connected: Vec<uuid::Uuid> = self.remote.server.as_ref().map(|s| s.connected().iter().map(|c| c.device).collect()).unwrap_or_default();
        let mut devices = self.remote.config.lock().devices.clone();
        let before = devices.clone();
        if devices.is_empty() {
            ui.label(RichText::new(t("None yet.")).weak());
        }
        let mut remove = None;
        egui::Grid::new("remote-devices").num_columns(4).spacing([12.0, 6.0]).show(ui, |ui| {
            for (i, d) in devices.iter_mut().enumerate() {
                let on = connected.contains(&d.id);
                ui.horizontal(|ui| {
                    ui.label(theme::icon(icons::FIBER_MANUAL_RECORD, 12.0).color(if on { theme::GREEN } else { theme::TEXT_FAINT }));
                    ui.label(&d.name);
                });
                role_combo(ui, ("role", d.id), &mut d.role);
                ui.label(RichText::new(if on { t("connected").into() } else { ago(d.last_seen) }).small().color(theme::TEXT_DIM));
                if theme::icon_button(ui, icons::DELETE, t("Remove: disconnects it and it must pair again")).clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = remove {
            devices.remove(i);
        }
        if devices != before {
            self.remote.config.lock().devices = devices;
            self.remote.save_config();
        }

        if let Some(s) = &self.remote.server {
            let events = s.events();
            if !events.is_empty() {
                ui.add_space(8.0);
                egui::CollapsingHeader::new(theme::caption(t("Activity"))).show(ui, |ui| {
                    for e in events.iter().rev() {
                        ui.label(RichText::new(e).font(theme::mono(11.0)).color(theme::TEXT_DIM));
                    }
                });
            }
        }
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                t("Connections are encrypted end to end. A pairing code works for 5 minutes and 5 tries; \
                 paired devices then log in with their own key, which Remove revokes."),
            )
            .small()
            .color(theme::TEXT_FAINT),
        );
    }

    fn connect_tab(&mut self, ui: &mut Ui, show: &mut Show) {
        let ctx = ui.ctx().clone();
        match self.remote.link.as_ref().map(|l| (l.state(), l.address.clone())) {
            Some((LinkState::Connected { server, role }, addr)) => {
                ui.horizontal(|ui| {
                    ui.label(theme::icon(icons::FIBER_MANUAL_RECORD, 12.0).color(theme::GREEN));
                    ui.label(RichText::new(tr!("Connected to {server}", server)).strong());
                    ui.label(RichText::new(format!("{addr} · {}", role.label())).color(theme::TEXT_DIM));
                });
                ui.label(
                    RichText::new(match role {
                        Role::View => t("You can follow the show here. The main device controls it."),
                        Role::Control => t("GO, stop, pause and the playhead act on the main device."),
                        Role::Edit => t("You can run and edit the show; changes are saved on the main device."),
                    })
                    .weak(),
                );
                if ui.button(theme::with_icon(icons::CLOSE, t("Disconnect"))).clicked() {
                    self.disconnect(show);
                }
                return;
            }
            Some((LinkState::Connecting, addr)) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(tr!("Connecting to {addr}…", addr));
                    if ui.button(t("Cancel")).clicked() {
                        self.remote.link = None;
                    }
                });
                return;
            }
            _ => {}
        }
        if let Some(n) = &self.remote.notice {
            ui.label(RichText::new(n).color(AMBER));
            ui.add_space(4.0);
        }

        // Discovery.
        ui.horizontal(|ui| {
            ui.label(theme::caption(t("On this network")));
            let running = self.remote.search.lock().running;
            if running {
                ui.spinner();
            } else if ui.small_button(theme::with_icon(icons::REFRESH, t("Search"))).clicked() {
                let s = self.remote.search.clone();
                s.lock().running = true;
                let c = ctx.clone();
                std::thread::spawn(move || {
                    let r = discovery::search(Duration::from_millis(900)).map_err(|e| e.to_string());
                    *s.lock() = Search { running: false, result: Some(r) };
                    c.request_repaint();
                });
            }
        });
        let (known_ids, my_id): (Vec<uuid::Uuid>, uuid::Uuid) = {
            let c = self.remote.config.lock();
            (c.servers.iter().map(|k| k.id).collect(), c.instance_id)
        };
        let found = self.remote.search.lock().result.clone();
        let mut go: Option<(String, Auth)> = None;
        match found {
            None => {
                ui.label(RichText::new(t("Search to list OpenQ devices sharing on this network.")).weak());
            }
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(RED));
            }
            Some(Ok(list)) => {
                let list: Vec<Found> = list.into_iter().filter(|f| f.id != my_id).collect();
                if list.is_empty() {
                    ui.label(RichText::new(t("Nothing found. Is remote access on there, and are you on the same network?")).weak());
                }
                for f in list {
                    ui.horizontal(|ui| {
                        ui.label(theme::icon(icons::LAN, 16.0).color(theme::TEXT_DIM));
                        ui.label(&f.name);
                        ui.label(RichText::new(f.address.to_string()).font(theme::mono(11.5)).color(theme::TEXT_DIM));
                        let paired = known_ids.contains(&f.id);
                        if paired {
                            ui.label(RichText::new(t("paired")).small().color(theme::GREEN));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if paired {
                                if ui.button(t("Connect")).clicked() {
                                    go = Some((f.address.to_string(), Auth::Token));
                                }
                            } else if ui.button(t("Pair…")).on_hover_text(t("Fill in the address; then enter the code shown there")).clicked() {
                                self.remote.address = f.address.to_string();
                            }
                        });
                    });
                }
            }
        }

        // Manual connection / pairing.
        ui.add_space(8.0);
        ui.label(theme::caption(t("Connect")));
        egui::Grid::new("remote-connect").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(t("Address"));
            ui.add(egui::TextEdit::singleline(&mut self.remote.address).hint_text(t("192.168.1.20 or host:port")).desired_width(240.0));
            ui.end_row();
            ui.label(t("Pairing code"));
            ui.add(
                egui::TextEdit::singleline(&mut self.remote.code)
                    .hint_text(t("XXXX-XXXX (first time only)"))
                    .font(theme::mono(14.0))
                    .desired_width(160.0),
            );
            ui.end_row();
            ui.label("");
            let ok = !self.remote.address.trim().is_empty();
            if ui.add_enabled(ok, egui::Button::new(theme::with_icon(icons::CABLE, t("Connect")))).clicked() {
                let code = std::mem::take(&mut self.remote.code);
                let auth = if code.trim().is_empty() { Auth::Token } else { Auth::Pair(code) };
                go = Some((self.remote.address.trim().to_string(), auth));
            }
            ui.end_row();
        });

        // Remembered main devices.
        let servers = self.remote.config.lock().servers.clone();
        if !servers.is_empty() {
            ui.add_space(8.0);
            ui.label(theme::caption(t("Paired main devices")));
            let mut forget = None;
            for k in &servers {
                ui.horizontal(|ui| {
                    ui.label(&k.name);
                    ui.label(RichText::new(&k.address).font(theme::mono(11.5)).color(theme::TEXT_DIM));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::icon_button(ui, icons::DELETE, t("Forget this device's key")).clicked() {
                            forget = Some(k.id);
                        }
                        if ui.button(t("Connect")).clicked() {
                            go = Some((k.address.clone(), Auth::Token));
                        }
                    });
                });
            }
            if let Some(id) = forget {
                self.remote.config.lock().servers.retain(|k| k.id != id);
                self.remote.save_config();
            }
        }
        ui.add_space(6.0);
        ui.label(
            RichText::new(t("While connected this window shows the main device's show; yours comes back when you disconnect."))
                .small()
                .color(theme::TEXT_FAINT),
        );

        if let Some((addr, auth)) = go {
            self.remote.address = addr.clone();
            self.connect(&ctx, show, addr, auth);
        }
    }
}

fn role_combo(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, role: &mut Role) -> bool {
    let mut changed = false;
    egui::ComboBox::from_id_salt(id).selected_text(role.label()).width(150.0).show_ui(ui, |ui| {
        for r in Role::ALL {
            changed |= ui.selectable_value(role, r, r.label()).changed();
        }
    });
    changed
}

fn ago(unix: u64) -> String {
    let s = crate::remote::unix_now().saturating_sub(unix);
    match s {
        0..=59 => t("just now").into(),
        60..=3599 => tr!("{n} min ago", n = s / 60),
        3600..=86_399 => tr!("{n} h ago", n = s / 3600),
        _ => tr!("{n} days ago", n = s / 86_400),
    }
}
