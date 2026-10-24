//! Remote UI: sharing this OpnQ with paired devices, and running as a replica of another one.

use super::widgets::{AMBER, RED};
use super::{App, Mode, icons, theme};
use crate::control::Command;
use crate::model::{CueId, Workspace};
use crate::remote::client::{Auth, Link, LinkState};
use crate::remote::discovery::{self, Found};
mod coupling_ui;

use crate::remote::coupling::{self, Coupling, Duty, Local, Member, Peer, Supervisor, Verdict};

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
    /// The main device's overrides that are off.
    overrides_off: u16,
    /// Last workspace both sides agree on; local differences are edits to send.
    synced: Option<Workspace>,
    /// [`Show::edit_gen`] at which the show was last known to match `synced`: comparing the two
    /// every frame cost ~8 ms on a 50 000-cue show.
    checked_gen: Option<u64>,
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
    /// Coupling: asks the other members what they are doing while this computer is in a group.
    supervisor: Option<Supervisor>,
    /// This device plays the show as a coupling group's main device, with this term.
    acting: Option<u64>,
    /// Since when this device has had no main device (coupling).
    orphan: Option<Instant>,
    /// How long to wait before taking over at all ([`Duty::Orphan`]).
    orphan_grace: Duration,
    /// The highest term seen.
    last_term: u64,
    /// The operator asked to take the show over now.
    wants_takeover: bool,
    /// The member the link was opened to by the coupling logic.
    following: Option<uuid::Uuid>,
    /// The Coupling mode window is open.
    pub(super) coupling_window: bool,
    join: coupling_ui::JoinUi,
    ready: coupling_ui::Ready,
    /// "Start a new group" was pressed once and waits for a second press.
    confirm_new_group: bool,
    /// A show package being unpacked to play this computer's media from.
    pending_media_package: Option<crate::package::Job>,
    /// The round of the supervisor whose verdict was acted on last.
    acted_round: u64,
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
            supervisor: None,
            acting: None,
            orphan: None,
            orphan_grace: Duration::ZERO,
            last_term: 0,
            wants_takeover: false,
            following: None,
            coupling_window: false,
            join: Default::default(),
            ready: Default::default(),
            confirm_new_group: false,
            pending_media_package: None,
            acted_round: 0,
        }
    }
}

impl RemoteUi {
    /// Connected to (or connecting to) a main device.
    pub(super) fn is_replica(&self) -> bool {
        self.replica.is_some()
    }

