//! Settings → Control surfaces: the MIDI input of this computer, the show's mappings (with
//! learn), and the per-frame glue (MIDI connection, learned inputs, a replica's commands).

use super::{App, icons, theme};
use crate::i18n::t;
use crate::show::Show;
use crate::surface::{Mapping, MidiSurface, SurfaceAction, SurfaceInput, SurfaceSettings};
use egui::{DragValue, RichText, Ui};

/// Where a learned input goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum LearnInto {
    /// A new mapping.
    New,
    /// Mapping `i`.
    Row(usize),
}

impl App {
    /// Keep the MIDI surface on the chosen port, take a learned input, and route the commands
    /// a replica's surfaces queued. Called every frame.
    pub(super) fn sync_surfaces(&mut self, ctx: &egui::Context, show: &mut Show) {
        if self.midi_surface.as_ref().map(|m| &m.port) != self.midi_port.as_ref() {
            self.midi_surface = self.midi_port.clone().map(|p| {
                let c = ctx.clone();
                MidiSurface::start(p, self.show.clone(), self.surfaces.clone(), move || c.request_repaint())
            });
        }
        if let Some(input) = self.surfaces.take_learned() {
            let maps = &mut show.ws.settings.surfaces.mappings;
            match self.learn_into.take() {
                Some(LearnInto::Row(i)) if i < maps.len() => maps[i].input = input,
                Some(_) => {
                    // A new mapping: a fader for continuous controls, a GO for buttons.
                    let action = match input {
                        SurfaceInput::MidiCc { .. } => SurfaceAction::ExecutorLevel(1),
                        _ => SurfaceAction::ExecutorGo(1),
                    };
                    maps.push(Mapping { input, action });
                }
                None => return,
            }
            show.edited();
        }
        if !self.surfaces.learning() {
            self.learn_into = None;
        }
        for c in self.surfaces.take_pending() {
            self.control(show, c);
        }
    }

