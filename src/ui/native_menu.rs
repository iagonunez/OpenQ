//! macOS: the menus live in the system menu bar (muda) instead of inside the window.
//!
//! Menu items carry no key equivalents except Quit and Settings: OpenQ's own key handling
//! (`handle_keys`) already reacts to the shortcuts, and a menu accelerator would swallow the key
//! before egui sees it (or fire the action twice).

use super::{App, Deferred, Mode};
use crate::control::Command;
use crate::i18n::{self, t};
use crate::model::{KindTag, StageId};
use crate::show::Show;
use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use std::sync::mpsc::{Receiver, channel};

/// Something the user picked in the menu bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    New,
    Open,
    Save,
    SaveAs,
    Import,
    Quit,
    Undo,
    Redo,
    Duplicate,
    Delete,
    Group,
    Renumber,
    Settings,
    NewCue(usize),
    Go,
    Stop,
    Pause,
    Resume,
    ToggleMode,
    StageWindow(StageId),
    StageMapping(StageId),
    Stages,
    Share,
    Connect,
    Disconnect,
    Preview,
    Log,
    OpenVis,
    Manual,
    ErrorLog,
    About,
}

impl Action {
    fn id(self) -> String {
        match self {
            Action::NewCue(i) => format!("cue:{i}"),
            Action::StageWindow(s) => format!("stage-window:{s}"),
            Action::StageMapping(s) => format!("stage-mapping:{s}"),
            other => format!("{other:?}"),
        }
    }

    fn parse(id: &str) -> Option<Action> {
        if let Some(i) = id.strip_prefix("cue:") {
            return i.parse().ok().map(Action::NewCue);
        }
        if let Some(s) = id.strip_prefix("stage-window:") {
            return s.parse().ok().map(Action::StageWindow);
        }
        if let Some(s) = id.strip_prefix("stage-mapping:") {
            return s.parse().ok().map(Action::StageMapping);
        }
        use Action::*;
        [
            New, Open, Save, SaveAs, Import, Quit, Undo, Redo, Duplicate, Delete, Group, Renumber, Settings, Go, Stop, Pause,
            Resume, ToggleMode, Stages, Share, Connect, Disconnect, Preview, Log, OpenVis, Manual, ErrorLog, About,
        ]
        .into_iter()
        .find(|a| a.id() == id)
    }
}

/// What the menu shows besides its fixed texts: enabled items, checkmarks and the stage list.
#[derive(Clone, PartialEq, Debug)]
struct MenuState {
    show_mode: bool,
    can_undo: bool,
    can_redo: bool,
    replica: bool,
    preview: bool,
    log: bool,
    /// Per stage: id, name, window open, mapping being edited.
    stages: Vec<(StageId, String, bool, bool)>,
}

/// The items that change after the menu is built.
struct Items {
    edit: Submenu,
    cue: Submenu,
    undo: MenuItem,
    redo: MenuItem,
    mode: MenuItem,
    disconnect: MenuItem,
    preview: CheckMenuItem,
    log: CheckMenuItem,
    stages: Vec<(CheckMenuItem, CheckMenuItem)>,
}

/// The installed native menu bar.
pub(super) struct NativeMenu {
    rx: Receiver<String>,
    menu: Menu,
    items: Items,
    /// Language and stage list the menu was built for; a change rebuilds it.
    built_for: (i18n::Lang, Vec<(StageId, String)>),
    shown: Option<MenuState>,
}

fn item(action: Action, text: impl AsRef<str>) -> MenuItem {
    MenuItem::with_id(action.id(), text, true, None)
}

fn check(action: Action, text: impl AsRef<str>) -> CheckMenuItem {
    CheckMenuItem::with_id(action.id(), text, true, false, None)
}

fn sep() -> PredefinedMenuItem {
    PredefinedMenuItem::separator()
}

fn submenu(text: &str, items: &[&dyn IsMenuItem]) -> Submenu {
    Submenu::with_items(text, true, items).expect("menu")
}

