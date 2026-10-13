//! Object audio views: the room with its speakers (Settings → Audio) and a cue's objects
//! (I/O tab). Drag objects or speakers to move them; speakers light up with the gain the
//! objects send them.

use super::icons as ic;
use super::theme;
use crate::spatial::{AudioObject, ObjectAudio, Speaker, default_object, speaker_gains};
use egui::{Color32, DragValue, Pos2, Rect, RichText, Sense, Stroke, Ui, vec2};

/// Room coordinates shown from the centre to the edge of the view (room walls are at 1).
const VIEW: f32 = 1.25;
/// Object colours, by channel.
const COLOURS: [Color32; 8] = [
    theme::BLUE,
    theme::AMBER,
    theme::GREEN,
    theme::RED,
    Color32::from_rgb(0xb0, 0x7c, 0xf0),
    Color32::from_rgb(0x3c, 0xc8, 0xc8),
    Color32::from_rgb(0xf0, 0x7c, 0xb0),
    Color32::from_rgb(0xc8, 0xc8, 0x3c),
];

fn colour(i: usize) -> Color32 {
    COLOURS[i % COLOURS.len()]
}

/// Draw the room. Objects (and speakers when `move_speakers`) can be dragged; returns true when
/// something moved.
pub(super) fn room_view(
    ui: &mut Ui,
    salt: &str,
    speakers: &mut [Speaker],
    move_speakers: bool,
    objects: &mut [AudioObject],
) -> bool {
    let side = ui.available_width().clamp(180.0, 340.0);
    let (rect, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
    let scale = side / 2.0 / VIEW;
    let to_screen = |x: f32, y: f32| rect.center() + vec2(x, y) * scale;
    let to_room = |p: Pos2| {
        let v = (p - rect.center()) / scale;
        (v.x.clamp(-VIEW, VIEW), v.y.clamp(-VIEW, VIEW))
    };
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 6.0, theme::BG_INPUT);
    let room = Rect::from_two_pos(to_screen(-1.0, -1.0), to_screen(1.0, 1.0));
    painter.rect_stroke(room, 2.0, Stroke::new(1.0, theme::BORDER), egui::StrokeKind::Middle);
    painter.line_segment([to_screen(0.0, -1.0), to_screen(0.0, 1.0)], Stroke::new(1.0, theme::BG_HOVER));
    painter.line_segment([to_screen(-1.0, 0.0), to_screen(1.0, 0.0)], Stroke::new(1.0, theme::BG_HOVER));
    painter.text(room.center_top() + vec2(0.0, 3.0), egui::Align2::CENTER_TOP, "FRONT", theme::semibold(9.5), theme::TEXT_FAINT);

    // How hard each speaker is driven (loudest object).
    let mut drive = vec![0f32; speakers.len()];
    for o in objects.iter() {
        for (d, g) in drive.iter_mut().zip(speaker_gains(o, speakers)) {
            *d = d.max(g);
        }
    }
    let mut changed = false;
    let id = ui.id().with(salt);
    for (i, s) in speakers.iter_mut().enumerate() {
        let p = to_screen(s.x, s.y);
        let r = Rect::from_center_size(p, vec2(26.0, 18.0));
        if move_speakers {
            let resp = ui.interact(r, id.with(("spk", i)), Sense::drag());
            if resp.dragged()
                && let Some(ptr) = resp.interact_pointer_pos()
            {
                (s.x, s.y) = to_room(ptr);
                changed = true;
            }
            if resp.hovered() || resp.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
        }
        let lit = Color32::from_rgb(0x2e, 0x30, 0x37).lerp_to_gamma(theme::GREEN, drive[i].clamp(0.0, 1.0) * 0.8);
        painter.rect_filled(r, 4.0, lit);
        painter.rect_stroke(r, 4.0, Stroke::new(1.0, theme::TEXT_FAINT), egui::StrokeKind::Inside);
        painter.text(p, egui::Align2::CENTER_CENTER, &s.name, theme::semibold(10.0), theme::TEXT);
        painter.text(r.center_bottom() + vec2(0.0, 2.0), egui::Align2::CENTER_TOP, format!("out {}", s.output + 1), egui::FontId::proportional(9.0), theme::TEXT_FAINT);
    }
    for (i, o) in objects.iter_mut().enumerate() {
        let p = to_screen(o.x, o.y);
        let c = colour(i);
        if o.spread > 0.0 {
            painter.circle_filled(p, (0.08 + 1.2 * o.spread) * scale, c.gamma_multiply(0.15));
        }
        let resp = ui.interact(Rect::from_center_size(p, vec2(22.0, 22.0)), id.with(("obj", i)), Sense::drag());
        if resp.dragged()
            && let Some(ptr) = resp.interact_pointer_pos()
        {
            (o.x, o.y) = to_room(ptr);
            changed = true;
        }
        if resp.hovered() || resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        let ring = if resp.hovered() || resp.dragged() { theme::TEXT } else { c };
        painter.circle(p, 9.0, c.gamma_multiply(0.85), Stroke::new(1.5, ring));
        painter.text(p, egui::Align2::CENTER_CENTER, (i + 1).to_string(), theme::semibold(10.0), Color32::BLACK);
        resp.on_hover_text(format!("Channel {}: x {:.2}, y {:.2}, spread {:.0} %", i + 1, o.x, o.y, o.spread * 100.0));
    }
    changed
}