    /// The overrides that are off where the show plays: the main device's on a replica.
    pub(super) fn overrides_off(&self) -> u16 {
        self.replica.as_ref().map_or_else(crate::overrides::off_bits, |r| r.overrides_off)
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
        if self.supervisor.is_some() {
            let rank = {
                let c = self.config.lock();
                c.coupling.rank(c.instance_id).map_or(0, |r| r + 1)
            };
            if self.acting.is_some() {
                return Some((tr!("Main device · priority {rank}", rank), false));
            }
            return Some(match &self.replica {
                Some(r) if self.link.is_some() => (tr!("Standby · following {server} · priority {rank}", server = r.server, rank), true),
                _ => (tr!("Standby · no main device · priority {rank}", rank), true),
            });
        }
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
            } else {
                self.notify(t("Waiting for a main device"));
            }
            return;
        }
        if let Err(e) = crate::control::apply(show, cmd, Instant::now()) {
            self.notify(e);
        }
    }

    /// Where [`App::control`] sends commands now, for commands made outside the main window's
    /// frames (the Playback window).
    pub(super) fn route(&self) -> super::light_playback::Route {
        use super::light_playback::Route;
        match &self.remote.replica {
            None => Route::Local,
            Some(r) if !r.role.can_control() => Route::ViewOnly,
            Some(_) => Route::Main(self.remote.link.as_ref().map(Link::sender)),
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
        // Devices may edit only while this main device is in edit mode.
        if let Some(server) = &self.remote.server {
            server.set_show_mode(self.mode == Mode::Show);
            let d = self.displays.lock();
            if d.loaded && !d.remote {
                server.set_displays(&d.list);
            }
        }
        self.coupling_tick(ctx, show);
        let Some(link) = &self.remote.link else { return };
        match link.state() {
            LinkState::Connecting => {}
            LinkState::Connected { server, role } => {
                match &mut self.remote.replica {
                    None => self.enter_replica(show, server, role),
                    // Another main device after a coupling: the old copy is only the baseline.
                    Some(r) => {
                        if r.server != server {
                            r.server = server;
                            r.synced = None;
                            r.checked_gen = None;
                            r.pending_playhead = None;
                        }
                        r.role = role;
                    }
                }
                self.replica_sync(show);
            }
            LinkState::Closed { reason, needs_pairing } => {
                self.remote.link = None;
                if self.remote.supervisor.is_some() {
                    // In a coupling group the mirrored show stays: this device may have to take
                    // over, or to follow the next main device.
                    if self.remote.replica.is_some() && self.remote.orphan.is_none() {
                        self.remote.orphan = Some(Instant::now());
                        self.remote.orphan_grace = Duration::ZERO;
                        log::warn!("coupling: lost the main device: {reason}");
                        self.notify(tr!("Lost the main device: {reason}", reason));
                    }
                    return;
                }
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

    /// Coupling: tell the server and the supervisor what this device is doing, and carry out
    /// what the supervisor decided ([`coupling::decide`]).
    fn coupling_tick(&mut self, ctx: &egui::Context, show: &mut Show) {
        let (fo, me) = {
            let c = self.remote.config.lock();
            (c.coupling.clone(), c.instance_id)
        };
        if !fo.active_for(me) {
            if self.remote.supervisor.take().is_some() {
                self.remote.acting = None;
                self.remote.orphan = None;
                self.remote.wants_takeover = false;
                if let Some(s) = &self.remote.server {
                    s.set_presence(None);
                }
            }
            return;
        }
        let now = Instant::now();
        if self.remote.server.is_none() && self.remote.server_error.is_none() {
            self.remote.start_server(ctx, self.show.clone());
        }
        if self.remote.supervisor.is_none() {
            let c = ctx.clone();
            self.remote.supervisor = Some(Supervisor::start(self.remote.config.clone(), move || c.request_repaint()));
            self.remote.acted_round = 0;
            if self.remote.link.is_none() {
                // Just started: give the others a moment to show up before deciding who plays.
                self.remote.orphan = Some(now);
                self.remote.orphan_grace = fo.silence_limit() + Duration::from_secs(1);
            }
        }
        let linked = self.remote.replica.is_some() && matches!(self.remote.link.as_ref().map(Link::state), Some(LinkState::Connected { .. }));
        let main = self.remote.link.as_ref().and_then(|l| self.remote.following.or_else(|| crate::remote::client::known_at(&self.remote.config.lock(), &l.address)));
        let duty = match (self.remote.acting, main) {
            (Some(term), _) => Duty::Main { term },
            (None, Some(main)) if linked => Duty::Following { main },
            _ => Duty::Orphan { since: *self.remote.orphan.get_or_insert(now), grace: self.remote.orphan_grace, term: self.remote.last_term },
        };
        if !matches!(duty, Duty::Orphan { .. }) {
            self.remote.orphan = None;
        }
        let Some(sup) = &self.remote.supervisor else { return };
        // A newer priority list from another member (it was edited there, or a computer joined).
        if let Some(roster) = sup.roster() {
            let outcome = self.remote.config.lock().coupling.adopt(me, roster);
            match outcome {
                coupling::Adopted::Same => {}
                coupling::Adopted::Changed => self.remote.save_config(),
                coupling::Adopted::Removed => {
                    self.remote.config.lock().coupling.leave();
                    self.remote.save_config();
                    self.notify(t("This computer was taken out of the coupling group"));
                    return;
                }
            }
        }
        sup.set(&fo, duty);
        let peers = sup.peers();
        for p in peers.values() {
            if let Peer::Main { term } | Peer::Standby { term, .. } = p {
                self.remote.last_term = self.remote.last_term.max(*term);
            }
        }
        if let Some(term) = self.remote.acting {
            self.remote.last_term = self.remote.last_term.max(term);
        }
        let verdict = sup.verdict();
        let round = sup.rounds();
        if let Some(server) = &self.remote.server {
            let acting = self.remote.acting.is_some();
            server.set_presence(Some(Local { acting, term: self.remote.last_term, orphan_since: if acting { None } else { self.remote.orphan } }));
        }
        ctx.request_repaint_after(Duration::from_millis(250));

        let mut verdict = if round != self.remote.acted_round { verdict } else { Verdict::Wait };
        if std::mem::take(&mut self.remote.wants_takeover) && self.remote.acting.is_none() {
            verdict = Verdict::TakeOver(self.remote.last_term + 1);
        }
        match verdict {
            Verdict::Wait => {}
            Verdict::Follow(id) => {
                self.remote.acted_round = round;
                let Some(m) = fo.members.iter().find(|m| m.id == id).cloned() else { return };
                self.follow_member(ctx, &m, fo.silence_limit());
            }
            Verdict::TakeOver(term) => {
                self.remote.acted_round = round;
                self.become_main(show, term);
            }
        }
    }

    /// Coupling: connect to `m`, the device playing the show, stepping down if this one was.
    fn follow_member(&mut self, ctx: &egui::Context, m: &Member, silence: Duration) {
        match self.remote.link.as_ref().map(Link::state) {
            Some(LinkState::Connecting) => return,
            Some(LinkState::Connected { .. }) if self.remote.following == Some(m.id) => return,
            _ => {}
        }
        if self.remote.acting.take().is_some() {
            log::warn!("coupling: {} plays the show; this device steps down", m.name);
            self.notify(tr!("{name} plays the show now; this device is on standby", name = m.name.clone()));
            self.remote.orphan = Some(Instant::now());
            self.remote.orphan_grace = Duration::ZERO;
        }
        self.remote.notice = None;
        self.remote.following = Some(m.id);
        let c = ctx.clone();
        self.remote.link = Some(Link::connect_within(&m.address, Auth::Group, self.remote.config.clone(), Some(silence), move || c.request_repaint()));
    }

    /// Coupling: this device plays the show from now on, with the copy it mirrored.
    fn become_main(&mut self, show: &mut Show, term: u64) {
        let mut replica = self.remote.replica.take();
        self.remote.link = None;
        self.remote.following = None;
        if replica.is_some() {
            *self.displays.lock() = Default::default();
            self.prober.set_remote(false);
            self.waveforms.set_remote(None);
            let mine = replica.as_mut().and_then(|r| r.stash.take()).map(|s| s.0).unwrap_or_default();
            // Media from this computer's own package: the mirrored show names the other's paths.
            let root = self.remote.config.lock().coupling.media_root.clone();
            if let Some(u) = root.as_deref().and_then(crate::package::unpacked) {
                let n = crate::package::remap(&mut show.ws, &u.dir, &u.manifest);
                log::info!("coupling: {n} media paths point into the package now");
            }
            show.take_over(&mine);
            self.selection.clear();
            self.inline = None;
            self.reset_undo();
        }
        self.remote.acting = Some(term);
        self.remote.last_term = self.remote.last_term.max(term);
        self.remote.orphan = None;
        log::warn!("coupling: this device plays the show now (term {term})");
        show.engine.report(format!("Coupling: this device took over the show (term {term})"));
        self.notify(t("This device plays the show now"));
    }

    /// Coupling: take the show over from the main device now (it steps down when it sees this).
    pub(super) fn take_over_now(&mut self) {
        self.remote.wants_takeover = true;
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
            overrides_off: 0,
            synced: None,
            checked_gen: None,
            last_edit: Instant::now(),
            mirrored_playhead: None,
            pending_playhead: None,
            stash: Some(stash),
            dmx: false,
        });
    }

    fn leave_replica(&mut self, show: &mut Show) {
        let Some(mut r) = self.remote.replica.take() else { return };
        // This computer's own displays again.
        *self.displays.lock() = Default::default();
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
        let mut reshaped = false;
        for msg in link.drain() {
            match msg {
                ServerMsg::Welcome { .. } => {}
                ServerMsg::Workspace { mut ws, title, keep_lighting } => {
                    r.title = title;
                    // The copy kept as "synced" is the clamped one too, so clamping isn't taken
                    // for a local edit and sent back.
                    crate::limits::workspace(&mut ws);
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
                        show.changed();
                        r.checked_gen = Some(show.edit_gen);
                        new_ws = true;
                    }
                }
                ServerMsg::Cues(mut patch) => {
                    patch.iter_mut().for_each(crate::limits::cue);
                    let applied = match &mut r.synced {
                        Some(synced) => crate::remote::apply_cue_patch(synced, &patch),
                        None => false,
                    };
                    if applied {
                        // Local edits not yet echoed are overwritten, as with a whole workspace.
                        crate::remote::apply_cue_patch(&mut show.ws, &patch);
                        r.checked_gen = changed_in_step(show, r.checked_gen);
                        show.engine.workspace_changed();
                    } else {
                        link.send(ClientMsg::Resync);
                    }
                }
                ServerMsg::Ops(mut ops) => {
                    ops.cues_mut().for_each(crate::limits::cue);
                    let applied = r.synced.as_mut().is_some_and(|synced| ops.apply(synced));
                    if applied {
                        // Merged into local edits not yet echoed; if that doesn't fit, they're dropped.
                        if !ops.apply(&mut show.ws)
                            && let Some(synced) = &r.synced
                        {
                            show.ws = synced.clone();
                        }
                        r.checked_gen = changed_in_step(show, r.checked_gen);
                        show.engine.workspace_changed();
                        reshaped = true;
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
                        // Both copies alike, as for a whole workspace.
                        if let Some(synced) = &mut r.synced {
                            crate::limits::lighting(&mut synced.lighting);
                        }
                        crate::limits::lighting(&mut show.ws.lighting);
                        r.checked_gen = changed_in_step(show, r.checked_gen);
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
                    r.overrides_off = s.overrides_off;
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
                ServerMsg::Grant(_) => {}
                ServerMsg::Displays(list) => {
                    let mut d = self.displays.lock();
                    d.list = list.into_iter().take(crate::displays::MAX_REMOTE).map(crate::displays::Display::cleaned).collect();
                    (d.loaded, d.remote, d.error) = (true, true, None);
                }
            }
        }

        // Local edits go to the main device (or are undone if not allowed).
        let unchecked = r.checked_gen != Some(show.edit_gen);
        // Edits that said which cues they changed are checked by those cues alone (comparing or
        // cloning a 50 000-cue show takes milliseconds under the lock).
        if unchecked
            && r.role.can_edit()
            && r.last_edit.elapsed() >= EDIT_INTERVAL
            && let Some(synced) = &mut r.synced
            && let Some(ids) = r.checked_gen.and_then(|g| show.changes.since(g, show.edit_gen))
            && crate::changes::same_rest(synced, &show.ws)
        {
            // None: a cue isn't in both copies (or stopped or became a group): compare in full.
            let mut cues = Some(Vec::new());
            for id in &ids {
                match (show.ws.cue_list.find(*id), synced.cue_list.find(*id)) {
                    (Some(c), Some(old)) if c.is_group() == old.is_group() => {
                        if !c.shallow_eq(old)
                            && let Some(list) = &mut cues
                        {
                            list.push(c.shallow_clone());
                        }
                    }
                    _ => cues = None,
                }
            }
            if let Some(cues) = cues {
                let lighting = crate::light::patch::diff(&synced.lighting, &show.ws.lighting);
                if !cues.is_empty() || lighting.is_some() {
                    crate::remote::apply_cue_patch(synced, &cues);
                    synced.lighting = show.ws.lighting.clone();
                    link.send(ClientMsg::Patch { cues, lighting: lighting.map(Box::new) });
                    r.last_edit = now;
                }
                r.checked_gen = Some(show.edit_gen);
            }
        }
        let unchecked = r.checked_gen != Some(show.edit_gen);
        if let Some(synced) = &r.synced
            && unchecked
            && show.ws != *synced
        {
            if r.role.can_edit() {
                if r.last_edit.elapsed() >= EDIT_INTERVAL {
                    match crate::remote::workspace_patch(synced, &show.ws) {
                        Some((cues, lighting)) => link.send(ClientMsg::Patch { cues, lighting: lighting.map(Box::new) }),
                        None if let Some(ops) = crate::changes::Reshape::diff(synced, &show.ws) => {
                            let lighting = crate::light::patch::diff(&synced.lighting, &show.ws.lighting).map(Box::new);
                            link.send(ClientMsg::Ops { ops: Box::new(ops), lighting });
                        }
                        None => {
                            let mut ws = show.ws.clone();
                            ws.lighting = Default::default();
                            let lighting = crate::light::patch::diff(&synced.lighting, &show.ws.lighting).map(Box::new);
                            link.send(ClientMsg::Reshape { ws: Box::new(ws), lighting });
                        }
                    }
                    r.synced = Some(show.ws.clone());
                    r.checked_gen = Some(show.edit_gen);
                    r.last_edit = now;
                }
            } else {
                show.ws = synced.clone();
                show.changed();
                r.checked_gen = Some(show.edit_gen);
            }
        } else if unchecked && r.synced.is_some() {
            r.checked_gen = Some(show.edit_gen);
        }
        let can_edit = r.role.can_edit();
        if new_ws || reshaped {
            let ws = &show.ws;
            self.selection.retain(|id| ws.cue_list.find(*id).is_some());
        }
        if new_ws {
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
                ui.label(tr!("{device} asks to change where this OpnQ sends or listens on the network:", device = r.device.as_str()));
                ui.add_space(6.0);
                theme::card_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for line in r.change.describe() {
                        ui.label(RichText::new(line).font(theme::mono(12.0)));
                    }
                });
                ui.add_space(4.0);
                ui.label(RichText::new(t("Allow only changes you expect: they decide which computers this one sends to.")).small().color(theme::TEXT_DIM));
                if !r.change.stages.is_empty() {
                    ui.label(RichText::new(t("Stage displays decide what goes fullscreen over this computer's screens.")).small().color(theme::TEXT_DIM));
                }
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
                    theme::tab(ui, &mut self.remote.tab, RemoteTab::Share, icons::LAN, t("Share this OpnQ"));
                    theme::tab(ui, &mut self.remote.tab, RemoteTab::Connect, icons::CABLE, t("Connect to another"));
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new(t("Pairing is for tablets and other operators. Computers that back each other up use coupling mode.")).small().color(theme::TEXT_FAINT));
                    if ui.small_button(t("Coupling mode…")).clicked() {
                        self.remote.coupling_window = true;
                    }
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
                ui.label(RichText::new(t("Search to list OpnQ devices sharing on this network.")).weak());
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

/// Bump the show's edit generation for a change that came from the main device and went into
/// both copies: still in step with the synced copy if it was before.
fn changed_in_step(show: &mut Show, checked: Option<u64>) -> Option<u64> {
    let was = checked == Some(show.edit_gen);
    show.changed();
    was.then_some(show.edit_gen)
}