impl NativeMenu {
    /// Installs the menu bar; a click wakes the UI (`ctx`) to handle it.
    fn new(ctx: &egui::Context, state: &MenuState) -> Self {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let _ = tx.send(e.id.0);
            ctx.request_repaint();
        }));
        let (menu, items) = Self::build(state);
        menu.init_for_nsapp();
        NativeMenu {
            rx,
            menu,
            items,
            built_for: (i18n::lang(), state.stages.iter().map(|s| (s.0, s.1.clone())).collect()),
            shown: None,
        }
    }

    fn build(state: &MenuState) -> (Menu, Items) {
        let menu = Menu::new();

        // Application menu (macOS names it after the app).
        let settings = MenuItem::with_id(
            Action::Settings.id(),
            t("Settings…"),
            true,
            Some(Accelerator::new(Modifiers::META, Code::Comma)),
        );
        let quit = MenuItem::with_id(Action::Quit.id(), t("Quit OpenQ"), true, Some(Accelerator::new(Modifiers::META, Code::KeyQ)));
        let hide = PredefinedMenuItem::hide(None);
        let hide_others = PredefinedMenuItem::hide_others(None);
        let show_all = PredefinedMenuItem::show_all(None);
        let (s1, s2, s3) = (sep(), sep(), sep());
        let app = submenu(
            "OpenQ",
            &[&item(Action::About, t("About OpenQ and licenses…")), &s1, &settings, &s2, &hide, &hide_others, &show_all, &s3, &quit],
        );

        let a = sep();
        let file = submenu(
            t("File"),
            &[
                &item(Action::New, t("New")),
                &item(Action::Open, t("Open…")),
                &item(Action::Save, t("Save")),
                &item(Action::SaveAs, t("Save As…")),
                &a,
                &item(Action::Import, t("Import media files…")),
            ],
        );

        let undo = item(Action::Undo, t("Undo"));
        let redo = item(Action::Redo, t("Redo"));
        let (a, b) = (sep(), sep());
        let edit = submenu(
            t("Edit"),
            &[
                &undo,
                &redo,
                &a,
                &item(Action::Duplicate, t("Duplicate")),
                &item(Action::Delete, t("Delete")),
                &item(Action::Group, t("Group selected")),
                &b,
                &item(Action::Renumber, t("Renumber all cues")),
            ],
        );

        let cue = Submenu::new(t("Cue"), true);
        for (i, tag) in KindTag::ALL.iter().enumerate().filter(|(_, t)| t.enabled()) {
            let _ = cue.append(&item(Action::NewCue(i), tr!("New {kind} cue", kind = tag.label())));
        }

        let mode = item(Action::ToggleMode, t("Enter show mode"));
        let a = sep();
        let show_menu = submenu(
            t("Show"),
            &[
                &item(Action::Go, t("GO")),
                &item(Action::Stop, t("Stop all")),
                &item(Action::Pause, t("Pause all")),
                &item(Action::Resume, t("Resume all")),
                &a,
                &mode,
            ],
        );

        let video = Submenu::new(t("Video"), true);
        let mut stages = Vec::new();
        for (id, name, _, _) in &state.stages {
            let window = check(Action::StageWindow(*id), t("Show stage window"));
            let mapping = check(Action::StageMapping(*id), t("Edit mapping"));
            let sub = submenu(&tr!("Stage: {name}", name = name), &[&window, &mapping]);
            let _ = video.append(&sub);
            stages.push((window, mapping));
        }
        if !stages.is_empty() {
            let _ = video.append(&sep());
        }
        let _ = video.append(&item(Action::Stages, t("Stages & mapping…")));

        let disconnect = item(Action::Disconnect, t("Disconnect"));
        let remote = submenu(
            t("Remote"),
            &[&item(Action::Share, t("Share this OpenQ…")), &item(Action::Connect, t("Connect to another OpenQ…")), &disconnect],
        );

        let preview = check(Action::Preview, t("Geometry preview"));
        let log = check(Action::Log, t("Engine log"));
        let view = Submenu::new(t("View"), true);
        if cfg!(feature = "video") {
            let _ = view.append(&preview);
        }
        let _ = view.append(&log);
        if super::lighting::ENABLED {
            let _ = view.append(&sep());
            let _ = view.append(&item(Action::OpenVis, t("Open in OpenVis")));
        }

        let window = submenu(
            t("Window"),
            &[&PredefinedMenuItem::minimize(None), &PredefinedMenuItem::maximize(None), &sep(), &PredefinedMenuItem::bring_all_to_front(None)],
        );

        let (a, help) = (sep(), Submenu::new(t("Help"), true));
        let _ = help.append(&item(Action::Manual, t("User manual")));
        let _ = help.append(&a);
        let _ = help.append(&item(Action::ErrorLog, t("Error log")));

        for sub in [&app, &file, &edit, &cue, &show_menu, &video, &remote, &view, &window, &help] {
            let _ = menu.append(sub);
        }
        window.set_as_windows_menu_for_nsapp();
        help.set_as_help_menu_for_nsapp();
        (menu, Items { edit, cue, undo, redo, mode, disconnect, preview, log, stages })
    }

    /// Brings the menu in line with `state`, rebuilding it when the language or stages changed.
    fn sync(&mut self, state: &MenuState) {
        let built_for = (i18n::lang(), state.stages.iter().map(|s| (s.0, s.1.clone())).collect::<Vec<_>>());
        if built_for != self.built_for {
            let (menu, items) = Self::build(state);
            menu.init_for_nsapp();
            self.menu = menu;
            self.items = items;
            self.built_for = built_for;
            self.shown = None;
        }
        if self.shown.as_ref() == Some(state) {
            return;
        }
        let it = &self.items;
        let edit_ok = !state.show_mode;
        it.edit.set_enabled(edit_ok);
        it.cue.set_enabled(edit_ok);
        it.undo.set_enabled(state.can_undo);
        it.redo.set_enabled(state.can_redo);
        it.disconnect.set_enabled(state.replica);
        it.mode.set_text(if state.show_mode { t("Back to edit mode") } else { t("Enter show mode") });
        it.preview.set_checked(state.preview);
        it.log.set_checked(state.log);
        for ((window, mapping), (_, _, open, editing)) in it.stages.iter().zip(&state.stages) {
            window.set_checked(*open);
            mapping.set_checked(*editing);
        }
        self.shown = Some(state.clone());
    }
}