    /// Settings → Control surfaces.
    pub(super) fn settings_surfaces(&mut self, ui: &mut Ui, show: &mut Show) {
        super::devices::ensure(&self.inputs, ui.ctx());
        let devs = self.inputs.lock().clone();
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        egui::Grid::new("surface-midi").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label(t("MIDI input"));
            ui.horizontal(|ui| {
                let mut port = self.midi_port.clone();
                let label = port.clone().unwrap_or_else(|| t("None").into());
                egui::ComboBox::from_id_salt("surface-port").width(240.0).selected_text(label).show_ui(ui, |ui| {
                    ui.selectable_value(&mut port, None, t("None"));
                    for p in &devs.midi_in {
                        ui.selectable_value(&mut port, Some(p.clone()), p);
                    }
                });
                if theme::icon_button(ui, icons::REFRESH, t("Look for devices again")).clicked() {
                    super::devices::refresh(&self.inputs, ui.ctx());
                }
                if port != self.midi_port {
                    if let Err(e) = crate::surface::save_midi_port(port.as_deref()) {
                        self.notify_error(tr!("Can't save the MIDI input: {e}", e = format!("{e:#}")));
                    }
                    self.midi_port = port;
                }
            });
            ui.end_row();
            ui.label("");
            let status = self.midi_surface.as_ref().map(|m| m.status());
            match status {
                Some(st) if st.connected.is_some() => ui.label(RichText::new(t("connected")).color(theme::GREEN).small()),
                Some(st) => ui.label(RichText::new(st.error.unwrap_or_else(|| t("connecting…").into())).color(theme::AMBER).small()),
                None => ui.label(RichText::new(t("This computer's MIDI controller (OSC comes through the OSC listener).")).weak().small()),
            };
            ui.end_row();
            ui.label(t("Last input"));
            match self.surfaces.last() {
                Some(l) => {
                    let v = l.value.map_or_else(String::new, |v| format!(" = {:.0} %", v * 100.0));
                    ui.label(RichText::new(format!("{}{v}", l.input.label())).font(theme::mono(12.0)))
                }
                None => ui.label(RichText::new("—").weak()),
            };
            ui.end_row();
            ui.label(t("MIDI clock"));
            ui.horizontal(|ui| {
                let mut on = show.ws.settings.surfaces.midi_clock;
                if ui.add_enabled(self.can_edit_show(), egui::Checkbox::new(&mut on, t("Effects follow its tempo"))).changed() {
                    show.ws.settings.surfaces.midi_clock = on;
                    show.edited();
                }
                match self.surfaces.clock() {
                    Some(bpm) => ui.label(RichText::new(format!("{bpm:.1} BPM")).font(theme::mono(12.0)).color(theme::GREEN)),
                    None => ui.label(RichText::new(t("no clock")).weak().small()),
                };
            });
            ui.end_row();
        });
        ui.add_space(8.0);
        let can_edit = self.can_edit_show();
        let learning = self.surfaces.learning();
        let maps_before = show.ws.settings.surfaces.clone();
        let maps = &mut show.ws.settings.surfaces.mappings;
        ui.horizontal(|ui| {
            ui.label(theme::caption(t("Mappings")));
            let new_learning = learning && self.learn_into == Some(LearnInto::New);
            let text = if new_learning { t("Move a control…") } else { t("Learn new") };
            if ui.add_enabled(can_edit, egui::Button::new(theme::with_icon(icons::ADD_CIRCLE, text)).selected(new_learning)).on_hover_text(t("Move a fader or press a button to add it")).clicked() {
                self.surfaces.set_learning(!new_learning);
                self.learn_into = (!new_learning).then_some(LearnInto::New);
            }
            ui.menu_button(theme::with_icon(icons::ADD, t("Executor bank")), |ui| {
                ui.label(RichText::new(t("Faders on CC 1–8 and buttons on notes 36–43, channel 1")).weak());
                if ui.add_enabled(can_edit, egui::Button::new(t("Add 8 executors"))).clicked() {
                    maps.extend(SurfaceSettings::executor_bank(8, 0, 1, 36));
                    ui.close();
                }
            });
        });
        ui.add_space(4.0);
        let mut remove = None;
        egui::ScrollArea::vertical().id_salt("surface-maps").max_height(320.0).show(ui, |ui| {
            egui::Grid::new("surface-maps-grid").num_columns(4).striped(true).spacing([10.0, 4.0]).show(ui, |ui| {
                for (i, m) in maps.iter_mut().enumerate() {
                    let this = learning && self.learn_into == Some(LearnInto::Row(i));
                    let text = match &m.input {
                        _ if this => t("Move a control…").to_string(),
                        // The address is edited next to it.
                        SurfaceInput::Osc(_) => "OSC".to_string(),
                        i => i.label(),
                    };
                    if ui.add_enabled(can_edit, egui::Button::new(RichText::new(text).font(theme::mono(12.0))).selected(this).min_size(egui::vec2(170.0, 0.0)))
                        .on_hover_text(t("Click, then move a control to learn it"))
                        .clicked()
                    {
                        self.surfaces.set_learning(!this);
                        self.learn_into = (!this).then_some(LearnInto::Row(i));
                    }
                    if let SurfaceInput::Osc(addr) = &mut m.input {
                        ui.add_enabled(can_edit, egui::TextEdit::singleline(addr).desired_width(140.0).font(theme::mono(12.0)));
                    } else {
                        ui.label("");
                    }
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(can_edit, |ui| {
                            egui::ComboBox::from_id_salt(("surface-action", i)).width(150.0).selected_text(m.action.label()).show_ui(ui, |ui| {
                                let n = m.action.executor().unwrap_or(1);
                                for k in SurfaceAction::KINDS {
                                    let k = k.with_executor(n);
                                    if ui.selectable_label(m.action.same_kind(k), k.label()).clicked() {
                                        m.action = k;
                                    }
                                }
                            });
                            if let Some(mut n) = m.action.executor() {
                                if ui.add(DragValue::new(&mut n).range(1..=999).prefix("#")).changed() {
                                    m.action = m.action.with_executor(n);
                                }
                                let name = show.ws.lighting.sequences.get(n as usize - 1).map(|s| s.name.clone());
                                ui.label(RichText::new(name.unwrap_or_else(|| t("no sequence").into())).color(theme::TEXT_DIM).small());
                            }
                        });
                    });
                    if ui.add_enabled(can_edit, egui::Button::new(icons::DELETE).small()).on_hover_text(t("Remove mapping")).clicked() {
                        remove = Some(i);
                    }
                    ui.end_row();
                }
            });
        });
        if let Some(i) = remove {
            maps.remove(i);
            self.surfaces.set_learning(false);
            self.learn_into = None;
        }
        if maps.is_empty() {
            ui.label(RichText::new(t("No mappings: learn a control, or add an executor bank.")).color(theme::TEXT_DIM));
        }
        ui.add_space(6.0);
        ui.label(
            RichText::new(t("OSC also takes /light/executor/<n>/level, /go, /release, /flash, /light/grandmaster, /light/blackout, /light/release and /light/tap without mappings."))
                .weak()
                .small(),
        );
        if show.ws.settings.surfaces != maps_before {
            show.edited();
        }
    }
}
