//! Inspector editors for effect stacks: Audio FX (Audio, Video soundtrack, Mic cues) and
//! Video FX (Video, Camera, Text cues). Each effect is a card with its on/off switch, order
//! and delete buttons and one slider per setting; the last card adds effects.

use super::icons as ic;
use super::theme::{self, Card};
use crate::effects::{AudioEffect, AudioFxKind, MAX_AUDIO_FX, MAX_VIDEO_FX, ParamSpec, VideoEffect, VideoFxKind};
use egui::{RichText, Slider, Ui};

/// A slider for one effect setting.
pub(super) fn param_slider(ui: &mut Ui, spec: &ParamSpec, v: &mut f32) -> bool {
    let span = spec.max - spec.min;
    let decimals = if span <= 2.0 { 2 } else if span <= 50.0 { 1 } else { 0 };
    if !v.is_finite() {
        *v = spec.default;
    }
    ui.add(
        Slider::new(v, spec.min..=spec.max)
            .logarithmic(spec.log)
            .fixed_decimals(decimals)
            .suffix(if spec.unit.is_empty() { String::new() } else { format!(" {}", spec.unit) }),
    )
    .changed()
}

/// What a card's tool row asked for.
enum Action {
    Up,
    Down,
    Remove,
}

/// The on/off switch and order/delete buttons at the top of an effect card.
fn tool_row(ui: &mut Ui, enabled: &mut bool, i: usize, n: usize) -> Option<Action> {
    let mut act = None;
    ui.horizontal(|ui| {
        ui.checkbox(enabled, "On");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if theme::icon_button(ui, ic::DELETE, "Remove effect").clicked() {
                act = Some(Action::Remove);
            }
            if i + 1 < n && theme::icon_button(ui, ic::ARROW_DOWNWARD, "Later in the chain").clicked() {
                act = Some(Action::Down);
            }
            if i > 0 && theme::icon_button(ui, ic::ARROW_UPWARD, "Earlier in the chain").clicked() {
                act = Some(Action::Up);
            }
        });
    });
    act
}

fn apply<T>(list: &mut Vec<T>, i: usize, act: Option<Action>) {
    match act {
        Some(Action::Up) if i > 0 => list.swap(i, i - 1),
        Some(Action::Down) if i + 1 < list.len() => list.swap(i, i + 1),
        Some(Action::Remove) => {
            list.remove(i);
        }
        _ => {}
    }
}

fn param_grid(ui: &mut Ui, salt: (&str, usize), specs: &[ParamSpec], params: &mut [f32]) {
    egui::Grid::new(salt).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        for (s, v) in specs.iter().zip(params.iter_mut()) {
            ui.label(s.name);
            param_slider(ui, s, v);
            ui.end_row();
        }
    });
}

/// The Audio FX tab: the cue's effect chain, run in order after the EQ.
pub(super) fn audio_fx_tab(ui: &mut Ui, fx: &mut Vec<AudioEffect>) {
    let titles: Vec<String> = fx.iter().enumerate().map(|(i, e)| format!("{}. {}", i + 1, e.kind.label())).collect();
    let mut specs: Vec<Card> = titles.iter().map(|t| Card::new(ic::INSTANT_MIX, t, 300.0)).collect();
    specs.push(Card::new(ic::ADD_CIRCLE, "Add effect", 260.0));
    let n = fx.len();
    let mut action = None;
    theme::cards(ui, "audio-fx", &specs, |ui, i| {
        if i == n {
            add_menu(ui, "add-afx", n < MAX_AUDIO_FX, AudioFxKind::ALL.iter().map(|k| (*k, k.label())), |k| fx.push(AudioEffect::new(k)));
            ui.add_space(6.0);
            ui.label(RichText::new("Effects run top to bottom on the cue's channels, after the EQ and before routing.").weak().small());
            return;
        }
        let e = &mut fx[i];
        if let Some(a) = tool_row(ui, &mut e.enabled, i, n) {
            action = Some((i, a));
        }
        ui.add_enabled_ui(e.enabled, |ui| {
            let specs = e.kind.params();
            param_grid(ui, ("afx", i), specs, &mut e.params[..specs.len()]);
        });
    });
    if let Some((i, a)) = action {
        apply(fx, i, Some(a));
    }
}

/// The Video FX tab: the layer's effect stack.
pub(super) fn video_fx_tab(ui: &mut Ui, fx: &mut Vec<VideoEffect>) {
    let titles: Vec<String> = fx.iter().enumerate().map(|(i, e)| format!("{}. {}", i + 1, e.kind.label())).collect();
    let mut specs: Vec<Card> = titles.iter().map(|t| Card::new(ic::AUTO_FIX_HIGH, t, 300.0)).collect();
    specs.push(Card::new(ic::ADD_CIRCLE, "Add effect", 260.0));
    let n = fx.len();
    let mut action = None;
    theme::cards(ui, "video-fx", &specs, |ui, i| {
        if i == n {
            let colour = fx.iter().filter(|e| !e.kind.is_spatial()).count();
            add_menu(ui, "add-vfx", colour < MAX_VIDEO_FX, VideoFxKind::ALL.iter().map(|k| (*k, k.label())), |k| fx.push(VideoEffect::new(k)));
            ui.add_space(6.0);
            ui.label(
                RichText::new("Colour effects run top to bottom; Blur, Pixellate and Sharpen always act first. Fade cues can fade these settings.")
                    .weak()
                    .small(),
            );
            return;
        }
        let e = &mut fx[i];
        if let Some(a) = tool_row(ui, &mut e.enabled, i, n) {
            action = Some((i, a));
        }
        ui.add_enabled_ui(e.enabled, |ui| video_effect_settings(ui, i, e));
    });
    if let Some((i, a)) = action {
        apply(fx, i, Some(a));
    }
}

/// The settings of one video effect (also used by Fade cues for their target values).
pub(super) fn video_effect_settings(ui: &mut Ui, i: usize, e: &mut VideoEffect) {
    let specs = e.kind.params();
    egui::Grid::new(("vfx", i)).num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        if e.kind.has_color() {
            ui.label(if e.kind == VideoFxKind::ChromaKey { "Key colour" } else { "Colour" });
            let mut c = egui::Color32::from_rgb(e.color[0], e.color[1], e.color[2]);
            if egui::color_picker::color_edit_button_srgba(ui, &mut c, egui::color_picker::Alpha::Opaque).changed() {
                e.color = [c.r(), c.g(), c.b()];
            }
            ui.end_row();
        }
        for (s, v) in specs.iter().zip(e.params.iter_mut()) {
            ui.label(s.name);
            param_slider(ui, s, v);
            ui.end_row();
        }
    });
}

/// A menu of effect types; `add` is called with the one picked.
fn add_menu<K: Copy>(ui: &mut Ui, salt: &str, room: bool, kinds: impl Iterator<Item = (K, &'static str)>, mut add: impl FnMut(K)) {
    ui.add_enabled_ui(room, |ui| {
        egui::ComboBox::from_id_salt(salt).width(200.0).selected_text(theme::with_icon(ic::ADD, "Add…")).show_ui(ui, |ui| {
            for (k, label) in kinds {
                if ui.selectable_label(false, label).clicked() {
                    add(k);
                }
            }
        });
    });
    if !room {
        ui.label(RichText::new("The chain is full.").weak().small());
    }
}