impl App {
    /// Whether the menus are in the system menu bar (then the in-window menu bar is not drawn).
    pub(super) fn has_native_menu(&self) -> bool {
        self.native_menu.is_some()
    }

    /// Installs the native menu bar on the first frame, keeps it current and handles its clicks.
    pub(super) fn native_menu_frame(&mut self, ctx: &egui::Context, show: &mut Show) {
        if cfg!(test) || std::env::var_os("OPENQ_SCREENSHOT").is_some() {
            return;
        }
        let state = MenuState {
            show_mode: self.mode == Mode::Show,
            can_undo: !self.undo.is_empty() || self.undo_pending.is_some(),
            can_redo: !self.redo.is_empty(),
            replica: self.remote.is_replica(),
            preview: cfg!(feature = "video") && self.show_preview,
            log: self.show_log,
            stages: show
                .ws
                .stages
                .iter()
                .map(|s| (s.id, s.name.clone(), self.stage_open.contains(&s.id), self.mapping_edit.contains(&s.id)))
                .collect(),
        };
        let menu = self.native_menu.get_or_insert_with(|| NativeMenu::new(ctx, &state));
        menu.sync(&state);
        let picked: Vec<Action> = menu.rx.try_iter().filter_map(|id| Action::parse(&id)).collect();
        for action in picked {
            self.run_action(ctx, show, action);
        }
    }