/// The object audio part of a cue's Routing card: the room with one object per channel, and
/// their numbers.
pub(super) fn object_editor(ui: &mut Ui, objects: &mut ObjectAudio, src: usize, room: &[Speaker]) {
    if objects.objects.len() != src {
        let mut all = objects.resolved(src);
        all.truncate(src);
        objects.objects = all;
    }
    if room.is_empty() {
        ui.label(RichText::new("No speakers: set up the room in Settings → Audio.").color(theme::AMBER));
    }
    ui.horizontal_top(|ui| {
        let mut speakers = room.to_vec();
        ui.vertical(|ui| {
            ui.set_width(ui.available_width().min(260.0));
            room_view(ui, "cue-room", &mut speakers, false, &mut objects.objects);
        });
        ui.vertical(|ui| {
            egui::Grid::new("objects").num_columns(4).spacing([8.0, 4.0]).show(ui, |ui| {
                ui.label("");
                ui.label(RichText::new("Left – right").weak());
                ui.label(RichText::new("Front – back").weak());
                ui.label(RichText::new("Spread").weak());
                ui.end_row();
                for (i, o) in objects.objects.iter_mut().enumerate() {
                    ui.label(RichText::new(format!("Ch {}", i + 1)).color(colour(i)));
                    ui.add(DragValue::new(&mut o.x).range(-VIEW..=VIEW).speed(0.01).fixed_decimals(2));
                    ui.add(DragValue::new(&mut o.y).range(-VIEW..=VIEW).speed(0.01).fixed_decimals(2));
                    let mut pct = o.spread * 100.0;
                    if ui.add(DragValue::new(&mut pct).range(0.0..=100.0).speed(0.5).fixed_decimals(0).suffix(" %")).changed() {
                        o.spread = pct / 100.0;
                    }
                    ui.end_row();
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.small_button("Reset positions").clicked() {
                    objects.objects = (0..src).map(|i| default_object(i, src)).collect();
                }
                if ui.small_button("All to centre").on_hover_text("Every channel at the middle of the room").clicked() {
                    objects.objects.iter_mut().for_each(|o| (o.x, o.y) = (0.0, 0.0));
                }
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new("Drag the numbered objects. Fade cues can move them. Output faders on the Levels tab still apply.")
                    .weak()
                    .small(),
            );
        });
    });
}

/// Settings → Audio: the room's speakers. Empty = the default layout for `channels` outputs.
pub(super) fn speaker_editor(ui: &mut Ui, speakers: &mut Vec<Speaker>, channels: usize) {
    let custom = !speakers.is_empty();
    let mut shown = if custom { speakers.clone() } else { crate::spatial::default_room(channels) };
    let mut edited = false;
    ui.horizontal(|ui| {
        ui.label(theme::with_icon(ic::SURROUND_SOUND, "Room for object audio"));
        ui.label(RichText::new(if custom { "custom layout" } else { "default layout for this device" }).weak());
    });
    ui.horizontal(|ui| {
        ui.label("Preset");
        for (label, layout) in [
            ("Stereo", crate::spatial::default_room(2)),
            ("Quad", crate::spatial::default_room(4)),
            ("5.1", crate::spatial::default_room(6)),
            ("7.1", crate::spatial::default_room(8)),
            ("Ring", crate::spatial::ring(channels.max(1))),
        ] {
            if ui.small_button(label).clicked() {
                shown = layout;
                edited = true;
            }
        }
        if custom && ui.small_button("Use default").on_hover_text("Follow the output device's channel count").clicked() {
            speakers.clear();
        }
    });
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.set_width(300.0);
            edited |= room_view(ui, "settings-room", &mut shown, true, &mut []);
        });
        ui.vertical(|ui| {
            let mut remove = None;
            egui::Grid::new("speakers").num_columns(5).spacing([6.0, 4.0]).show(ui, |ui| {
                ui.label(RichText::new("Name").weak());
                ui.label(RichText::new("Output").weak());
                ui.label(RichText::new("x").weak());
                ui.label(RichText::new("y").weak());
                ui.label("");
                ui.end_row();
                for (i, s) in shown.iter_mut().enumerate() {
                    edited |= ui.add(egui::TextEdit::singleline(&mut s.name).desired_width(50.0)).changed();
                    let mut out = s.output + 1;
                    if ui.add(DragValue::new(&mut out).range(1..=crate::model::MAX_OUT_CHANNELS as u16)).changed() {
                        s.output = out - 1;
                        edited = true;
                    }
                    edited |= ui.add(DragValue::new(&mut s.x).range(-VIEW..=VIEW).speed(0.01).fixed_decimals(2)).changed();
                    edited |= ui.add(DragValue::new(&mut s.y).range(-VIEW..=VIEW).speed(0.01).fixed_decimals(2)).changed();
                    if theme::icon_button(ui, ic::DELETE, "Remove speaker").clicked() {
                        remove = Some(i);
                    }
                    ui.end_row();
                }
            });
            if let Some(i) = remove {
                shown.remove(i);
                edited = true;
            }
            if ui.button(theme::with_icon(ic::ADD, "Add speaker")).clicked() {
                let output = shown.iter().map(|s| s.output + 1).max().unwrap_or(0).min(crate::model::MAX_OUT_CHANNELS as u16 - 1);
                shown.push(Speaker { name: format!("{}", shown.len() + 1), output, x: 0.0, y: 0.0 });
                edited = true;
            }
            ui.add_space(4.0);
            ui.label(RichText::new("Front is the top. Speakers can share an output. The room is saved with the show.").weak().small());
        });
    });
    if edited {
        *speakers = shown;
    }
}