    fn run_action(&mut self, ctx: &egui::Context, show: &mut Show, action: Action) {
        match action {
            Action::New => self.deferred.push(Deferred::New),
            Action::Open => self.deferred.push(Deferred::Open),
            Action::Save => self.deferred.push(Deferred::Save),
            Action::SaveAs => self.deferred.push(Deferred::SaveAs),
            Action::Import => self.deferred.push(Deferred::AddMediaCues),
            Action::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Action::Undo => self.undo(show),
            Action::Redo => self.redo(show),
            Action::Duplicate => self.duplicate_selected(show),
            Action::Delete => self.delete_selected(show),
            Action::Group => self.group_selected(show),
            Action::Renumber => self.renumber_all(show),
            Action::Settings => self.show_settings = true,
            Action::NewCue(i) => {
                if let Some(tag) = KindTag::ALL.get(i).filter(|t| t.enabled()) {
                    self.add_cue(show, *tag);
                }
            }
            Action::Go => self.control(show, Command::Go),
            Action::Stop => self.control(show, Command::Stop),
            Action::Pause => self.control(show, Command::PauseAll),
            Action::Resume => self.control(show, Command::ResumeAll),
            Action::ToggleMode => self.toggle_mode(),
            Action::StageWindow(id) => {
                if !self.stage_open.remove(&id) {
                    self.stage_open.insert(id);
                }
            }
            Action::StageMapping(id) => {
                if self.mapping_edit.remove(&id) {
                    return;
                }
                self.mapping_edit.insert(id);
                self.stage_open.insert(id);
            }
            Action::Stages => {
                self.show_settings = true;
                self.settings_tab = super::SettingsTab::Stages;
            }
            Action::Share => {
                self.remote.window = true;
                self.remote.tab = super::remote::RemoteTab::Share;
            }
            Action::Connect => {
                self.remote.window = true;
                self.remote.tab = super::remote::RemoteTab::Connect;
            }
            Action::Disconnect => self.disconnect(show),
            Action::Preview => self.show_preview = !self.show_preview,
            Action::Log => self.show_log = !self.show_log,
            Action::OpenVis => self.deferred.push(super::Deferred::OpenVis),
            Action::Manual => self.open_manual(),
            Action::ErrorLog => self.open_error_log(),
            Action::About => self.show_about = true,
        }
    }
}

/// Centres the close / minimise / zoom buttons in a title bar `bar_h` points tall (macOS puts them in
/// a 28 pt bar). The buttons live in the system's title bar container, which is resized to match.
/// Called every frame because macOS lays the container out again on resize and fullscreen changes;
/// nothing is touched while it already has the right size.
pub(super) fn place_window_buttons(frame: &eframe::Frame, bar_h: f64) {
    use objc2_app_kit::{NSView, NSWindowButton};
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = frame.window_handle() else { return };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else { return };
    // SAFETY: the handle's view stays alive while the window does, and this runs on the main thread
    // (eframe calls `App::ui` there).
    let view: &NSView = unsafe { h.ns_view.cast::<NSView>().as_ref() };
    let Some(window) = view.window() else { return };
    let Some(close) = window.standardWindowButton(NSWindowButton::NSWindowCloseButton) else { return };
    let Some(titlebar) = (unsafe { close.superview() }) else { return };
    let Some(container) = (unsafe { titlebar.superview() }) else { return };
    let win_h = window.frame().size.height;
    let want = NSRect::new(
        NSPoint::new(0.0, win_h - bar_h),
        NSSize::new(container.frame().size.width, bar_h),
    );
    let have = container.frame();
    if (have.origin.y - want.origin.y).abs() > 0.5 || (have.size.height - want.size.height).abs() > 0.5 {
        unsafe { container.setFrame(want) };
    }
    for which in [NSWindowButton::NSWindowCloseButton, NSWindowButton::NSWindowMiniaturizeButton, NSWindowButton::NSWindowZoomButton] {
        let Some(b) = window.standardWindowButton(which) else { continue };
        let f = b.frame();
        let y = ((bar_h - f.size.height) / 2.0).round();
        if (f.origin.y - y).abs() > 0.5 {
            unsafe { b.setFrameOrigin(NSPoint::new(f.origin.x, y)) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        let stage = uuid::Uuid::new_v4();
        for a in [Action::New, Action::Quit, Action::Log, Action::About, Action::NewCue(7), Action::StageWindow(stage), Action::StageMapping(stage)] {
            assert_eq!(Action::parse(&a.id()), Some(a));
        }
    }
}
